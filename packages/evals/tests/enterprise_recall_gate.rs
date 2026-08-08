//! BETA-41 回归门：企业合成集 hybrid 各桶不跌破提交 baseline。
//! 跑 checked-in 缓存向量（确定性）。vectors.json / baseline.json 未提交（bootstrap 前）→ 跳过，
//! 与 semantic-recall Phase D 同款语义。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::path::PathBuf;

use scout_evals::semantic_quality::data::{
    check_enterprise_integrity, check_vectors, load_cases, load_corpus, load_vectors,
    ENTERPRISE_BUCKETS,
};
use scout_evals::semantic_quality::report::{aggregate, score_case, BucketAgg};
use scout_evals::semantic_quality::{EVAL_SIMILARITY_FLOOR, TOP_K};
use scout_result_normalizer::{
    DEFAULT_COSINE_ROUTING_THRESHOLD, DEFAULT_RRF_K, DEFAULT_SEMANTIC_WEIGHT,
};

fn fixt(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/enterprise-recall")
        .join(rel)
}

fn find_bucket<'a>(aggs: &'a [BucketAgg], name: &str) -> Option<&'a BucketAgg> {
    aggs.iter().find(|a| a.bucket == name)
}

#[test]
#[allow(clippy::items_after_statements)]
fn enterprise_hybrid_does_not_regress_vs_baseline() {
    if !fixt("vectors.json").exists() || !fixt("baseline.json").exists() {
        eprintln!("跳过：enterprise vectors.json / baseline.json 未提交（bootstrap 前）");
        return;
    }
    let corpus = load_corpus(&fixt("corpus.json")).unwrap();
    let cases = load_cases(&fixt("cases.json")).unwrap();
    check_enterprise_integrity(&corpus, &cases).unwrap();
    let vectors = load_vectors(&fixt("vectors.json")).unwrap();
    check_vectors(&corpus, &cases, &vectors).unwrap();

    let scores: Vec<_> = cases
        .iter()
        .map(|c| {
            score_case(
                c,
                &corpus,
                &vectors,
                EVAL_SIMILARITY_FLOOR,
                DEFAULT_SEMANTIC_WEIGHT,
                DEFAULT_RRF_K,
                DEFAULT_COSINE_ROUTING_THRESHOLD,
                TOP_K,
            )
        })
        .collect();
    let aggs = aggregate(&scores);

    let baseline: Vec<BucketAgg> =
        serde_json::from_str(&std::fs::read_to_string(fixt("baseline.json")).unwrap()).unwrap();

    const EPS: f64 = 1e-6;
    // 各桶 + OVERALL：HYB 与 HYBR（recall / ndcg）均不退步 baseline 同臂水位。
    for bucket in ENTERPRISE_BUCKETS.iter().copied().chain(["OVERALL"]) {
        if let (Some(base), Some(now)) =
            (find_bucket(&baseline, bucket), find_bucket(&aggs, bucket))
        {
            assert!(
                now.hybrid_recall + EPS >= base.hybrid_recall,
                "{bucket} HYB_R 回退: {:.3} < baseline {:.3}",
                now.hybrid_recall,
                base.hybrid_recall
            );
            assert!(
                now.hybrid_ndcg + EPS >= base.hybrid_ndcg,
                "{bucket} HYB_N 回退: {:.3} < baseline {:.3}",
                now.hybrid_ndcg,
                base.hybrid_ndcg
            );
            assert!(
                now.hybrid_routed_recall + EPS >= base.hybrid_routed_recall,
                "{bucket} HYBR_R 回退: {:.3} < baseline {:.3}",
                now.hybrid_routed_recall,
                base.hybrid_routed_recall
            );
            assert!(
                now.hybrid_routed_ndcg + EPS >= base.hybrid_routed_ndcg,
                "{bucket} HYBR_N 回退: {:.3} < baseline {:.3}",
                now.hybrid_routed_ndcg,
                base.hybrid_routed_ndcg
            );
        }
    }
}
