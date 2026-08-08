//! BETA-15B-6 → ... → BETA-15B-10 回归门：合成集 hybrid 在关键桶不跌破提交 baseline。
//! 跑 checked-in 缓存向量（确定性）。vectors.json / baseline.json 未提交（Phase D 前）→ 跳过。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::path::PathBuf;

use scout_evals::semantic_quality::data::{
    check_integrity, check_vectors, load_cases, load_corpus, load_vectors,
};
use scout_evals::semantic_quality::report::{aggregate, score_case, BucketAgg};
use scout_evals::semantic_quality::{EVAL_SIMILARITY_FLOOR, TOP_K};
use scout_result_normalizer::{
    DEFAULT_COSINE_ROUTING_THRESHOLD, DEFAULT_RRF_K, DEFAULT_SEMANTIC_WEIGHT,
};

fn fixt(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/semantic-recall")
        .join(rel)
}

fn find_bucket<'a>(aggs: &'a [BucketAgg], name: &str) -> Option<&'a BucketAgg> {
    aggs.iter().find(|a| a.bucket == name)
}

#[test]
#[allow(clippy::items_after_statements)]
fn hybrid_does_not_regress_key_buckets_vs_baseline() {
    if !fixt("vectors.json").exists() || !fixt("baseline.json").exists() {
        eprintln!("跳过：vectors.json / baseline.json 未提交（Phase D 用户 bootstrap 前）");
        return;
    }
    let corpus = load_corpus(&fixt("corpus.json")).unwrap();
    let cases = load_cases(&fixt("cases.json")).unwrap();
    check_integrity(&corpus, &cases).unwrap();
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
    for bucket in ["crosslang", "exact-name", "OVERALL"] {
        if let (Some(base), Some(now)) =
            (find_bucket(&baseline, bucket), find_bucket(&aggs, bucket))
        {
            assert!(
                now.hybrid_recall + EPS >= base.hybrid_recall,
                "{bucket} hybrid Recall@10 回退: {:.3} < baseline {:.3}",
                now.hybrid_recall,
                base.hybrid_recall
            );
        }
    }

    // BETA-15B-3 A-2 红线：exact-name 桶 hybrid recall 必须 = 1.0。
    // 调权重/下限/路由的任何改动若破坏这条，应在 evals 阶段就被门挡下。
    let exact_name = aggs
        .iter()
        .find(|a| a.bucket == "exact-name")
        .expect("exact-name 桶必须存在");
    assert!(
        (exact_name.hybrid_recall - 1.0).abs() < EPS,
        "exact-name hybrid recall 跌破 1.0：{} ← 硬红线（FTS 对精确名约束）",
        exact_name.hybrid_recall
    );

    // BETA-15B-3 A-5 红线 + BETA-15B-6 v2 → v3 → BETA-15B-10 v5 校验：HYBR（hybrid_routed）各桶不退步 HYB baseline，
    // exact-name HYBR_R = 1.0 硬断言，OVERALL/crosslang HYBR_N 自锁 baseline.hybrid_routed_*
    // —— 4 红线动态读 baseline、A-5 T*=0.60 → v2/v3 T*=0.70 → v5 T*=0.70 bake（cosine_threshold 字面值不变、baseline 数值刷新）后无需替换。
    // 诚实边界：bge-m3 真水位 crosslang 0.686 < 0.700 spec 字面、移交未来 cycle = 更大 / 跨厂 embedding 模型。
    // 4a：exact-name HYBR_R = 1.0（硬红线）
    let exact_name_now = aggs
        .iter()
        .find(|a| a.bucket == "exact-name")
        .expect("exact-name 桶");
    assert!(
        (exact_name_now.hybrid_routed_recall - 1.0).abs() < EPS,
        "exact-name HYBR_R 跌破 1.0：{} ← A-3 硬红线",
        exact_name_now.hybrid_routed_recall
    );
    // 4b：各桶 HYBR_N ≥ HYB baseline 同桶（不退步）+ HYBR_R 不退步 HYB baseline
    for bucket in [
        "synonym",
        "concept",
        "crosslang",
        "content-not-name",
        "exact-name",
        "OVERALL",
    ] {
        if let (Some(base), Some(now)) =
            (find_bucket(&baseline, bucket), find_bucket(&aggs, bucket))
        {
            assert!(
                now.hybrid_routed_ndcg + EPS >= base.hybrid_ndcg,
                "{bucket} HYBR_N 退步 HYB baseline：{:.3} < {:.3}",
                now.hybrid_routed_ndcg,
                base.hybrid_ndcg
            );
            assert!(
                now.hybrid_routed_recall + EPS >= base.hybrid_recall,
                "{bucket} HYBR_R 退步 HYB baseline：{:.3} < {:.3}",
                now.hybrid_routed_recall,
                base.hybrid_recall
            );
        }
    }
    // 4c / 4d：HYBR_N 守 baseline 锁定水位（A-3 sweep 实测 t* 时所得；新 baseline 已写、此处与 baseline.hybrid_routed_* 相比即可）
    for bucket in ["crosslang", "OVERALL"] {
        if let (Some(base), Some(now)) =
            (find_bucket(&baseline, bucket), find_bucket(&aggs, bucket))
        {
            assert!(
                now.hybrid_routed_ndcg + EPS >= base.hybrid_routed_ndcg,
                "{bucket} HYBR_N 跌破新 baseline：{:.3} < baseline {:.3}",
                now.hybrid_routed_ndcg,
                base.hybrid_routed_ndcg
            );
        }
    }
}
