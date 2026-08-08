//! 单次 `ModelFallback` 推理 probe —— 用于调试模型 raw output 是否能反序列化为
//! SearchIntent。仅 `--features model-fallback` 下可用。
//!
//! 用法：
//!   `DYLD_LIBRARY_PATH=$PWD/target/release` \
//!     `./target/release/fallback_probe` "找最近的"

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;

use scout_intent_parser::prompt::PromptBuilder;
use scout_model_runtime::{GenerateParams, ModelDaemon, ModelLoadParams};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("用法: fallback_probe <query>");
        std::process::exit(2);
    }
    let query = &args[1];

    // BETA-17 winner = Qwen3-0.6B（与 v1 准确率对等、更小更快）。SCOUT_MODEL_PATH 优先。
    let model_path: PathBuf = std::env::var_os("SCOUT_MODEL_PATH").map_or_else(
        || PathBuf::from("models/qwen3-0.6b-q4_k_m.gguf"),
        PathBuf::from,
    );

    eprintln!("[probe] loading model: {}", model_path.display());
    let start = std::time::Instant::now();
    let daemon = Arc::new(ModelDaemon::load_blocking(
        &model_path,
        ModelLoadParams {
            gpu_layers: 999,
            context_size: 4096,
        },
    )?);
    eprintln!("[probe] model loaded in {:?}", start.elapsed());

    let pb = PromptBuilder::default();
    let mut full = String::new();
    full.push_str(&pb.system_prompt());
    full.push_str("\n\n");
    full.push_str(&pb.user_prompt(query));

    eprintln!("[probe] prompt 长度: {} chars", full.len());

    let grammar = std::env::var("SCOUT_PROBE_GRAMMAR")
        .ok()
        .and_then(|p| std::fs::read_to_string(&p).ok().map(|s| (p, s)));
    if let Some((p, _)) = &grammar {
        eprintln!("[probe] grammar loaded from: {p}");
    }
    let params = GenerateParams {
        max_tokens: 256,
        temperature: 0.0,
        top_p: 1.0,
        stop_sequences: vec!["\n\n".to_owned(), "输入：".to_owned()],
        seed: 42,
        grammar: grammar.map(|(_, s)| s),
        stop_at_json: true,
    };

    let infer_start = std::time::Instant::now();
    let raw = daemon.generate(&full, &params)?;
    eprintln!("[probe] 推理耗时: {:?}", infer_start.elapsed());

    println!("===RAW MODEL OUTPUT===");
    println!("{raw}");
    println!("===END===");

    match serde_json::from_str::<scout_search_backend::SearchIntent>(raw.trim()) {
        Ok(intent) => {
            eprintln!("[probe] ✅ 反序列化成功");
            println!("\n反序列化的 intent:");
            println!("{}", serde_json::to_string_pretty(&intent)?);
        }
        Err(err) => {
            eprintln!("[probe] ❌ 反序列化失败: {err}");
        }
    }

    Ok(())
}
