//! BETA-26 探针主产出：在冻结语料 + 评测集上跑三组检索，输出分桶 Recall@10 / nDCG@10，
//! 给出 go/no-go 数字。三组配置：
//!   A. FTS5-only —— trigram tokenizer 上的 BM25，是要打败的基线（今日关键词搜索的真实写照）。
//!   B. vector-only —— 在预算好的 embedding 上做暴力 cosine。
//!   C. hybrid —— A + B 的 RRF 融合。
//!
//! 关键正确性约定：
//! 1. FTS5 中文查询构造（头号坑）：corpus_fts 用 trigram tokenizer，中文无空格，整句当一个
//!    token 做短语 MATCH 会几乎命中不了，会把基线人为做烂、使对比失真。故把 query 归一化后
//!    （只留 is_alphanumeric 的字符，丢空格标点）切成所有重叠 3-char 窗口，去重，每个 trigram
//!    双引号包裹当字面 token，用 OR 连接 —— 与 tokenizer 索引 doc 的方式对齐，得到诚实的
//!    BM25-over-shared-trigrams 排名。归一化后不足 3 字则退化为整串引号匹配。
//! 2. FTS5 索引「全文」，向量只 embed 了前 1200 字 —— 这一不对称是刻意的、反映现实：FTS5 真
//!    的索引全部内容，embedding 受上下文窗口限制。报告里需注明。
//! 3. per-case 输出供「泄漏」检测：把每 case 的三路 recall/ndcg@10 写 retrieval-results.json，
//!    并把 fts_recall >= 0.5 的疑似泄漏 case 直接打到 stdout。
//!
//! BETA-26 分块实验：设环境变量 `VECTORS=chunked` 切到「分块」模式（默认不设 = 读 vectors.bin
//! 整篇截断单向量，原行为）。分块模式下读 vectors-chunked.bin（每行 `<doc_id>\t<chunk_idx>\t<csv>`），
//! 向量检索时对每个 chunk 算 cosine，再按 doc_id 取「该 doc 所有 chunk 的最大 cosine」聚合到 doc
//! 级（max-pooling，chunk→doc 的标准聚合），按这个 max 排名取 top-POOL。FTS5 路径完全不变（仍全文
//! trigram），RRF 仍融合 FTS5 + doc 级向量排名。结果写 retrieval-results-chunked.json（不覆盖截断版）。

use anyhow::{Context, Result};
use model_runtime::{get_default_loader, ModelLoadParams};
use rusqlite::Connection;
use serde::Serialize;
use spike_retrieval::{
    cosine, ndcg_at_k, recall_at_k, rrf_fuse, weighted_rrf_fuse, CorpusDoc, EvalCase,
};
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::Path;

const TOPK: usize = 10;
const POOL: usize = 50;

/// BETA-26 融合策略调优：可选融合模式（env `FUSION`，默认 `rrf` 保持原行为/原输出不变）。
/// - `rrf`      —— 普通 RRF，k 取 env `RRF_K`（默认 60）。基线。
/// - `wrrf`     —— 加权 RRF：FTS 权重 env `RRF_W_FTS`（默认 1.0）、向量权重 env `RRF_W_VEC`（默认 1.0）。
/// - `adaptive` —— query 自适应路由：本 query 的 FTS5 命中集为空（0 命中）则纯向量排名；否则用加权 RRF。
/// - `adaptive2`—— 更软的路由：FTS5 命中数 < env `ADAPT_MIN`（默认 3）则纯向量；否则加权 RRF。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FusionMode {
    Rrf,
    Wrrf,
    Adaptive,
    Adaptive2,
}

/// 一次运行的融合配置（全部从 env 读，带默认值）。
struct FusionConfig {
    mode: FusionMode,
    k: usize,
    w_fts: f64,
    w_vec: f64,
    adapt_min: usize,
}

impl FusionConfig {
    fn from_env() -> Result<Self> {
        let mode = match std::env::var("FUSION").as_deref().unwrap_or("rrf") {
            "rrf" => FusionMode::Rrf,
            "wrrf" => FusionMode::Wrrf,
            "adaptive" => FusionMode::Adaptive,
            "adaptive2" => FusionMode::Adaptive2,
            other => anyhow::bail!("未知 FUSION={other}（可选 rrf|wrrf|adaptive|adaptive2）"),
        };
        let parse_usize = |name: &str, def: usize| -> Result<usize> {
            match std::env::var(name) {
                Ok(s) => s
                    .parse::<usize>()
                    .with_context(|| format!("{name} 不是合法整数: {s}")),
                Err(_) => Ok(def),
            }
        };
        let parse_f64 = |name: &str, def: f64| -> Result<f64> {
            match std::env::var(name) {
                Ok(s) => s
                    .parse::<f64>()
                    .with_context(|| format!("{name} 不是合法浮点: {s}")),
                Err(_) => Ok(def),
            }
        };
        Ok(FusionConfig {
            mode,
            k: parse_usize("RRF_K", 60)?,
            w_fts: parse_f64("RRF_W_FTS", 1.0)?,
            w_vec: parse_f64("RRF_W_VEC", 1.0)?,
            adapt_min: parse_usize("ADAPT_MIN", 3)?,
        })
    }

    /// 是否为「典型基线」：FUSION 缺省=rrf 且 RRF_K=60。只有基线才覆盖正式 retrieval-results.json。
    fn is_canonical_baseline(&self) -> bool {
        self.mode == FusionMode::Rrf && self.k == 60
    }

    /// 人类可读的 variant 标签（用于 stdout 摘要行与变体结果文件名）。
    fn label(&self) -> String {
        match self.mode {
            FusionMode::Rrf => format!("rrf(k={})", self.k),
            FusionMode::Wrrf => {
                format!("wrrf(k={},wf={},wv={})", self.k, self.w_fts, self.w_vec)
            }
            FusionMode::Adaptive => {
                format!("adaptive(k={},wf={},wv={})", self.k, self.w_fts, self.w_vec)
            }
            FusionMode::Adaptive2 => format!(
                "adaptive2(min={},k={},wf={},wv={})",
                self.adapt_min, self.k, self.w_fts, self.w_vec
            ),
        }
    }

    /// 按当前模式融合一条 case 的 FTS / 向量榜单，得到 hybrid 排名。
    fn fuse(&self, fts_ids: &[String], vec_ids: &[String]) -> Vec<String> {
        let fts_v = fts_ids.to_vec();
        let vec_v = vec_ids.to_vec();
        match self.mode {
            FusionMode::Rrf => rrf_fuse(&[&fts_v, &vec_v], self.k),
            FusionMode::Wrrf => {
                weighted_rrf_fuse(&[&fts_v, &vec_v], &[self.w_fts, self.w_vec], self.k)
            }
            FusionMode::Adaptive => {
                // FTS 命中为空 → 纯向量；否则加权 RRF。
                if fts_ids.is_empty() {
                    vec_v
                } else {
                    weighted_rrf_fuse(&[&fts_v, &vec_v], &[self.w_fts, self.w_vec], self.k)
                }
            }
            FusionMode::Adaptive2 => {
                // FTS 命中数 < adapt_min（含 0）→ 纯向量；否则加权 RRF。
                if fts_ids.len() < self.adapt_min {
                    vec_v
                } else {
                    weighted_rrf_fuse(&[&fts_v, &vec_v], &[self.w_fts, self.w_vec], self.k)
                }
            }
        }
    }
}

/// per-case 落盘结构（供用户做泄漏分析）。
#[derive(Serialize)]
struct CaseResult {
    id: String,
    bucket: String,
    query: String,
    fts_recall: f64,
    vec_recall: f64,
    hybrid_recall: f64,
    fts_ndcg: f64,
    vec_ndcg: f64,
    hybrid_ndcg: f64,
}

fn main() -> Result<()> {
    let corpus_path = "packages/spike-retrieval/fixtures/corpus.jsonl";
    let chunk_mode = std::env::var("VECTORS")
        .map(|v| v == "chunked")
        .unwrap_or(false);
    let vectors_path = if chunk_mode {
        "packages/spike-retrieval/fixtures/vectors-chunked.bin"
    } else {
        "packages/spike-retrieval/fixtures/vectors.bin"
    };
    let cases_path = "packages/spike-retrieval/fixtures/evalset/cases.json";
    // BETA-26 融合策略：从 env 读融合配置。
    let fusion = FusionConfig::from_env()?;
    eprintln!("融合模式 = {}", fusion.label());
    // 输出文件规则：
    // - 分块模式写独立结果文件，绝不覆盖截断版（两份并排对比）。
    // - 截断模式下，只有「典型基线」（FUSION 缺省=rrf 且 RRF_K=60）才覆盖正式 retrieval-results.json；
    //   其它融合变体写到系统临时目录（绝不落在仓库内，避免污染基线 / 误提交结果 json）。
    let out_path: String = if chunk_mode {
        "packages/spike-retrieval/fixtures/retrieval-results-chunked.json".to_string()
    } else if fusion.is_canonical_baseline() {
        "packages/spike-retrieval/fixtures/retrieval-results.json".to_string()
    } else {
        std::env::temp_dir()
            .join("beta26-retrieval-results-variant.json")
            .to_string_lossy()
            .into_owned()
    };
    let model = std::env::var("EMBED_MODEL")
        .unwrap_or_else(|_| "models/qwen3-embedding-0.6b-q8_0.gguf".into());

    // 1) 读语料（全文喂 FTS5）。
    let docs = load_corpus(corpus_path)?;
    eprintln!("语料 {} 篇已读入", docs.len());

    // 2) 读预算向量。截断模式：每 doc 一条 (doc_id, vec)；分块模式：每 chunk 一条
    //    (doc_id, chunk_idx, vec)，检索时按 doc max-pool 聚合。
    let vectors: VectorIndex = if chunk_mode {
        let chunks = load_chunked_vectors(vectors_path)?;
        eprintln!(
            "分块向量 {} 块已读入（覆盖 {} 篇 doc）",
            chunks.len(),
            chunks
                .iter()
                .map(|(id, _, _)| id)
                .collect::<HashSet<_>>()
                .len()
        );
        VectorIndex::Chunked(chunks)
    } else {
        let v = load_vectors(vectors_path)?;
        eprintln!("向量 {} 条已读入", v.len());
        VectorIndex::Flat(v)
    };

    // 3) 建内存 FTS5（trigram），插全文。
    let conn = build_fts(&docs)?;
    eprintln!("corpus_fts 已建（trigram，全文索引）");

    // 4) 读评测集。
    let cases: Vec<EvalCase> = serde_json::from_str(
        &std::fs::read_to_string(cases_path)
            .with_context(|| format!("读评测集失败: {cases_path}"))?,
    )
    .with_context(|| format!("解析评测集失败: {cases_path}"))?;
    eprintln!("评测 case {} 条已读入", cases.len());

    // 5) 加载 embedding 运行时（与 embed_corpus.rs 完全一致的加载器用法）。
    let loader = get_default_loader();
    let rt = loader
        .load(
            Path::new(&model),
            &ModelLoadParams {
                gpu_layers: 0,
                context_size: 0,
            },
        )
        .with_context(|| format!("加载 embedding 模型失败: {model}"))?;

    let mut case_results: Vec<CaseResult> = Vec::with_capacity(cases.len());
    // 分桶累加：bucket -> (sum_fts_r, sum_vec_r, sum_hyb_r, sum_fts_n, sum_vec_n, sum_hyb_n, count)
    let mut zero_all: Vec<String> = Vec::new();

    for case in &cases {
        // 相关集合 + grade 表。
        let relevant: HashSet<String> = case.relevant.iter().map(|r| r.doc_id.clone()).collect();
        let grades: HashMap<String, u8> = case
            .relevant
            .iter()
            .map(|r| (r.doc_id.clone(), r.grade))
            .collect();

        // A. FTS5：trigram-OR + bm25。MATCH 出错（畸形）按空结果处理，计数不中断。
        let fts_ids = fts_query(&conn, &case.query).unwrap_or_default();

        // B. vector：query embed 一次，暴力 cosine over all vectors，取 POOL。
        //    分块模式下 vectors 为 chunk 级，vector_query 内部按 doc max-pool 聚合。
        let vec_ids = match rt.embed(&case.query) {
            Ok(qv) if !qv.is_empty() => vectors.query(&qv),
            _ => Vec::new(),
        };

        // C. hybrid：按当前 FUSION 模式融合 A + B（默认 rrf k=60，保持原行为）。
        let hybrid_ids = fusion.fuse(&fts_ids, &vec_ids);

        let fts_recall = recall_at_k(&fts_ids, &relevant, TOPK);
        let vec_recall = recall_at_k(&vec_ids, &relevant, TOPK);
        let hybrid_recall = recall_at_k(&hybrid_ids, &relevant, TOPK);
        let fts_ndcg = ndcg_at_k(&fts_ids, &grades, TOPK);
        let vec_ndcg = ndcg_at_k(&vec_ids, &grades, TOPK);
        let hybrid_ndcg = ndcg_at_k(&hybrid_ids, &grades, TOPK);

        if fts_recall == 0.0 && vec_recall == 0.0 && hybrid_recall == 0.0 {
            zero_all.push(format!("{} [{}]", case.id, case.bucket));
        }

        case_results.push(CaseResult {
            id: case.id.clone(),
            bucket: case.bucket.clone(),
            query: case.query.clone(),
            fts_recall,
            vec_recall,
            hybrid_recall,
            fts_ndcg,
            vec_ndcg,
            hybrid_ndcg,
        });
    }

    // 6) 落盘 per-case。
    std::fs::write(&out_path, serde_json::to_string_pretty(&case_results)?)
        .with_context(|| format!("写 per-case 结果失败: {out_path}"))?;
    eprintln!("per-case 结果已写 {out_path}");

    // 7) 聚合 + 打表。
    print_aggregate(&case_results);

    // 7b) BETA-26 融合调优：单行 variant 摘要，便于跨多次运行拼比较表。
    //     列出 hybrid（当前融合模式）以及 vec-only / fts-only 参考的 4 个关键指标。
    print_variant_summary(&fusion, &case_results);

    // 8) 疑似泄漏 case（fts_recall >= 0.5）。
    print_leaked(&case_results);

    // 9) 三路全 0 的 case（可能误标 / target 不在语料）。
    if zero_all.is_empty() {
        println!("\n== 三路全 0 的 case：无 ==");
    } else {
        println!(
            "\n== 三路全 0 的 case（{} 个，可能误标或 target 不在语料）==",
            zero_all.len()
        );
        for c in &zero_all {
            println!("  {c}");
        }
    }

    Ok(())
}

/// 读 corpus.jsonl 为 doc 列表（全文保留）。
fn load_corpus(path: &str) -> Result<Vec<CorpusDoc>> {
    let f = std::io::BufReader::new(
        std::fs::File::open(path).with_context(|| format!("打开语料失败: {path}"))?,
    );
    let mut docs = Vec::new();
    for line in f.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        docs.push(serde_json::from_str::<CorpusDoc>(&line)?);
    }
    Ok(docs)
}

/// 读 vectors.bin：每行 `<doc_id>\t<comma-sep f32>`。返回 (doc_id, vec) 列表（保序）。
fn load_vectors(path: &str) -> Result<Vec<(String, Vec<f32>)>> {
    let f = std::io::BufReader::new(
        std::fs::File::open(path).with_context(|| format!("打开向量失败: {path}"))?,
    );
    let mut out = Vec::new();
    for line in f.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let (id, csv) = line
            .split_once('\t')
            .with_context(|| format!("向量行缺 tab 分隔: {}", &line[..line.len().min(40)]))?;
        let v: Vec<f32> = csv
            .split(',')
            .map(|s| s.parse::<f32>())
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("解析向量失败: doc {id}"))?;
        out.push((id.to_string(), v));
    }
    Ok(out)
}

/// 读 vectors-chunked.bin：每行 `<doc_id>\t<chunk_idx>\t<comma-sep f32>`。
/// 返回 (doc_id, chunk_idx, vec) 列表（保序）。
fn load_chunked_vectors(path: &str) -> Result<Vec<(String, usize, Vec<f32>)>> {
    let f = std::io::BufReader::new(
        std::fs::File::open(path).with_context(|| format!("打开分块向量失败: {path}"))?,
    );
    let mut out = Vec::new();
    for line in f.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, '\t');
        let id = parts
            .next()
            .with_context(|| format!("分块向量行缺 doc_id: {}", &line[..line.len().min(40)]))?;
        let idx_str = parts
            .next()
            .with_context(|| format!("分块向量行缺 chunk_idx: {}", &line[..line.len().min(40)]))?;
        let csv = parts
            .next()
            .with_context(|| format!("分块向量行缺向量 csv: {}", &line[..line.len().min(40)]))?;
        let idx: usize = idx_str
            .parse()
            .with_context(|| format!("解析 chunk_idx 失败: doc {id} idx={idx_str}"))?;
        let v: Vec<f32> = csv
            .split(',')
            .map(|s| s.parse::<f32>())
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("解析分块向量失败: doc {id} chunk {idx}"))?;
        out.push((id.to_string(), idx, v));
    }
    Ok(out)
}

/// 建内存 FTS5（trigram），插全文。
fn build_fts(docs: &[CorpusDoc]) -> Result<Connection> {
    let conn = Connection::open_in_memory().context("打开内存 sqlite 失败")?;
    conn.execute_batch(
        "CREATE VIRTUAL TABLE corpus_fts USING fts5(id UNINDEXED, text, tokenize='trigram');",
    )
    .context("建 corpus_fts 失败")?;
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare("INSERT INTO corpus_fts(id, text) VALUES (?1, ?2)")?;
        for d in docs {
            stmt.execute((&d.id, &d.text))?;
        }
    }
    tx.commit()?;
    Ok(conn)
}

/// 把中文/混排 query 构造成 FTS5 trigram-OR MATCH 串。
/// 归一化：只留 is_alphanumeric 的字符（CJK + latin + digit），丢空格标点。
/// 然后所有重叠 3-char 窗口去重，各自双引号包裹（转义内部 `"`），OR 连接。
/// 归一化后不足 3 字 → 退化为整串引号匹配。空串返回 None（调用方按空结果处理）。
fn build_match_query(query: &str) -> Option<String> {
    let norm: Vec<char> = query.chars().filter(|c| c.is_alphanumeric()).collect();
    if norm.is_empty() {
        return None;
    }
    let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    if norm.len() < 3 {
        let s: String = norm.iter().collect();
        return Some(quote(&s));
    }
    let mut seen = HashSet::new();
    let mut terms = Vec::new();
    for w in norm.windows(3) {
        let tri: String = w.iter().collect();
        if seen.insert(tri.clone()) {
            terms.push(quote(&tri));
        }
    }
    Some(terms.join(" OR "))
}

/// 跑 FTS5 检索，返回 top-POOL doc id（按 bm25 升序 = 更相关在前）。
/// MATCH 畸形报错时返回 Ok(空) —— 由调用方计数、不中断整轮。
fn fts_query(conn: &Connection, query: &str) -> Result<Vec<String>> {
    let Some(match_str) = build_match_query(query) else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare(
        "SELECT id FROM corpus_fts WHERE corpus_fts MATCH ?1 ORDER BY bm25(corpus_fts) LIMIT ?2",
    )?;
    let rows = stmt.query_map((&match_str, POOL as i64), |r| r.get::<_, String>(0));
    match rows {
        Ok(it) => {
            let mut ids = Vec::new();
            for r in it {
                ids.push(r?);
            }
            Ok(ids)
        }
        // MATCH 串畸形等错误：当空结果，调用方 unwrap_or_default 接住。
        Err(_) => Ok(Vec::new()),
    }
}

/// 向量索引：截断模式每 doc 一条；分块模式每 chunk 一条（检索时按 doc max-pool）。
enum VectorIndex {
    Flat(Vec<(String, Vec<f32>)>),
    Chunked(Vec<(String, usize, Vec<f32>)>),
}

impl VectorIndex {
    /// 暴力 cosine + 取 top-POOL doc id（降序，越大越相关）。
    /// - Flat：直接对每 doc 向量打分排名。
    /// - Chunked：对每 chunk 打分，按 doc_id 取「该 doc 所有 chunk 的最大 cosine」聚合到 doc 级。
    fn query(&self, qv: &[f32]) -> Vec<String> {
        match self {
            VectorIndex::Flat(vectors) => {
                let mut scored: Vec<(f32, &str)> = vectors
                    .iter()
                    .map(|(id, v)| (cosine(qv, v), id.as_str()))
                    .collect();
                sort_take_ids(&mut scored)
            }
            VectorIndex::Chunked(chunks) => {
                // max-pool：doc_id -> 该 doc 出现过的最大 chunk cosine。
                let mut best: HashMap<&str, f32> = HashMap::new();
                for (id, _idx, v) in chunks {
                    let s = cosine(qv, v);
                    let e = best.entry(id.as_str()).or_insert(f32::MIN);
                    if s > *e {
                        *e = s;
                    }
                }
                let mut scored: Vec<(f32, &str)> =
                    best.into_iter().map(|(id, s)| (s, id)).collect();
                sort_take_ids(&mut scored)
            }
        }
    }
}

/// 按分数降序排序后取 top-POOL 的 doc id。NaN 当最小处理（零向量已判 0，不会真出现）。
fn sort_take_ids(scored: &mut [(f32, &str)]) -> Vec<String> {
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored
        .iter()
        .take(POOL)
        .map(|(_, id)| (*id).to_string())
        .collect()
}

/// 聚合分桶 + overall，打 Recall@10 / nDCG@10 表 + ΔRecall。
fn print_aggregate(results: &[CaseResult]) {
    // bucket（保持出现顺序）。
    let mut buckets: Vec<String> = Vec::new();
    for r in results {
        if !buckets.contains(&r.bucket) {
            buckets.push(r.bucket.clone());
        }
    }

    // 每桶 + overall 的均值。
    let mean = |sel: &dyn Fn(&CaseResult) -> f64, filter_bucket: Option<&str>| -> (f64, usize) {
        let subset: Vec<f64> = results
            .iter()
            .filter(|r| filter_bucket.is_none_or(|b| r.bucket == b))
            .map(sel)
            .collect();
        if subset.is_empty() {
            (0.0, 0)
        } else {
            (
                subset.iter().sum::<f64>() / subset.len() as f64,
                subset.len(),
            )
        }
    };

    println!("\n========== 聚合：Recall@10 / nDCG@10（均值）==========");
    println!(
        "{:<20} {:>3}  | {:>8} {:>8} | {:>8} {:>8} | {:>8} {:>8}",
        "bucket", "n", "FTS_R", "FTS_N", "VEC_R", "VEC_N", "HYB_R", "HYB_N"
    );
    println!("{}", "-".repeat(86));

    let print_row = |label: &str, fb: Option<&str>| {
        let (fr, n) = mean(&|r| r.fts_recall, fb);
        let (fn_, _) = mean(&|r| r.fts_ndcg, fb);
        let (vr, _) = mean(&|r| r.vec_recall, fb);
        let (vn, _) = mean(&|r| r.vec_ndcg, fb);
        let (hr, _) = mean(&|r| r.hybrid_recall, fb);
        let (hn, _) = mean(&|r| r.hybrid_ndcg, fb);
        println!(
            "{label:<20} {n:>3}  | {fr:>8.3} {fn_:>8.3} | {vr:>8.3} {vn:>8.3} | {hr:>8.3} {hn:>8.3}"
        );
    };

    for b in &buckets {
        print_row(b, Some(b));
    }
    println!("{}", "-".repeat(86));
    print_row("OVERALL", None);

    // ΔRecall@10 = hybrid − FTS5。
    println!("\n========== hybrid − FTS5  ΔRecall@10 ==========");
    let delta = |fb: Option<&str>| -> f64 {
        let (fr, _) = mean(&|r| r.fts_recall, fb);
        let (hr, _) = mean(&|r| r.hybrid_recall, fb);
        (hr - fr) * 100.0
    };
    for b in &buckets {
        println!("  {:<20} {:+.1}pp", b, delta(Some(b)));
    }
    let d_overall = delta(None);
    println!("  {:<20} {:+.1}pp", "OVERALL", d_overall);
    println!("\nhybrid − FTS5 ΔRecall@10 = {d_overall:+.1}pp（overall）");
}

/// BETA-26 融合调优：打印当前 variant 的单行关键摘要 + vec-only / fts-only 参考行。
/// 4 个关键指标：overall R@10、pure-fuzzy（fts_recall<0.5 子集）R@10、exact-name 桶 R@10（守门，应=1.000）、overall nDCG@10。
/// 用机器可解析的 `VARIANT_SUMMARY` 前缀，便于跨多次运行 grep 拼表。
fn print_variant_summary(fusion: &FusionConfig, results: &[CaseResult]) {
    // 子集均值小工具。
    let mean = |sel: &dyn Fn(&CaseResult) -> f64, pred: &dyn Fn(&CaseResult) -> bool| -> f64 {
        let xs: Vec<f64> = results.iter().filter(|r| pred(r)).map(sel).collect();
        if xs.is_empty() {
            0.0
        } else {
            xs.iter().sum::<f64>() / xs.len() as f64
        }
    };
    let all = |_: &CaseResult| true;
    let fuzzy = |r: &CaseResult| r.fts_recall < 0.5;
    let exact = |r: &CaseResult| r.bucket == "exact-name";
    let n_fuzzy = results.iter().filter(|r| fuzzy(r)).count();

    // 三组的 4 指标（hybrid=当前融合模式；vec/fts 为标准 standalone 参考）。
    let row = |tag: &str,
               r_sel: &dyn Fn(&CaseResult) -> f64,
               n_sel: &dyn Fn(&CaseResult) -> f64| {
        let overall_r = mean(r_sel, &all);
        let fuzzy_r = mean(r_sel, &fuzzy);
        let exact_r = mean(r_sel, &exact);
        let overall_n = mean(n_sel, &all);
        println!(
            "VARIANT_SUMMARY\t{tag}\tR@10={overall_r:.3}\tfuzzyR@10={fuzzy_r:.3}\texactR@10={exact_r:.3}\tnDCG@10={overall_n:.3}"
        );
    };

    println!(
        "\n========== BETA-26 variant 摘要（pure-fuzzy=fts_recall<0.5，{n_fuzzy} case；exact-name 为守门桶）==========",
    );
    row(
        &format!("HYBRID[{}]", fusion.label()),
        &|r| r.hybrid_recall,
        &|r| r.hybrid_ndcg,
    );
    row("VEC_ONLY", &|r| r.vec_recall, &|r| r.vec_ndcg);
    row("FTS_ONLY", &|r| r.fts_recall, &|r| r.fts_ndcg);
}

/// 打印疑似泄漏 case（fts_recall >= 0.5）—— 词面重叠让 FTS5 已经赢了，说明 query 不够模糊。
fn print_leaked(results: &[CaseResult]) {
    let leaked: Vec<&CaseResult> = results.iter().filter(|r| r.fts_recall >= 0.5).collect();
    if leaked.is_empty() {
        println!("\n== 疑似泄漏 case（fts_recall >= 0.5）：无 ==");
        return;
    }
    println!(
        "\n== 疑似泄漏 case（fts_recall >= 0.5，共 {} 个）==",
        leaked.len()
    );
    for r in &leaked {
        println!(
            "  {} [{}] fts_recall={:.2}  query={}",
            r.id, r.bucket, r.fts_recall, r.query
        );
    }
}
