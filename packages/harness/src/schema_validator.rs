use jsonschema::Validator;
use scout_search_backend::SearchIntent;
use serde_json::Value;
use std::fmt;
use thiserror::Error;

/// Schema 校验相关的错误。
#[derive(Debug, Error)]
pub enum SchemaError {
    /// Schema 文件本身加载或编译失败。
    #[error("Schema 加载失败: {0}")]
    Load(String),

    /// 数据不符合 JSON Schema 规范。
    #[error("Schema 校验未通过: {0:?}")]
    Validation(Vec<ValidationError>),

    /// JSON 解析或反序列化为 [`SearchIntent`] 失败。
    #[error("JSON 处理失败: {0}")]
    Json(#[from] serde_json::Error),
}

/// 单条校验错误信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// 错误发生的 JSON 路径（如 `$.intent`）。
    pub path: String,
    /// 具体的错误描述。
    pub message: String,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.path, self.message)
    }
}

/// Schema 校验服务。
///
/// 负责在运行时确保输入的 JSON 符合 `docs/schema/search-intent.v1.json` 定义，
/// 并将其转换为类型安全的 [`SearchIntent`]。
pub struct SchemaValidator {
    compiled: Validator,
}

impl SchemaValidator {
    /// 加载内嵌的 schema 文件并初始化校验器。
    ///
    /// # Errors
    ///
    /// 如果内嵌的 JSON Schema 格式有误或无法编译，返回 [`SchemaError::Load`].
    pub fn new() -> Result<Self, SchemaError> {
        let schema_str = include_str!("../../../docs/schema/search-intent.v1.json");
        let schema_json: Value = serde_json::from_str(schema_str)?;
        let compiled = jsonschema::validator_for(&schema_json)
            .map_err(|e| SchemaError::Load(e.to_string()))?;
        Ok(Self { compiled })
    }

    /// 对一个已解析的 [`Value`] 做严格校验。
    ///
    /// # Errors
    ///
    /// 如果校验失败，返回所有的错误列表。
    pub fn validate_value(&self, value: &Value) -> Result<(), Vec<ValidationError>> {
        let errors: Vec<_> = self.compiled.iter_errors(value).collect();
        if !errors.is_empty() {
            let validation_errors = errors
                .into_iter()
                .map(|e| ValidationError {
                    path: e.instance_path.to_string(),
                    message: e.to_string(),
                })
                .collect();
            return Err(validation_errors);
        }
        Ok(())
    }

    /// 校验原始 JSON 字符串并反序列化。
    ///
    /// 流程：解析 JSON -> JSON Schema 校验 -> Serde 反序列化。
    ///
    /// # Errors
    ///
    /// - [`SchemaError::Json`]: JSON 格式错误或反序列化失败。
    /// - [`SchemaError::Validation`]: 不符合 Schema 规范。
    pub fn validate_str(&self, raw: &str) -> Result<SearchIntent, SchemaError> {
        let value: Value = serde_json::from_str(raw)?;
        self.validate_value(&value)
            .map_err(SchemaError::Validation)?;
        let intent: SearchIntent = serde_json::from_value(value)?;
        Ok(intent)
    }
}

impl fmt::Debug for SchemaValidator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchemaValidator").finish()
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::print_stdout
    )]
    use super::*;
    use serde_json::json;

    fn get_cases() -> Vec<Value> {
        let cases_str = include_str!("../../search-backends/common/tests/fixtures/cases.json");
        let cases: Value = serde_json::from_str(cases_str).unwrap();
        cases
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["intent"].clone())
            .collect()
    }

    #[test]
    fn validate_all_fixtures() {
        let validator = SchemaValidator::new().unwrap();
        let cases = get_cases();
        assert_eq!(cases.len(), 50, "必须覆盖 50 条 fixture");

        for (i, case) in cases.iter().enumerate() {
            if let Err(errors) = validator.validate_value(case) {
                panic!("Fixture #{} 校验失败: {:?}", i + 1, errors);
            }
        }
    }

    #[test]
    fn reject_invalid_samples() {
        let validator = SchemaValidator::new().unwrap();

        // 缺 schema_version
        let res = validator.validate_value(&json!({"intent": "file_search"}));
        assert!(res.is_err());
        assert!(res
            .unwrap_err()
            .iter()
            .any(|e| e.path.is_empty() && e.message.contains("schema_version")));

        // 未知 intent
        let res = validator.validate_value(&json!({
            "schema_version": "1.0",
            "intent": "magic_search"
        }));
        assert!(res.is_err());

        // limit > 500
        let res = validator.validate_value(&json!({
            "schema_version": "1.0",
            "intent": "file_search",
            "limit": 501
        }));
        assert!(res.is_err());
        let errs = res.unwrap_err();
        assert!(errs
            .iter()
            .any(|e| e.path == "/limit" && e.message.contains("500")));
    }

    #[test]
    fn validate_str_e2e() {
        let validator = SchemaValidator::new().unwrap();
        let raw = r#"{"schema_version": "1.0", "intent": "file_search", "keywords": ["rust"]}"#;
        let intent = validator.validate_str(raw).unwrap();
        match intent {
            SearchIntent::FileSearch(s) => assert_eq!(s.keywords.unwrap()[0], "rust"),
            _ => panic!("变体匹配错误"),
        }
    }
}
