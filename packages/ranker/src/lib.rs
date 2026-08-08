//! BETA-05 Ranker：对 BETA-04 fan-out 合并集排序。
//!
//! - **默认（相关性）**：纯启发式打分——name-match（查询词在文件名）+ match-type + 多源一致，
//!   写入 `result.score`，降序 + tiebreak（modified 新→前 / name 升序）。
//! - **显式 sort**（时间/大小/名称）：跨源生效（合并集本无全局排序）。
//!
//! 纯函数、无 IO。BM25 不接（跨语料不可比，留未来）；仅作用于 fan-out 路径。

// usize→f64 的相关性计算遍布本 crate（计数 / 比例），统一允许 precision_loss。
// 文档含 BM25 / name-match 等领域词，沿用项目对 doc_markdown 的处理。
#![allow(clippy::cast_precision_loss, clippy::doc_markdown)]

use std::cmp::Ordering;
use std::collections::HashSet;

use scout_result_normalizer::MergedResult;
use scout_search_backend::{
    intent_sort_order, ExpandedSearchIntent, MatchType, SearchIntent, SortOrder,
};

// 相关性权重（文档化常量，和为 1 → score ∈ [0,1]）。
const W_NAME: f64 = 0.5;
const W_MATCH: f64 = 0.3;
const W_SOURCE: f64 = 0.2;

/// 排序上下文：查询关键词（相关性用）+ 用户显式排序。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RankContext {
    /// 查询关键词（小写、去重、非空），用于 name-match。
    pub keywords: Vec<String>,
    /// 用户显式排序；`None` / `RelevanceDesc` → 相关性启发式。
    pub sort: Option<SortOrder>,
    /// 输入是否已经带有可跨来源比较的融合分（当前为 Hybrid RRF）。
    /// true 时保留该分数为主排序，启发式只作 tie-break，避免把 semantic_weight/RRF 覆盖掉。
    pub preserve_existing_relevance: bool,
}

impl RankContext {
    /// 从扩展意图提取关键词（base intent + 同义词组）+ 排序。
    #[must_use]
    pub fn from_expanded(expanded: &ExpandedSearchIntent) -> Self {
        let mut raw: Vec<String> = Vec::new();
        match &expanded.base {
            SearchIntent::FileSearch(fs) => {
                if let Some(k) = &fs.keywords {
                    raw.extend(k.iter().cloned());
                }
            }
            SearchIntent::MediaSearch(ms) => {
                if let Some(k) = &ms.keywords {
                    raw.extend(k.iter().cloned());
                }
                raw.extend(
                    [&ms.artist, &ms.title, &ms.album]
                        .into_iter()
                        .flatten()
                        .cloned(),
                );
            }
            SearchIntent::Refine(_) | SearchIntent::FileAction(_) | SearchIntent::Clarify(_) => {}
        }
        for group in &expanded.keyword_groups {
            for term in group.all() {
                raw.push(term.to_string());
            }
        }

        let mut seen = HashSet::new();
        let keywords = raw
            .into_iter()
            .map(|s| s.to_lowercase())
            .filter(|s| !s.trim().is_empty() && seen.insert(s.clone()))
            .collect();

        Self {
            keywords,
            sort: intent_sort_order(&expanded.base),
            preserve_existing_relevance: false,
        }
    }

    /// 标记输入已由 RRF 等统一融合器产生可比较分数。
    #[must_use]
    pub const fn preserve_existing_relevance(mut self) -> Self {
        self.preserve_existing_relevance = true;
        self
    }
}

/// 对合并集排序。显式 sort → 跨源生效；否则相关性降序（写入 `result.score`）。
#[must_use]
pub fn rank(mut results: Vec<MergedResult>, ctx: &RankContext) -> Vec<MergedResult> {
    match ctx.sort {
        Some(sort) if sort != SortOrder::RelevanceDesc => {
            sort_explicit(&mut results, sort);
        }
        _ if ctx.preserve_existing_relevance => {
            results.sort_by(|a, b| {
                cmp_f64_desc(a.result.score, b.result.score)
                    .then_with(|| {
                        relevance_score(b, &ctx.keywords)
                            .partial_cmp(&relevance_score(a, &ctx.keywords))
                            .unwrap_or(Ordering::Equal)
                    })
                    .then_with(|| {
                        cmp_opt_desc(
                            a.result.metadata.modified_time,
                            b.result.metadata.modified_time,
                        )
                    })
                    .then_with(|| {
                        a.result
                            .name
                            .to_lowercase()
                            .cmp(&b.result.name.to_lowercase())
                    })
            });
        }
        _ => {
            for m in &mut results {
                m.result.score = Some(relevance_score(m, &ctx.keywords));
            }
            results.sort_by(|a, b| {
                // score 降序 → modified_time 降序 → name 升序（稳定确定性）。
                cmp_f64_desc(a.result.score, b.result.score)
                    .then_with(|| {
                        cmp_opt_desc(
                            a.result.metadata.modified_time,
                            b.result.metadata.modified_time,
                        )
                    })
                    .then_with(|| {
                        a.result
                            .name
                            .to_lowercase()
                            .cmp(&b.result.name.to_lowercase())
                    })
            });
        }
    }
    results
}

/// 把若干**已各自排好序**的桶按 round-robin 交错为单一列表（按 canonical `path` 去重）。
///
/// 跨范畴多类型查询的均衡展示用：每个桶是一个 `file_type`（如图片桶 / 视频桶），轮流取首条
/// → 各类型在结果前若干条都可见，避免少数派类型被多数派碾压不可见。桶内顺序保持调用方
/// 传入时的排序（通常已 [`rank`] 过）。跨桶按扩展名天然不重，path 去重为防御性。
#[must_use]
pub fn interleave(buckets: Vec<Vec<MergedResult>>) -> Vec<MergedResult> {
    let total: usize = buckets.iter().map(Vec::len).sum();
    let mut seen: HashSet<std::path::PathBuf> = HashSet::with_capacity(total);
    let mut out: Vec<MergedResult> = Vec::with_capacity(total);
    let mut iters: Vec<std::vec::IntoIter<MergedResult>> =
        buckets.into_iter().map(Vec::into_iter).collect();

    let mut progressed = true;
    while progressed {
        progressed = false;
        for it in &mut iters {
            if let Some(m) = it.next() {
                progressed = true;
                if seen.insert(m.result.path.clone()) {
                    out.push(m);
                }
            }
        }
    }
    out
}

/// 相关性分 ∈ [0,1]：`0.5·name_match + 0.3·match_weight + 0.2·source_boost`。
fn relevance_score(m: &MergedResult, keywords: &[String]) -> f64 {
    W_NAME * name_match(&m.result.name, keywords)
        + W_MATCH * match_type_weight(&m.match_types)
        + W_SOURCE * source_boost(m.sources.len())
}

/// 命中 `keywords` 的比例（文件名小写子串）。无 keyword → 0。
fn name_match(name: &str, keywords: &[String]) -> f64 {
    if keywords.is_empty() {
        return 0.0;
    }
    let lname = name.to_lowercase();
    let hits = keywords
        .iter()
        .filter(|k| lname.contains(k.as_str()))
        .count();
    hits as f64 / keywords.len() as f64
}

/// match_types 取最大权重：Filename 1.0 / Metadata 0.85 / Content 0.7 / Semantic 0.7 / Ocr 0.6。
fn match_type_weight(match_types: &[MatchType]) -> f64 {
    match_types
        .iter()
        .map(|mt| match mt {
            MatchType::Filename => 1.0,
            MatchType::Metadata => 0.85,
            // 语义向量命中与 Content 同权（BETA-15B 初始值；调优属 15B-3）。
            MatchType::Content | MatchType::Semantic => 0.7,
            MatchType::Ocr => 0.6,
        })
        .fold(0.0_f64, f64::max)
}

/// 多源一致加权：单源 0，3+ 源封顶 1。
fn source_boost(n: usize) -> f64 {
    let extra = n.saturating_sub(1).min(2);
    extra as f64 / 2.0
}

/// 显式排序（语义对齐 `common::sort_results`，作用于 `MergedResult`，缺失字段排末尾）。
fn sort_explicit(results: &mut [MergedResult], sort: SortOrder) {
    match sort {
        SortOrder::RelevanceDesc => {}
        SortOrder::ModifiedDesc => results.sort_by(|a, b| {
            cmp_opt_desc(
                a.result.metadata.modified_time,
                b.result.metadata.modified_time,
            )
        }),
        SortOrder::ModifiedAsc => results.sort_by(|a, b| {
            cmp_opt_asc(
                a.result.metadata.modified_time,
                b.result.metadata.modified_time,
            )
        }),
        SortOrder::CreatedDesc => results.sort_by(|a, b| {
            cmp_opt_desc(
                a.result.metadata.created_time,
                b.result.metadata.created_time,
            )
        }),
        SortOrder::CreatedAsc => results.sort_by(|a, b| {
            cmp_opt_asc(
                a.result.metadata.created_time,
                b.result.metadata.created_time,
            )
        }),
        SortOrder::AccessedDesc => results.sort_by(|a, b| {
            cmp_opt_desc(
                a.result.metadata.accessed_time,
                b.result.metadata.accessed_time,
            )
        }),
        SortOrder::SizeDesc => results.sort_by(|a, b| {
            cmp_opt_desc(a.result.metadata.size_bytes, b.result.metadata.size_bytes)
        }),
        SortOrder::SizeAsc => results.sort_by(|a, b| {
            cmp_opt_asc(a.result.metadata.size_bytes, b.result.metadata.size_bytes)
        }),
        SortOrder::NameAsc => results.sort_by_key(|m| m.result.name.to_lowercase()),
        SortOrder::NameDesc => {
            results.sort_by_key(|m| std::cmp::Reverse(m.result.name.to_lowercase()));
        }
    }
}

/// 升序；`None`（缺失）始终排末尾（与 spec「缺失字段排末尾」一致）。
fn cmp_opt_asc<T: Ord>(a: Option<T>, b: Option<T>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// 降序；`None`（缺失）始终排末尾（值降序但缺失项不抢到首位）。
fn cmp_opt_desc<T: Ord>(a: Option<T>, b: Option<T>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// f64 降序；`None` 排末尾；NaN 不会出现（score ∈ [0,1]）。
fn cmp_f64_desc(a: Option<f64>, b: Option<f64>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => y.partial_cmp(&x).unwrap_or(Ordering::Equal),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use scout_search_backend::{
        BackendKind, FileSearch, MediaSearch, MediaType, SchemaVersion, SearchResult,
        SearchResultMetadata,
    };
    use std::path::PathBuf;

    fn merged(name: &str, sources: Vec<BackendKind>, match_types: Vec<MatchType>) -> MergedResult {
        MergedResult {
            result: SearchResult {
                id: name.to_owned(),
                path: PathBuf::from(format!("/{name}")),
                name: name.to_owned(),
                source: sources[0],
                match_type: match_types[0],
                score: None,
                metadata: SearchResultMetadata::default(),
            },
            sources,
            match_types,
            semantic_cosine: None,
        }
    }

    fn ctx(keywords: &[&str]) -> RankContext {
        RankContext {
            keywords: keywords.iter().map(|s| s.to_lowercase()).collect(),
            sort: None,
            preserve_existing_relevance: false,
        }
    }

    #[test]
    fn name_hit_ranks_first() {
        let results = vec![
            merged(
                "random.txt",
                vec![BackendKind::Spotlight],
                vec![MatchType::Content],
            ),
            merged(
                "budget-report.txt",
                vec![BackendKind::Spotlight],
                vec![MatchType::Content],
            ),
        ];
        let out = rank(results, &ctx(&["budget"]));
        assert_eq!(out[0].result.name, "budget-report.txt", "命中关键词应排首");
        assert!(out[0].result.score.unwrap() > out[1].result.score.unwrap());
    }

    #[test]
    fn filename_match_type_beats_content() {
        // 同样命中关键词，但 Filename match 权重 > Content。
        let results = vec![
            merged(
                "budget.txt",
                vec![BackendKind::Spotlight],
                vec![MatchType::Content],
            ),
            merged(
                "budget.txt",
                vec![BackendKind::Spotlight],
                vec![MatchType::Filename],
            ),
        ];
        // 不同 path 才不被视作同一条——这里 name 相同但 path 不同。
        let mut results = results;
        results[0].result.path = PathBuf::from("/a/budget.txt");
        results[1].result.path = PathBuf::from("/b/budget.txt");
        let out = rank(results, &ctx(&["budget"]));
        assert_eq!(out[0].match_types, vec![MatchType::Filename]);
    }

    #[test]
    fn multi_source_beats_single_when_otherwise_equal() {
        let results = vec![
            merged(
                "a.txt",
                vec![BackendKind::Spotlight],
                vec![MatchType::Content],
            ),
            merged(
                "a.txt",
                vec![BackendKind::Spotlight, BackendKind::NativeIndex],
                vec![MatchType::Content],
            ),
        ];
        let mut results = results;
        results[0].result.path = PathBuf::from("/x/a.txt");
        results[1].result.path = PathBuf::from("/y/a.txt");
        let out = rank(results, &ctx(&["nomatch"]));
        assert_eq!(out[0].sources.len(), 2, "多源应排前");
    }

    #[test]
    fn score_within_unit_range() {
        let results = vec![merged(
            "budget.txt",
            vec![
                BackendKind::Spotlight,
                BackendKind::NativeIndex,
                BackendKind::WindowsSearch,
            ],
            vec![MatchType::Filename],
        )];
        let out = rank(results, &ctx(&["budget"]));
        let s = out[0].result.score.unwrap();
        assert!((0.0..=1.0).contains(&s), "score 应 ∈ [0,1], 实得 {s}");
        // 全满：name 1.0 + filename 1.0 + 3 源 1.0 → 0.5+0.3+0.2 = 1.0。
        assert!((s - 1.0).abs() < 1e-9, "全满应为 1.0, 实得 {s}");
    }

    #[test]
    fn tiebreak_by_modified_then_name() {
        use chrono::{TimeZone, Utc};
        // 同分（无 keyword、同 match/source）→ modified 新者前。
        let mut a = merged(
            "zzz.txt",
            vec![BackendKind::Spotlight],
            vec![MatchType::Content],
        );
        let mut b = merged(
            "aaa.txt",
            vec![BackendKind::Spotlight],
            vec![MatchType::Content],
        );
        a.result.path = PathBuf::from("/1/zzz.txt");
        b.result.path = PathBuf::from("/2/aaa.txt");
        a.result.metadata.modified_time = Some(Utc.timestamp_opt(2000, 0).single().unwrap());
        b.result.metadata.modified_time = Some(Utc.timestamp_opt(1000, 0).single().unwrap());
        let out = rank(vec![b, a], &ctx(&[]));
        assert_eq!(out[0].result.name, "zzz.txt", "modified 新者应排首");
    }

    #[test]
    fn empty_input() {
        assert!(rank(vec![], &ctx(&["x"])).is_empty());
    }

    #[test]
    fn hybrid_preserves_rrf_score_as_primary_order() {
        let mut strong_heuristic = merged(
            "budget-report.txt",
            vec![BackendKind::Spotlight, BackendKind::NativeIndex],
            vec![MatchType::Filename],
        );
        strong_heuristic.result.score = Some(0.10);
        let mut high_rrf = merged(
            "unrelated.txt",
            vec![BackendKind::SemanticIndex],
            vec![MatchType::Semantic],
        );
        high_rrf.result.score = Some(0.90);

        let rc = ctx(&["budget"]).preserve_existing_relevance();
        let out = rank(vec![strong_heuristic, high_rrf], &rc);
        assert_eq!(out[0].result.name, "unrelated.txt");
        assert_eq!(out[0].result.score, Some(0.90), "RRF 分数不得被覆盖");
        assert_eq!(out[1].result.score, Some(0.10), "RRF 分数不得被覆盖");
    }

    #[test]
    fn explicit_size_desc_sorts_largest_first() {
        let mut a = merged(
            "a.txt",
            vec![BackendKind::Spotlight],
            vec![MatchType::Filename],
        );
        let mut b = merged(
            "b.txt",
            vec![BackendKind::Spotlight],
            vec![MatchType::Filename],
        );
        a.result.path = PathBuf::from("/1/a.txt");
        b.result.path = PathBuf::from("/2/b.txt");
        a.result.metadata.size_bytes = Some(10);
        b.result.metadata.size_bytes = Some(999);
        let rc = RankContext {
            keywords: vec![],
            sort: Some(SortOrder::SizeDesc),
            preserve_existing_relevance: false,
        };
        let out = rank(vec![a, b], &rc);
        assert_eq!(out[0].result.name, "b.txt", "最大应排首");
        // 显式排序不写 relevance score。
        assert!(out[0].result.score.is_none());
    }

    #[test]
    fn explicit_name_asc() {
        let mut a = merged(
            "Zebra.txt",
            vec![BackendKind::Spotlight],
            vec![MatchType::Filename],
        );
        let mut b = merged(
            "apple.txt",
            vec![BackendKind::Spotlight],
            vec![MatchType::Filename],
        );
        a.result.path = PathBuf::from("/1/Zebra.txt");
        b.result.path = PathBuf::from("/2/apple.txt");
        let rc = RankContext {
            keywords: vec![],
            sort: Some(SortOrder::NameAsc),
            preserve_existing_relevance: false,
        };
        let out = rank(vec![a, b], &rc);
        assert_eq!(out[0].result.name, "apple.txt");
    }

    #[test]
    fn missing_field_sorts_last_in_explicit() {
        let mut a = merged(
            "a.txt",
            vec![BackendKind::Spotlight],
            vec![MatchType::Filename],
        );
        let mut b = merged(
            "b.txt",
            vec![BackendKind::Spotlight],
            vec![MatchType::Filename],
        );
        a.result.path = PathBuf::from("/1/a.txt");
        b.result.path = PathBuf::from("/2/b.txt");
        a.result.metadata.size_bytes = Some(5);
        // b 无 size → 排末尾。
        let rc = RankContext {
            keywords: vec![],
            sort: Some(SortOrder::SizeDesc),
            preserve_existing_relevance: false,
        };
        let out = rank(vec![b, a], &rc);
        assert_eq!(out[0].result.name, "a.txt", "有 size 的排前，None 末尾");
    }

    #[test]
    fn from_expanded_extracts_keywords_and_sort() {
        let base = SearchIntent::FileSearch(FileSearch {
            schema_version: SchemaVersion::V1,
            language: None,
            keywords: Some(vec!["Budget".to_owned()]),
            extensions: None,
            file_type: None,
            location: None,
            modified_time: None,
            created_time: None,
            accessed_time: None,
            size: None,
            exclude_extensions: None,
            exclude_file_type: None,
            sort: Some(SortOrder::ModifiedDesc),
            limit: None,
        });
        let rc = RankContext::from_expanded(&ExpandedSearchIntent::identity(base));
        assert_eq!(rc.keywords, vec!["budget".to_owned()], "应小写");
        assert_eq!(rc.sort, Some(SortOrder::ModifiedDesc));
    }

    #[test]
    fn interleave_round_robin_order() {
        // 桶 A 三条、桶 B 两条 → a1,b1,a2,b2,a3（不等长，长桶尾部续接）。
        let a = vec![
            merged(
                "a1.jpg",
                vec![BackendKind::WindowsSearch],
                vec![MatchType::Filename],
            ),
            merged(
                "a2.jpg",
                vec![BackendKind::WindowsSearch],
                vec![MatchType::Filename],
            ),
            merged(
                "a3.jpg",
                vec![BackendKind::WindowsSearch],
                vec![MatchType::Filename],
            ),
        ];
        let b = vec![
            merged(
                "b1.mp4",
                vec![BackendKind::WindowsSearch],
                vec![MatchType::Filename],
            ),
            merged(
                "b2.mp4",
                vec![BackendKind::WindowsSearch],
                vec![MatchType::Filename],
            ),
        ];
        let out = interleave(vec![a, b]);
        let names: Vec<&str> = out.iter().map(|m| m.result.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["a1.jpg", "b1.mp4", "a2.jpg", "b2.mp4", "a3.jpg"]
        );
    }

    #[test]
    fn interleave_dedups_by_path_across_buckets() {
        // 同 path 出现在两个桶（防御性）：只保留首次（来自首个桶的轮次）。
        let mut a1 = merged(
            "x.dat",
            vec![BackendKind::WindowsSearch],
            vec![MatchType::Filename],
        );
        let mut dup = merged(
            "x.dat",
            vec![BackendKind::Everything],
            vec![MatchType::Filename],
        );
        a1.result.path = PathBuf::from("/shared/x.dat");
        dup.result.path = PathBuf::from("/shared/x.dat");
        let b1 = merged(
            "y.dat",
            vec![BackendKind::WindowsSearch],
            vec![MatchType::Filename],
        );
        let out = interleave(vec![vec![a1], vec![dup, b1]]);
        let names: Vec<&str> = out.iter().map(|m| m.result.name.as_str()).collect();
        // a1(x) 先入 → dup(x) 被去重跳过 → y 续接。
        assert_eq!(names, vec!["x.dat", "y.dat"]);
    }

    #[test]
    fn interleave_empty() {
        assert!(interleave(vec![]).is_empty());
        assert!(interleave(vec![vec![], vec![]]).is_empty());
    }

    #[test]
    fn from_expanded_media_includes_artist() {
        let base = SearchIntent::MediaSearch(MediaSearch {
            schema_version: SchemaVersion::V1,
            language: None,
            media_type: MediaType::Audio,
            artist: Some("周华健".to_owned()),
            title: None,
            album: None,
            genre: None,
            quality: None,
            duration: None,
            keywords: None,
            extensions: None,
            file_type: None,
            location: None,
            modified_time: None,
            created_time: None,
            accessed_time: None,
            size: None,
            exclude_extensions: None,
            exclude_file_type: None,
            sort: None,
            limit: None,
        });
        let rc = RankContext::from_expanded(&ExpandedSearchIntent::identity(base));
        assert!(rc.keywords.contains(&"周华健".to_owned()));
    }
}
