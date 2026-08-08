//! BETA-15B-6：合成评测集完整性（corpus/cases 始终 checked-in，常跑）。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scout_evals::semantic_quality::data::{check_integrity, load_cases, load_corpus, BUCKETS};
use std::path::PathBuf;

fn fixt(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/semantic-recall")
        .join(rel)
}

#[test]
fn fixtures_are_well_formed() {
    let corpus = load_corpus(&fixt("corpus.json")).expect("读 corpus.json");
    let cases = load_cases(&fixt("cases.json")).expect("读 cases.json");
    check_integrity(&corpus, &cases).expect("完整性");
    assert!(
        corpus.len() >= 100,
        "语料应 >= 100 篇, 实得 {}",
        corpus.len()
    );
    assert!(cases.len() >= 40, "评测集应 >= 40 条, 实得 {}", cases.len());
    // 5 桶都有 case
    for b in BUCKETS {
        assert!(cases.iter().any(|c| c.bucket == *b), "桶 {b} 无 case");
    }
    // 跨语言：corpus 同时含 zh 和 en
    assert!(corpus.iter().any(|d| d.lang == "zh") && corpus.iter().any(|d| d.lang == "en"));
}
