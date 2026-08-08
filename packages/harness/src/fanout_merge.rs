//! BETA-04 fan-out 多源查询 + 归一化合并。
//!
//! 与 [`crate::fallback_chain`]（选一个后端，失败才回退）不同：本模块**同时查询**
//! [`IntentRouter::route_search_fanout`](crate::IntentRouter::route_search_fanout) 给出的
//! 后端集合（系统搜索 + 本地索引），把各源结果交 [`scout_result_normalizer::merge_results`]
//! 按 canonical path 去重合并后逐条发出。让本地索引补上系统搜索按 artist/正文搜不到的命中。
//!
//! v1 **顺序收集**各后端结果后合并（后端数通常 2-3 个，且部分失败不致命）；真并发优化按需。

use std::sync::Arc;

use futures_util::StreamExt;
use scout_result_normalizer::{
    fuse_rrf_lists_with_fts_routing, lang::detect_lang, merge_results, MergedResult, RouteVerdict,
    DEFAULT_RRF_K,
};
use scout_search_backend::{
    extensions_for_file_type, media_derived_file_types, relative_time_bounds, BackendKind,
    CancellationToken, ExpandedSearchIntent, FileType, SearchIntent, SearchResult, SizeExpression,
    SizeUnit, TimeExpression,
};

use crate::SearchableTool;

/// fan-out 合并执行结果。
#[derive(Debug, Clone, Default)]
pub struct FanoutOutcome {
    /// 合并去重后的结果总数。
    pub total: usize,
    /// 成功取到结果流的后端（供 UI 显示「via …」）。
    pub sources_queried: Vec<BackendKind>,
    /// 各后端错误 `(tool_id, message)`；部分失败不致命，仅合并成功者。
    pub errors: Vec<(String, String)>,
    /// VEC top-1 cosine 阈值路由判定（仅 `run_fanout_merge_rrf` 路径填充；其他路径为 `None`）。
    /// BETA-15B-3 A-5 信号 = vec 臂 top-1 cosine 阈值；`query_lang` 由 wiring 后置覆写、作 BETA-15B-5 badge 槽位。
    pub route_verdict: Option<RouteVerdict>,
}

/// 纯融合：**非语义** fan-out 路径。输入各后端**按查询顺序**收集拼接的结果，
/// 交 [`merge_results`] 按 canonical path 去重合并。
///
/// 从 [`run_fanout_merge`] 抽出，让「串行查询（daemon/MCP 路径）」与「并发收集
/// （desktop 路径）」共享**逐字节等价**的合并语义——两侧只需保证喂入的 `collected`
/// 保持相同的后端顺序即可（合并 tie-break 依赖到达顺序）。
#[must_use]
pub fn fuse_fanout_merge(collected: Vec<SearchResult>) -> Vec<MergedResult> {
    merge_results(collected)
}

/// 对后端候选执行统一结构化约束。系统后端通常已在查询语法中执行，但 Local FTS/Semantic
/// 只消费内容词；融合前再过滤一次，防它们把错误扩展名、目录、时间或大小的候选带回结果。
///
/// 缺失 metadata 时只对当前候选做一次本地 `metadata()` 补取；约束字段仍无法取得则严格拒绝，
/// 不静默放宽用户条件。
#[must_use]
pub fn filter_results_for_intent(
    results: Vec<SearchResult>,
    intent: &SearchIntent,
) -> Vec<SearchResult> {
    results
        .into_iter()
        .filter(|result| result_matches_intent(result, intent))
        .collect()
}

/// 单条结果是否满足 intent 的结构化约束（文件类型/扩展名/路径/时间/大小等）。
/// `pub(crate)` 供 [`crate::fallback_chain::run_fallback_chain`] 复用——单后端链路
/// 没有 fan-out 的多源合并步骤，但同样需要这层约束校验（见该处调用点注释）。
#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
pub(crate) fn result_matches_intent(result: &SearchResult, intent: &SearchIntent) -> bool {
    let derived_media_types;
    let constraints = match intent {
        SearchIntent::FileSearch(search) => (
            search.extensions.as_deref(),
            search.file_type.as_deref(),
            search.location.as_ref(),
            search.modified_time.as_ref(),
            search.created_time.as_ref(),
            search.accessed_time.as_ref(),
            search.size.as_ref(),
            search.exclude_extensions.as_deref(),
            search.exclude_file_type.as_deref(),
            None,
        ),
        SearchIntent::MediaSearch(search) => {
            derived_media_types = media_derived_file_types(search);
            (
                search.extensions.as_deref(),
                derived_media_types.as_deref(),
                search.location.as_ref(),
                search.modified_time.as_ref(),
                search.created_time.as_ref(),
                search.accessed_time.as_ref(),
                search.size.as_ref(),
                search.exclude_extensions.as_deref(),
                search.exclude_file_type.as_deref(),
                search.duration.as_ref(),
            )
        }
        SearchIntent::Refine(_) | SearchIntent::FileAction(_) | SearchIntent::Clarify(_) => {
            return true;
        }
    };
    let (
        extensions,
        file_types,
        location,
        modified_time,
        created_time,
        accessed_time,
        size,
        exclude_extensions,
        exclude_file_types,
        duration,
    ) = constraints;

    let ext = result
        .path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if extensions.is_some_and(|allowed| {
        !allowed
            .iter()
            .any(|candidate| candidate.trim_start_matches('.').eq_ignore_ascii_case(&ext))
    }) {
        return false;
    }
    if file_types.is_some_and(|types| !extension_matches_any_type(&ext, types)) {
        return false;
    }
    if exclude_extensions.is_some_and(|blocked| {
        blocked
            .iter()
            .any(|candidate| candidate.trim_start_matches('.').eq_ignore_ascii_case(&ext))
    }) || exclude_file_types.is_some_and(|types| extension_matches_any_type(&ext, types))
    {
        return false;
    }
    if let Some(location) = location {
        if location.include.as_ref().is_some_and(|roots| {
            !roots
                .iter()
                .any(|root| result.path.starts_with(std::path::Path::new(root)))
        }) {
            return false;
        }
        if location.exclude.as_ref().is_some_and(|roots| {
            roots
                .iter()
                .any(|root| result.path.starts_with(std::path::Path::new(root)))
        }) {
            return false;
        }
    }

    let fs_meta = || std::fs::metadata(&result.path).ok();
    let modified = result
        .metadata
        .modified_time
        .or_else(|| fs_meta()?.modified().ok().map(chrono::DateTime::from));
    let created = result
        .metadata
        .created_time
        .or_else(|| fs_meta()?.created().ok().map(chrono::DateTime::from));
    let accessed = result
        .metadata
        .accessed_time
        .or_else(|| fs_meta()?.accessed().ok().map(chrono::DateTime::from));
    if modified_time.is_some_and(|expr| !modified.is_some_and(|value| time_matches(value, expr)))
        || created_time.is_some_and(|expr| !created.is_some_and(|value| time_matches(value, expr)))
        || accessed_time
            .is_some_and(|expr| !accessed.is_some_and(|value| time_matches(value, expr)))
    {
        return false;
    }
    let size_bytes = result
        .metadata
        .size_bytes
        .or_else(|| fs_meta().map(|meta| meta.len()));
    if size.is_some_and(|expr| !size_bytes.is_some_and(|value| scalar_matches(value as f64, expr)))
    {
        return false;
    }
    if duration.is_some_and(|expr| {
        !result
            .metadata
            .duration_seconds
            .is_some_and(|value| scalar_matches(value, expr))
    }) {
        return false;
    }
    true
}

fn extension_matches_any_type(ext: &str, types: &[FileType]) -> bool {
    types.iter().any(|file_type| {
        extensions_for_file_type(*file_type)
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(ext))
    })
}

fn time_matches(value: chrono::DateTime<chrono::Utc>, expr: &TimeExpression) -> bool {
    use chrono::{Duration, Local};
    let date = value.with_timezone(&Local).date_naive();
    match expr {
        TimeExpression::Relative { value } => {
            let today = Local::now().date_naive();
            let (start, end) = relative_time_bounds(*value);
            date >= today + Duration::days(i64::from(start))
                && date < today + Duration::days(i64::from(end))
        }
        TimeExpression::Absolute { from, to } => date >= *from && date <= *to,
        TimeExpression::Before { value } => date < *value,
        TimeExpression::After { value } => date > *value,
    }
}

fn scalar_matches(value: f64, expr: &SizeExpression) -> bool {
    let factor = match expr {
        SizeExpression::GreaterThan { unit, .. }
        | SizeExpression::LessThan { unit, .. }
        | SizeExpression::Between { unit, .. } => match unit {
            SizeUnit::B | SizeUnit::Sec => 1.0,
            SizeUnit::Kb => 1_000.0,
            SizeUnit::Mb | SizeUnit::Min => {
                if matches!(*unit, SizeUnit::Min) {
                    60.0
                } else {
                    1_000_000.0
                }
            }
            SizeUnit::Gb => 1_000_000_000.0,
            SizeUnit::Hour => 3_600.0,
        },
    };
    match expr {
        SizeExpression::GreaterThan {
            value: threshold, ..
        } => value > threshold * factor,
        SizeExpression::LessThan {
            value: threshold, ..
        } => value < threshold * factor,
        SizeExpression::Between { min, max, .. } => value >= min * factor && value <= max * factor,
    }
}

/// 纯融合：**语义** fan-out 路径（加权 RRF + FTS 路由）。输入已按 `backend_kind`
/// 拆分好的 `fts_list`（非 `SemanticIndex`）与 `vec_list`（`SemanticIndex`），
/// 各自内部保持后端到达顺序（=rank）。返回融合结果 + [`RouteVerdict`]
/// （`query_lang` 由 `detect_lang(query)` 后置覆写填真值，其余字段来自 wrapper）。
///
/// 从 [`run_fanout_merge_rrf`] 抽出，让串行查询与并发收集共享逐字节等价的融合语义。
/// `cosine_threshold` 不再是内部写死的 `DEFAULT_COSINE_ROUTING_THRESHOLD`——调用方须传入
/// 按当前生效 embedding 模型解析出的校准值（见
/// `scout_result_normalizer::cosine_threshold_for_model`），阈值与模型版本结构化绑定，
/// 换模型忘了补校准记录时由调用方告警，而不是静默沿用上一代模型的旧阈值。
/// `DEFAULT_RRF_K` 仍是内部常量（BETA-26 实测对 k 不敏感，未发现需要 model-aware 化）。
#[must_use]
pub fn fuse_fanout_rrf(
    fts_lists: Vec<Vec<SearchResult>>,
    vec_lists: Vec<Vec<SearchResult>>,
    semantic_weight: f64,
    cosine_threshold: f64,
    query: &str,
) -> (Vec<MergedResult>, RouteVerdict) {
    let (merged, verdict) = fuse_rrf_lists_with_fts_routing(
        fts_lists,
        vec_lists,
        DEFAULT_RRF_K,
        semantic_weight,
        cosine_threshold,
    );
    let verdict = RouteVerdict {
        query_lang: detect_lang(query),
        ..verdict
    };
    (merged, verdict)
}

/// 同时查询 `backends` 中所有后端、归一化合并后逐条 `on_result`。
///
/// 顺序对每个后端 `search_expanded` → 收集流 → 累积；任一后端 pre-stream Err 或流中途 Err
/// 记入 `errors`、不中断其它后端。全部收齐后 `merge_results` 去重合并，对每条 [`MergedResult`]
/// 调一次 `on_result`。取消时尽快停止。
///
/// 回调用泛型（`R: FnMut(MergedResult) + Send`）使返回 future 满足 `Send`（兼容 Tauri executor）。
#[must_use = "FanoutOutcome 须被检查；total==0 时需向用户报告空态/错误"]
pub async fn run_fanout_merge<R>(
    backends: &[Arc<dyn SearchableTool>],
    expanded: &ExpandedSearchIntent,
    cancel: CancellationToken,
    on_result: &mut R,
) -> FanoutOutcome
where
    R: FnMut(MergedResult) + Send,
{
    let mut collected: Vec<SearchResult> = Vec::new();
    let mut sources_queried: Vec<BackendKind> = Vec::new();
    let mut errors: Vec<(String, String)> = Vec::new();

    for tool in backends {
        if cancel.is_cancelled() {
            break;
        }
        let tool_id = tool.id().to_owned();
        match tool.search_expanded(expanded, cancel.clone()).await {
            Err(err) => errors.push((tool_id, err.to_string())),
            Ok(mut stream) => {
                if let Some(kind) = tool.capability().backend_kind {
                    if !sources_queried.contains(&kind) {
                        sources_queried.push(kind);
                    }
                }
                while let Some(item) = stream.next().await {
                    if cancel.is_cancelled() {
                        break;
                    }
                    match item {
                        Ok(result) => collected.push(result),
                        Err(err) => {
                            errors.push((tool_id.clone(), err.to_string()));
                            break;
                        }
                    }
                }
            }
        }
    }

    let collected = filter_results_for_intent(collected, &expanded.base);
    let merged = fuse_fanout_merge(collected);
    let total = merged.len();
    for m in merged {
        on_result(m);
    }

    FanoutOutcome {
        total,
        sources_queried,
        errors,
        route_verdict: None,
    }
}

/// 与 [`run_fanout_merge`] 同样多源查询，但**保留各后端排名**用加权 RRF 融合
/// （语义召回臂 + FTS 臂的 hybrid 路径用此变体）。
///
/// 区别于 [`run_fanout_merge`] 的扁平 `merge_results`：此处把每个后端结果保留为一条
/// **有序列表**（到达顺序=rank），交 [`fuse_rrf_with_fts_routing`] 跨列表按 path 累加加权倒数排名。
/// 语义臂（`BackendKind::SemanticIndex`）列表用调用方指定的 `semantic_weight`
/// （生产用 `AppSettings.semantic_weight` live-read，详 BETA-15B-3 A-2 spec），其余权重 1.0。
/// BETA-15B-3 A-5 起按 `backend_kind` 把 `SemanticIndex` 入 vec 臂、其它入 fts 臂，
/// `vec[0].score`（cosine）≥ `cosine_threshold` 时 wrapper 跳过 FTS 臂——`cosine_threshold`
/// 由调用方传入（生产按当前生效 embedding 模型经
/// `scout_result_normalizer::cosine_threshold_for_model` 解析，不再是内部写死的
/// `DEFAULT_COSINE_ROUTING_THRESHOLD`，阈值与模型版本结构化绑定，详见该函数文档）。
/// 判定 `RouteVerdict` 透传到 `FanoutOutcome.route_verdict` 供后续 UI 消费；
/// `verdict.query_lang` 由 wiring 用 `detect_lang(query)` 后置覆写填真值
/// （wrapper 内部默认 Mixed 占位）。
/// 错误/取消语义与 [`run_fanout_merge`] 一致：部分失败记 `errors`、不中断其它后端。
#[must_use = "FanoutOutcome 须被检查；total==0 时需向用户报告空态/错误"]
pub async fn run_fanout_merge_rrf<R>(
    backends: &[Arc<dyn SearchableTool>],
    expanded: &ExpandedSearchIntent,
    cancel: CancellationToken,
    on_result: &mut R,
    semantic_weight: f64,
    cosine_threshold: f64,
    query: &str,
) -> FanoutOutcome
where
    R: FnMut(MergedResult) + Send,
{
    // BETA-15B-3 A-5：按 BackendKind 分离 FTS 臂（任何非 SemanticIndex）与 VEC 臂（SemanticIndex），
    // 喂 fuse_rrf_with_fts_routing wrapper；vec[0].score（cosine）≥ cosine_threshold 时跳过 FTS 臂。
    // verdict.query_lang 在 wrapper 返回后由 detect_lang(query) 后置覆写填真值。
    let mut fts_lists: Vec<Vec<SearchResult>> = Vec::new();
    let mut vec_lists: Vec<Vec<SearchResult>> = Vec::new();
    let mut sources_queried: Vec<BackendKind> = Vec::new();
    let mut errors: Vec<(String, String)> = Vec::new();

    for tool in backends {
        if cancel.is_cancelled() {
            break;
        }
        let tool_id = tool.id().to_owned();
        let backend_kind = tool.capability().backend_kind;
        match tool.search_expanded(expanded, cancel.clone()).await {
            Err(err) => errors.push((tool_id, err.to_string())),
            Ok(mut stream) => {
                if let Some(kind) = backend_kind {
                    if !sources_queried.contains(&kind) {
                        sources_queried.push(kind);
                    }
                }
                let mut list: Vec<SearchResult> = Vec::new();
                while let Some(item) = stream.next().await {
                    if cancel.is_cancelled() {
                        break;
                    }
                    match item {
                        Ok(result) => list.push(result),
                        Err(err) => {
                            errors.push((tool_id.clone(), err.to_string()));
                            break;
                        }
                    }
                }
                let list = filter_results_for_intent(list, &expanded.base);
                // 按 backend_kind 归口：SemanticIndex → vec_list；其它 → fts_list。
                // 每个后端保持独立有序列表，避免同类后端的 rank 位置被扁平化。
                if matches!(backend_kind, Some(BackendKind::SemanticIndex)) {
                    vec_lists.push(list);
                } else {
                    fts_lists.push(list);
                }
            }
        }
    }

    // BETA-15B-3 A-5：wrapper 5 参（cosine_threshold 替换 A-4 6 参的 lang 信号 + max）。
    // 任一臂空 → wrapper 内 early-return guard 兜底；wrapper 内部 verdict.query_lang
    // 默认 Mixed 占位、fuse_fanout_rrf 用 detect_lang(query) 后置覆写填真值（BETA-15B-5 badge 元数据）。
    let (merged, verdict) = fuse_fanout_rrf(
        fts_lists,
        vec_lists,
        semantic_weight,
        cosine_threshold,
        query,
    );
    let total = merged.len();
    for m in merged {
        on_result(m);
    }

    FanoutOutcome {
        total,
        sources_queried,
        errors,
        route_verdict: Some(verdict),
    }
}

/// 内容 fan-out 零结果时，回退到纯文件名后端（如 Everything）按文件名补一轮召回。
///
/// 先对 `content_backends` 跑 [`run_fanout_merge`]；若合并后 `total > 0`（或已取消，
/// 或 `filename_fallback` 为空）直接返回——**文件名兜底不触发**，常见路径零行为变化。
/// 仅当内容轮**干净零结果**时，`on_fallback()` 通知调用方（供 UI 提示）后，对
/// `filename_fallback` 再跑一轮 merge 并返回其 outcome（errors 合并两轮，便于诊断）。
///
/// 闭合 [`IntentRouter::route_search_fanout`](crate::IntentRouter::route_search_fanout)
/// 内容分支不含 Everything 的盲区：文件在系统索引/本地索引未覆盖的位置、但文件名含关键词时，
/// 仍能按文件名命中。两轮都空 → `total == 0`。
#[must_use = "FanoutOutcome 须被检查；total==0 时需向用户报告空态/错误"]
pub async fn run_fanout_merge_with_fallback<R, F>(
    content_backends: &[Arc<dyn SearchableTool>],
    filename_fallback: &[Arc<dyn SearchableTool>],
    expanded: &ExpandedSearchIntent,
    cancel: CancellationToken,
    on_result: &mut R,
    on_fallback: &mut F,
) -> FanoutOutcome
where
    R: FnMut(MergedResult) + Send,
    F: FnMut() + Send,
{
    let content = run_fanout_merge(content_backends, expanded, cancel.clone(), on_result).await;
    if content.total > 0 || cancel.is_cancelled() || filename_fallback.is_empty() {
        return content;
    }

    // 内容轮干净零结果 → 文件名兜底。
    on_fallback();
    let fb = run_fanout_merge(filename_fallback, expanded, cancel, on_result).await;

    // 合并两轮的 sources/errors；total 以兜底轮为准（内容轮本就 0）。
    let mut sources_queried = content.sources_queried;
    for kind in fb.sources_queried {
        if !sources_queried.contains(&kind) {
            sources_queried.push(kind);
        }
    }
    let mut errors = content.errors;
    errors.extend(fb.errors);
    FanoutOutcome {
        total: fb.total,
        sources_queried,
        errors,
        route_verdict: None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::missing_docs_in_private_items
    )]

    use super::*;
    use crate::{SearchTool, SupportedIntent};
    use futures_util::FutureExt;
    use scout_result_normalizer::DEFAULT_SEMANTIC_WEIGHT;
    use scout_search_backend::{
        backend_stream_from_results, BackendSearchFuture, FileSearch, ImplementationStatus,
        MatchType, SchemaVersion, SearchBackend, SearchError, SearchIntent, SearchResult,
        SearchResultMetadata,
    };
    use std::path::PathBuf;

    use futures_executor::block_on;

    fn intent() -> SearchIntent {
        SearchIntent::FileSearch(FileSearch {
            schema_version: SchemaVersion::V1,
            language: None,
            keywords: Some(vec!["budget".to_owned()]),
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
        })
    }

    fn result_at(path: &str, source: BackendKind, mt: MatchType) -> SearchResult {
        SearchResult {
            id: path.to_owned(),
            path: PathBuf::from(path),
            name: path.to_owned(),
            source,
            match_type: mt,
            score: None,
            metadata: SearchResultMetadata::default(),
        }
    }

    #[test]
    fn structured_constraints_filter_backend_candidates_before_merge() {
        let mut requested = intent();
        let SearchIntent::FileSearch(search) = &mut requested else {
            unreachable!();
        };
        search.extensions = Some(vec!["pdf".to_owned()]);
        search.location = Some(scout_search_backend::Location {
            hint: None,
            include: Some(vec!["/allowed".to_owned()]),
            exclude: Some(vec!["/allowed/private".to_owned()]),
        });
        let filtered = filter_results_for_intent(
            vec![
                result_at(
                    "/allowed/report.pdf",
                    BackendKind::NativeIndex,
                    MatchType::Content,
                ),
                result_at(
                    "/allowed/report.txt",
                    BackendKind::SemanticIndex,
                    MatchType::Semantic,
                ),
                result_at(
                    "/allowed/private/secret.pdf",
                    BackendKind::NativeIndex,
                    MatchType::Content,
                ),
                result_at(
                    "/outside/report.pdf",
                    BackendKind::Spotlight,
                    MatchType::Filename,
                ),
            ],
            &requested,
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].path, PathBuf::from("/allowed/report.pdf"));
    }

    enum Script {
        Results(Vec<SearchResult>),
        Error(SearchError),
    }

    struct MockBackend {
        kind: BackendKind,
        script: std::sync::Mutex<Option<Script>>,
    }

    impl std::fmt::Debug for MockBackend {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MockBackend")
                .field("kind", &self.kind)
                .finish_non_exhaustive()
        }
    }

    impl SearchBackend for MockBackend {
        fn kind(&self) -> BackendKind {
            self.kind
        }
        fn implementation_status(&self) -> ImplementationStatus {
            ImplementationStatus::Real
        }
        fn is_available(&self) -> bool {
            true
        }
        fn search<'a>(
            &'a self,
            _intent: &'a SearchIntent,
            cancel: CancellationToken,
        ) -> BackendSearchFuture<'a> {
            let script = self.script.lock().unwrap().take();
            match script {
                Some(Script::Results(rs)) => {
                    async move { Ok(backend_stream_from_results(rs, cancel)) }.boxed()
                }
                Some(Script::Error(err)) => async move { Err(err) }.boxed(),
                None => async move { Ok(backend_stream_from_results(Vec::new(), cancel)) }.boxed(),
            }
        }
    }

    fn tool(id: &'static str, kind: BackendKind, script: Script) -> Arc<dyn SearchableTool> {
        Arc::new(SearchTool::new(
            id,
            id,
            MockBackend {
                kind,
                script: std::sync::Mutex::new(Some(script)),
            },
            vec![SupportedIntent::FileSearch],
            "mock",
        ))
    }

    #[test]
    fn merges_overlapping_paths_across_two_backends() {
        // A: /a, /b（filename）；B: /b（metadata，更富）, /c → 合并 3 条，/b 双源。
        let mut rich_b = result_at("/b", BackendKind::NativeIndex, MatchType::Metadata);
        rich_b.metadata.artist = Some("周华健".to_owned());
        let backends = vec![
            tool(
                "search.spotlight",
                BackendKind::Spotlight,
                Script::Results(vec![
                    result_at("/a", BackendKind::Spotlight, MatchType::Filename),
                    result_at("/b", BackendKind::Spotlight, MatchType::Filename),
                ]),
            ),
            tool(
                "search.local",
                BackendKind::NativeIndex,
                Script::Results(vec![
                    rich_b,
                    result_at("/c", BackendKind::NativeIndex, MatchType::Content),
                ]),
            ),
        ];
        let mut got: Vec<MergedResult> = Vec::new();
        let outcome = block_on(run_fanout_merge(
            &backends,
            &ExpandedSearchIntent::identity(intent()),
            CancellationToken::new(),
            &mut |m| got.push(m),
        ));
        assert_eq!(outcome.total, 3);
        assert_eq!(got.len(), 3);
        assert_eq!(
            outcome.sources_queried,
            vec![BackendKind::Spotlight, BackendKind::NativeIndex]
        );
        // /b 合并了两源，代表取富 metadata。
        let b = got
            .iter()
            .find(|m| m.result.path == std::path::Path::new("/b"))
            .unwrap();
        assert_eq!(b.sources.len(), 2);
        assert_eq!(b.result.metadata.artist.as_deref(), Some("周华健"));
    }

    #[test]
    fn rrf_fuses_ranks_semantic_weighted() {
        // FTS 臂（NativeIndex）有序 [A, B]；语义臂（SemanticIndex）有序 [B, C]。
        // B 同时命中两臂、且语义臂加权，应排第一；合并去重后共 3 条。
        let backends = vec![
            tool(
                "search.local",
                BackendKind::NativeIndex,
                Script::Results(vec![
                    result_at("/a", BackendKind::NativeIndex, MatchType::Content),
                    result_at("/b", BackendKind::NativeIndex, MatchType::Content),
                ]),
            ),
            tool(
                "search.semantic",
                BackendKind::SemanticIndex,
                Script::Results(vec![
                    result_at("/b", BackendKind::SemanticIndex, MatchType::Content),
                    result_at("/c", BackendKind::SemanticIndex, MatchType::Content),
                ]),
            ),
        ];
        let mut got: Vec<MergedResult> = Vec::new();
        let outcome = block_on(run_fanout_merge_rrf(
            &backends,
            &ExpandedSearchIntent::identity(intent()),
            CancellationToken::new(),
            &mut |m| got.push(m),
            DEFAULT_SEMANTIC_WEIGHT,
            scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD,
            "budget",
        ));
        assert_eq!(outcome.total, 3);
        assert_eq!(got.len(), 3);
        assert_eq!(
            outcome.sources_queried,
            vec![BackendKind::NativeIndex, BackendKind::SemanticIndex]
        );
        // RRF 降序：/b 双臂命中 + 语义加权 → 排第一。
        assert_eq!(got[0].result.path, std::path::Path::new("/b"));
        // /b 合并了两源。
        assert_eq!(got[0].sources.len(), 2);
    }

    #[test]
    fn rrf_respects_semantic_weight_parameter() {
        // 同一组后端 + 同一 query，weight=1.0 vs weight=10.0 应产出**可观测不同**的 score。
        // FTS 臂 [A, B]；语义臂 [B, C]。B 双臂命中——改变语义臂权重直接改变 B 的 RRF 累加分。
        let make_backends = || {
            vec![
                tool(
                    "search.local",
                    BackendKind::NativeIndex,
                    Script::Results(vec![
                        result_at("/a", BackendKind::NativeIndex, MatchType::Content),
                        result_at("/b", BackendKind::NativeIndex, MatchType::Content),
                    ]),
                ),
                tool(
                    "search.semantic",
                    BackendKind::SemanticIndex,
                    Script::Results(vec![
                        result_at("/b", BackendKind::SemanticIndex, MatchType::Content),
                        result_at("/c", BackendKind::SemanticIndex, MatchType::Content),
                    ]),
                ),
            ]
        };

        let mut out_low: Vec<MergedResult> = Vec::new();
        let _ = block_on(run_fanout_merge_rrf(
            &make_backends(),
            &ExpandedSearchIntent::identity(intent()),
            CancellationToken::new(),
            &mut |m| out_low.push(m),
            1.0,
            scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD,
            "budget",
        ));

        let mut out_high: Vec<MergedResult> = Vec::new();
        let _ = block_on(run_fanout_merge_rrf(
            &make_backends(),
            &ExpandedSearchIntent::identity(intent()),
            CancellationToken::new(),
            &mut |m| out_high.push(m),
            10.0,
            scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD,
            "budget",
        ));

        // 方向性断言：B 双臂命中 → 高语义权重让 B 的 RRF 累加分严格 > 低权重时（验证 weight
        // 参数真透传 fuse_rrf 且方向正确，比裸 assert_ne 更不易被未来重构误打误撞通过）。
        let score_b_low = out_low
            .iter()
            .find(|m| m.result.path == std::path::Path::new("/b"))
            .and_then(|m| m.result.score)
            .expect("/b 应在 low-weight 结果集");
        let score_b_high = out_high
            .iter()
            .find(|m| m.result.path == std::path::Path::new("/b"))
            .and_then(|m| m.result.score)
            .expect("/b 应在 high-weight 结果集");
        assert!(
            score_b_high > score_b_low,
            "高语义权重应让 B（双臂命中）的 RRF 累加分严格更高：low={score_b_low} high={score_b_high}"
        );
    }

    // BETA-15B-3 A-5：vec[0].score（cosine）阈值路由三态分支覆盖。
    // wrapper 真触发跳 FTS 需 DEFAULT_COSINE_ROUTING_THRESHOLD 实际 bake（非 1.01 占位）；
    // 本 cycle 仍 placeholder（A-5 T7 sweep / T8 bake 才落地）→ 用 `||` 守护退化路径，确保占位时也 byte-equal。
    #[test]
    fn fanout_rrf_high_cosine_skips_fts() {
        // vec backend 返带 score=0.9 的 SearchResult → cosine_top1=0.9
        // bake 后 T* < 0.9 时跳 FTS；T*>=0.9（含 1.01 降级）时不跳
        let backends = vec![
            tool(
                "search.local",
                BackendKind::NativeIndex,
                Script::Results(vec![result_at(
                    "/annual_leave.md",
                    BackendKind::NativeIndex,
                    MatchType::Filename,
                )]),
            ),
            tool(
                "search.semantic",
                BackendKind::SemanticIndex,
                Script::Results(vec![{
                    let mut r = result_at(
                        "/year_off_rules.md",
                        BackendKind::SemanticIndex,
                        MatchType::Content,
                    );
                    r.score = Some(0.9);
                    r
                }]),
            ),
        ];
        let mut got: Vec<MergedResult> = Vec::new();
        let outcome = block_on(run_fanout_merge_rrf(
            &backends,
            &ExpandedSearchIntent::identity(intent()),
            CancellationToken::new(),
            &mut |m| got.push(m),
            10.0,
            scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD,
            "annual leave policy", // En query → query_lang 覆写为 En
        ));
        let v = outcome.route_verdict.expect("RRF 路径应填 route_verdict");
        assert_eq!(v.query_lang, scout_result_normalizer::lang::Lang::En);
        assert!((v.vec_top1_cosine - 0.9).abs() < f64::EPSILON);
        // T* < 0.9 时跳；T* ≥ 0.9（如 1.01 降级）时不跳 —— 用 `||` 让任一 T* bake 都通过
        assert!(
            v.skipped_fts || scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD > 0.9,
            "T*={} 下 cosine=0.9 应跳 FTS（或 spec §5 降级 T*>0.9 时不跳）",
            scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD
        );
    }

    #[test]
    fn fanout_rrf_low_cosine_does_not_skip() {
        // vec backend 返带 score=0.10 → cosine_top1=0.10 < 任一合理 T* → 不跳
        let backends = vec![
            tool(
                "search.local",
                BackendKind::NativeIndex,
                Script::Results(vec![result_at(
                    "/年假规定.md",
                    BackendKind::NativeIndex,
                    MatchType::Content,
                )]),
            ),
            tool(
                "search.semantic",
                BackendKind::SemanticIndex,
                Script::Results(vec![{
                    let mut r = result_at(
                        "/年假规定.md",
                        BackendKind::SemanticIndex,
                        MatchType::Content,
                    );
                    r.score = Some(0.10);
                    r
                }]),
            ),
        ];
        let mut got: Vec<MergedResult> = Vec::new();
        let outcome = block_on(run_fanout_merge_rrf(
            &backends,
            &ExpandedSearchIntent::identity(intent()),
            CancellationToken::new(),
            &mut |m| got.push(m),
            10.0,
            scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD,
            "年假规定", // ZH query → query_lang 覆写为 Zh
        ));
        let v = outcome.route_verdict.expect("RRF 路径应填 route_verdict");
        assert_eq!(v.query_lang, scout_result_normalizer::lang::Lang::Zh);
        assert!((v.vec_top1_cosine - 0.10).abs() < f64::EPSILON);
        assert!(
            !v.skipped_fts || scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD <= 0.10,
            "cosine=0.10 应不跳（除非 T* ≤ 0.10）"
        );
    }

    #[test]
    fn fanout_rrf_verdict_query_lang_metadata_mixed() {
        // Mixed query 元数据覆写：query_lang 字段填 Mixed；不影响路由判定（cosine 信号驱动）
        let backends = vec![
            tool(
                "search.local",
                BackendKind::NativeIndex,
                Script::Results(vec![result_at(
                    "/qwen_tuning.md",
                    BackendKind::NativeIndex,
                    MatchType::Filename,
                )]),
            ),
            tool(
                "search.semantic",
                BackendKind::SemanticIndex,
                Script::Results(vec![{
                    let mut r = result_at(
                        "/年假规定.md",
                        BackendKind::SemanticIndex,
                        MatchType::Content,
                    );
                    r.score = Some(0.40);
                    r
                }]),
            ),
        ];
        let mut got: Vec<MergedResult> = Vec::new();
        let outcome = block_on(run_fanout_merge_rrf(
            &backends,
            &ExpandedSearchIntent::identity(intent()),
            CancellationToken::new(),
            &mut |m| got.push(m),
            10.0,
            scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD,
            "qwen 调优", // Mixed query：3 ASCII + 2 CJK → ratio 0.4
        ));
        let v = outcome.route_verdict.expect("RRF 路径应填 route_verdict");
        assert_eq!(v.query_lang, scout_result_normalizer::lang::Lang::Mixed);
        assert!((v.vec_top1_cosine - 0.40).abs() < f64::EPSILON);
    }

    // ===== 纯 fuse 函数：直接喂预构建列表，验与 run_ 系列逐字节等价 =====

    #[test]
    fn fuse_fanout_merge_dedups_by_path() {
        // /b 双源（filename + 更富 metadata）→ 合并去重后 3 条、/b 取富 metadata。
        let mut rich_b = result_at("/b", BackendKind::NativeIndex, MatchType::Metadata);
        rich_b.metadata.artist = Some("周华健".to_owned());
        let collected = vec![
            result_at("/a", BackendKind::Spotlight, MatchType::Filename),
            result_at("/b", BackendKind::Spotlight, MatchType::Filename),
            rich_b,
            result_at("/c", BackendKind::NativeIndex, MatchType::Content),
        ];
        let merged = fuse_fanout_merge(collected);
        assert_eq!(merged.len(), 3);
        let b = merged
            .iter()
            .find(|m| m.result.path == std::path::Path::new("/b"))
            .unwrap();
        assert_eq!(b.sources.len(), 2);
        assert_eq!(b.result.metadata.artist.as_deref(), Some("周华健"));
    }

    #[test]
    fn fuse_fanout_rrf_fuses_and_overrides_query_lang() {
        // FTS 臂 [A, B]、VEC 臂 [B, C]（cosine 0.1 不跳 FTS）→ B 双臂命中 + 语义加权排第一；
        // 共 3 条；query_lang 由 detect_lang 覆写为 Zh。
        let fts = vec![
            result_at("/a", BackendKind::NativeIndex, MatchType::Content),
            result_at("/b", BackendKind::NativeIndex, MatchType::Content),
        ];
        let vec_list = vec![
            {
                let mut r = result_at("/b", BackendKind::SemanticIndex, MatchType::Content);
                r.score = Some(0.10);
                r
            },
            result_at("/c", BackendKind::SemanticIndex, MatchType::Content),
        ];
        let (merged, verdict) = fuse_fanout_rrf(
            vec![fts],
            vec![vec_list],
            10.0,
            scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD,
            "年假规定",
        );
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].result.path, std::path::Path::new("/b"));
        assert_eq!(merged[0].sources.len(), 2);
        assert_eq!(verdict.query_lang, scout_result_normalizer::lang::Lang::Zh);
        assert!((verdict.vec_top1_cosine - 0.10).abs() < f64::EPSILON);
        assert!(!verdict.skipped_fts);
    }

    #[test]
    fn fuse_fanout_rrf_high_cosine_skips_fts() {
        // VEC top-1 cosine 0.9 → T* < 0.9 时跳 FTS（占位 T*>0.9 时退化不跳）；用 `||` 守护占位。
        let fts = vec![result_at(
            "/annual_leave.md",
            BackendKind::NativeIndex,
            MatchType::Filename,
        )];
        let vec_list = vec![{
            let mut r = result_at(
                "/year_off_rules.md",
                BackendKind::SemanticIndex,
                MatchType::Content,
            );
            r.score = Some(0.9);
            r
        }];
        let (_merged, verdict) = fuse_fanout_rrf(
            vec![fts],
            vec![vec_list],
            10.0,
            scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD,
            "annual leave policy",
        );
        assert_eq!(verdict.query_lang, scout_result_normalizer::lang::Lang::En);
        assert!((verdict.vec_top1_cosine - 0.9).abs() < f64::EPSILON);
        assert!(
            verdict.skipped_fts || scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD > 0.9,
            "T*={} 下 cosine=0.9 应跳 FTS（或 spec §5 降级 T*>0.9 时不跳）",
            scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD
        );
    }

    #[test]
    fn partial_failure_still_merges_others() {
        let backends = vec![
            tool(
                "search.windows",
                BackendKind::WindowsSearch,
                Script::Error(SearchError::BackendUnavailable {
                    reason: "down".to_owned(),
                }),
            ),
            tool(
                "search.local",
                BackendKind::NativeIndex,
                Script::Results(vec![result_at(
                    "/c",
                    BackendKind::NativeIndex,
                    MatchType::Content,
                )]),
            ),
        ];
        let mut got = Vec::new();
        let outcome = block_on(run_fanout_merge(
            &backends,
            &ExpandedSearchIntent::identity(intent()),
            CancellationToken::new(),
            &mut |m| got.push(m),
        ));
        assert_eq!(outcome.total, 1, "失败后端被跳过，成功后端结果保留");
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0].0, "search.windows");
    }

    #[test]
    fn all_failed_total_zero() {
        let backends = vec![tool(
            "search.windows",
            BackendKind::WindowsSearch,
            Script::Error(SearchError::Io {
                detail: "boom".to_owned(),
            }),
        )];
        let mut got = Vec::new();
        let outcome = block_on(run_fanout_merge(
            &backends,
            &ExpandedSearchIntent::identity(intent()),
            CancellationToken::new(),
            &mut |m| got.push(m),
        ));
        assert_eq!(outcome.total, 0);
        assert!(got.is_empty());
        assert_eq!(outcome.errors.len(), 1);
    }

    // ===== run_fanout_merge_with_fallback：内容轮零结果才文件名兜底 =====

    #[test]
    fn fallback_not_triggered_when_content_has_results() {
        // 内容轮（local）有结果 → 文件名兜底（everything）不被调用。
        let content = vec![tool(
            "search.local",
            BackendKind::NativeIndex,
            Script::Results(vec![result_at(
                "/c",
                BackendKind::NativeIndex,
                MatchType::Content,
            )]),
        )];
        let fallback = vec![tool(
            "search.everything",
            BackendKind::Everything,
            Script::Results(vec![result_at(
                "/noise",
                BackendKind::Everything,
                MatchType::Filename,
            )]),
        )];
        let mut got = Vec::new();
        let mut fallback_used = false;
        let outcome = block_on(run_fanout_merge_with_fallback(
            &content,
            &fallback,
            &ExpandedSearchIntent::identity(intent()),
            CancellationToken::new(),
            &mut |m| got.push(m),
            &mut || fallback_used = true,
        ));
        assert_eq!(outcome.total, 1);
        assert!(!fallback_used, "内容轮有结果时不应触发文件名兜底");
        // /noise 不应混入。
        assert!(got
            .iter()
            .all(|m| m.result.path != std::path::Path::new("/noise")));
    }

    #[test]
    fn fallback_triggered_when_content_empty() {
        // 内容轮（windows）干净零结果 → 文件名兜底（everything）命中并返回。
        let content = vec![tool(
            "search.windows",
            BackendKind::WindowsSearch,
            Script::Results(vec![]),
        )];
        let fallback = vec![tool(
            "search.everything",
            BackendKind::Everything,
            Script::Results(vec![result_at(
                "/non-indexed/预算报告.xlsx",
                BackendKind::Everything,
                MatchType::Filename,
            )]),
        )];
        let mut got = Vec::new();
        let mut fallback_used = false;
        let outcome = block_on(run_fanout_merge_with_fallback(
            &content,
            &fallback,
            &ExpandedSearchIntent::identity(intent()),
            CancellationToken::new(),
            &mut |m| got.push(m),
            &mut || fallback_used = true,
        ));
        assert!(fallback_used, "内容轮零结果应触发文件名兜底");
        assert_eq!(outcome.total, 1);
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].result.path,
            std::path::Path::new("/non-indexed/预算报告.xlsx")
        );
    }

    #[test]
    fn fallback_both_empty_total_zero() {
        let content = vec![tool(
            "search.windows",
            BackendKind::WindowsSearch,
            Script::Results(vec![]),
        )];
        let fallback = vec![tool(
            "search.everything",
            BackendKind::Everything,
            Script::Results(vec![]),
        )];
        let mut got = Vec::new();
        let mut fallback_used = false;
        let outcome = block_on(run_fanout_merge_with_fallback(
            &content,
            &fallback,
            &ExpandedSearchIntent::identity(intent()),
            CancellationToken::new(),
            &mut |m| got.push(m),
            &mut || fallback_used = true,
        ));
        assert!(fallback_used, "内容轮零结果仍应尝试兜底");
        assert_eq!(outcome.total, 0);
        assert!(got.is_empty());
    }

    #[test]
    fn fallback_noop_when_no_filename_backend() {
        // macOS 形态：无纯文件名后端 → 内容轮零结果也不触发兜底（filename_fallback 空）。
        let content = vec![tool(
            "search.spotlight",
            BackendKind::Spotlight,
            Script::Results(vec![]),
        )];
        let fallback: Vec<Arc<dyn SearchableTool>> = Vec::new();
        let mut got = Vec::new();
        let mut fallback_used = false;
        let outcome = block_on(run_fanout_merge_with_fallback(
            &content,
            &fallback,
            &ExpandedSearchIntent::identity(intent()),
            CancellationToken::new(),
            &mut |m| got.push(m),
            &mut || fallback_used = true,
        ));
        assert_eq!(outcome.total, 0);
        assert!(!fallback_used, "无文件名后端时 on_fallback 不应被调用");
    }

    #[test]
    fn cancelled_before_query_yields_nothing() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let backends = vec![tool(
            "search.local",
            BackendKind::NativeIndex,
            Script::Results(vec![result_at(
                "/a",
                BackendKind::NativeIndex,
                MatchType::Content,
            )]),
        )];
        let mut got = Vec::new();
        let outcome = block_on(run_fanout_merge(
            &backends,
            &ExpandedSearchIntent::identity(intent()),
            cancel,
            &mut |m| got.push(m),
        ));
        assert_eq!(outcome.total, 0);
        assert!(got.is_empty());
    }
}
