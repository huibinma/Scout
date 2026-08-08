//! Recall@k / nDCG@k 纯函数（公式照搬 BETA-26 已验证版本）。

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;

/// top-k 命中的相关文档数 / 相关文档总数。
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn recall_at_k<S: BuildHasher>(
    ranked: &[String],
    relevant: &HashSet<String, S>,
    k: usize,
) -> f64 {
    if relevant.is_empty() {
        return 0.0;
    }
    let hits = ranked
        .iter()
        .take(k)
        .filter(|id| relevant.contains(*id))
        .count();
    hits as f64 / relevant.len() as f64
}

/// nDCG@k：增益 `2^grade − 1`，折扣 `1/log2(rank+2)`，除以理想排序 DCG。
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn ndcg_at_k<S: BuildHasher>(
    ranked: &[String],
    grades: &HashMap<String, u8, S>,
    k: usize,
) -> f64 {
    let dcg = |ids: &[String]| -> f64 {
        ids.iter()
            .take(k)
            .enumerate()
            .map(|(i, id)| {
                let g = f64::from(*grades.get(id).unwrap_or(&0));
                (2f64.powf(g) - 1.0) / (i as f64 + 2.0).log2()
            })
            .sum()
    };
    let actual = dcg(ranked);
    let mut ideal_ids: Vec<String> = grades.keys().cloned().collect();
    ideal_ids.sort_by(|a, b| grades[b].cmp(&grades[a]));
    let ideal = dcg(&ideal_ids);
    #[allow(clippy::float_cmp)]
    if ideal == 0.0 {
        0.0
    } else {
        actual / ideal
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn recall_counts_hits_in_top_k_over_total_relevant() {
        let ranked = ids(&["a", "x", "b", "y", "z"]);
        let relevant: HashSet<String> = ids(&["a", "b", "c"]).into_iter().collect();
        assert!((recall_at_k(&ranked, &relevant, 3) - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn recall_empty_relevant_is_zero() {
        assert_eq!(recall_at_k(&ids(&["a"]), &HashSet::new(), 10), 0.0);
    }

    #[test]
    fn ndcg_perfect_ranking_is_one() {
        let ranked = ids(&["a", "b"]);
        let mut grades = HashMap::new();
        grades.insert("a".to_owned(), 3u8);
        grades.insert("b".to_owned(), 1u8);
        assert!((ndcg_at_k(&ranked, &grades, 10) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ndcg_reversed_ranking_is_less_than_one() {
        let ranked = ids(&["b", "a"]);
        let mut grades = HashMap::new();
        grades.insert("a".to_owned(), 3u8);
        grades.insert("b".to_owned(), 1u8);
        let v = ndcg_at_k(&ranked, &grades, 10);
        assert!(v > 0.0 && v < 1.0, "reversed nDCG 应 ∈(0,1)，实得 {v}");
    }

    #[test]
    fn ndcg_no_relevant_is_zero() {
        assert_eq!(ndcg_at_k(&ids(&["a"]), &HashMap::new(), 10), 0.0);
    }
}
