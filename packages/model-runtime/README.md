# scout-model-runtime

Scout 本地小模型推理运行时。

## 功能

- 支持 GGUF 格式模型（量化 Q4_K_M）
- 抽象 `ModelLoader` 与 `LlamaModelRuntime` trait
- 多后端支持：
  - `llama-cpp` (默认，通过 `llama-cpp-4` 绑定，需系统安装 `cmake`)
  - `candle` (纯 Rust 实现，无需 `cmake`，作为 fallback 或轻量环境使用)
  - `stub` (仅回声，用于 CI 和快速原型)
- 跨平台特征：`metal` (macOS), `cuda` (NVIDIA), `vulkan` (Windows/Linux)

## 快速开始

### 1. 添加依赖

```toml
[dependencies]
scout-model-runtime = { path = "../../packages/model-runtime", features = ["candle"] }
```

### 2. 调用示例

```rust
use scout_model_runtime::{get_default_loader, ModelLoadParams, GenerateParams};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let loader = get_default_loader();
    let model = loader.load(
        &PathBuf::from("models/qwen2.5-1.5b-instruct-q4_k_m.gguf"),
        &ModelLoadParams::default()
    )?;

    let params = GenerateParams {
        max_tokens: 128,
        temperature: 0.7,
        ..Default::default()
    };

    let response = model.generate("查找昨天修改过的 PDF 文件", &params)?;
    println!("模型输出: {}", response);

    Ok(())
}
```

## 环境变量

- `SCOUT_MODEL_PATH`: 模型文件搜索的基础路径。

## 模型标识

`canonical_model_id(path)` 从 GGUF 文件路径派生统一的模型标识（剥掉 `-q8_0`/`-q4_k_m`/`-f16`
等量化后缀，只留模型本体名）——桌面端与 daemon 端此前各自实现（一个用固定常量、一个用裸
`file_stem()`），同一模型算出两种 id，写进 `document_vectors.embed_model` 后互不识别、也没法
共用一份 [`scout-result-normalizer`](../result-normalizer) 的按模型校准阈值表。2026-07-28 起
两端统一调用本函数。

## 故障排除

### 无法编译 `llama-cpp` 特性

`llama.cpp` 的编译依赖 `cmake` 和 C++17 编译器。如果在 macOS 上遇到 `cmake not found`，请确保已安装：

```bash
brew install cmake
```

如果无法安装 `cmake`，请改用 `candle` 特性：

```bash
cargo build --features candle --no-default-features
```

## 测试模型推荐

- **基座模型**: Qwen2.5-1.5B-Instruct-GGUF
- **量化格式**: Q4_K_M
- **下载地址**: [Hugging Face - Qwen2.5-1.5B-Instruct-GGUF](https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF)
