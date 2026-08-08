//! 逐 case 三臂打分 + 分桶聚合。

use super::arms::{fts_rank, hybrid_rank, vector_rank};
use super::data::{SemanticCase, SemanticDoc, VectorCache};
use super::metrics::{ndcg_at_k, recall_at_k};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// 单 case 四臂打分（FTS / VEC / HYBR-rrf / HYBR-routed）。
#[derive(Debug, Clone, Serialize)]
pub struct CaseScores {
    pub id: String,
    pub bucket: String,
    pub fts_recall: f64,
    pub vec_recall: f64,
    pub hybrid_recall: f64,
    pub fts_ndcg: f64,
    pub vec_ndcg: f64,
    pub hybrid_ndcg: f64,
    pub hybrid_routed_recall: f64,
    pub hybrid_routed_ndcg: f64,
}

/// 分桶（及 OVERALL）均值。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketAgg {
    pub bucket: String,
    pub n: usize,
    pub fts_recall: f64,
    pub vec_recall: f64,
    pub hybrid_recall: f64,
    pub fts_ndcg: f64,
    pub vec_ndcg: f64,
    pub hybrid_ndcg: f64,
    pub hybrid_routed_recall: f64,
    pub hybrid_routed_ndcg: f64,
}

/// 跑四臂 + 算 Recall@k/nDCG@k。`floor`/`weight`/`k_rrf`/`cosine_threshold` 是生产融合旋钮。
#[must_use]
#[allow(clippy::cast_precision_loss, clippy::too_many_arguments)]
pub fn score_case(
    case: &SemanticCase,
    corpus: &[SemanticDoc],
    vectors: &VectorCache,
    floor: f32,
    weight: f64,
    k_rrf: f64,
    cosine_threshold: f64,
    top_k: usize,
) -> CaseScores {
    /// 三臂取的候选池大小（指标只看前 `top_k`，池子放宽以容纳融合重排）。
    const POOL: usize = 50;

    let relevant_set: HashSet<String> = case.relevant.iter().map(|r| r.doc_id.clone()).collect();
    let grades: HashMap<String, u8> = case
        .relevant
        .iter()
        .map(|r| (r.doc_id.clone(), r.grade))
        .collect();

    let fts = fts_rank(corpus, &case.query, POOL).unwrap_or_default();
    let empty = Vec::new();
    let qv = vectors.query_vectors.get(&case.id).unwrap_or(&empty);
    let vec_scored = vector_rank(qv, &vectors.doc_vectors, floor, POOL);
    // 只要 doc_id 给 ndcg / recall 算分（不消费 cosine）
    let vec_ids: Vec<String> = vec_scored.iter().map(|(id, _)| id.clone()).collect();
    let hybrid = hybrid_rank(corpus, &fts, &vec_scored, weight, k_rrf);
    let hybrid_routed =
        super::arms::hybrid_routed_rank(corpus, &fts, &vec_scored, cosine_threshold, weight, k_rrf);

    CaseScores {
        id: case.id.clone(),
        bucket: case.bucket.clone(),
        fts_recall: recall_at_k(&fts, &relevant_set, top_k),
        vec_recall: recall_at_k(&vec_ids, &relevant_set, top_k),
        hybrid_recall: recall_at_k(&hybrid, &relevant_set, top_k),
        fts_ndcg: ndcg_at_k(&fts, &grades, top_k),
        vec_ndcg: ndcg_at_k(&vec_ids, &grades, top_k),
        hybrid_ndcg: ndcg_at_k(&hybrid, &grades, top_k),
        hybrid_routed_recall: recall_at_k(&hybrid_routed, &relevant_set, top_k),
        hybrid_routed_ndcg: ndcg_at_k(&hybrid_routed, &grades, top_k),
    }
}

/// 分桶（保出现序）+ OVERALL 均值。
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn aggregate(scores: &[CaseScores]) -> Vec<BucketAgg> {
    let mut buckets: Vec<String> = Vec::new();
    for s in scores {
        if !buckets.contains(&s.bucket) {
            buckets.push(s.bucket.clone());
        }
    }
    let agg_for = |subset: &[&CaseScores], name: &str| -> BucketAgg {
        let n = subset.len();
        let mean = |sel: &dyn Fn(&CaseScores) -> f64| -> f64 {
            if n == 0 {
                0.0
            } else {
                subset.iter().map(|s| sel(s)).sum::<f64>() / n as f64
            }
        };
        BucketAgg {
            bucket: name.to_owned(),
            n,
            fts_recall: mean(&|s| s.fts_recall),
            vec_recall: mean(&|s| s.vec_recall),
            hybrid_recall: mean(&|s| s.hybrid_recall),
            fts_ndcg: mean(&|s| s.fts_ndcg),
            vec_ndcg: mean(&|s| s.vec_ndcg),
            hybrid_ndcg: mean(&|s| s.hybrid_ndcg),
            hybrid_routed_recall: mean(&|s| s.hybrid_routed_recall),
            hybrid_routed_ndcg: mean(&|s| s.hybrid_routed_ndcg),
        }
    };
    let mut out: Vec<BucketAgg> = buckets
        .iter()
        .map(|b| {
            let subset: Vec<&CaseScores> = scores.iter().filter(|s| &s.bucket == b).collect();
            agg_for(&subset, b)
        })
        .collect();
    let all: Vec<&CaseScores> = scores.iter().collect();
    out.push(agg_for(&all, "OVERALL"));
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn aggregate_means_per_bucket_and_overall() {
        let scores = vec![
            CaseScores {
                id: "a".into(),
                bucket: "crosslang".into(),
                fts_recall: 0.0,
                vec_recall: 1.0,
                hybrid_recall: 1.0,
                fts_ndcg: 0.0,
                vec_ndcg: 1.0,
                hybrid_ndcg: 1.0,
                hybrid_routed_recall: 1.0,
                hybrid_routed_ndcg: 1.0,
            },
            CaseScores {
                id: "b".into(),
                bucket: "crosslang".into(),
                fts_recall: 0.0,
                vec_recall: 0.0,
                hybrid_recall: 0.5,
                fts_ndcg: 0.0,
                vec_ndcg: 0.0,
                hybrid_ndcg: 0.5,
                hybrid_routed_recall: 0.5,
                hybrid_routed_ndcg: 0.5,
            },
        ];
        let aggs = aggregate(&scores);
        let cl = aggs.iter().find(|a| a.bucket == "crosslang").unwrap();
        assert_eq!(cl.n, 2);
        assert!((cl.hybrid_recall - 0.75).abs() < 1e-9);
        let overall = aggs.iter().find(|a| a.bucket == "OVERALL").unwrap();
        assert_eq!(overall.n, 2);
    }

    #[test]
    fn score_case_runs_four_arms_with_cosine_routing() {
        let corpus = vec![
            SemanticDoc {
                doc_id: "d1".into(),
                lang: "zh".into(),
                title: "年假".into(),
                body: "年假和远程办公规定".into(),
                scenario: None,
                doc_type: None,
                dup_group: None,
            },
            SemanticDoc {
                doc_id: "d2".into(),
                lang: "en".into(),
                title: "leave".into(),
                body: "annual leave policy".into(),
                scenario: None,
                doc_type: None,
                dup_group: None,
            },
        ];
        let case = SemanticCase {
            id: "c1".into(),
            bucket: "crosslang".into(),
            query: "年假规定".into(),
            relevant: vec![super::super::data::RelevantDoc {
                doc_id: "d1".into(),
                grade: 3,
            }],
        };
        let mut vc = VectorCache {
            model_id: "m".into(),
            dim: 2,
            doc_vectors: BTreeMap::new(),
            query_vectors: BTreeMap::new(),
        };
        vc.doc_vectors.insert("d1".into(), vec![1.0, 0.0]);
        vc.doc_vectors.insert("d2".into(), vec![0.9, 0.1]);
        vc.query_vectors.insert("c1".into(), vec![1.0, 0.0]);
        // d1 cosine = 1.0、d2 cosine ≈ 0.9；threshold = 0.5 → cosine_top1 = 1.0 ≥ 0.5 → 跳 FTS
        let s = score_case(&case, &corpus, &vc, 0.30, 2.0, 60.0, 0.50, 10);
        assert_eq!(s.id, "c1");
        assert!(s.vec_recall > 0.0, "向量臂应召回 d1");
        // HYBR-routed 跳 FTS → 应等价纯 vec
        assert!(
            (s.hybrid_routed_recall - s.vec_recall).abs() < 1e-9,
            "跳 FTS 后 HYBR_R 应等于 VEC_R"
        );
    }
}
