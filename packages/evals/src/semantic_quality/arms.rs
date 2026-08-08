//! 三臂排名：FTS5（trigram）/ 向量（cosine + 下限）/ hybrid（生产 `fuse_rrf`）。

use super::data::SemanticDoc;
use scout_indexer::vectors::cosine;
use scout_result_normalizer::fuse_rrf;
use scout_search_backend::{BackendKind, MatchType, SearchResult, SearchResultMetadata};
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::PathBuf;

/// 把 query 构造成 FTS5 trigram-OR MATCH 串（照搬 BETA-26 已验证逻辑）。
fn build_match_query(query: &str) -> Option<String> {
    let norm: Vec<char> = query.chars().filter(|c| c.is_alphanumeric()).collect();
    if norm.is_empty() {
        return None;
    }
    let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    if norm.len() < 3 {
        let s: String = norm.iter().collect();
        return Some(quote(&s));
    }
    let mut seen = HashSet::new();
    let mut terms = Vec::new();
    for w in norm.windows(3) {
        let tri: String = w.iter().collect();
        if seen.insert(tri.clone()) {
            terms.push(quote(&tri));
        }
    }
    Some(terms.join(" OR "))
}

/// FTS5 臂：内存 trigram 索引合成正文 → `BM25` 排名 → top `limit` 的 `doc_id`（有序）。
pub fn fts_rank(corpus: &[SemanticDoc], query: &str, limit: usize) -> anyhow::Result<Vec<String>> {
    let Some(match_str) = build_match_query(query) else {
        return Ok(Vec::new());
    };
    let conn = rusqlite::Connection::open_in_memory()?;
    conn.execute_batch(
        "CREATE VIRTUAL TABLE corpus_fts USING fts5(id UNINDEXED, text, tokenize='trigram');",
    )?;
    {
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare("INSERT INTO corpus_fts(id, text) VALUES (?1, ?2)")?;
            for d in corpus {
                let text = format!("{}\n{}", d.title, d.body);
                stmt.execute((&d.doc_id, &text))?;
            }
        }
        tx.commit()?;
    }
    let mut stmt = conn.prepare(
        "SELECT id FROM corpus_fts WHERE corpus_fts MATCH ?1 ORDER BY bm25(corpus_fts) LIMIT ?2",
    )?;
    let limit = i64::try_from(limit)?;
    let rows = stmt.query_map((&match_str, limit), |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// 向量臂：query 向量 vs 全 doc 向量 cosine → 过滤 `>= floor` → 降序 → top `limit` 的 `(doc_id, cosine)`。
/// BETA-15B-3 A-5：返 tuple 让 cosine 透传给 `hybrid_routed_rank` 内 `to_results_with_scores`，
/// `SearchResult.score` 挂 cosine 后 wrapper 取 `vec[0].score` 作路由信号。
#[must_use]
pub fn vector_rank(
    query_vec: &[f32],
    doc_vectors: &BTreeMap<String, Vec<f32>>,
    floor: f32,
    limit: usize,
) -> Vec<(String, f32)> {
    let mut scored: Vec<(f32, &str)> = doc_vectors
        .iter()
        .map(|(id, v)| (cosine(query_vec, v), id.as_str()))
        .filter(|(s, _)| *s >= floor)
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored
        .into_iter()
        .take(limit)
        .map(|(s, id)| (id.to_owned(), s))
        .collect()
}

/// 把有序 `doc_id` 列表包装成生产 `SearchResult`：
/// `path` 仍 `doc_id`（dedup key 不变）；`name` 取 corpus 中 `doc.title` 供合并代表结果展示
/// （A-5 起 wrapper 路由由 cosine 驱动、不再消费 `name` 作 lang 信号）；找不到 → fallback `doc_id`。
fn to_results(
    corpus: &[SemanticDoc],
    ids: &[String],
    source: BackendKind,
    mt: MatchType,
) -> Vec<SearchResult> {
    ids.iter()
        .map(|id| {
            let name = corpus
                .iter()
                .find(|d| d.doc_id == *id)
                .map_or_else(|| id.clone(), |d| d.title.clone());
            SearchResult {
                id: id.clone(),
                path: PathBuf::from(id),
                name,
                source,
                match_type: mt,
                score: None,
                metadata: SearchResultMetadata::default(),
            }
        })
        .collect()
}

/// 把 `(doc_id, cosine)` 列表包装成生产 `SearchResult`：与 [`to_results`] 同款 path/name 规则，
/// 多挂 `score = Some(f64::from(cosine))`。BETA-15B-3 A-5：让 wrapper 内
/// `vec[0].score.unwrap_or(0.0)` 拿到真 cosine 作路由信号。
fn to_results_with_scores(
    corpus: &[SemanticDoc],
    scored: &[(String, f32)],
    source: BackendKind,
    mt: MatchType,
) -> Vec<SearchResult> {
    scored
        .iter()
        .map(|(id, s)| {
            let name = corpus
                .iter()
                .find(|d| d.doc_id == *id)
                .map_or_else(|| id.clone(), |d| d.title.clone());
            SearchResult {
                id: id.clone(),
                path: PathBuf::from(id),
                name,
                source,
                match_type: mt,
                score: Some(f64::from(*s)),
                metadata: SearchResultMetadata::default(),
            }
        })
        .collect()
}

/// hybrid 臂：FTS 臂(`NativeIndex`) + 向量臂(`SemanticIndex`) 喂**生产** `fuse_rrf`,
/// 取融合后有序 `doc_id`。`semantic_weight`/`k` 即生产融合的两个调优旋钮。
#[must_use]
pub fn hybrid_rank(
    corpus: &[SemanticDoc],
    fts: &[String],
    vec_scored: &[(String, f32)],
    semantic_weight: f64,
    k: f64,
) -> Vec<String> {
    let lists = vec![
        to_results(corpus, fts, BackendKind::NativeIndex, MatchType::Content),
        to_results_with_scores(
            corpus,
            vec_scored,
            BackendKind::SemanticIndex,
            MatchType::Semantic,
        ),
    ];
    fuse_rrf(lists, k, semantic_weight)
        .into_iter()
        .map(|m| m.result.id)
        .collect()
}

/// 加 cosine 路由的 hybrid 臂：VEC top-1 cosine ≥ `cosine_threshold` 时跳过 FTS
/// （hybrid 退化为纯向量）。喂 **生产 wrapper** `fuse_rrf_with_fts_routing`，
/// `semantic_weight`/`k`/`cosine_threshold` 即生产路由的三个旋钮。
///
/// **参数顺序注意**：本函数 `(corpus, fts, vec_scored, cosine_threshold, semantic_weight, k)`
/// 与生产 wrapper [`scout_result_normalizer::fuse_rrf_with_fts_routing`] 的
/// `(fts, vec, rrf_k, semantic_weight, cosine_threshold)` 在 `cosine_threshold` 与
/// `rrf_k/semantic_weight` 两组次序颠倒——arms 层延续 A-3 `(routing 旋钮, rrf 旋钮)` 约定，
/// wrapper 层是 `(rrf 旋钮, routing 旋钮)`。caller 拼调用时**先看 arms 签名、不要照搬 wrapper 顺序**。
///
/// 评测层不消费 `RouteVerdict.query_lang`（默认 Mixed 占位、wiring 后置覆写）。
#[must_use]
pub fn hybrid_routed_rank(
    corpus: &[SemanticDoc],
    fts: &[String],
    vec_scored: &[(String, f32)],
    cosine_threshold: f64,
    semantic_weight: f64,
    k: f64,
) -> Vec<String> {
    let fts_results = to_results(corpus, fts, BackendKind::NativeIndex, MatchType::Content);
    let vec_results = to_results_with_scores(
        corpus,
        vec_scored,
        BackendKind::SemanticIndex,
        MatchType::Semantic,
    );
    let (merged, _verdict) = scout_result_normalizer::fuse_rrf_with_fts_routing(
        fts_results,
        vec_results,
        k,
        semantic_weight,
        cosine_threshold,
    );
    merged.into_iter().map(|m| m.result.id).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// 测试用 doc 工厂：title 取 body（与生产 `to_results` 行为一致；A-5 起 `hybrid_routed_rank`
    /// 内部纯 cosine 驱动、title 不再影响路由）。
    fn doc(id: &str, body: &str) -> SemanticDoc {
        SemanticDoc {
            doc_id: id.into(),
            lang: "zh".into(),
            title: body.into(),
            body: body.into(),
            scenario: None,
            doc_type: None,
            dup_group: None,
        }
    }

    #[test]
    fn fts_ranks_trigram_overlap_doc_first() {
        let corpus = vec![
            doc("d1", "年假规定与远程办公的细则说明"),
            doc("d2", "完全无关的烹饪食谱内容"),
        ];
        let ranked = fts_rank(&corpus, "年假规定", 10).unwrap();
        assert_eq!(ranked.first().map(String::as_str), Some("d1"));
        assert!(!ranked.contains(&"d2".to_owned()), "无关文档不应命中");
    }

    #[test]
    fn fts_empty_query_yields_empty() {
        let corpus = vec![doc("d1", "x")];
        assert!(fts_rank(&corpus, "   ", 10).unwrap().is_empty());
    }

    #[test]
    fn vector_ranks_by_cosine_and_applies_floor_with_scores() {
        use std::collections::BTreeMap;
        let mut docs: BTreeMap<String, Vec<f32>> = BTreeMap::new();
        docs.insert("near".into(), vec![1.0, 0.0]);
        docs.insert("mid".into(), vec![0.6, 0.8]);
        docs.insert("far".into(), vec![0.0, 1.0]);
        let q = vec![1.0_f32, 0.0];
        let ranked = vector_rank(&q, &docs, 0.30, 10);
        // 返 (doc_id, cosine) tuple、降序、地板过滤；近 cosine≈1.0、中 cosine≈0.6、远 cosine=0.0 < 0.30 被过滤
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].0, "near");
        assert!((ranked[0].1 - 1.0).abs() < 1e-5);
        assert_eq!(ranked[1].0, "mid");
        assert!((ranked[1].1 - 0.6).abs() < 1e-5);
    }

    #[test]
    fn hybrid_fuses_both_arms_and_weights_semantic() {
        let corpus = vec![doc("d_both", "x"), doc("d_fts", "y"), doc("d_vec", "z")];
        let fts = vec!["d_both".to_owned(), "d_fts".to_owned()];
        let vec_scored = vec![("d_both".to_owned(), 0.85), ("d_vec".to_owned(), 0.70)];
        let ranked = hybrid_rank(&corpus, &fts, &vec_scored, 2.0, 60.0);
        assert_eq!(ranked.first().map(String::as_str), Some("d_both"));
        for id in ["d_both", "d_fts", "d_vec"] {
            assert!(ranked.contains(&id.to_owned()), "{id} 应在融合结果");
        }
    }

    #[test]
    fn hybrid_routed_low_cosine_uses_both_arms() {
        // vec[0].cosine < threshold → 不跳；hybrid 用两臂
        let corpus = vec![doc("d_a", "policy text"), doc("d_b", "leave guide")];
        let fts = vec!["d_a".to_owned()];
        let vec_scored = vec![("d_a".to_owned(), 0.40), ("d_b".to_owned(), 0.30)];
        let ranked = hybrid_routed_rank(&corpus, &fts, &vec_scored, 0.80, 10.0, 60.0);
        for id in ["d_a", "d_b"] {
            assert!(ranked.contains(&id.to_owned()), "{id} 应在融合结果");
        }
    }

    #[test]
    fn hybrid_routed_high_cosine_skips_fts() {
        // vec[0].cosine >= threshold → 跳 FTS；hybrid 退化为纯向量
        let corpus = vec![doc("d_fts", "fts only"), doc("d_vec", "semantic hit")];
        let fts = vec!["d_fts".to_owned()];
        let vec_scored = vec![("d_vec".to_owned(), 0.85)];
        let ranked = hybrid_routed_rank(&corpus, &fts, &vec_scored, 0.80, 10.0, 60.0);
        assert!(
            !ranked.contains(&"d_fts".to_owned()),
            "跳 FTS 后 d_fts 不在结果"
        );
        assert!(ranked.contains(&"d_vec".to_owned()));
    }

    #[test]
    fn hybrid_routed_threshold_above_one_never_skips() {
        // threshold = 1.01 > cosine 物理上限 → 永不跳（与 hybrid_rank 等价）
        let corpus = vec![doc("d_a", "policy"), doc("d_vec", "vector hit")];
        let fts = vec!["d_a".to_owned()];
        let vec_scored = vec![("d_vec".to_owned(), 0.99)];
        let routed = hybrid_routed_rank(&corpus, &fts, &vec_scored, 1.01, 10.0, 60.0);
        let direct = hybrid_rank(&corpus, &fts, &vec_scored, 10.0, 60.0);
        assert_eq!(routed, direct);
    }

    #[test]
    fn hybrid_routed_both_empty_returns_empty() {
        let corpus = vec![];
        let ranked = hybrid_routed_rank(&corpus, &[], &[], 0.50, 10.0, 60.0);
        assert!(ranked.is_empty());
    }
}
