//! BETA-26 语义检索质量探针——一次性丢弃 crate。
//! 产出 go/no-go 数字 + 方法学，非生产代码。GO/NO-GO 后可删。

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// 冻结语料里的一篇文档（chunk 粒度）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusDoc {
    pub id: String,
    pub path: String,
    pub text: String,
}

/// 一条模糊检索 case：query + 应命中的文件 id（按语义应然，独立于任何检索器实际返回）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalCase {
    pub id: String,
    /// 5 桶之一：synonym | concept | crosslang | ocr | content-not-name
    pub bucket: String,
    pub query: String,
    /// doc id -> 相关度分级 1..=3（用于 nDCG）
    pub relevant: Vec<RelevantDoc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelevantDoc {
    pub doc_id: String,
    pub grade: u8,
}

/// 两向量 cosine（输入未必归一化，这里不假设）。
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    // 零向量直接判 0，避免除零；此处与 0.0 精确比较是刻意的边界检查。
    #[allow(clippy::float_cmp)]
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Reciprocal Rank Fusion：每个榜单贡献 1/(k + rank)，按总分降序。返回融合后的 doc id 序（去重）。
pub fn rrf_fuse(rankings: &[&Vec<String>], k: usize) -> Vec<String> {
    let mut score: HashMap<String, f64> = HashMap::new();
    for ranking in rankings {
        for (rank, id) in ranking.iter().enumerate() {
            *score.entry(id.clone()).or_insert(0.0) += 1.0 / (k as f64 + (rank as f64 + 1.0));
        }
    }
    let mut ids: Vec<String> = score.keys().cloned().collect();
    ids.sort_by(|a, b| {
        score[b]
            .partial_cmp(&score[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.cmp(b))
    });
    ids
}

/// 加权 RRF：每个榜单按其权重贡献 `weight * 1/(k + rank)`，按总分降序。
/// `rankings` 与 `weights` 一一对应（长度须一致；不一致则缺失权重按 0 处理）。
/// 这是 `rrf_fuse` 的推广：所有权重取 1.0 时与 `rrf_fuse` 等价（含同分 tie-break 一致）。
pub fn weighted_rrf_fuse(rankings: &[&Vec<String>], weights: &[f64], k: usize) -> Vec<String> {
    let mut score: HashMap<String, f64> = HashMap::new();
    for (i, ranking) in rankings.iter().enumerate() {
        let w = weights.get(i).copied().unwrap_or(0.0);
        for (rank, id) in ranking.iter().enumerate() {
            *score.entry(id.clone()).or_insert(0.0) += w / (k as f64 + (rank as f64 + 1.0));
        }
    }
    let mut ids: Vec<String> = score.keys().cloned().collect();
    ids.sort_by(|a, b| {
        score[b]
            .partial_cmp(&score[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.cmp(b))
    });
    ids
}

/// Recall@k：前 k 命中的相关文件数 / 相关文件总数。
pub fn recall_at_k(ranked: &[String], relevant: &HashSet<String>, k: usize) -> f64 {
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

/// nDCG@k：grade 作增益 (2^g - 1)，折扣 1/log2(rank+1)，对理想 DCG 归一化。
pub fn ndcg_at_k(ranked: &[String], grades: &HashMap<String, u8>, k: usize) -> f64 {
    let dcg = |ids: &[String]| -> f64 {
        ids.iter()
            .take(k)
            .enumerate()
            .map(|(i, id)| {
                let g = *grades.get(id).unwrap_or(&0) as f64;
                (2f64.powf(g) - 1.0) / (i as f64 + 2.0).log2()
            })
            .sum()
    };
    let actual = dcg(ranked);
    let mut ideal_ids: Vec<String> = grades.keys().cloned().collect();
    ideal_ids.sort_by(|a, b| grades[b].cmp(&grades[a]));
    let ideal = dcg(&ideal_ids);
    // 理想 DCG 为 0（无相关项）时归一化无意义，返回 0。
    #[allow(clippy::float_cmp)]
    if ideal == 0.0 {
        0.0
    } else {
        actual / ideal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_basic() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    }

    #[test]
    fn rrf_fuses_two_rankings() {
        let fts = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let vec = vec!["a".to_string(), "c".to_string(), "d".to_string()];
        let fused = rrf_fuse(&[&fts, &vec], 60);
        assert_eq!(fused[0], "a");
        assert!(fused.contains(&"d".to_string()));
    }

    #[test]
    fn weighted_rrf_equals_rrf_when_unit_weights() {
        // 单位权重时 weighted_rrf_fuse 必须与 rrf_fuse 完全一致（顺序、tie-break）。
        let fts = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let vec = vec!["a".to_string(), "c".to_string(), "d".to_string()];
        let baseline = rrf_fuse(&[&fts, &vec], 60);
        let weighted = weighted_rrf_fuse(&[&fts, &vec], &[1.0, 1.0], 60);
        assert_eq!(baseline, weighted, "单位权重应退化为普通 RRF");
    }

    #[test]
    fn weighted_rrf_vec_weight_lifts_vec_only_hit() {
        // 提高向量榜单权重应把「只在向量榜、排名靠前」的 doc 顶到「只在 FTS 榜、排名靠前」的 doc 之前。
        // fts: x 在第 1 位（rank=0）；vec: y 在第 1 位（rank=0）。等权下 tie-break 按字典序 x < y。
        let fts = vec!["x".to_string()];
        let vec = vec!["y".to_string()];
        let equal = weighted_rrf_fuse(&[&fts, &vec], &[1.0, 1.0], 60);
        assert_eq!(equal[0], "x", "等权同分时按字典序 x 在前");
        let favor_vec = weighted_rrf_fuse(&[&fts, &vec], &[1.0, 3.0], 60);
        assert_eq!(favor_vec[0], "y", "向量权重更高时 y 应反超 x");
    }

    #[test]
    fn recall_at_k_counts_hits() {
        let ranked = vec!["x".to_string(), "y".to_string(), "z".to_string()];
        let relevant: std::collections::HashSet<String> =
            ["y".to_string(), "w".to_string()].into_iter().collect();
        assert!((recall_at_k(&ranked, &relevant, 3) - 0.5).abs() < 1e-6);
        assert!((recall_at_k(&ranked, &relevant, 1) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn ndcg_at_k_rewards_high_rank() {
        let grades: std::collections::HashMap<String, u8> =
            [("a".to_string(), 3u8), ("b".to_string(), 2u8)]
                .into_iter()
                .collect();
        let perfect = vec!["a".to_string(), "b".to_string()];
        let swapped = vec!["b".to_string(), "a".to_string()];
        let n_perfect = ndcg_at_k(&perfect, &grades, 10);
        let n_swapped = ndcg_at_k(&swapped, &grades, 10);
        assert!((n_perfect - 1.0).abs() < 1e-6, "理想序 nDCG 应为 1.0");
        assert!(n_swapped < n_perfect, "次优序 nDCG 应更低");
    }
}
