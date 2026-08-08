//! BETA-04 Result Normalizer：把多源搜索结果按 canonical path 去重合并。
//!
//! 把 fan-out 多后端（系统搜索 + 本地索引）返回的 [`SearchResult`] 合并为去重后的
//! [`MergedResult`] 列表——同一文件被多个后端命中时合成一条，保留全部来源 / 命中类型，
//! 代表结果取 metadata 最丰富者。**排序（BM25 / 打分）留 BETA-05 Ranker**，本层只保持首现序。
//!
//! 纯函数、无 IO：路径规范化由各 backend 负责（产出 `SearchResult` 时 `canonicalize`），
//! 本层按 `path` 字节相等去重。

pub mod lang;

use std::collections::HashMap;
use std::path::PathBuf;

use scout_search_backend::{BackendKind, MatchType, SearchResult};

/// 合并后的一条结果：代表结果 + 多源溯源。
#[derive(Debug, Clone, PartialEq)]
pub struct MergedResult {
    /// 代表结果（metadata 最丰富者）。
    pub result: SearchResult,
    /// 命中此 path 的所有后端（稳定去重序）。
    pub sources: Vec<BackendKind>,
    /// 命中类型并集（稳定去重序）。
    pub match_types: Vec<MatchType>,
    /// 语义原始 cosine（0-1）：源于 `BackendKind::SemanticIndex` 的 `result.score` 直传。
    /// 多源同 path 时取 max。**与 `result.score` 语义不同**：`result.score` 融合后是 RRF 累积分（排序用），
    /// 而 `semantic_cosine` 是给用户看的**真相似度**（评估 `semantic_similarity_floor`、按相似度排序）。
    /// BETA-33 cycle 3 v3（v0.9.3）新加。
    pub semantic_cosine: Option<f64>,
}

/// 按 canonical path 去重合并多源结果，保持首现顺序。
///
/// 合并规则：
/// - `sources` / `match_types` 取并集（稳定去重）；
/// - 代表结果取 [`metadata_richness`] 最高者（并列保留先到者）；
/// - `score` 取所有同 path 结果的最大值。
#[must_use]
pub fn merge_results(results: Vec<SearchResult>) -> Vec<MergedResult> {
    let mut order: Vec<PathBuf> = Vec::new();
    let mut map: HashMap<PathBuf, MergedResult> = HashMap::new();

    for r in results {
        // 语义原始 cosine 捕获（仅当来源是 SemanticIndex 时）。
        let sem_cos = if r.source == BackendKind::SemanticIndex {
            r.score
        } else {
            None
        };
        if let Some(m) = map.get_mut(&r.path) {
            if !m.sources.contains(&r.source) {
                m.sources.push(r.source);
            }
            if !m.match_types.contains(&r.match_type) {
                m.match_types.push(r.match_type);
            }
            m.semantic_cosine = max_opt(m.semantic_cosine, sem_cos);
            let best_score = max_opt(m.result.score, r.score);
            if metadata_richness(&r) > metadata_richness(&m.result) {
                let mut rep = r;
                rep.score = best_score;
                m.result = rep;
            } else {
                m.result.score = best_score;
            }
        } else {
            order.push(r.path.clone());
            map.insert(
                r.path.clone(),
                MergedResult {
                    sources: vec![r.source],
                    match_types: vec![r.match_type],
                    result: r,
                    semantic_cosine: sem_cos,
                },
            );
        }
    }

    order.into_iter().filter_map(|p| map.remove(&p)).collect()
}

/// `SearchResult` 的 metadata 丰富度 = 非空元数据字段数（用于选代表结果）。
#[must_use]
pub fn metadata_richness(r: &SearchResult) -> usize {
    let m = &r.metadata;
    [
        m.modified_time.is_some(),
        m.created_time.is_some(),
        m.accessed_time.is_some(),
        m.size_bytes.is_some(),
        m.artist.is_some(),
        m.title.is_some(),
        m.album.is_some(),
        m.duration_seconds.is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count()
}

/// 默认 RRF k（BETA-26 实测对 k 不敏感）。
pub const DEFAULT_RRF_K: f64 = 60.0;
/// 默认语义臂权重（FTS 臂权重固定 1.0）。BETA-15B-3 A-2 sweep 选定 W=10.0。
/// 用户可经 `AppSettings.semantic_weight` 覆盖；clamp[0.5, 50.0]。
pub const DEFAULT_SEMANTIC_WEIGHT: f64 = 10.0;

/// VEC top-1 cosine 绝对分数阈值路由：`vec[0].score >= threshold` 时跳过 FTS。
/// BETA-15B-3 A-5 sweep 选定 T\* = 0.60（合成集 v1 / 11 例 content-not-name 桶）；
/// **BETA-15B-6 v2 扩 content-not-name 桶 11→20 后 sweep 微调到 T\* = 0.70**
/// （spec §2.2 接受标准 Branch B：v2 数据集让 sweep best 偏移 +0.10、
/// 处于 Branch B 字面区间 \[0.55, 0.65\] inclusive 上界、bake 跟随）。
/// v2 实测：OVERALL `HYBR_N` 0.854 / crosslang 0.717 / content-not-name 0.853
/// （vs v2 HYB baseline 0.842/0.657/0.852、各桶不退步 ✓）。
/// 诚实边界：v2 上 OVERALL 0.854 < A-5 v1 的 0.871——A-5 v1 11 例 content-not-name 带轻微运气、
/// v2 扩量后真水位 0.854；下 cycle 抓手 = 更大 embedding 模型 / cosine + lang 组合信号 /
/// 评测集再扩量验 T\* 进一步偏移。
/// → BETA-15B-10 v5 T\*=0.70 bake（dataset 81/127、bge-m3 真水位 sweep best 在 T=0.45
/// 但保守选 T=0.70 守 (4b) 严格全过；字面值与 v2/v3 相同、baseline 数值刷新到 v5 corpus）。
///
/// **2026-07-28 起不再是唯一生效值**——仅作 [`cosine_threshold_for_model`] 查不到校准记录时
/// 的保守回落（未知/自定义模型，或已知模型尚未走完 sweep+bake 评审）。已收录模型一律走
/// [`CALIBRATED_COSINE_THRESHOLDS`]。**这个值本身是给 `bge-m3` 调的**（该模型现仍在表内、
/// 数值不变——见下），把它当全局默认只是历史包袱，不代表对任意模型都合适。
pub const DEFAULT_COSINE_ROUTING_THRESHOLD: f64 = 0.70;

/// 路由计数的 top-K 截断窗口；与评测 `TOP_K` 一致。
pub const DEFAULT_FTS_ROUTING_TOP_K: usize = 10;

/// 已校准 embedding 模型 → cosine 路由阈值。**架构意图**：把"要不要信语义臂"这个判断
/// 从孤立的全局常量，改成随生产 embedding 模型一起声明的校准数据——换模型必须在这里
/// 显式补一条（经过 sweep + spec 验收评审的值，而非裸 sweep-best，参考 bge-m3 那条
/// 0.70 是 sweep-best 0.45 之上保守选的，不是 sweep 输出原样照搬），否则
/// [`cosine_threshold_for_model`] 查不到、回落 [`DEFAULT_COSINE_ROUTING_THRESHOLD`]
/// 并由调用方告警——而不是像 BETA-15B-11-v2 把生产模型从 `bge-m3` 切到
/// `embeddinggemma-300m` 那次一样，阈值悄悄继续沿用上一代模型的旧值、直到有人手动
/// 发现（ROADMAP.md BETA-15B 父卡 follow-up ①「`cosine_threshold` 在 embeddinggemma
/// 上 sweep & bake」至今仍挂着，就是这个缺口的证据）。
///
/// **`embeddinggemma-300m`（当前生产默认模型）故意不在表里**：
/// `embedding_model.rs` 模块文档记了一个 no-prefix 模式的 sweep-best（T\*=0.60，
/// OVERALL=0.882），但明确标注是"follow-up cycle 工作"——还没走完 bge-m3 那样的
/// spec 验收评审就直接烘焙进生产，等于在没有复核的情况下引入一个新魔数，重蹈这次
/// 要修的覆辙。**未收录 = 现状零回归**（回落 0.70，与切模型前的实际行为一致），
/// 但现在换成**可观测**的回落（见 [`cosine_threshold_for_model`] 调用方的告警日志），
/// 不再是无声的过期配置。等 embeddinggemma 的评审值定下来，在这里加一行即可。
///
/// key 按**前缀**匹配 `TextEmbedder::model_id()`：各调用方对 `model_id` 的派生方式不完全
/// 一致（桌面端用短量固定常量如 `"bge-m3"`；daemon 从 GGUF 文件名 `file_stem()` 派生、
/// 带量化后缀如 `"bge-m3-q8_0"`）——前缀匹配统一兼容两种形态，且同一模型不同量化档位
/// 共用一条校准记录（量化不改权重分布，不应改变阈值）。
pub const CALIBRATED_COSINE_THRESHOLDS: &[(&str, f64)] = &[
    // BETA-15B-10（2026-06-26）：bge-m3 sweep + spec 验收评审后 bake，
    // 见 packages/evals/fixtures/semantic-recall/README.md v5 节 + 本文件上方 doc。
    ("bge-m3", 0.70),
];

/// 按 `model_id` 查已校准 cosine 路由阈值（前缀匹配，语义见 [`CALIBRATED_COSINE_THRESHOLDS`]
/// 文档）。未收录 → `None`，调用方决定回落值与是否告警（本函数不做 I/O/日志，保持纯函数）。
#[must_use]
pub fn cosine_threshold_for_model(model_id: &str) -> Option<f64> {
    CALIBRATED_COSINE_THRESHOLDS
        .iter()
        .find(|(key, _)| model_id.starts_with(key))
        .map(|(_, threshold)| *threshold)
}

/// 加权 Reciprocal Rank Fusion：每个 backend 一个**有序**列表（位置=rank）。
/// 语义臂（`BackendKind::SemanticIndex`）列表用 `semantic_weight`，其余权重 1.0。
/// 跨列表按 path 累加 `weight / (k + rank + 1)`，`sources`/`match_types` 取并集，
/// 代表结果取 metadata 最丰富者，score = 累加 RRF，按 score 降序返回。
#[must_use]
pub fn fuse_rrf(lists: Vec<Vec<SearchResult>>, k: f64, semantic_weight: f64) -> Vec<MergedResult> {
    debug_assert!(
        k > 0.0 && semantic_weight > 0.0,
        "k 与 semantic_weight 须为正"
    );
    let mut order: Vec<PathBuf> = Vec::new();
    let mut map: HashMap<PathBuf, (MergedResult, f64)> = HashMap::new();

    for list in lists {
        for (rank, r) in list.into_iter().enumerate() {
            let weight = if r.source == BackendKind::SemanticIndex {
                semantic_weight
            } else {
                1.0
            };
            #[allow(clippy::cast_precision_loss)]
            let contrib = weight / (k + rank as f64 + 1.0);
            // 语义原始 cosine 捕获（融合前的 r.score、只对 SemanticIndex 来源有效）。
            let sem_cos = if r.source == BackendKind::SemanticIndex {
                r.score
            } else {
                None
            };
            if let Some((m, score)) = map.get_mut(&r.path) {
                if !m.sources.contains(&r.source) {
                    m.sources.push(r.source);
                }
                if !m.match_types.contains(&r.match_type) {
                    m.match_types.push(r.match_type);
                }
                m.semantic_cosine = max_opt(m.semantic_cosine, sem_cos);
                // sources / match_types 已在上方并入；此处只换代表结果（取 metadata 更丰富者）。
                if metadata_richness(&r) > metadata_richness(&m.result) {
                    m.result = r;
                }
                *score += contrib;
            } else {
                order.push(r.path.clone());
                map.insert(
                    r.path.clone(),
                    (
                        MergedResult {
                            sources: vec![r.source],
                            match_types: vec![r.match_type],
                            result: r,
                            semantic_cosine: sem_cos,
                        },
                        contrib,
                    ),
                );
            }
        }
    }

    let mut out: Vec<(MergedResult, f64)> =
        order.into_iter().filter_map(|p| map.remove(&p)).collect();
    for (m, score) in &mut out {
        m.result.score = Some(*score);
    }
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out.into_iter().map(|(m, _)| m).collect()
}

/// 路由判定副产物，便于评测/badge/调试消费。
/// 已透传到 `FanoutOutcome.route_verdict`，作 BETA-15B-5 可解释 v1 badge 槽位。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RouteVerdict {
    /// 是否跳过 FTS 臂（true = `vec_top1_cosine` ≥ `cosine_threshold`、hybrid 退化为纯向量）。
    pub skipped_fts: bool,
    /// 检测到的 query 语种（A-5 起仅作可观测元数据；wrapper 内部默认 `Mixed` 占位、
    /// wiring 后置覆写填真值；不驱动路由动作）。
    pub query_lang: crate::lang::Lang,
    /// VEC top-1 cosine 实测值（`vec[0].score.unwrap_or(0.0)`、f64 升精度）。
    pub vec_top1_cosine: f64,
    /// 当时使用的阈值（便于事后审计）。
    pub cosine_threshold: f64,
}

/// 加路由的 RRF 融合 wrapper：FTS/VEC 两臂分别传入,
/// VEC top-1 cosine（`vec[0].score`）≥ `cosine_threshold` 时跳过 FTS 臂
/// （hybrid 退化为纯向量）。
///
/// `fuse_rrf` 本身不动；wrapper 只决定 `fts_list` 是否进入 N 列表融合。
///
/// **任一臂空时不跳过 FTS**：无路由信号，保留兜底；`skipped_fts = false`、`vec_top1_cosine = 0.0`。
///
/// **vec[0].score == None**（不应发生但兜底）：`unwrap_or(0.0)` → 退化为不跳。
///
/// **`query_lang` 默认填 `Lang::Mixed` 占位**：wrapper 不知道 query 真值；
/// 评测层不消费、生产 wiring 在 wrapper 返回后用 struct-update 覆写填真值。
///
/// 与 [`fuse_rrf`] 等价性：当不跳 FTS 时，wrapper 结果完全等价 `fuse_rrf(vec![fts, vec], k, weight)`。
#[must_use]
pub fn fuse_rrf_with_fts_routing(
    fts_list: Vec<SearchResult>,
    vec_list: Vec<SearchResult>,
    rrf_k: f64,
    semantic_weight: f64,
    cosine_threshold: f64,
) -> (Vec<MergedResult>, RouteVerdict) {
    // 任一臂空 → 无路由信号；不跳过 FTS（preserve 一臂兜底）。
    if fts_list.is_empty() || vec_list.is_empty() {
        let merged = fuse_rrf(vec![fts_list, vec_list], rrf_k, semantic_weight);
        return (
            merged,
            RouteVerdict {
                skipped_fts: false,
                query_lang: crate::lang::Lang::Mixed,
                vec_top1_cosine: 0.0,
                cosine_threshold,
            },
        );
    }

    // 两臂都非空：算 vec top-1 cosine、严格 ≥ 阈值时跳 FTS。
    let cosine_top1 = vec_list[0].score.unwrap_or(0.0);
    let skipped_fts = cosine_top1 >= cosine_threshold;

    let merged = if skipped_fts {
        fuse_rrf(vec![vec_list], rrf_k, semantic_weight)
    } else {
        fuse_rrf(vec![fts_list, vec_list], rrf_k, semantic_weight)
    };

    (
        merged,
        RouteVerdict {
            skipped_fts,
            query_lang: crate::lang::Lang::Mixed,
            vec_top1_cosine: cosine_top1,
            cosine_threshold,
        },
    )
}

/// [`fuse_rrf_with_fts_routing`] 的多后端正确版本：每个 backend 保留为一条独立有序列表，
/// 不把 Spotlight/WindowsSearch/NativeIndex 等 FTS 来源先串接成一条伪列表。
///
/// RRF 的 rank 必须在各来源内部从 1 重新计数；先 `extend` 会让后加入来源的第一名被当成
/// 前一来源的第 N+1 名，系统性压低其权重。语义路由取所有语义列表 top-1 的最大 cosine。
#[must_use]
pub fn fuse_rrf_lists_with_fts_routing(
    fts_lists: Vec<Vec<SearchResult>>,
    vec_lists: Vec<Vec<SearchResult>>,
    rrf_k: f64,
    semantic_weight: f64,
    cosine_threshold: f64,
) -> (Vec<MergedResult>, RouteVerdict) {
    let fts_nonempty = fts_lists.iter().any(|list| !list.is_empty());
    let vec_nonempty = vec_lists.iter().any(|list| !list.is_empty());
    if !fts_nonempty || !vec_nonempty {
        let mut lists = fts_lists;
        lists.extend(vec_lists);
        return (
            fuse_rrf(lists, rrf_k, semantic_weight),
            RouteVerdict {
                skipped_fts: false,
                query_lang: crate::lang::Lang::Mixed,
                vec_top1_cosine: 0.0,
                cosine_threshold,
            },
        );
    }

    let cosine_top1 = vec_lists
        .iter()
        .filter_map(|list| list.first().and_then(|r| r.score))
        .fold(0.0_f64, f64::max);
    let skipped_fts = cosine_top1 >= cosine_threshold;
    let mut lists = if skipped_fts { Vec::new() } else { fts_lists };
    lists.extend(vec_lists);
    (
        fuse_rrf(lists, rrf_k, semantic_weight),
        RouteVerdict {
            skipped_fts,
            query_lang: crate::lang::Lang::Mixed,
            vec_top1_cosine: cosine_top1,
            cosine_threshold,
        },
    )
}

fn max_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use scout_search_backend::SearchResultMetadata;
    use std::path::PathBuf;

    fn result(path: &str, source: BackendKind, mt: MatchType) -> SearchResult {
        SearchResult {
            id: path.to_string(),
            path: PathBuf::from(path),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            source,
            match_type: mt,
            score: None,
            metadata: SearchResultMetadata::default(),
        }
    }

    #[test]
    fn cosine_threshold_for_model_exact_match() {
        assert_eq!(cosine_threshold_for_model("bge-m3"), Some(0.70));
    }

    #[test]
    fn cosine_threshold_for_model_matches_quantized_filename_stem() {
        // daemon 的 model_id 是 GGUF 文件名 file_stem()，带量化后缀——前缀匹配须兼容。
        assert_eq!(cosine_threshold_for_model("bge-m3-q8_0"), Some(0.70));
    }

    #[test]
    fn cosine_threshold_for_model_unknown_returns_none() {
        // embeddinggemma-300m 尚未走完评审 bake，故意不在表里（见常量文档）；
        // 未知模型统一交调用方回落 DEFAULT_COSINE_ROUTING_THRESHOLD + 告警。
        assert_eq!(cosine_threshold_for_model("embeddinggemma-300m"), None);
        assert_eq!(cosine_threshold_for_model("qwen3-embedding-0.6b"), None);
    }

    #[test]
    fn distinct_paths_not_merged() {
        let out = merge_results(vec![
            result("/a.txt", BackendKind::Spotlight, MatchType::Filename),
            result("/b.txt", BackendKind::NativeIndex, MatchType::Content),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn same_path_merges_sources_and_match_types() {
        let out = merge_results(vec![
            result("/a.txt", BackendKind::Spotlight, MatchType::Filename),
            result("/a.txt", BackendKind::NativeIndex, MatchType::Content),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].sources,
            vec![BackendKind::Spotlight, BackendKind::NativeIndex]
        );
        assert_eq!(
            out[0].match_types,
            vec![MatchType::Filename, MatchType::Content]
        );
    }

    #[test]
    fn duplicate_source_deduped() {
        let out = merge_results(vec![
            result("/a.txt", BackendKind::Spotlight, MatchType::Filename),
            result("/a.txt", BackendKind::Spotlight, MatchType::Filename),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].sources, vec![BackendKind::Spotlight]);
        assert_eq!(out[0].match_types, vec![MatchType::Filename]);
    }

    #[test]
    fn representative_takes_richest_metadata() {
        let mut rich = result("/a.mp3", BackendKind::NativeIndex, MatchType::Metadata);
        rich.metadata.artist = Some("周华健".to_string());
        rich.metadata.duration_seconds = Some(240.0);
        let poor = result("/a.mp3", BackendKind::Spotlight, MatchType::Filename);
        // 先到者贫瘠，后到者丰富 → 代表应取丰富者。
        let out = merge_results(vec![poor, rich]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].result.metadata.artist.as_deref(), Some("周华健"));
        assert_eq!(out[0].result.source, BackendKind::NativeIndex);
        // 来源仍含两者。
        assert!(out[0].sources.contains(&BackendKind::Spotlight));
        assert!(out[0].sources.contains(&BackendKind::NativeIndex));
    }

    #[test]
    fn score_takes_max() {
        let mut a = result("/a.txt", BackendKind::Spotlight, MatchType::Filename);
        a.score = Some(0.3);
        let mut b = result("/a.txt", BackendKind::NativeIndex, MatchType::Content);
        b.score = Some(0.9);
        let out = merge_results(vec![a, b]);
        assert_eq!(out[0].result.score, Some(0.9));
    }

    #[test]
    fn preserves_first_seen_order() {
        let out = merge_results(vec![
            result("/z.txt", BackendKind::Spotlight, MatchType::Filename),
            result("/a.txt", BackendKind::Spotlight, MatchType::Filename),
            result("/z.txt", BackendKind::NativeIndex, MatchType::Content),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].result.path, PathBuf::from("/z.txt"));
        assert_eq!(out[1].result.path, PathBuf::from("/a.txt"));
    }

    #[test]
    fn empty_input() {
        assert!(merge_results(vec![]).is_empty());
    }

    /// `fuse_rrf` 测试辅助：构造最小 `SearchResult`（与 `result()` 同字段，名称区分语义）。
    fn sr(path: &str, source: BackendKind, mt: MatchType) -> SearchResult {
        SearchResult {
            id: path.to_string(),
            path: std::path::PathBuf::from(path),
            name: path.trim_start_matches('/').to_string(),
            source,
            match_type: mt,
            score: None,
            metadata: SearchResultMetadata::default(),
        }
    }

    #[test]
    fn fuse_rrf_combines_ranks_across_backends() {
        let a = sr("/a", BackendKind::NativeIndex, MatchType::Content);
        let b_fts = sr("/b", BackendKind::NativeIndex, MatchType::Content);
        let b_sem = sr("/b", BackendKind::SemanticIndex, MatchType::Semantic);
        let c = sr("/c", BackendKind::SemanticIndex, MatchType::Semantic);

        let fused = fuse_rrf(
            vec![vec![a, b_fts], vec![b_sem, c]],
            DEFAULT_RRF_K,
            DEFAULT_SEMANTIC_WEIGHT,
        );

        assert_eq!(fused[0].result.path, std::path::PathBuf::from("/b"));
        assert_eq!(fused[0].sources.len(), 2);
        assert!(fused[0].match_types.contains(&MatchType::Semantic));
        assert!(fused[0].match_types.contains(&MatchType::Content));
        assert_eq!(fused.len(), 3);
    }

    #[test]
    fn fuse_rrf_empty_lists_yield_empty() {
        assert!(fuse_rrf(vec![], DEFAULT_RRF_K, DEFAULT_SEMANTIC_WEIGHT).is_empty());
        assert!(fuse_rrf(vec![vec![], vec![]], DEFAULT_RRF_K, DEFAULT_SEMANTIC_WEIGHT).is_empty());
    }

    #[test]
    fn routed_rrf_keeps_each_fts_backend_rank_one() {
        let spotlight_top = sr("/spotlight", BackendKind::Spotlight, MatchType::Filename);
        let native_top = sr("/native", BackendKind::NativeIndex, MatchType::Content);
        let (out, _) = fuse_rrf_lists_with_fts_routing(
            vec![vec![spotlight_top], vec![native_top]],
            vec![],
            DEFAULT_RRF_K,
            DEFAULT_SEMANTIC_WEIGHT,
            DEFAULT_COSINE_ROUTING_THRESHOLD,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].result.score, out[1].result.score,
            "不同 FTS backend 的第一名都必须按 rank=1 计分"
        );
    }

    #[test]
    fn wrapper_high_cosine_skips_fts() {
        // vec[0].score >= threshold → 跳 FTS
        let fts = vec![result(
            "/en.txt",
            BackendKind::NativeIndex,
            MatchType::Content,
        )];
        let mut vec_arm = vec![
            result(
                "/vec_top.md",
                BackendKind::SemanticIndex,
                MatchType::Semantic,
            ),
            result(
                "/vec_second.md",
                BackendKind::SemanticIndex,
                MatchType::Semantic,
            ),
        ];
        vec_arm[0].score = Some(0.85);
        vec_arm[1].score = Some(0.40);
        let (out, v) = fuse_rrf_with_fts_routing(fts, vec_arm, DEFAULT_RRF_K, 10.0, 0.80);
        assert!(v.skipped_fts);
        assert!((v.vec_top1_cosine - 0.85).abs() < f64::EPSILON);
        assert!((v.cosine_threshold - 0.80).abs() < f64::EPSILON);
        assert_eq!(out.len(), 2);
        assert!(out
            .iter()
            .all(|m| m.result.path.to_string_lossy().contains(".md")));
    }

    #[test]
    fn wrapper_low_cosine_does_not_skip() {
        // vec[0].score < threshold → 不跳
        let fts = vec![result(
            "/en.txt",
            BackendKind::NativeIndex,
            MatchType::Content,
        )];
        let mut vec_arm = vec![
            result(
                "/policy.md",
                BackendKind::SemanticIndex,
                MatchType::Semantic,
            ),
            result("/leave.md", BackendKind::SemanticIndex, MatchType::Semantic),
        ];
        vec_arm[0].score = Some(0.40);
        vec_arm[1].score = Some(0.30);
        let (out, v) = fuse_rrf_with_fts_routing(fts, vec_arm, DEFAULT_RRF_K, 10.0, 0.80);
        assert!(!v.skipped_fts);
        assert!((v.vec_top1_cosine - 0.40).abs() < f64::EPSILON);
        assert!(out
            .iter()
            .any(|m| m.result.path.to_string_lossy().contains("en.txt")));
    }

    #[test]
    fn wrapper_empty_arm_does_not_skip() {
        // vec 空 → empty-arm guard → 不跳、fuse_rrf 兜底
        let fts = vec![result(
            "/en.txt",
            BackendKind::NativeIndex,
            MatchType::Content,
        )];
        let vec_arm: Vec<SearchResult> = vec![];
        let (out, v) = fuse_rrf_with_fts_routing(fts, vec_arm, DEFAULT_RRF_K, 10.0, 0.50);
        assert!(!v.skipped_fts);
        assert!((v.vec_top1_cosine - 0.0).abs() < f64::EPSILON);
        assert_eq!(v.query_lang, crate::lang::Lang::Mixed); // empty 时默认 Mixed 占位
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn wrapper_threshold_above_one_never_skips() {
        // threshold = 1.01 > cosine ∈ [0,1] 上限 → 永不跳（spec §5 降级值）
        let fts = vec![result(
            "/en.txt",
            BackendKind::NativeIndex,
            MatchType::Content,
        )];
        let mut vec_arm = vec![result(
            "/vec_top.md",
            BackendKind::SemanticIndex,
            MatchType::Semantic,
        )];
        vec_arm[0].score = Some(0.99); // 极高 cosine
        let (_, v) = fuse_rrf_with_fts_routing(fts, vec_arm, DEFAULT_RRF_K, 10.0, 1.01);
        assert!(!v.skipped_fts);
    }

    #[test]
    fn wrapper_threshold_zero_always_skips() {
        // threshold = 0.0 → 任意 cosine ≥ 0 → 永远跳（≈纯 vec 控制）
        let fts = vec![result(
            "/en.txt",
            BackendKind::NativeIndex,
            MatchType::Content,
        )];
        let mut vec_arm = vec![result(
            "/vec_top.md",
            BackendKind::SemanticIndex,
            MatchType::Semantic,
        )];
        vec_arm[0].score = Some(0.10);
        let (out, v) = fuse_rrf_with_fts_routing(fts, vec_arm, DEFAULT_RRF_K, 10.0, 0.0);
        assert!(v.skipped_fts);
        assert!(out
            .iter()
            .all(|m| !m.result.path.to_string_lossy().contains("en.txt")));
    }

    #[test]
    fn wrapper_no_score_treated_as_zero() {
        // vec[0].score = None → unwrap_or(0.0) → cosine_top1 = 0 → 不跳（除非 threshold ≤ 0）
        let fts = vec![result(
            "/en.txt",
            BackendKind::NativeIndex,
            MatchType::Content,
        )];
        let vec_arm = vec![result(
            "/vec_top.md",
            BackendKind::SemanticIndex,
            MatchType::Semantic,
        )]; // score: None default
        let (out, v) = fuse_rrf_with_fts_routing(fts, vec_arm, DEFAULT_RRF_K, 10.0, 0.50);
        assert!(!v.skipped_fts);
        assert!((v.vec_top1_cosine - 0.0).abs() < f64::EPSILON);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn single_source_passthrough() {
        let out = merge_results(vec![result(
            "/a.txt",
            BackendKind::Spotlight,
            MatchType::Filename,
        )]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].sources, vec![BackendKind::Spotlight]);
    }

    #[test]
    fn default_semantic_weight_is_baked_value() {
        // BETA-15B-3 A-2：bake 后的默认值（task 2 sweep 选定 W=10.0，
        // OVERALL nDCG 最大 + crosslang +0.067 + exact-name 满分守住）。
        // 修改此处须同步 baseline.json + 跑回归门确认不退化。
        assert!(
            (DEFAULT_SEMANTIC_WEIGHT - 10.0).abs() < f64::EPSILON,
            "DEFAULT_SEMANTIC_WEIGHT 已变，须同步 baseline.json"
        );
    }
}
