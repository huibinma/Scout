//! 集成测试：反序列化 schema §7 的全部 47 条用例（含 #39/#45/#46 的子用例，共 50 条）。
//!
//! 验证目标（PROTO-02 出场标准）：
//! 1. 每条用例 JSON 都能反序列化为 `SearchIntent`。
//! 2. 反序列化后的变体名与 fixture 标注的 `variant` 一致。
//! 3. Round-trip：`SearchIntent → JSON → SearchIntent` 结果与原值相等（PartialEq）。
//!
//! Fixture 文件：`tests/fixtures/cases.json`。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use scout_search_backend::SearchIntent;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Case {
    id: String,
    title: String,
    variant: String,
    intent: serde_json::Value,
}

fn load_cases() -> Vec<Case> {
    let json = include_str!("fixtures/cases.json");
    serde_json::from_str(json).expect("cases.json 解析失败")
}

fn variant_name(intent: &SearchIntent) -> &'static str {
    match intent {
        SearchIntent::FileSearch(_) => "FileSearch",
        SearchIntent::MediaSearch(_) => "MediaSearch",
        SearchIntent::FileAction(_) => "FileAction",
        SearchIntent::Refine(_) => "Refine",
        SearchIntent::Clarify(_) => "Clarify",
    }
}

#[test]
fn fixture_count_matches_schema_v1() {
    // schema §7 共 47 条用例 + #39/#45/#46 各有一个子用例 = 50 条
    let cases = load_cases();
    assert_eq!(
        cases.len(),
        50,
        "fixture 数量与 schema §7 不一致；若 schema 用例数变化，请同步更新 fixture 与本断言"
    );
}

#[test]
fn all_cases_deserialize_and_variant_matches() {
    let cases = load_cases();
    let mut failures: Vec<String> = Vec::new();

    for case in &cases {
        match serde_json::from_value::<SearchIntent>(case.intent.clone()) {
            Ok(intent) => {
                let actual = variant_name(&intent);
                if actual != case.variant {
                    failures.push(format!(
                        "case #{} ({}): 变体不匹配 — fixture={}, 实际={}",
                        case.id, case.title, case.variant, actual
                    ));
                }
            }
            Err(e) => {
                failures.push(format!(
                    "case #{} ({}): 反序列化失败 — {}",
                    case.id, case.title, e
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} 条用例失败：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn all_cases_roundtrip() {
    let cases = load_cases();
    let mut failures: Vec<String> = Vec::new();

    for case in &cases {
        let Ok(intent_a) = serde_json::from_value::<SearchIntent>(case.intent.clone()) else {
            // 反序列化失败已在另一个测试覆盖，这里跳过避免重复噪声
            continue;
        };
        let serialized = match serde_json::to_value(&intent_a) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("case #{} 序列化失败：{e}", case.id));
                continue;
            }
        };
        let intent_b: SearchIntent = match serde_json::from_value(serialized.clone()) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!(
                    "case #{} round-trip 反序列化失败：{e}\n中间 JSON：{serialized}",
                    case.id
                ));
                continue;
            }
        };
        if intent_a != intent_b {
            failures.push(format!(
                "case #{} round-trip 值不一致\n原：{intent_a:?}\n后：{intent_b:?}",
                case.id
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} 条用例 round-trip 失败：\n{}",
        failures.len(),
        failures.join("\n")
    );
}
