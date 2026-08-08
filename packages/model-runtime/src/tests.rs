#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::float_cmp
)]

use crate::{
    canonical_model_id, first_json_object_complete, get_default_loader, GenerateParams,
    ModelLoadParams, ModelLoader, StubLoader,
};
use std::path::PathBuf;

// 2026-07-28：桌面/daemon 统一 model_id 派生——剥掉已知 GGUF 量化后缀。
#[test]
fn canonical_model_id_strips_q8_0_suffix() {
    let p = PathBuf::from("/models/embeddinggemma-300m-q8_0.gguf");
    assert_eq!(canonical_model_id(&p), "embeddinggemma-300m");
}

#[test]
fn canonical_model_id_strips_bge_m3_q8_0_suffix() {
    // 剥完后应与桌面端此前的硬编码常量逐字节相同（零迁移成本，见函数文档）。
    let p = PathBuf::from("/models/bge-m3-q8_0.gguf");
    assert_eq!(canonical_model_id(&p), "bge-m3");
}

#[test]
fn canonical_model_id_strips_multi_underscore_quant_tag() {
    // 量化标签内部用下划线（q4_k_m），rsplit_once('-') 把它当一整段——不应被误切碎。
    let p = PathBuf::from("/models/qwen3-embedding-0.6b-q4_k_m.gguf");
    assert_eq!(canonical_model_id(&p), "qwen3-embedding-0.6b");
}

#[test]
fn canonical_model_id_strips_f16_suffix() {
    let p = PathBuf::from("/models/some-model-f16.gguf");
    assert_eq!(canonical_model_id(&p), "some-model");
}

#[test]
fn canonical_model_id_no_recognized_suffix_keeps_full_stem() {
    // 宁可漏剥、不误剥：结尾片段不像量化标签时原样保留整个 stem。
    let p = PathBuf::from("/models/custom-model.gguf");
    assert_eq!(canonical_model_id(&p), "custom-model");
}

#[test]
fn canonical_model_id_no_hyphen_returns_stem_unchanged() {
    let p = PathBuf::from("/models/qwen3.gguf");
    assert_eq!(canonical_model_id(&p), "qwen3");
}

#[test]
fn canonical_model_id_missing_stem_falls_back_to_placeholder() {
    let p = PathBuf::from("/");
    assert_eq!(canonical_model_id(&p), "unknown-embedder");
}

#[test]
fn test_generate_params_default() {
    let params = GenerateParams::default();
    assert_eq!(params.max_tokens, 512);
    assert_eq!(params.temperature, 0.7);
    assert_eq!(params.seed, 42);
    // BETA-17：默认不提前停（向后兼容，full 路径行为不变除非显式开启）。
    assert!(!params.stop_at_json);
}

// BETA-17：`first_json_object_complete` —— 首个 JSON 对象闭合检测（stop_at_json 用）。
#[test]
fn json_complete_flat_object() {
    assert!(first_json_object_complete(r#"{"size":null}"#));
}

#[test]
fn json_incomplete_while_open() {
    // 还没闭合 —— 仍在生成中。
    assert!(!first_json_object_complete(r#"{"size":"#));
    assert!(!first_json_object_complete(r#"{"a":{"b":1}"#)); // 外层未闭
    assert!(!first_json_object_complete(""));
    assert!(!first_json_object_complete("前导文字还没出现对象"));
}

#[test]
fn json_complete_nested_object() {
    // 嵌套对象：只有外层闭合才算完成。
    assert!(first_json_object_complete(
        r#"{"modified_time":{"type":"relative","value":"last_7_days"}}"#
    ));
}

#[test]
fn json_braces_inside_string_ignored() {
    // 字符串值里的花括号不参与深度计数（否则会过早或过晚停止）。
    assert!(first_json_object_complete(r#"{"name":"a}b{c"}"#));
    assert!(!first_json_object_complete(r#"{"name":"}"#)); // 括号在未闭合字符串内
}

#[test]
fn json_escaped_quote_in_string() {
    // 转义引号不应误判字符串结束。
    assert!(first_json_object_complete(r#"{"k":"he said \"hi\" }"}"#));
}

#[test]
fn json_stops_at_first_object_repeated() {
    // 模型"复读"场景：首个对象闭合即视为完成（后续重复内容被忽略）。
    assert!(first_json_object_complete(r#"{"sort":"size_desc"}{"sort"#));
}

#[test]
fn test_stub_loader() {
    let loader = StubLoader;
    let model = loader
        .load(&PathBuf::from("mock.gguf"), &ModelLoadParams::default())
        .unwrap();
    let response = model.generate("Hello", &GenerateParams::default()).unwrap();
    assert_eq!(response, "Echo: Hello");
}
#[test]
fn test_get_default_loader() {
    let loader = get_default_loader();
    // Default in test should be stub unless features are enabled.
    // 当 `cargo test --workspace` 经 feature 统一把 llama-cpp 真 loader 拉进来时
    // （如 throwaway 的 spike-retrieval 无条件开它），占位 "mock.gguf" 无法被真 loader
    // 加载 → 加载失败即跳过（本测试只验默认 stub loader 的 echo 行为）。
    let Ok(model) = loader.load(&PathBuf::from("mock.gguf"), &ModelLoadParams::default()) else {
        return;
    };
    let response = model.generate("Test", &GenerateParams::default()).unwrap();
    assert!(response.contains("Test"));
}

#[test]
#[ignore = "需要真实 candle 模型文件，CI 无模型时跳过"]
fn test_candle_e2e() {
    #[cfg(feature = "candle")]
    {
        let loader = CandleLoader::new();
        // This requires a real model file
        let model_path = PathBuf::from("models/qwen2.5-1.5b-instruct-q4_k_m.gguf");
        if model_path.exists() {
            let model = loader
                .load(&model_path, &ModelLoadParams::default())
                .unwrap();
            let response = model.generate("你好", &GenerateParams::default()).unwrap();
            println!("Response: {response}");
            assert!(!response.is_empty());
        }
    }
}

// BETA-25：真机冒烟——验证静态链接的 llama 后端能加载已部署模型并产出非空生成。
// 默认 ignore（需真实 gguf + Metal）。运行：
//   cargo test -p scout-model-runtime --features llama-cpp,metal beta25_static_llama_smoke -- --ignored --nocapture
#[cfg(feature = "llama-cpp")]
#[test]
#[ignore = "需真实 gguf 模型 + llama-cpp 后端；CI 无模型时跳过"]
fn beta25_static_llama_smoke() {
    use crate::{GenerateParams, LlamaLoader, ModelLoadParams, ModelLoader};

    // dirs 不是 model-runtime 依赖；优先读环境变量，回退绝对路径（macOS 部署位置）。
    let model_path: PathBuf = std::env::var("SCOUT_BETA25_MODEL").map_or_else(
        |_| {
            PathBuf::from(
                "/Users/alice/Library/Application Support/Scout/models/qwen3-0.6b-q4_k_m.gguf",
            )
        },
        PathBuf::from,
    );

    assert!(
        model_path.exists(),
        "模型不存在：{}（先部署 BETA-24 模型）",
        model_path.display()
    );

    let loader = LlamaLoader::new().expect("LlamaLoader::new");
    let model = loader
        .load(
            &model_path,
            &ModelLoadParams {
                gpu_layers: 99,
                context_size: 2048,
            },
        )
        .expect("load model");
    let out = model
        .generate("你好", &GenerateParams::default())
        .expect("generate");
    println!("生成结果：{out}");
    assert!(!out.trim().is_empty(), "生成结果为空");
}

// BETA-64 T9a：真机冒烟——验证常驻 embedding context 复用（`run_embed` 的 `embed_ctx`
// 跨调用复用 + 每次 decode 前 `clear_kv_cache`）没有让前一次调用的 KV 状态泄漏进本次
// 池化结果。默认 ignore（需真实 embedding gguf + llama-cpp 后端）。运行：
//   SCOUT_BETA64_EMBED_MODEL=/path/to/embedding.gguf \
//   cargo test -p scout-model-runtime --features llama-cpp,metal beta64_t9a -- --ignored --nocapture
#[cfg(feature = "llama-cpp")]
#[test]
#[ignore = "需真实 embedding gguf 模型 + llama-cpp 后端；CI 无模型时跳过"]
fn beta64_t9a_embed_context_reuse_does_not_leak_kv_state() {
    use crate::{LlamaLoader, ModelLoader};

    let model_path: PathBuf = std::env::var("SCOUT_BETA64_EMBED_MODEL").map_or_else(
        |_| PathBuf::from("/nonexistent-scout-beta64-embed-model.gguf"),
        PathBuf::from,
    );
    assert!(
        model_path.exists(),
        "模型不存在：{}（设置 SCOUT_BETA64_EMBED_MODEL 指向一份真实 embedding gguf）",
        model_path.display()
    );

    let loader = LlamaLoader::new().expect("LlamaLoader::new");
    let model = loader
        .load(
            &model_path,
            &ModelLoadParams {
                gpu_layers: 99,
                context_size: 2048,
            },
        )
        .expect("load model");

    // 三次调用共用同一个 worker 线程、同一个复用的 embed context：a → 另一段文本 → 再 a。
    let v1a = model
        .embed("Scout 是一个本地优先的桌面搜索工具，支持文档、音乐与图片的语义检索。")
        .expect("embed 1a");
    let v2 = model
        .embed("这是完全不同的第二段文本，用来确认两次 embed 之间没有互相串味。")
        .expect("embed 2");
    let v1b = model
        .embed("Scout 是一个本地优先的桌面搜索工具，支持文档、音乐与图片的语义检索。")
        .expect("embed 1b");

    assert!(!v1a.is_empty(), "v1a 不应为空向量");
    assert_eq!(v1a.len(), v2.len(), "同一模型的向量维度应恒定");
    assert_eq!(v1a.len(), v1b.len(), "同一模型的向量维度应恒定");

    // 同文本两次 embed（中间夹一次不同文本、复用同一个 context）应逐位几乎相同——
    // 如果 context 复用没清干净 KV cache，第二次同文本调用会掺进中间那次调用的残留状态，
    // 结果会明显偏离第一次。
    let max_abs_diff = v1a
        .iter()
        .zip(v1b.iter())
        .map(|(a, b)| (a - b).abs())
        .fold(0.0_f32, f32::max);
    assert!(
        max_abs_diff < 1e-4,
        "同文本复用 context 后向量应几乎相同，实得最大逐位差 {max_abs_diff}"
    );

    // 不同文本应产出明显不同的向量（cosine 明显 < 1）——防"复用 context 后所有向量
    // 趋同"这类更隐蔽的池化污染。
    let dot: f32 = v1a.iter().zip(v2.iter()).map(|(a, b)| a * b).sum();
    println!("v1a·v2 cosine = {dot}（v1a/v1b 均已 L2 归一化，v2 同样）");
    assert!(
        dot < 0.999,
        "不同文本的向量不应几乎重合（怀疑 KV cache 未清干净）：cosine={dot}"
    );
}
