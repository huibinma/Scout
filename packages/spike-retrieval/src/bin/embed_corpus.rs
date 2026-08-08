//! BETA-26 全语料 embedding：读冻结语料 corpus.jsonl，逐文档过本地 Qwen3-Embedding
//! 模型（model-runtime 的 `embed()`），把句向量写入 vectors.bin，供 Task 6 检索对比读取。
//!
//! 稳健性约定（朴素版不安全，故刻意如此）：
//! 1. 首 1200 字截断——`embed()` 的 context `n_ctx = 2048`，语料里大量长 markdown/changelog
//!    的 token 数远超 2048，会被 decode 拒绝返回 Err。取开头 1200 字（中英混排稳在 2048
//!    token 以内）足以捕获文档主题；spec 明确禁止做 chunking 马拉松，故采开头截断这一简化。
//! 2. 逐文档错误隔离——单篇失败（Err 或仍溢出）只 SKIP 计数，绝不中断整轮；末尾打印跳过数。
//! 3. 空向量守卫——embed 回空 vec 同样跳过计数。
//!
//! 同时打印成本数字（吞吐 / 总耗时 / 文件大小），喂给 spike 的 kill-criteria。
//!
//! BETA-26 分块实验：设环境变量 `CHUNK=1` 切到「分块」模式（默认不设 = 原首 1200 字截断，
//! vectors.bin 不动、可复现）。分块模式下每篇按 800 字/150 字重叠（按 char）切窗，步长 650，
//! 每篇最多 20 块（即很长文档只覆盖前 ~13k 字，输出会注明），逐块 embed（同样的逐块错误隔离
//! 与空向量守卫），写 vectors-chunked.bin，每行 `<doc_id>\t<chunk_idx>\t<csv f32>`。
//! 这是为了对比「整篇截断单向量」与「分块多向量 + max-pool」哪种检索更好——BETA-15B 的设计岔路。

use anyhow::{Context, Result};
use model_runtime::{get_default_loader, ModelLoadParams};
use spike_retrieval::CorpusDoc;
use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Instant;

/// 分块参数（按 char，不按 byte）：窗口 800 字，重叠 150 字 → 步长 650；每篇最多 20 块。
const CHUNK_SIZE: usize = 800;
const CHUNK_OVERLAP: usize = 150;
const CHUNK_STEP: usize = CHUNK_SIZE - CHUNK_OVERLAP; // 650
const MAX_CHUNKS_PER_DOC: usize = 20;

/// 把一篇文档切成 char 级重叠窗口（窗口 800/步长 650），上限 MAX_CHUNKS_PER_DOC。
/// 返回每块的 String。空文档返回空 vec。
fn chunk_text(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let end = (start + CHUNK_SIZE).min(chars.len());
        chunks.push(chars[start..end].iter().collect::<String>());
        if chunks.len() >= MAX_CHUNKS_PER_DOC || end == chars.len() {
            break;
        }
        start += CHUNK_STEP;
    }
    chunks
}

fn main() -> Result<()> {
    let corpus = "packages/spike-retrieval/fixtures/corpus.jsonl";
    let chunk_mode = std::env::var("CHUNK").map(|v| v == "1").unwrap_or(false);
    // 分块模式写独立文件，绝不覆盖截断版 vectors.bin（保证截断结果可复现）。
    let out = if chunk_mode {
        "packages/spike-retrieval/fixtures/vectors-chunked.bin"
    } else {
        "packages/spike-retrieval/fixtures/vectors.bin"
    };
    let model = std::env::var("EMBED_MODEL")
        .unwrap_or_else(|_| "models/qwen3-embedding-0.6b-q8_0.gguf".into());
    // 节流阀：吞吐先测时设 LIMIT=N 只跑前 N 篇估算总耗时；正式全量跑不设此变量。
    let limit: Option<usize> = std::env::var("LIMIT").ok().and_then(|s| s.parse().ok());

    // 加载 embedding 运行时：用真实加载器（metal feature 下走 llama-cpp 后端）。
    // context_size 留 0 → 后端用默认 2048；gpu_layers 0 由后端按 feature 决定上送层数。
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

    let f = std::io::BufReader::new(
        std::fs::File::open(corpus).with_context(|| format!("打开语料失败: {corpus}"))?,
    );
    let mut w = std::io::BufWriter::new(
        std::fs::File::create(out).with_context(|| format!("创建输出失败: {out}"))?,
    );
    let t0 = Instant::now();
    if chunk_mode {
        eprintln!(
            "CHUNK=1 分块模式：窗口 {CHUNK_SIZE} 字 / 重叠 {CHUNK_OVERLAP} 字 / 步长 {CHUNK_STEP} / 每篇上限 {MAX_CHUNKS_PER_DOC} 块"
        );
        // 分块模式统计：文档数 / 总块数 / 跳过块数 / 被上限截断的长文档数。
        let (mut n_docs, mut n_chunks, mut skipped, mut capped) = (0usize, 0usize, 0usize, 0usize);
        for line in f.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let doc: CorpusDoc = serde_json::from_str(&line)?;
            let chunks = chunk_text(&doc.text);
            // 命中 20 块上限且原文还更长 → 计一次「被截断的长文档」。
            if chunks.len() == MAX_CHUNKS_PER_DOC
                && doc.text.chars().count() > (MAX_CHUNKS_PER_DOC - 1) * CHUNK_STEP + CHUNK_SIZE
            {
                capped += 1;
            }
            n_docs += 1;
            for (idx, chunk) in chunks.iter().enumerate() {
                match rt.embed(chunk) {
                    // 逐块错误隔离 + 空向量守卫：单块失败只跳过计数，不中断整轮。
                    Ok(v) if !v.is_empty() => {
                        let csv = v
                            .iter()
                            .map(std::string::ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(",");
                        writeln!(w, "{}\t{}\t{}", doc.id, idx, csv)?;
                        n_chunks += 1;
                        if n_chunks % 2000 == 0 {
                            eprintln!(
                                "  embedded {n_chunks} chunks ({n_docs} docs)... ({:.1}s)",
                                t0.elapsed().as_secs_f64()
                            );
                        }
                    }
                    _ => {
                        skipped += 1;
                    }
                }
            }
            if let Some(lim) = limit {
                if n_docs >= lim {
                    eprintln!("LIMIT={lim} 命中，提前结束（吞吐测算模式）。");
                    break;
                }
            }
        }
        w.flush()?;
        let secs = t0.elapsed().as_secs_f64();
        let bytes = std::fs::metadata(out)?.len();
        eprintln!(
            "✅ CHUNK embed {n_docs} docs → {n_chunks} chunks in {secs:.1}s ({:.1} chunks/s) | skipped {skipped} chunks | {capped} docs 触 {MAX_CHUNKS_PER_DOC} 块上限被截断 | vectors-chunked.bin {:.1}MB",
            n_chunks as f64 / secs.max(0.001),
            bytes as f64 / 1e6
        );
        return Ok(());
    }

    let (mut n, mut skipped) = (0usize, 0usize);
    for line in f.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let doc: CorpusDoc = serde_json::from_str(&line)?;
        // 首 1200 字截断（按 char，不按 byte，避免切断多字节 UTF-8）。
        let text: String = doc.text.chars().take(1200).collect();
        match rt.embed(&text) {
            // 逐文档错误隔离 + 空向量守卫：单篇失败只跳过计数，不中断整轮。
            Ok(v) if !v.is_empty() => {
                let csv = v
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                writeln!(w, "{}\t{}", doc.id, csv)?;
                n += 1;
                if n % 200 == 0 {
                    eprintln!("  embedded {n}... ({:.1}s)", t0.elapsed().as_secs_f64());
                }
            }
            _ => {
                skipped += 1;
            }
        }
        if let Some(lim) = limit {
            if n + skipped >= lim {
                eprintln!("LIMIT={lim} 命中，提前结束（吞吐测算模式）。");
                break;
            }
        }
    }
    w.flush()?;
    let secs = t0.elapsed().as_secs_f64();
    let bytes = std::fs::metadata(out)?.len();
    eprintln!(
        "✅ embed {n} docs in {secs:.1}s ({:.1} docs/s) | skipped {skipped} | vectors.bin {:.1}MB",
        n as f64 / secs.max(0.001),
        bytes as f64 / 1e6
    );
    Ok(())
}
