//! JSON Schema 文件与 Rust serde 类型的交叉测试。
//!
//! 当前 sandbox 没有网络，无法拉取 `jsonschema` crate；这里先用
//! `serde_json` + Rust 类型 + v1.0 运行时约束做离线等价烟雾测试。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scout_search_backend::{
    FileActionKind, SearchIntent, SizeExpression, TargetRef, TargetSelector,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Case {
    intent: serde_json::Value,
}

fn load_schema() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../../../docs/schema/search-intent.v1.json"
    ))
    .expect("search-intent.v1.json 必须是合法 JSON")
}

fn load_cases() -> Vec<Case> {
    serde_json::from_str(include_str!("fixtures/cases.json")).expect("cases.json 解析失败")
}

fn validate_runtime_constraints(intent: &SearchIntent) -> Result<(), String> {
    match intent {
        SearchIntent::FileSearch(search) => {
            validate_limit(search.limit)?;
            validate_size(search.size.as_ref())?;
        }
        SearchIntent::MediaSearch(search) => {
            validate_limit(search.limit)?;
            validate_size(search.size.as_ref())?;
            validate_size(search.duration.as_ref())?;
        }
        SearchIntent::FileAction(action) => {
            match action.action {
                FileActionKind::Copy | FileActionKind::Move => {
                    require_non_empty(action.destination.as_deref(), "destination")?;
                }
                FileActionKind::Rename => {
                    require_non_empty(action.new_name.as_deref(), "new_name")?;
                }
                FileActionKind::Open | FileActionKind::Locate | FileActionKind::Delete => {}
            }
            if matches!(
                action.action,
                FileActionKind::Copy
                    | FileActionKind::Move
                    | FileActionKind::Rename
                    | FileActionKind::Delete
            ) && !action.requires_confirmation
            {
                return Err("write actions require confirmation".to_owned());
            }
            validate_target_ref(&action.target_ref)?;
        }
        SearchIntent::Refine(refine) => {
            validate_limit(refine.delta.limit)?;
            validate_size(refine.delta.size.as_ref())?;
            validate_size(refine.delta.duration.as_ref())?;
        }
        SearchIntent::Clarify(clarify) => {
            require_non_empty(Some(clarify.question.as_str()), "question")?;
        }
    }

    Ok(())
}

fn validate_limit(limit: Option<u32>) -> Result<(), String> {
    if limit.is_some_and(|value| value == 0 || value > 500) {
        return Err("limit must be between 1 and 500".to_owned());
    }
    Ok(())
}

fn validate_size(size: Option<&SizeExpression>) -> Result<(), String> {
    match size {
        Some(
            SizeExpression::GreaterThan { value, .. } | SizeExpression::LessThan { value, .. },
        ) if *value <= 0.0 => Err("size value must be positive".to_owned()),
        Some(SizeExpression::Between { min, max, .. }) if *min < 0.0 || *max <= 0.0 => {
            Err("size range must be non-negative and have positive max".to_owned())
        }
        _ => Ok(()),
    }
}

fn validate_target_ref(target_ref: &TargetRef) -> Result<(), String> {
    match target_ref {
        TargetRef::LastResults { selector } => match selector {
            TargetSelector::Index { value } if *value == 0 => {
                Err("target index must be 1-based".to_owned())
            }
            TargetSelector::Indices { values } if values.is_empty() || values.contains(&0) => {
                Err("target indices must be non-empty and 1-based".to_owned())
            }
            TargetSelector::Index { .. } | TargetSelector::Indices { .. } | TargetSelector::All => {
                Ok(())
            }
        },
        TargetRef::Path { value } => require_non_empty(Some(value.as_str()), "target path"),
        TargetRef::Paths { values } => {
            if values.is_empty() {
                return Err("target paths must be non-empty".to_owned());
            }
            for v in values {
                require_non_empty(Some(v.as_str()), "target path")?;
            }
            Ok(())
        }
    }
}

fn require_non_empty(value: Option<&str>, field: &str) -> Result<(), String> {
    if value.map_or(true, str::is_empty) {
        return Err(format!("{field} must be non-empty"));
    }
    Ok(())
}

fn assert_schema_rejects(value: serde_json::Value) {
    let intent = serde_json::from_value::<SearchIntent>(value)
        .map_err(|error| error.to_string())
        .and_then(|intent| validate_runtime_constraints(&intent));
    assert!(intent.is_err(), "非法样本必须被拒绝");
}

#[test]
fn schema_file_is_valid_json_schema_2020_12_document() {
    let schema = load_schema();

    assert_eq!(
        schema.get("$schema").and_then(serde_json::Value::as_str),
        Some("https://json-schema.org/draft/2020-12/schema")
    );
    assert_eq!(
        schema.get("type").and_then(serde_json::Value::as_str),
        Some("object")
    );
    assert!(schema.get("$defs").is_some());
}

#[test]
fn all_fixture_cases_pass_schema_and_serde_cross_check() {
    let cases = load_cases();
    assert_eq!(cases.len(), 50);

    for case in cases {
        let intent: SearchIntent = serde_json::from_value(case.intent).unwrap();
        validate_runtime_constraints(&intent).unwrap();
    }
}

#[test]
fn intentionally_invalid_samples_are_rejected() {
    for sample in [
        serde_json::json!({"schema_version":"1.0","intent":"file_search","limit":501}),
        serde_json::json!({"schema_version":"1.0","intent":"file_search","extra":true}),
        serde_json::json!({"schema_version":"1.0","intent":"file_action","action":"copy","target_ref":{"source":"path","value":"/tmp/a"},"requires_confirmation":true}),
        serde_json::json!({"schema_version":"1.0","intent":"file_action","action":"rename","target_ref":{"source":"path","value":"/tmp/a"},"new_name":"b","requires_confirmation":false}),
        serde_json::json!({"schema_version":"1.0","intent":"file_action","action":"open","target_ref":{"source":"last_results","selector":{"type":"index","value":0}},"requires_confirmation":false}),
        serde_json::json!({"schema_version":"1.0","intent":"clarify","reason":"unknown","question":""}),
    ] {
        assert_schema_rejects(sample);
    }
}

#[test]
fn target_ref_paths_serde_roundtrip() {
    let tr = TargetRef::Paths {
        values: vec!["/tmp/a.pdf".to_owned(), "/tmp/b.pdf".to_owned()],
    };
    let json = serde_json::to_string(&tr).unwrap();
    assert!(json.contains("\"source\":\"paths\""), "实得 {json}");
    let back: TargetRef = serde_json::from_str(&json).unwrap();
    assert_eq!(back, tr);
}

#[test]
fn validate_target_ref_paths_non_empty_ok() {
    let tr = TargetRef::Paths {
        values: vec!["/tmp/a.pdf".to_owned()],
    };
    assert!(validate_target_ref(&tr).is_ok());
}

#[test]
fn validate_target_ref_paths_empty_errs() {
    let tr = TargetRef::Paths { values: vec![] };
    assert!(validate_target_ref(&tr).is_err());
}
