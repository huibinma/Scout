//! BETA-13：v0.9 评测集完整性门。校验**已提交的产物文件**，与生成器解耦——
//! 即便有人手改了 cases.json / coverage-cases.json，这里也会抓出来。
//!
//! 守三条不变量：
//! 1. coverage 每条 `expected_intent` 是合法 `SearchIntent`（schema 门）。
//! 2. v0.9/cases.json = v0.5（逐字在前）+ coverage（逐字在后），即确定性合并产物。
//! 3. 全局 id 唯一。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use scout_search_backend::SearchIntent;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Deserialize)]
struct RawCase {
    id: String,
    query: String,
    language: String,
    expected_intent: Value,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn load(path: &Path) -> Vec<RawCase> {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("读 {} 失败：{e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("解析 {} 失败：{e}", path.display()))
}

#[test]
fn coverage_cases_are_schema_valid() {
    let coverage = load(&fixtures_dir().join("v0.9").join("coverage-cases.json"));
    for case in &coverage {
        serde_json::from_value::<SearchIntent>(case.expected_intent.clone()).unwrap_or_else(|e| {
            panic!(
                "coverage case {} expected_intent 非法 SearchIntent：{e}",
                case.id
            )
        });
    }
}

#[test]
fn v09_is_deterministic_merge_of_v05_and_coverage() {
    let base = load(&fixtures_dir().join("v0.5").join("cases.json"));
    let coverage = load(&fixtures_dir().join("v0.9").join("coverage-cases.json"));
    let merged = load(&fixtures_dir().join("v0.9").join("cases.json"));

    assert_eq!(
        merged.len(),
        base.len() + coverage.len(),
        "v0.9 总数应 = v0.5 + coverage"
    );
    // 逐字保留：base 段在前、coverage 段在后。
    assert_eq!(
        &merged[..base.len()],
        base.as_slice(),
        "v0.9 前段应逐字等于 v0.5"
    );
    assert_eq!(
        &merged[base.len()..],
        coverage.as_slice(),
        "v0.9 后段应逐字等于 coverage"
    );
}

#[test]
fn v09_ids_are_globally_unique() {
    let merged = load(&fixtures_dir().join("v0.9").join("cases.json"));
    let mut seen = std::collections::HashSet::new();
    for case in &merged {
        assert!(seen.insert(case.id.as_str()), "v0.9 id 冲突：{}", case.id);
    }
}
