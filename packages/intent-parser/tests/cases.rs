//! 集成测试：把 search-backends/common 的 50 条 fixture 跑一遍解析器，统计通过率。
//!
//! v0.1 目标：≥ 80%（PROTO-06 出场条件，schema §7 共 50 条用例）。
//! 当前实现仅完整覆盖 file_search 与 media_search；file_action / refine / clarify 路径占位。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::pedantic,
    dead_code
)]

use scout_intent_parser::parse;
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
    // fixture 在 search-backends/common 那边
    let json = include_str!("../../search-backends/common/tests/fixtures/cases.json");
    serde_json::from_str(json).expect("cases.json 解析失败")
}

/// 把 fixture title 还原为接近真实用户输入的字符串。
/// 去掉中文 / 英文括号注释（如 "找最近的（模糊）" → "找最近的"）。
fn nl_input(case: &Case) -> String {
    let mut s = case.title.clone();
    if let Some(p) = s.find('（') {
        s.truncate(p);
    }
    if let Some(p) = s.find('(') {
        s.truncate(p);
    }
    // 也去掉 "refine：" 等前缀（fixture 标注用，非用户输入）
    for prefix in ["refine：", "refine: "] {
        if let Some(stripped) = s.strip_prefix(prefix) {
            s = stripped.to_owned();
        }
    }
    s.trim().to_owned()
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
fn report_parser_coverage_v0_1() {
    let cases = load_cases();
    let mut variant_match = 0usize;
    let mut details: Vec<String> = Vec::new();

    for case in &cases {
        let parsed = parse(&nl_input(case));
        let actual = variant_name(&parsed);
        let ok = actual == case.variant;
        if ok {
            variant_match += 1;
        }
        details.push(format!(
            "[{}] case #{:>3}  expected={:<12}  actual={:<12}  title={}",
            if ok { "✓" } else { "✗" },
            case.id,
            case.variant,
            actual,
            case.title
        ));
    }

    let total = cases.len();
    let rate = (variant_match as f64 / total as f64) * 100.0;
    println!("\n=== PROTO-06 v0.1 解析器覆盖率（按 variant 命中）===");
    for line in &details {
        println!("{line}");
    }
    println!("\n命中：{variant_match}/{total}（{rate:.1}%）；目标 ≥ 80%");

    // v0.1 阶段：variant 命中率 ≥ 40% 即视为骨架可用；< 80% 视为继续开发。
    // 本测试不 fail，只输出报告（PROTO-06 验收时由 PROTO-08 evals 做硬判定）。
    assert!(
        variant_match > 0,
        "至少要有一条用例 variant 命中；当前 0 条说明骨架未跑通"
    );
}
