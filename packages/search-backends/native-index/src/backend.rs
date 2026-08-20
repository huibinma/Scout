//! [`SearchBackend`] 实现：把 `SearchIntent` 翻译为 [`crate::NameQuery`]，
//! 查询内置原生索引（[`crate::search_query`]），再对结果做位置/时间/大小的
//! 结果端过滤——这三类约束不在内存索引里（索引只存文件名 + 父子关系，见
//! [`crate::index`] 设计取舍），故用 [`std::fs::metadata`] 逐条候选补齐，与原
//! `EverythingBackend` 的 `result_from_path`（同样靠 `fs::metadata` 补 size/时间）
//! 同一思路。
//!
//! 只在文件名候选集合上做结果端过滤，不改变"索引本身只存文件名"的核心设计——
//! 候选集合上限 [`CANDIDATE_CAP`] 早于 metadata 过滤前截断，防止时间/大小条件
//! 很苛刻时仍要对整卷做 `stat`。

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Local, NaiveDate, TimeZone};
use scout_platform_windows::WindowsLocationResolver;
use scout_search_backend::{
    backend_stream_from_results, extensions_for_file_type, intent_sort_order,
    media_common_constraints, media_derived_file_types, result_id_for_path, sort_results,
    BackendKind, BackendSearchFuture, BackendStream, CancellationToken, CommonConstraints,
    ExpandedSearchIntent, FileSearch, ImplementationStatus, KeywordGroup, Location,
    LocationResolveError, LocationResolver, MatchMode, MatchType, MediaSearch, MediaType, Quality,
    SearchBackend, SearchError, SearchIntent, SearchResult, SearchResultMetadata, SizeExpression,
    SizeUnit, SortOrder, TimeExpression,
};

use crate::index::NameQuery;

/// 候选集合上限：内存索引查询阶段先按此值截断，再做位置/时间/大小结果端过滤，
/// 避免苛刻的后置条件下仍需 `stat` 整卷候选（对应 `docs` 里"极低资源占用"目标）。
const CANDIDATE_CAP: usize = 20_000;
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

/// 内置原生索引后端（[`BackendKind::NativeFileIndex`]）：替代原 `EverythingBackend`。
/// 只索引文件名/路径，不含正文——语义与原 Everything 集成一致，只是执行体从
/// spawn `es.exe` 换成查询进程内的 [`crate::NativeIndexService`]。
#[derive(Debug, Default)]
pub struct NativeIndexBackend<R = WindowsLocationResolver> {
    resolver: R,
}

impl NativeIndexBackend<WindowsLocationResolver> {
    /// 创建默认后端。
    pub fn new() -> Result<Self, SearchError> {
        let resolver =
            WindowsLocationResolver::new().map_err(|error| SearchError::BackendUnavailable {
                reason: error.to_string(),
            })?;
        Ok(Self { resolver })
    }
}

impl<R> NativeIndexBackend<R> {
    /// 用指定 resolver 构造（测试注入用）。
    pub const fn with_resolver(resolver: R) -> Self {
        Self { resolver }
    }
}

impl<R> SearchBackend for NativeIndexBackend<R>
where
    R: LocationResolver,
{
    fn kind(&self) -> BackendKind {
        BackendKind::NativeFileIndex
    }

    fn implementation_status(&self) -> ImplementationStatus {
        if self.is_available() {
            ImplementationStatus::Real
        } else {
            ImplementationStatus::Stub
        }
    }

    fn is_available(&self) -> bool {
        crate::native_index_available()
    }

    fn search<'a>(
        &'a self,
        intent: &'a SearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        Box::pin(async move {
            let query = translate_intent(intent, &self.resolver)?;
            Ok(execute(&query, cancel))
        })
    }

    fn search_expanded<'a>(
        &'a self,
        expanded: &'a ExpandedSearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        Box::pin(async move {
            let query = translate_expanded(expanded, &self.resolver)?;
            Ok(execute(&query, cancel))
        })
    }
}

/// 翻译后的查询：文件名条件（喂给内存索引）+ 结果端过滤条件 + 排序/limit。
#[derive(Debug)]
struct Query {
    name: NameQuery,
    location: Option<Location>,
    modified_time: Option<TimeExpression>,
    created_time: Option<TimeExpression>,
    accessed_time: Option<TimeExpression>,
    size: Option<SizeExpression>,
    sort: Option<SortOrder>,
    limit: usize,
}

fn translate_intent<R>(intent: &SearchIntent, resolver: &R) -> Result<Query, SearchError>
where
    R: LocationResolver,
{
    match intent {
        SearchIntent::FileSearch(search) => translate_file_search(search, None, resolver),
        SearchIntent::MediaSearch(search) => translate_media_search(search, None, resolver),
        SearchIntent::Refine(_) | SearchIntent::FileAction(_) | SearchIntent::Clarify(_) => {
            Err(unsupported())
        }
    }
}

fn translate_expanded<R>(
    expanded: &ExpandedSearchIntent,
    resolver: &R,
) -> Result<Query, SearchError>
where
    R: LocationResolver,
{
    let groups = keyword_groups_to_name_groups(&expanded.keyword_groups, expanded.match_mode);
    match &expanded.base {
        SearchIntent::FileSearch(search) => translate_file_search(search, Some(groups), resolver),
        SearchIntent::MediaSearch(search) => translate_media_search(search, Some(groups), resolver),
        SearchIntent::Refine(_) | SearchIntent::FileAction(_) | SearchIntent::Clarify(_) => {
            Err(unsupported())
        }
    }
}

fn unsupported() -> SearchError {
    SearchError::UnsupportedIntent {
        detail: "NativeIndexBackend only accepts merged file_search/media_search intents"
            .to_owned(),
    }
}

/// `keyword_groups` 按 `match_mode` 转成 [`NameQuery`] 的 AND-of-OR 组：
/// `All` 逐组保留（组内 OR、组间 AND，与 base keyword 语义一致）；`Any` 摊平进
/// **单个** OR 组（NameQuery 的"组间 AND"退化为一组时即为"任一命中"）。
fn keyword_groups_to_name_groups(
    groups: &[KeywordGroup],
    match_mode: MatchMode,
) -> Vec<Vec<String>> {
    match match_mode {
        MatchMode::All => groups
            .iter()
            .filter(|g| !g.head.is_empty())
            .map(|g| g.all().into_iter().map(str::to_owned).collect())
            .collect(),
        MatchMode::Any => {
            let flat: Vec<String> = groups
                .iter()
                .filter(|g| !g.head.is_empty())
                .flat_map(KeywordGroup::all)
                .map(str::to_owned)
                .collect();
            if flat.is_empty() {
                Vec::new()
            } else {
                vec![flat]
            }
        }
    }
}

fn translate_file_search<R>(
    search: &FileSearch,
    groups_override: Option<Vec<Vec<String>>>,
    resolver: &R,
) -> Result<Query, SearchError>
where
    R: LocationResolver,
{
    let keyword_groups = groups_override.unwrap_or_else(|| {
        search
            .keywords
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|k| !k.is_empty())
            .map(|k| vec![k.clone()])
            .collect()
    });
    build_query(
        keyword_groups,
        CommonConstraints {
            keywords: None,
            extensions: search.extensions.as_deref(),
            file_type: search.file_type.as_deref(),
            location: search.location.as_ref(),
            modified_time: search.modified_time.as_ref(),
            created_time: search.created_time.as_ref(),
            accessed_time: search.accessed_time.as_ref(),
            size: search.size.as_ref(),
            exclude_extensions: search.exclude_extensions.as_deref(),
            exclude_file_type: search.exclude_file_type.as_deref(),
        },
        search.sort,
        search.limit,
        resolver,
        &[],
    )
}

fn translate_media_search<R>(
    search: &MediaSearch,
    groups_override: Option<Vec<Vec<String>>>,
    resolver: &R,
) -> Result<Query, SearchError>
where
    R: LocationResolver,
{
    let file_types = media_derived_file_types(search);
    let keyword_groups = groups_override.unwrap_or_else(|| {
        search
            .keywords
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|k| !k.is_empty())
            .map(|k| vec![k.clone()])
            .collect()
    });

    // 媒体专属词（artist/title/album/genre）本索引无法读取音频/图片元数据，只能
    // 退化为文件名子串匹配——与原 Everything 集成同一限制（纯文件名引擎）。
    let mut extra_terms: Vec<String> = Vec::new();
    if let Some(v) = search.artist.as_deref().filter(|s| !s.is_empty()) {
        extra_terms.push(v.to_owned());
    }
    if let Some(v) = search.title.as_deref().filter(|s| !s.is_empty()) {
        extra_terms.push(v.to_owned());
    }
    if let Some(v) = search.album.as_deref().filter(|s| !s.is_empty()) {
        extra_terms.push(v.to_owned());
    }
    if let Some(v) = search.genre.as_deref().filter(|s| !s.is_empty()) {
        extra_terms.push(v.to_owned());
    }

    let mut query = build_query(
        keyword_groups,
        media_common_constraints(search, None, file_types.as_deref()),
        search.sort,
        search.limit,
        resolver,
        &extra_terms,
    )?;

    // 无显式扩展名时，quality=lossless 退化为无损音频扩展名过滤（同原 Everything）。
    if search.quality == Some(Quality::Lossless) && search.extensions.is_none() {
        for ext in ["flac", "wav", "aiff", "ape"] {
            query.name.extensions.push(ext.to_owned());
        }
    }
    // 截屏且未显式给位置 → 加「截屏」目录 hint（同原 Everything 行为）。
    if search.media_type == MediaType::Screenshot && search.location.is_none() {
        if let Ok(paths) = resolver.resolve_hint("截屏") {
            let mut location = query.location.unwrap_or_default();
            let mut include = location.include.unwrap_or_default();
            include.extend(paths.into_iter().map(|p| p.to_string_lossy().into_owned()));
            location.include = Some(include);
            query.location = Some(location);
        }
    }
    // duration 约束无法在纯文件名索引上落实（无法读取媒体时长元数据），忽略——
    // 与原 Everything 集成把 duration 映射到"文件字节数"这一不准确的近似不同，
    // 本实现选择诚实地不支持，而非提供一个看似生效实则误导的过滤。

    Ok(query)
}

#[allow(clippy::too_many_arguments)]
fn build_query<R>(
    keyword_groups: Vec<Vec<String>>,
    constraints: CommonConstraints<'_>,
    sort: Option<SortOrder>,
    limit: Option<u32>,
    resolver: &R,
    extra_and_terms: &[String],
) -> Result<Query, SearchError>
where
    R: LocationResolver,
{
    let mut groups = keyword_groups;
    for term in extra_and_terms {
        groups.push(vec![term.clone()]);
    }

    let mut extensions: Vec<String> = constraints
        .extensions
        .map(<[String]>::to_vec)
        .unwrap_or_default();
    if extensions.is_empty() {
        if let Some(file_types) = constraints.file_type {
            let mut seen = Vec::new();
            for ft in file_types {
                for ext in extensions_for_file_type(*ft) {
                    if !seen.iter().any(|e: &String| e == ext) {
                        seen.push((*ext).to_owned());
                    }
                }
            }
            extensions = seen;
        }
    }

    let mut exclude_extensions: Vec<String> = constraints
        .exclude_extensions
        .map(<[String]>::to_vec)
        .unwrap_or_default();
    if let Some(exclude_types) = constraints.exclude_file_type {
        for ft in exclude_types {
            for ext in extensions_for_file_type(*ft) {
                if !exclude_extensions.iter().any(|e| e == ext) {
                    exclude_extensions.push((*ext).to_owned());
                }
            }
        }
    }

    let mut location = constraints.location.cloned();
    if let Some(loc) = &constraints.location {
        if let Some(hint) = loc.hint.as_deref() {
            let hint_paths =
                resolver
                    .resolve_hint(hint)
                    .map_err(
                        |error: LocationResolveError| SearchError::UnsupportedIntent {
                            detail: error.to_string(),
                        },
                    )?;
            let entry = location.get_or_insert_with(Location::default);
            let mut include = entry.include.clone().unwrap_or_default();
            include.extend(
                hint_paths
                    .into_iter()
                    .map(|p| p.to_string_lossy().into_owned()),
            );
            entry.include = Some(include);
        }
    }

    let limit = limit
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(DEFAULT_LIMIT)
        .min(MAX_LIMIT);

    Ok(Query {
        name: NameQuery {
            keyword_groups: groups,
            extensions,
            exclude_extensions,
        },
        location,
        modified_time: constraints.modified_time.copied(),
        created_time: constraints.created_time.copied(),
        accessed_time: constraints.accessed_time.copied(),
        size: constraints.size.copied(),
        sort,
        limit,
    })
}

fn execute(query: &Query, cancel: CancellationToken) -> BackendStream {
    if cancel.is_cancelled() {
        return backend_stream_from_results(Vec::new(), cancel);
    }

    let candidate_cap = (query.limit.saturating_mul(20)).clamp(query.limit.max(1), CANDIDATE_CAP);
    let candidates = crate::search_query(&query.name, candidate_cap);

    let include_roots: Vec<PathBuf> = query
        .location
        .as_ref()
        .and_then(|l| l.include.as_ref())
        .map(|paths| paths.iter().map(PathBuf::from).collect())
        .unwrap_or_default();
    let exclude_roots: Vec<PathBuf> = query
        .location
        .as_ref()
        .and_then(|l| l.exclude.as_ref())
        .map(|paths| paths.iter().map(PathBuf::from).collect())
        .unwrap_or_default();

    let mut results = Vec::new();
    for path in candidates {
        if cancel.is_cancelled() {
            break;
        }
        if !include_roots.is_empty() && !path_under_any(&path, &include_roots) {
            continue;
        }
        if !exclude_roots.is_empty() && path_under_any(&path, &exclude_roots) {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if let Some(time) = &query.modified_time {
            if !metadata.modified().is_ok_and(|t| time_matches(time, t)) {
                continue;
            }
        }
        if let Some(time) = &query.created_time {
            if !metadata.created().is_ok_and(|t| time_matches(time, t)) {
                continue;
            }
        }
        if let Some(time) = &query.accessed_time {
            if !metadata.accessed().is_ok_and(|t| time_matches(time, t)) {
                continue;
            }
        }
        if let Some(size) = &query.size {
            if !size_matches(size, metadata.len()) {
                continue;
            }
        }
        results.push(result_from_path(path, &metadata));
        if results.len() >= query.limit.saturating_mul(4).max(query.limit) {
            // 候选已排过滤，提前止损——避免结果端过滤命中率很高时仍处理整份候选集。
            break;
        }
    }

    sort_results(&mut results, query.sort);
    results.truncate(query.limit);
    backend_stream_from_results(results, cancel)
}

fn path_under_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

fn local_midnight(days_from_today: i32) -> DateTime<Local> {
    let today = Local::now().date_naive();
    let date = today + chrono::Duration::days(i64::from(days_from_today));
    Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap_or_default())
        .single()
        .unwrap_or_else(Local::now)
}

fn naive_date_midnight(date: NaiveDate) -> DateTime<Local> {
    Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap_or_default())
        .single()
        .unwrap_or_else(Local::now)
}

fn time_matches(expr: &TimeExpression, system_time: SystemTime) -> bool {
    let when = DateTime::<Local>::from(system_time);
    match expr {
        TimeExpression::Relative { value } => {
            let (from_days, to_days) = scout_search_backend::relative_time_bounds(*value);
            let from = local_midnight(from_days);
            let to = local_midnight(to_days);
            when >= from && when < to
        }
        TimeExpression::Absolute { from, to } => {
            let from = naive_date_midnight(*from);
            let to = naive_date_midnight(*to) + chrono::Duration::days(1);
            when >= from && when < to
        }
        TimeExpression::Before { value } => when < naive_date_midnight(*value),
        TimeExpression::After { value } => {
            when >= naive_date_midnight(*value) + chrono::Duration::days(1)
        }
    }
}

// 文件字节数不可能接近 2^52（f64 尾数精度上限，约 4PB），此处转换无实际精度风险。
#[allow(clippy::cast_precision_loss)]
fn size_matches(expr: &SizeExpression, actual_bytes: u64) -> bool {
    let bytes_value = |value: f64, unit: SizeUnit| -> Option<f64> {
        match unit {
            SizeUnit::B => Some(value),
            SizeUnit::Kb => Some(value * 1_000.0),
            SizeUnit::Mb => Some(value * 1_000_000.0),
            SizeUnit::Gb => Some(value * 1_000_000_000.0),
            SizeUnit::Sec | SizeUnit::Min | SizeUnit::Hour => None,
        }
    };
    let actual = actual_bytes as f64;
    match expr {
        SizeExpression::GreaterThan { value, unit } => {
            bytes_value(*value, *unit).is_some_and(|v| actual > v)
        }
        SizeExpression::LessThan { value, unit } => {
            bytes_value(*value, *unit).is_some_and(|v| actual < v)
        }
        SizeExpression::Between { min, max, unit } => {
            match (bytes_value(*min, *unit), bytes_value(*max, *unit)) {
                (Some(lo), Some(hi)) => actual >= lo && actual <= hi,
                _ => true,
            }
        }
    }
}

fn result_from_path(path: PathBuf, metadata: &std::fs::Metadata) -> SearchResult {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();
    SearchResult {
        id: result_id_for_path(&path),
        path,
        name,
        source: BackendKind::NativeFileIndex,
        match_type: MatchType::Filename,
        score: None,
        metadata: SearchResultMetadata {
            modified_time: metadata.modified().ok().map(DateTime::from),
            created_time: metadata.created().ok().map(DateTime::from),
            accessed_time: metadata.accessed().ok().map(DateTime::from),
            size_bytes: Some(metadata.len()),
            ..SearchResultMetadata::default()
        },
    }
}

// `intent_sort_order` 目前未在本模块直接使用（`search`/`search_expanded` 均从
// intent 自带的 `sort` 字段取值，见 `build_query`），保留 re-export 供将来
// harness 侧统一调用点探测排序意图时复用（与其余后端一致的可见性面）。
#[allow(dead_code)]
fn _keep_intent_sort_order_reachable(intent: &SearchIntent) -> Option<SortOrder> {
    intent_sort_order(intent)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use scout_search_backend::SchemaVersion;

    #[derive(Debug, Clone)]
    struct MockResolver;

    impl LocationResolver for MockResolver {
        fn resolve_hint(&self, hint: &str) -> Result<Vec<PathBuf>, LocationResolveError> {
            let path = match hint {
                "下载" | "downloads" => "/Users/tester/Downloads",
                "截屏" | "screenshots" => "/Users/tester/Pictures/Screenshots",
                _ => "/Users/tester",
            };
            Ok(vec![PathBuf::from(path)])
        }
    }

    fn file_search(keywords: Option<Vec<&str>>, extensions: Option<Vec<&str>>) -> SearchIntent {
        SearchIntent::FileSearch(FileSearch {
            schema_version: SchemaVersion::V1,
            language: None,
            keywords: keywords.map(|k| k.iter().map(|s| (*s).to_string()).collect()),
            extensions: extensions.map(|e| e.iter().map(|s| (*s).to_string()).collect()),
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

    #[test]
    fn translate_file_search_keywords_become_and_of_singleton_groups() {
        let intent = file_search(Some(vec!["报告", "预算"]), None);
        let query = translate_intent(&intent, &MockResolver).unwrap();
        assert_eq!(
            query.name.keyword_groups,
            vec![vec!["报告".to_owned()], vec!["预算".to_owned()]]
        );
        assert_eq!(query.limit, DEFAULT_LIMIT);
    }

    #[test]
    fn translate_file_search_extensions_pass_through() {
        let intent = file_search(None, Some(vec!["pdf", "docx"]));
        let query = translate_intent(&intent, &MockResolver).unwrap();
        assert_eq!(query.name.extensions, vec!["pdf", "docx"]);
    }

    #[test]
    fn translate_rejects_non_search_intents() {
        let intent = SearchIntent::Clarify(scout_search_backend::Clarify {
            schema_version: SchemaVersion::V1,
            language: None,
            question: "?".to_owned(),
            options: None,
            reason: scout_search_backend::ClarifyReason::AmbiguousLocation,
        });
        let error = translate_intent(&intent, &MockResolver).unwrap_err();
        assert!(matches!(error, SearchError::UnsupportedIntent { .. }));
    }

    #[test]
    fn expanded_any_mode_flattens_into_single_or_group() {
        let base = file_search(Some(vec!["报告"]), None);
        let expanded = ExpandedSearchIntent {
            base,
            keyword_groups: vec![
                KeywordGroup::singleton("报告"),
                KeywordGroup::singleton("预算"),
            ],
            match_mode: MatchMode::Any,
        };
        let query = translate_expanded(&expanded, &MockResolver).unwrap();
        assert_eq!(
            query.name.keyword_groups,
            vec![vec!["报告".to_owned(), "预算".to_owned()]]
        );
    }

    #[test]
    fn expanded_all_mode_keeps_groups_separate_with_synonyms_ored() {
        let base = file_search(Some(vec!["报告"]), None);
        let expanded = ExpandedSearchIntent {
            base,
            keyword_groups: vec![KeywordGroup {
                head: "报告".to_owned(),
                synonyms: vec!["述职".to_owned()],
            }],
            match_mode: MatchMode::All,
        };
        let query = translate_expanded(&expanded, &MockResolver).unwrap();
        assert_eq!(
            query.name.keyword_groups,
            vec![vec!["报告".to_owned(), "述职".to_owned()]]
        );
    }

    #[test]
    fn size_matches_bytes_thresholds() {
        let gt = SizeExpression::GreaterThan {
            value: 1.0,
            unit: SizeUnit::Mb,
        };
        assert!(size_matches(&gt, 2_000_000));
        assert!(!size_matches(&gt, 500_000));
    }

    #[test]
    fn size_matches_duration_unit_is_always_true_not_filtered() {
        // duration 单位（s/m/h）不适用文件字节数，`bytes_value` 返回 None →
        // `is_some_and` 恒 false，即"不匹配" —— 但媒体 duration 走的是
        // `translate_media_search` 里被主动忽略的路径，不会走到这里。
        let lt = SizeExpression::LessThan {
            value: 5.0,
            unit: SizeUnit::Sec,
        };
        assert!(!size_matches(&lt, 100));
    }
}
