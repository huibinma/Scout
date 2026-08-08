//! BETA-15A 召回门槛集成测试：用 ship 词典 + checked-in fixtures 跑全管线，
//! 断言 recall >= 70% 且 fp <= 5%。随 `cargo test --workspace`（ci.sh test 步）自动执行。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use scout_evals::recall::{check_integrity, load_cases, load_corpus, run_recall};
use scout_harness::YamlSynonymExpander;

fn ws(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

#[test]
fn synonym_recall_meets_gate() {
    let expander = YamlSynonymExpander::from_paths(
        &ws("resources/synonyms/zh.yaml"),
        &ws("resources/synonyms/en.yaml"),
    )
    .expect("ship 词典应加载成功");

    let corpus = load_corpus(&ws("packages/evals/fixtures/synonym-recall/corpus.json")).unwrap();
    let cases = load_cases(&ws("packages/evals/fixtures/synonym-recall/cases.json")).unwrap();
    check_integrity(&corpus, &cases).expect("fixture 引用完整");
    assert!(
        cases.len() >= 30,
        "BETA-15A 验收要求 >= 30 case, 实际 {}",
        cases.len()
    );

    let report = run_recall(&expander, &corpus, &cases);
    assert!(
        report.passes_gate(),
        "召回门槛未过: recall={:.1}% fp={:.1}%",
        report.recall_rate() * 100.0,
        report.false_positive_rate() * 100.0
    );
}
