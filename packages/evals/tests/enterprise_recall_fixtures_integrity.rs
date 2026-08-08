//! BETA-41：企业场景评测集完整性（corpus/cases 始终 checked-in，常跑）。
//! 含「合成集入仓做 CI 门控」隐私红线的机器可查部分（邮箱域名白名单，见
//! `check_enterprise_integrity`）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use scout_evals::semantic_quality::data::{
    check_enterprise_integrity, load_cases, load_corpus, ENTERPRISE_BUCKETS, ENTERPRISE_SCENARIOS,
};
use std::path::PathBuf;

fn fixt(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/enterprise-recall")
        .join(rel)
}

#[test]
fn enterprise_fixtures_are_well_formed() {
    let corpus = load_corpus(&fixt("corpus.json")).expect("读 corpus.json");
    let cases = load_cases(&fixt("cases.json")).expect("读 cases.json");
    check_enterprise_integrity(&corpus, &cases).expect("企业集完整性 + 隐私红线");

    assert!(
        corpus.len() >= 100,
        "语料应 >= 100 篇, 实得 {}",
        corpus.len()
    );
    assert!(cases.len() >= 45, "评测集应 >= 45 条, 实得 {}", cases.len());

    // 五桶都有 case（每桶 ≥ 9，spec §4.3 中档规模下限）
    for b in ENTERPRISE_BUCKETS {
        let n = cases.iter().filter(|c| c.bucket == *b).count();
        assert!(n >= 9, "桶 {b} 仅 {n} 条（应 ≥ 9）");
    }

    // 三场景都有语料（每场景 ≥ 30）
    for s in ENTERPRISE_SCENARIOS {
        let n = corpus
            .iter()
            .filter(|d| d.scenario.as_deref() == Some(*s))
            .count();
        assert!(n >= 30, "场景 {s} 仅 {n} 篇（应 ≥ 30）");
    }

    // 近重复组 ≥ 8 组（BETA-38 doc identity 靶子）
    let groups: std::collections::HashSet<&str> = corpus
        .iter()
        .filter_map(|d| d.dup_group.as_deref())
        .collect();
    assert!(
        groups.len() >= 8,
        "近重复组仅 {} 组（应 ≥ 8）",
        groups.len()
    );

    // 扫描件语料 ≥ 15 篇（BETA-35 命中率报告的分母下限）
    let scanned = corpus
        .iter()
        .filter(|d| d.doc_type.as_deref() == Some("scanned"))
        .count();
    assert!(scanned >= 15, "scanned 语料仅 {scanned} 篇（应 ≥ 15）");

    // 跨语言：zh 与 en 语料同时存在
    assert!(corpus.iter().any(|d| d.lang == "zh") && corpus.iter().any(|d| d.lang == "en"));

    // near-dup 桶的每条 case：relevant 覆盖某 dup_group 的全部成员（召回不该丢副本）
    for c in cases.iter().filter(|c| c.bucket == "near-dup") {
        let rel_ids: std::collections::HashSet<&str> =
            c.relevant.iter().map(|r| r.doc_id.as_str()).collect();
        let group = corpus
            .iter()
            .filter(|d| rel_ids.contains(d.doc_id.as_str()))
            .find_map(|d| d.dup_group.as_deref())
            .unwrap_or_else(|| panic!("near-dup case {} 未指向任何 dup_group 成员", c.id));
        for member in corpus
            .iter()
            .filter(|d| d.dup_group.as_deref() == Some(group))
        {
            assert!(
                rel_ids.contains(member.doc_id.as_str()),
                "near-dup case {} 漏标组 {group} 成员 {}",
                c.id,
                member.doc_id
            );
        }
    }
}
