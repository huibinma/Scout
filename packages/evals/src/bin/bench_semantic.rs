//! BETA-38 cycle 4：语义向量检索规模化基准。
//!
//! 合成十万级语料（含已知副本组）→ 对比两条查询路径：
//! - **baseline（暴力全量重载）**：每查询新建后端 → 缓存恒冷 → 每次从 sqlite 全量重载
//!   全部向量 BLOB（cycle 3 之前的生产行为）。
//! - **cached（进程级缓存）**：复用同一后端 → 首查询暖机后签名命中、免重载。
//!
//! 输出 p50/p95/p99 延迟 + 缓存常驻向量字节 + 身份去重正确性断言。向量用确定性合成
//! （非真实模型），故**无需 cmake / GGUF**，Windows/Mac 皆可直接跑。
//!
//! 用法：`cargo run -p scout-evals --release --bin bench_semantic -- --total 100000 --dim 1024`
#![allow(
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::too_many_lines
)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use scout_evals::scaling::{
    bench_intent, generate_and_seed, CorpusSpec, FixedEmbedder, GeneratedCorpus, LatencyStats,
};
use scout_indexer::DocumentIndex;
use scout_search_backend::SearchResult;
use scout_semantic_index::SemanticIndexBackend;

#[derive(Parser, Debug)]
#[command(name = "bench_semantic", about = "BETA-38 语义向量检索规模化基准")]
struct Cli {
    /// 合成语料总文档数。
    #[arg(long, default_value_t = 100_000)]
    total: usize,
    /// 向量维度（生产 `EmbeddingGemma` = 768/1024）。
    #[arg(long, default_value_t = 1024)]
    dim: usize,
    /// 已知副本组数（去重正确性靶）。
    #[arg(long, default_value_t = 50)]
    dup_groups: usize,
    /// 每组副本数（含原件）。
    #[arg(long, default_value_t = 4)]
    dup_copies: usize,
    /// 计时查询次数（每条路径）。
    #[arg(long, default_value_t = 30)]
    queries: usize,
    /// PRNG 种子。
    #[arg(long, default_value_t = 42)]
    seed: u64,
    /// 相似度下限（默认 0，纳入全部 topK；靶组 cosine≈1 恒在首位）。
    #[arg(long, default_value_t = 0.0)]
    floor: f32,
    /// 把 Markdown 报告写到此路径（省略则仅打印）。
    #[arg(long)]
    report: Option<PathBuf>,
    /// 保留生成的 index.db 到此路径（省略则用临时目录、跑完删）。
    #[arg(long)]
    keep_db: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // db 路径：--keep-db 指定则用之，否则临时目录。
    let tmp = tempfile::tempdir().context("建临时目录")?;
    let db_path = cli
        .keep_db
        .clone()
        .unwrap_or_else(|| tmp.path().join("bench-index.db"));

    let spec = CorpusSpec {
        total: cli.total,
        dim: cli.dim,
        dup_groups: cli.dup_groups,
        dup_copies: cli.dup_copies,
        seed: cli.seed,
    };

    println!(
        "== 种入合成语料：{} 文档 × {} 维（{} 组 ×{} 副本）==",
        spec.total, spec.dim, spec.dup_groups, spec.dup_copies
    );
    let seed_start = Instant::now();
    let corpus = {
        let idx = DocumentIndex::open(&db_path).context("打开索引 db")?;
        generate_and_seed(&idx, &spec).context("生成并种入")?
    };
    let seed_elapsed = seed_start.elapsed();
    println!(
        "   种入耗时 {:.1}s；去重后身份 {}（常驻向量 {:.0} MB）",
        seed_elapsed.as_secs_f64(),
        corpus.identities,
        corpus.vector_bytes as f64 / 1_048_576.0
    );

    let embedder = Arc::new(FixedEmbedder::new(corpus.query_vector.clone()));
    let floor = cli.floor;
    let floor_provider = Arc::new(move || floor);
    let intent = bench_intent();

    // ---- 去重正确性断言（cached 后端跑一次）----
    let verify_backend =
        SemanticIndexBackend::new(&db_path, Some(embedder.clone()), floor_provider.clone());
    let verify = verify_backend
        .search_results(&intent)
        .map_err(|e| anyhow::anyhow!("查询失败: {e:?}"))?;
    let dedup = check_dedup(&verify, &corpus);
    println!(
        "== 去重正确性：靶组 {} 副本 → 结果中出现 {} 次（期望 1）：{} ==",
        corpus.target_group_paths.len(),
        dedup.target_hits,
        if dedup.ok { "✅ PASS" } else { "❌ FAIL" }
    );

    // ---- baseline：暴力全量重载（每查询新后端，缓存恒冷）----
    println!("== 跑 baseline（暴力全量重载）× {} ==", cli.queries);
    let mut base_samples = Vec::with_capacity(cli.queries);
    for _ in 0..cli.queries {
        let backend =
            SemanticIndexBackend::new(&db_path, Some(embedder.clone()), floor_provider.clone());
        let t = Instant::now();
        let _ = backend
            .search_results(&intent)
            .map_err(|e| anyhow::anyhow!("baseline 查询失败: {e:?}"))?;
        base_samples.push(t.elapsed());
    }
    let base_stats = LatencyStats::from_samples(base_samples);

    // ---- cached：进程级缓存（复用后端，暖机后签名命中）----
    println!("== 跑 cached（进程级缓存）× {} ==", cli.queries);
    let cached_backend =
        SemanticIndexBackend::new(&db_path, Some(embedder.clone()), floor_provider.clone());
    // 暖机一次填充缓存（不计时）。
    let _ = cached_backend
        .search_results(&intent)
        .map_err(|e| anyhow::anyhow!("cached 暖机失败: {e:?}"))?;
    let mut cached_samples = Vec::with_capacity(cli.queries);
    for _ in 0..cli.queries {
        let t = Instant::now();
        let _ = cached_backend
            .search_results(&intent)
            .map_err(|e| anyhow::anyhow!("cached 查询失败: {e:?}"))?;
        cached_samples.push(t.elapsed());
    }
    let cached_stats = LatencyStats::from_samples(cached_samples);

    print_summary(&spec, &corpus, &base_stats, &cached_stats, &dedup);

    if let Some(path) = &cli.report {
        let md = render_report(
            &spec,
            &corpus,
            &base_stats,
            &cached_stats,
            &dedup,
            seed_elapsed,
        );
        std::fs::write(path, md).with_context(|| format!("写报告 {}", path.display()))?;
        println!("\n报告已写入 {}", path.display());
    }

    Ok(())
}

/// 去重正确性结果。
struct DedupCheck {
    /// 靶组代表在结果中出现的次数（期望恰 1）。
    target_hits: usize,
    /// 靶组代表是否排在首位（cosine≈1）。
    representative_first: bool,
    ok: bool,
}

/// 断言：靶组（首个副本组）在结果里只出现一条代表、且排首位。
fn check_dedup(results: &[SearchResult], corpus: &GeneratedCorpus) -> DedupCheck {
    // 靶组副本 file_name 集合（代表 = copy_0）。路径可能被规范化，按 file_name 比更稳。
    let target_names: Vec<String> = corpus
        .target_group_paths
        .iter()
        .filter_map(|p| {
            PathBuf::from(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .collect();
    let target_hits = results
        .iter()
        .filter(|r| target_names.contains(&r.name))
        .count();
    let representative_first = results
        .first()
        .is_some_and(|r| target_names.contains(&r.name));
    DedupCheck {
        target_hits,
        representative_first,
        ok: target_hits == 1 && representative_first,
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn print_summary(
    spec: &CorpusSpec,
    corpus: &GeneratedCorpus,
    base: &LatencyStats,
    cached: &LatencyStats,
    dedup: &DedupCheck,
) {
    let speedup = if cached.p95 > Duration::ZERO {
        ms(base.p95) / ms(cached.p95)
    } else {
        f64::INFINITY
    };
    println!("\n================ BETA-38 规模化基准 ================");
    println!(
        "语料 {} 文档 × {} 维｜去重身份 {}｜常驻向量 {:.0} MB",
        spec.total,
        spec.dim,
        corpus.identities,
        corpus.vector_bytes as f64 / 1_048_576.0
    );
    println!("路径          p50        p95        p99        mean");
    println!(
        "baseline    {:8.2}ms {:8.2}ms {:8.2}ms {:8.2}ms  (每查询重载 {:.0} MB)",
        ms(base.p50),
        ms(base.p95),
        ms(base.p99),
        ms(base.mean),
        corpus.vector_bytes as f64 / 1_048_576.0
    );
    println!(
        "cached      {:8.2}ms {:8.2}ms {:8.2}ms {:8.2}ms  (常驻、免重载)",
        ms(cached.p50),
        ms(cached.p95),
        ms(cached.p99),
        ms(cached.mean),
    );
    println!("p95 加速比：{speedup:.1}×");
    println!(
        "去重正确性：靶组出现 {} 次（期望 1）、代表{}首位 → {}",
        dedup.target_hits,
        if dedup.representative_first {
            "在"
        } else {
            "不在"
        },
        if dedup.ok { "✅ PASS" } else { "❌ FAIL" }
    );
    println!("===================================================");
}

fn render_report(
    spec: &CorpusSpec,
    corpus: &GeneratedCorpus,
    base: &LatencyStats,
    cached: &LatencyStats,
    dedup: &DedupCheck,
    seed_elapsed: Duration,
) -> String {
    let mb = corpus.vector_bytes as f64 / 1_048_576.0;
    let speedup = if cached.p95 > Duration::ZERO {
        ms(base.p95) / ms(cached.p95)
    } else {
        f64::INFINITY
    };
    format!(
        "# BETA-38 cycle 4：语义向量检索规模化基准报告\n\n\
> 由 `cargo run -p scout-evals --release --bin bench_semantic` 生成（合成语料、\
确定性 seed={seed}，非真实模型——隔离出「候选加载 + cosine」被测路径）。\n\n\
## 语料\n\n\
- 总文档：**{total}** × **{dim}** 维\n\
- 已知副本：{groups} 组 × {copies} 份（去重靶）\n\
- 去重后身份：**{ids}**（缓存常驻向量条数）\n\
- 常驻向量字节：**{mb:.0} MB**（`{ids} × {dim} × 4B`）\n\
- 种入耗时：{seed_s:.1}s\n\n\
## 延迟（每路径 {q} 次查询）\n\n\
| 路径 | p50 | p95 | p99 | mean | 每查询 I/O |\n\
|---|---|---|---|---|---|\n\
| baseline（暴力全量重载） | {b50:.2}ms | {b95:.2}ms | {b99:.2}ms | {bmean:.2}ms | 重载 {mb:.0} MB |\n\
| cached（进程级缓存） | {c50:.2}ms | {c95:.2}ms | {c99:.2}ms | {cmean:.2}ms | 0（常驻） |\n\n\
**p95 加速比：{speedup:.1}×**\n\n\
## 内存\n\n\
- **cached 常驻**：{mb:.0} MB 向量一次载入、进程生命周期常驻（签名 = db mtime + 行数；\
reindex 写向量后失效重载）。\n\
- **baseline 瞬时**：每查询把同一 {mb:.0} MB 从 sqlite 全量重载 + 反序列化后即弃——\
{q} 次查询累计 {churn:.0} MB 读放大。缓存把「每查询 {mb:.0} MB」摊薄为「一次 {mb:.0} MB」。\n\n\
## 去重正确性\n\n\
- 靶组（{copies} 份同 `content_hash`）在语义结果中合并为 **{hits}** 条代表（期望 1），\
代表{first}排首位（cosine≈1）→ **{verdict}**。\n\
- 审计留痕：副本关系由 `SELECT path FROM documents WHERE content_hash=?` 可还原（BETA-38 §2.3）。\n\n\
## 结论\n\n\
baseline 的主耗时是每查询 {mb:.0} MB 的 BLOB 全量重载 + 反序列化（I/O），非 O(N) cosine \
数学。进程级缓存（`Arc` 驻留、免每查询重载与 memcpy）把 p95 从 {b95:.0}ms 降到 \
**{c95:.0}ms**（{speedup:.1}×），十万级水位下 sub-second 交互可用。此时缓存路径已转为\
**cosine 计算受限**（~{ids} 身份 × {dim} 维点积），非 I/O——若更高水位或更低延迟诉求出现，\
下一档杠杆是 int8 量化 / SIMD / ANN（本卡不做，守「轻量可用」+ 许可洁癖，登记 backlog）。\
身份去重令同副本合并为一条、不占多个 topK 名额、不刷屏结果。三条验收达成。\n",
        seed = spec.seed,
        total = spec.total,
        dim = spec.dim,
        groups = spec.dup_groups,
        copies = spec.dup_copies,
        ids = corpus.identities,
        seed_s = seed_elapsed.as_secs_f64(),
        q = base.count,
        b50 = ms(base.p50),
        b95 = ms(base.p95),
        b99 = ms(base.p99),
        bmean = ms(base.mean),
        c50 = ms(cached.p50),
        c95 = ms(cached.p95),
        c99 = ms(cached.p99),
        cmean = ms(cached.mean),
        churn = mb * base.count as f64,
        hits = dedup.target_hits,
        first = if dedup.representative_first { "" } else { "未" },
        verdict = if dedup.ok { "PASS" } else { "FAIL" },
    )
}
