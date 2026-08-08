//! GGUF metadata → `LlamaPoolingType` 检测（纯逻辑、可单测）。
//!
//! 与 llama.cpp upstream `LLAMA_POOLING_TYPE_*` 枚举对齐：
//! 0=None, 1=Mean, 2=Cls, 3=Last, 4=Rank（reranker 模型用、如 bge-reranker-v2-m3）。
//! GGUF 标准 metadata key 形式为 `<arch>.pooling_type`（i32/u32），
//! arch 取自 `general.architecture`。
//!
//! BETA-15B-8：替换 `llama.rs` 中硬编码 `LlamaPoolingType::Last`，
//! 解 BETA-15B-7 v4 cycle 暴露的 bge-m3（bert arch 声明 CLS）被错配为 Last 的 infra 缺陷。

#![cfg(feature = "llama-cpp")]

use crate::ModelError;
use llama_cpp_4::context::params::LlamaPoolingType;

/// 与 llama.cpp upstream `LLAMA_POOLING_TYPE_*` 对齐。注意 llama-cpp-4 0.3.0
/// bindings 只暴露到 `Last=3`、未暴露 `Rank=4`（reranker 用、本 cycle 不涉及）；
/// 如未来接 reranker 模型需先升 llama-cpp-4 binding 后扩此函数。
pub(crate) fn map_gguf_pooling_value(v: i64) -> Result<LlamaPoolingType, ModelError> {
    match v {
        0 => Ok(LlamaPoolingType::None),
        1 => Ok(LlamaPoolingType::Mean),
        2 => Ok(LlamaPoolingType::Cls),
        3 => Ok(LlamaPoolingType::Last),
        _ => Err(ModelError::LoadError(format!(
            "invalid GGUF pooling_type value: {v} (expected 0..=3; \
             Rank=4 reserved for rerankers, not exposed by llama-cpp-4 0.3.0 bindings)"
        ))),
    }
}

pub(crate) fn default_pooling_for_arch(arch: &str) -> Result<LlamaPoolingType, ModelError> {
    match arch {
        "bert" | "nomic-bert" | "jina-bert-v2" | "roberta" => Ok(LlamaPoolingType::Cls),
        "t5" | "gemma-embedding" => Ok(LlamaPoolingType::Mean),
        "llama" | "qwen2" | "qwen3" | "mistral" => Ok(LlamaPoolingType::Last),
        _ => Err(ModelError::LoadError(format!(
            "unknown architecture '{arch}' and GGUF did not declare <arch>.pooling_type; \
             declare pooling_type in GGUF metadata or extend default_pooling_for_arch heuristic table"
        ))),
    }
}

pub(crate) fn detect_pooling_type(
    arch: &str,
    pooling_meta: Option<i64>,
) -> Result<LlamaPoolingType, ModelError> {
    match pooling_meta {
        Some(v) => map_gguf_pooling_value(v),
        None => default_pooling_for_arch(arch),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn map_gguf_pooling_value_covers_all_known() {
        assert_eq!(map_gguf_pooling_value(0).unwrap(), LlamaPoolingType::None);
        assert_eq!(map_gguf_pooling_value(1).unwrap(), LlamaPoolingType::Mean);
        assert_eq!(map_gguf_pooling_value(2).unwrap(), LlamaPoolingType::Cls);
        assert_eq!(map_gguf_pooling_value(3).unwrap(), LlamaPoolingType::Last);
    }

    #[test]
    fn map_gguf_pooling_value_rejects_out_of_range() {
        assert!(map_gguf_pooling_value(-1).is_err());
        assert!(map_gguf_pooling_value(5).is_err());
        assert!(map_gguf_pooling_value(99).is_err());
    }

    #[test]
    fn default_pooling_for_arch_bert_family_is_cls() {
        assert_eq!(
            default_pooling_for_arch("bert").unwrap(),
            LlamaPoolingType::Cls
        );
        assert_eq!(
            default_pooling_for_arch("nomic-bert").unwrap(),
            LlamaPoolingType::Cls
        );
        assert_eq!(
            default_pooling_for_arch("jina-bert-v2").unwrap(),
            LlamaPoolingType::Cls
        );
        assert_eq!(
            default_pooling_for_arch("roberta").unwrap(),
            LlamaPoolingType::Cls
        );
    }

    #[test]
    fn default_pooling_for_arch_decoder_family_is_last() {
        assert_eq!(
            default_pooling_for_arch("llama").unwrap(),
            LlamaPoolingType::Last
        );
        assert_eq!(
            default_pooling_for_arch("qwen2").unwrap(),
            LlamaPoolingType::Last
        );
        assert_eq!(
            default_pooling_for_arch("qwen3").unwrap(),
            LlamaPoolingType::Last
        );
        assert_eq!(
            default_pooling_for_arch("mistral").unwrap(),
            LlamaPoolingType::Last
        );
    }

    #[test]
    fn default_pooling_for_arch_t5_is_mean() {
        assert_eq!(
            default_pooling_for_arch("t5").unwrap(),
            LlamaPoolingType::Mean
        );
    }

    #[test]
    fn default_pooling_for_arch_gemma_embedding_is_mean() {
        assert_eq!(
            default_pooling_for_arch("gemma-embedding").unwrap(),
            LlamaPoolingType::Mean
        );
    }

    #[test]
    fn default_pooling_for_arch_unknown_fails() {
        assert!(default_pooling_for_arch("frobnicator").is_err());
        assert!(default_pooling_for_arch("").is_err());
    }

    #[test]
    fn detect_pooling_type_metadata_overrides_heuristic() {
        // qwen3 + meta=3 → Last（现行 qwen3-embedding-0.6b/8b 行为锚、零回归保护）
        assert_eq!(
            detect_pooling_type("qwen3", Some(3)).unwrap(),
            LlamaPoolingType::Last
        );
        // bert + meta=2 → Cls（修正 v4 cycle bge-m3 错配的回归保护单测）
        assert_eq!(
            detect_pooling_type("bert", Some(2)).unwrap(),
            LlamaPoolingType::Cls
        );
    }

    #[test]
    fn detect_pooling_type_missing_metadata_uses_heuristic() {
        assert_eq!(
            detect_pooling_type("bert", None).unwrap(),
            LlamaPoolingType::Cls
        );
        assert_eq!(
            detect_pooling_type("qwen3", None).unwrap(),
            LlamaPoolingType::Last
        );
        assert!(detect_pooling_type("frobnicator", None).is_err());
    }

    #[test]
    fn detect_pooling_type_invalid_metadata_fails_even_with_known_arch() {
        // arch 已知但 metadata 越界 = 仍 fail（不静默 fallback 到 heuristic、避免藏 bug）
        assert!(detect_pooling_type("qwen3", Some(99)).is_err());
    }
}
