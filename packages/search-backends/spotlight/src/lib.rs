//! Spotlight 搜索后端 v0.1。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use scout_platform_macos::MacOsLocationResolver;
use scout_search_backend::{
    backend_stream_from_results, intent_sort_order, relative_time_bounds, sort_results,
    BackendKind, BackendSearchFuture, CancellationToken, CommonConstraints, ExpandedSearchIntent,
    FileSearch, FileType, KeywordGroup, LocationResolver, MatchMode, MatchType, MediaSearch,
    MediaType, Quality, SearchBackend, SearchError, SearchIntent, SearchResult,
    SearchResultMetadata, SizeExpression, SizeUnit, SortOrder, TimeExpression,
};
// 跨后端共用的小工具收拢在 common，后端按原名别名引入，调用点零改动。
use scout_search_backend::{
    is_path_excluded as is_excluded, result_id_for_path as result_id,
    validate_absolute_search_path as validate_search_path,
};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;
const DEFAULT_MDFIND_TIMEOUT: Duration = Duration::from_secs(10);

/// Spotlight `mdfind` 后端。
#[derive(Debug)]
pub struct SpotlightBackend<R = MacOsLocationResolver> {
    mdfind_path: PathBuf,
    resolver: R,
    timeout: Duration,
}

impl SpotlightBackend<MacOsLocationResolver> {
    /// 创建默认 Spotlight 后端。
    pub fn new() -> Result<Self, SearchError> {
        let resolver =
            MacOsLocationResolver::new().map_err(|error| SearchError::BackendUnavailable {
                reason: error.to_string(),
            })?;

        Ok(Self::with_resolver(resolver))
    }
}

impl<R> SpotlightBackend<R> {
    /// 使用指定 resolver 创建后端，便于测试注入。
    #[must_use]
    pub fn with_resolver(resolver: R) -> Self {
        Self {
            mdfind_path: PathBuf::from("mdfind"),
            resolver,
            timeout: DEFAULT_MDFIND_TIMEOUT,
        }
    }

    /// 覆盖 `mdfind` 路径，便于集成测试。
    #[must_use]
    pub fn with_mdfind_path(mut self, path: PathBuf) -> Self {
        self.mdfind_path = path;
        self
    }

    /// 覆盖单次 `mdfind` 调用超时，便于测试。
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl<R> SearchBackend for SpotlightBackend<R>
where
    R: LocationResolver,
{
    fn kind(&self) -> BackendKind {
        BackendKind::Spotlight
    }

    fn is_available(&self) -> bool {
        executable_exists(&self.mdfind_path)
    }

    fn search<'a>(
        &'a self,
        intent: &'a SearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        Box::pin(async move {
            let translated = translate_intent(intent, &self.resolver)?;
            let results = run_translated(
                &self.mdfind_path,
                self.timeout,
                &translated,
                intent_sort_order(intent),
                &cancel,
            )?;
            Ok(backend_stream_from_results(results, cancel))
        })
    }

    fn search_expanded<'a>(
        &'a self,
        expanded: &'a ExpandedSearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        Box::pin(async move {
            let translated = translate_intent_expanded(expanded, &self.resolver)?;
            let results = run_translated(
                &self.mdfind_path,
                self.timeout,
                &translated,
                intent_sort_order(&expanded.base),
                &cancel,
            )?;
            Ok(backend_stream_from_results(results, cancel))
        })
    }
}

/// 已翻译的 `mdfind` 查询。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotlightQuery {
    /// 传给 `mdfind` 的谓词参数。
    pub predicate: String,
    /// `-onlyin` 路径列表。
    pub only_in: Vec<PathBuf>,
    /// 结果端排除路径。
    pub exclude_paths: Vec<PathBuf>,
    /// 返回上限。
    pub limit: usize,
}

/// 一次搜索翻译产物：Q1（纯 comparison 复合）+ 可选 Q2（纯 string 复合）+ Q2 后置过滤。
/// `only_in` / `exclude_paths` / `limit` 两条查询各持一份（finish 内 Q1 clone、Q2 move）。
#[derive(Debug, Clone, PartialEq)]
pub struct TranslatedQuery {
    pub q1: SpotlightQuery,
    pub q2: Option<SpotlightQuery>,
    pub(crate) post_filters: Vec<PostFilter>,
}

/// 把 `SearchIntent` 翻译为 Spotlight 谓词。
pub fn translate_intent<R>(
    intent: &SearchIntent,
    resolver: &R,
) -> Result<TranslatedQuery, SearchError>
where
    R: LocationResolver,
{
    match intent {
        SearchIntent::FileSearch(search) => translate_file_search(search, resolver),
        SearchIntent::MediaSearch(search) => translate_media_search(search, resolver),
        SearchIntent::Refine(_) | SearchIntent::FileAction(_) | SearchIntent::Clarify(_) => {
            Err(SearchError::UnsupportedIntent {
                detail: "SpotlightBackend only accepts merged file_search/media_search intents"
                    .to_owned(),
            })
        }
    }
}

/// 把 `ExpandedSearchIntent` 翻译为 Spotlight 谓词（组内 OR、组间 AND）。
///
/// 若所有 keyword 组都是 singleton（未扩词），与 `translate_intent(&expanded.base)` 等价。
pub fn translate_intent_expanded<R>(
    expanded: &ExpandedSearchIntent,
    resolver: &R,
) -> Result<TranslatedQuery, SearchError>
where
    R: LocationResolver,
{
    // identity 优化：直接走原有翻译路径
    if expanded.is_identity() {
        return translate_intent(&expanded.base, resolver);
    }

    match &expanded.base {
        SearchIntent::FileSearch(search) => translate_file_search_expanded(
            search,
            &expanded.keyword_groups,
            expanded.match_mode,
            resolver,
        ),
        SearchIntent::MediaSearch(search) => {
            // MediaSearch 的同义词扩展：复用 media 翻译，仅替换 keyword 谓词部分
            translate_media_search_expanded(
                search,
                &expanded.keyword_groups,
                expanded.match_mode,
                resolver,
            )
        }
        SearchIntent::Refine(_) | SearchIntent::FileAction(_) | SearchIntent::Clarify(_) => {
            Err(SearchError::UnsupportedIntent {
                detail: "SpotlightBackend only accepts merged file_search/media_search intents"
                    .to_owned(),
            })
        }
    }
}

fn translate_file_search_expanded<R>(
    search: &FileSearch,
    groups: &[KeywordGroup],
    match_mode: MatchMode,
    resolver: &R,
) -> Result<TranslatedQuery, SearchError>
where
    R: LocationResolver,
{
    let mut builder = QueryBuilder::new(search.limit);
    // 用扩展后的 keyword 组替代原始 keyword 列表
    add_keyword_groups_to_builder(&mut builder, groups, match_mode);
    add_common_file_constraints(
        &mut builder,
        resolver,
        CommonConstraints {
            keywords: None, // 已由 groups 处理，不再重复添加
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
    )?;
    Ok(builder.finish())
}

fn translate_media_search_expanded<R>(
    search: &MediaSearch,
    groups: &[KeywordGroup],
    match_mode: MatchMode,
    resolver: &R,
) -> Result<TranslatedQuery, SearchError>
where
    R: LocationResolver,
{
    let mut builder = QueryBuilder::new(search.limit);
    let media_file_type = match search.media_type {
        MediaType::Audio => Some(FileType::Audio),
        MediaType::Image | MediaType::Screenshot => Some(FileType::Image),
        MediaType::Video => Some(FileType::Video),
    };
    // BETA-18：file_type 现为多值；media_type 推断的单一类型作 fallback 包成单元素。
    let file_types: Option<Vec<FileType>> = search
        .file_type
        .clone()
        .or_else(|| media_file_type.map(|ft| vec![ft]));

    // 用扩展后的 keyword 组替代原始 keyword 列表
    add_keyword_groups_to_builder(&mut builder, groups, match_mode);
    add_common_file_constraints(
        &mut builder,
        resolver,
        CommonConstraints {
            keywords: None, // 已由 groups 处理，不再重复添加
            extensions: search.extensions.as_deref(),
            file_type: file_types.as_deref(),
            location: search.location.as_ref(),
            modified_time: search.modified_time.as_ref(),
            created_time: search.created_time.as_ref(),
            accessed_time: search.accessed_time.as_ref(),
            size: search.size.as_ref(),
            exclude_extensions: search.exclude_extensions.as_deref(),
            exclude_file_type: search.exclude_file_type.as_deref(),
        },
    )?;

    if search.media_type == MediaType::Screenshot && search.location.is_none() {
        add_resolved_hint(&mut builder, resolver, "截屏")?;
    }
    add_media_metadata_constraints(&mut builder, search)?;

    Ok(builder.finish())
}

fn translate_file_search<R>(
    search: &FileSearch,
    resolver: &R,
) -> Result<TranslatedQuery, SearchError>
where
    R: LocationResolver,
{
    let mut builder = QueryBuilder::new(search.limit);
    add_common_file_constraints(
        &mut builder,
        resolver,
        CommonConstraints {
            keywords: search.keywords.as_deref(),
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
    )?;
    Ok(builder.finish())
}

fn translate_media_search<R>(
    search: &MediaSearch,
    resolver: &R,
) -> Result<TranslatedQuery, SearchError>
where
    R: LocationResolver,
{
    let mut builder = QueryBuilder::new(search.limit);
    let media_file_type = match search.media_type {
        MediaType::Audio => Some(FileType::Audio),
        MediaType::Image | MediaType::Screenshot => Some(FileType::Image),
        MediaType::Video => Some(FileType::Video),
    };
    // BETA-18：file_type 现为多值；media_type 推断的单一类型作 fallback 包成单元素。
    let file_types: Option<Vec<FileType>> = search
        .file_type
        .clone()
        .or_else(|| media_file_type.map(|ft| vec![ft]));

    add_common_file_constraints(
        &mut builder,
        resolver,
        CommonConstraints {
            keywords: search.keywords.as_deref(),
            extensions: search.extensions.as_deref(),
            file_type: file_types.as_deref(),
            location: search.location.as_ref(),
            modified_time: search.modified_time.as_ref(),
            created_time: search.created_time.as_ref(),
            accessed_time: search.accessed_time.as_ref(),
            size: search.size.as_ref(),
            exclude_extensions: search.exclude_extensions.as_deref(),
            exclude_file_type: search.exclude_file_type.as_deref(),
        },
    )?;

    if search.media_type == MediaType::Screenshot && search.location.is_none() {
        add_resolved_hint(&mut builder, resolver, "截屏")?;
    }
    add_media_metadata_constraints(&mut builder, search)?;

    Ok(builder.finish())
}

/// 媒体元数据约束：artist/title/album/genre 为 string 类（→ Q2），lossless 默认扩展名 / duration
/// 为 comparison 类（→ Q1）。Task 5 再补 media PostFilter；此处仅做分类路由。
fn add_media_metadata_constraints(
    builder: &mut QueryBuilder,
    search: &MediaSearch,
) -> Result<(), SearchError> {
    if let Some(artist) = search.artist.as_deref() {
        builder.and_str(format!(
            "(kMDItemAuthors CONTAINS[cd] \"{}\" || kMDItemMusicalGenre CONTAINS[cd] \"{}\")",
            escape_predicate_string(artist),
            escape_predicate_string(artist)
        ));
    }
    if let Some(title) = search.title.as_deref() {
        builder.and_str(format!(
            "(kMDItemTitle CONTAINS[cd] \"{}\" || kMDItemDisplayName CONTAINS[cd] \"{}\")",
            escape_predicate_string(title),
            escape_predicate_string(title)
        ));
    }
    if let Some(album) = search.album.as_deref() {
        builder.and_str(format!(
            "kMDItemAlbum CONTAINS[cd] \"{}\"",
            escape_predicate_string(album)
        ));
    }
    if let Some(genre) = search.genre.as_deref() {
        builder.and_str(format!(
            "kMDItemMusicalGenre CONTAINS[cd] \"{}\"",
            escape_predicate_string(genre)
        ));
    }
    if search.quality == Some(Quality::Lossless) && search.extensions.is_none() {
        builder.and_extensions(&["flac", "wav", "aiff", "ape"], false);
    }
    // duration 仅进 Q1 谓词，不产 PostFilter：SearchResultMetadata 无 duration 字段，
    // 无法在 Rust 端复刻；duration 叠加内容(Q2)命中的场景罕见，known limitation(BETA-15D)。
    if let Some(duration) = search.duration.as_ref() {
        builder.and_cmp(size_predicate_with_field(
            "kMDItemDurationSeconds",
            duration,
            UnitDomain::Duration,
        )?);
    }
    Ok(())
}

fn add_common_file_constraints<R>(
    builder: &mut QueryBuilder,
    resolver: &R,
    constraints: CommonConstraints<'_>,
) -> Result<(), SearchError>
where
    R: LocationResolver,
{
    if let Some(keywords) = constraints.keywords {
        for keyword in keywords.iter().filter(|keyword| !keyword.is_empty()) {
            builder.and_cmp(name_glob_predicate(keyword));
            builder.and_str(content_predicate(keyword));
        }
    }

    if let Some(extensions) = constraints.extensions {
        if !extensions.is_empty() {
            builder.and_extensions(extensions, false);
        }
    } else if let Some(file_types) = constraints.file_type {
        // BETA-18：多 file_type → 扩展名并集（去重保序）。
        let mut exts: Vec<&'static str> = Vec::new();
        for ft in file_types {
            for e in file_type_extensions(*ft) {
                if !exts.contains(e) {
                    exts.push(e);
                }
            }
        }
        if !exts.is_empty() {
            builder.and_extensions(&exts, false);
        }
    }

    if let Some(time) = constraints.modified_time {
        builder.and_cmp(time_predicate("kMDItemContentModificationDate", time));
        builder.and_post_filter(PostFilter {
            time: vec![TimeField::Modified(TimeFilter::from_expression(time)?)],
            ..PostFilter::default()
        });
    }
    if let Some(time) = constraints.created_time {
        builder.and_cmp(time_predicate("kMDItemContentCreationDate", time));
        builder.and_post_filter(PostFilter {
            time: vec![TimeField::Created(TimeFilter::from_expression(time)?)],
            ..PostFilter::default()
        });
    }
    if let Some(time) = constraints.accessed_time {
        builder.and_cmp(time_predicate("kMDItemLastUsedDate", time));
        builder.and_post_filter(PostFilter {
            time: vec![TimeField::Accessed(TimeFilter::from_expression(time)?)],
            ..PostFilter::default()
        });
    }
    if let Some(size) = constraints.size {
        builder.and_cmp(size_predicate_with_field(
            "kMDItemFSSize",
            size,
            UnitDomain::Bytes,
        )?);
        builder.and_post_filter(PostFilter {
            size: Some(SizeFilter::from_expression(size, UnitDomain::Bytes)?),
            ..PostFilter::default()
        });
    }

    if let Some(location) = constraints.location {
        if let Some(hint) = location.hint.as_deref() {
            add_resolved_hint(builder, resolver, hint)?;
        }
        if let Some(includes) = location.include.as_ref() {
            for path in includes {
                builder.only_in.push(validate_search_path(path)?);
            }
        }
        if let Some(excludes) = location.exclude.as_ref() {
            for path in excludes {
                builder.exclude_paths.push(validate_search_path(path)?);
            }
        }
    }

    if let Some(exclude_extensions) = constraints.exclude_extensions {
        if !exclude_extensions.is_empty() {
            builder.and_extensions(exclude_extensions, true);
        }
    }
    if let Some(exclude_file_type) = constraints.exclude_file_type {
        for file_type in exclude_file_type {
            builder.and_extensions(file_type_extensions(*file_type), true);
        }
    }

    Ok(())
}

fn add_resolved_hint<R>(
    builder: &mut QueryBuilder,
    resolver: &R,
    hint: &str,
) -> Result<(), SearchError>
where
    R: LocationResolver,
{
    let paths = resolver
        .resolve_hint(hint)
        .map_err(|error| SearchError::UnsupportedIntent {
            detail: error.to_string(),
        })?;
    builder.only_in.extend(paths);
    Ok(())
}

#[derive(Debug)]
struct QueryBuilder {
    cmp_predicates: Vec<String>,
    str_predicates: Vec<String>,
    post_filters: Vec<PostFilter>,
    only_in: Vec<PathBuf>,
    exclude_paths: Vec<PathBuf>,
    limit: usize,
}

impl QueryBuilder {
    fn new(limit: Option<u32>) -> Self {
        let limit = limit
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(DEFAULT_LIMIT)
            .min(MAX_LIMIT);
        Self {
            cmp_predicates: Vec::new(),
            str_predicates: Vec::new(),
            post_filters: Vec::new(),
            only_in: Vec::new(),
            exclude_paths: Vec::new(),
            limit,
        }
    }

    /// comparison 类谓词 → Q1。
    fn and_cmp(&mut self, predicate: String) {
        self.cmp_predicates.push(predicate);
    }

    /// string 类谓词 → Q2。
    fn and_str(&mut self, predicate: String) {
        self.str_predicates.push(predicate);
    }

    /// Q2 结果的 Rust 端约束（与 Q1 比较约束等价）。
    fn and_post_filter(&mut self, filter: PostFilter) {
        if !filter.is_empty() {
            self.post_filters.push(filter);
        }
    }

    /// 扩展名约束：Q1 加 cmp 谓词，Q2 加等价 PostFilter（同一组扩展名派生，避免阈值漂移）。
    fn and_extensions<S: AsRef<str>>(&mut self, extensions: &[S], negate: bool) {
        self.and_cmp(extension_predicate(extensions, negate));
        self.and_post_filter(PostFilter {
            extension: Some(ExtensionFilter {
                extensions: extensions
                    .iter()
                    .map(|e| e.as_ref().trim_start_matches('.').to_owned())
                    .collect(),
                negate,
            }),
            ..PostFilter::default()
        });
    }

    fn finish(self) -> TranslatedQuery {
        let q1_predicate = if self.cmp_predicates.is_empty() {
            "kMDItemFSName == \"*\"".to_owned()
        } else {
            self.cmp_predicates.join(" && ")
        };
        let q1 = SpotlightQuery {
            predicate: q1_predicate,
            only_in: self.only_in.clone(),
            exclude_paths: self.exclude_paths.clone(),
            limit: self.limit,
        };
        let (q2, post_filters) = if self.str_predicates.is_empty() {
            // 无 Q2 → post_filters 永不被应用（只过滤 Q2 结果），丢弃以保持表示诚实。
            (None, Vec::new())
        } else {
            (
                Some(SpotlightQuery {
                    predicate: self.str_predicates.join(" && "),
                    only_in: self.only_in,
                    exclude_paths: self.exclude_paths,
                    limit: self.limit,
                }),
                self.post_filters,
            )
        };
        TranslatedQuery {
            q1,
            q2,
            post_filters,
        }
    }
}

/// 文件名 glob 关键词谓词（Q1，comparison 类）：`FSName` + `DisplayName` 两字段子串 glob。
fn name_glob_predicate(keyword: &str) -> String {
    let g = escape_glob_pattern(keyword);
    format!("(kMDItemFSName == \"*{g}*\"cd || kMDItemDisplayName == \"*{g}*\"cd)")
}

/// 内容关键词谓词（Q2，string 类）。
fn content_predicate(keyword: &str) -> String {
    format!(
        "kMDItemTextContent CONTAINS[cd] \"{}\"",
        escape_predicate_string(keyword)
    )
}

/// 同义词组：文件名 glob，组内所有词跨 FSName/DisplayName OR。singleton 时与 `name_glob_predicate(head)` byte-equal。
fn name_glob_predicate_expanded(group: &KeywordGroup) -> String {
    if group.is_singleton() {
        return name_glob_predicate(&group.head);
    }
    let mut parts = Vec::with_capacity(group.all().len() * 2);
    for w in group.all() {
        let g = escape_glob_pattern(w);
        parts.push(format!("kMDItemFSName == \"*{g}*\"cd"));
        parts.push(format!("kMDItemDisplayName == \"*{g}*\"cd"));
    }
    format!("({})", parts.join(" || "))
}

/// 同义词组：内容 CONTAINS，组内所有词 OR。singleton 时与 `content_predicate(head)` byte-equal。
fn content_predicate_expanded(group: &KeywordGroup) -> String {
    if group.is_singleton() {
        return content_predicate(&group.head);
    }
    let parts: Vec<String> = group
        .all()
        .iter()
        .map(|w| {
            format!(
                "kMDItemTextContent CONTAINS[cd] \"{}\"",
                escape_predicate_string(w)
            )
        })
        .collect();
    format!("({})", parts.join(" || "))
}

/// 把 `groups` 的 Q1（文件名 glob）与 Q2（内容 CONTAINS）谓词分别合并成**单个**组合谓词
/// 推给 builder，组间连接符按全局 `match_mode` 取 `&&`（[`MatchMode::All`]，全部复合条件
/// 命中）或 `||`（[`MatchMode::Any`]，任一条件命中）。必须合并成单个谓词再推——`QueryBuilder`
/// 的 `cmp_predicates`/`str_predicates` 列表混装了扩展名/时间/大小/路径等其余约束，
/// `finish()` 对整个列表恒以 `&&` 连接；若逐组分别 push，组间语义就没法脱离「与其余约束
/// 一起 AND」，无法表达「任一关键词组命中即可，但其余约束仍必须满足」。
fn add_keyword_groups_to_builder(
    builder: &mut QueryBuilder,
    groups: &[KeywordGroup],
    match_mode: MatchMode,
) {
    let joiner = match match_mode {
        MatchMode::All => " && ",
        MatchMode::Any => " || ",
    };
    let mut cmp_parts = Vec::new();
    let mut str_parts = Vec::new();
    for group in groups.iter().filter(|g| !g.head.is_empty()) {
        cmp_parts.push(name_glob_predicate_expanded(group));
        str_parts.push(content_predicate_expanded(group));
    }
    if let Some(combined) = combine_predicates(cmp_parts, joiner) {
        builder.and_cmp(combined);
    }
    if let Some(combined) = combine_predicates(str_parts, joiner) {
        builder.and_str(combined);
    }
}

/// 把多个谓词字符串合并成一个：0 个 → `None`；1 个 → 原样返回（不加括号，与旧行为
/// byte-equal）；≥2 个 → 括号包裹后按 `joiner` 连接。
fn combine_predicates(parts: Vec<String>, joiner: &str) -> Option<String> {
    match parts.len() {
        0 => None,
        1 => parts.into_iter().next(),
        _ => Some(format!("({})", parts.join(joiner))),
    }
}

fn extension_predicate<S>(extensions: &[S], negate: bool) -> String
where
    S: AsRef<str>,
{
    let op = if negate { "!=" } else { "==" };
    let joiner = if negate { " && " } else { " || " };
    let parts = extensions
        .iter()
        .map(|extension| {
            let extension = extension.as_ref().trim_start_matches('.');
            format!(
                "kMDItemFSName {op} \"*.{}\"cd",
                escape_predicate_string(extension)
            )
        })
        .collect::<Vec<_>>()
        .join(joiner);
    format!("({parts})")
}

fn time_predicate(field: &str, time: &TimeExpression) -> String {
    match time {
        TimeExpression::Relative { value } => {
            let (from, to) = relative_time_bounds(*value);
            format!("{field} >= $time.today({from}) && {field} < $time.today({to})")
        }
        TimeExpression::Absolute { from, to } => format!(
            "{field} >= $time.iso(\"{from}T00:00:00\") && {field} <= $time.iso(\"{to}T23:59:59\")"
        ),
        TimeExpression::Before { value } => {
            format!("{field} < $time.iso(\"{value}T00:00:00\")")
        }
        TimeExpression::After { value } => {
            format!("{field} > $time.iso(\"{value}T23:59:59\")")
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum UnitDomain {
    Bytes,
    Duration,
}

fn size_predicate_with_field(
    field: &str,
    size: &SizeExpression,
    domain: UnitDomain,
) -> Result<String, SearchError> {
    match size {
        SizeExpression::GreaterThan { value, unit } => {
            Ok(format!("{field} > {}", unit_value(*value, *unit, domain)?))
        }
        SizeExpression::LessThan { value, unit } => {
            Ok(format!("{field} < {}", unit_value(*value, *unit, domain)?))
        }
        SizeExpression::Between { min, max, unit } => Ok(format!(
            "{field} >= {} && {field} <= {}",
            unit_value(*min, *unit, domain)?,
            unit_value(*max, *unit, domain)?
        )),
    }
}

/// 扩展名约束。macOS 26 拆分查询后，Q2（内容 CONTAINS）无法携带扩展名比较，
/// 需在 Rust 端按此约束二次过滤，语义须与谓词 `kMDItemFSName == "*.pdf"cd` 等价。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExtensionFilter {
    extensions: Vec<String>, // 不含前导点
    negate: bool,
}

impl ExtensionFilter {
    fn matches(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        let hit = ext.as_deref().is_some_and(|e| {
            self.extensions
                .iter()
                .any(|want| want.eq_ignore_ascii_case(e))
        });
        if self.negate {
            !hit
        } else {
            hit
        }
    }
}

/// 大小/时长约束。`min`/`max` 为归一化后的数值（字节或秒），
/// `*_inclusive` 标记是否含界，须与谓词的 `>`/`<`/`>=`/`<=` 严格对齐。
#[derive(Debug, Clone, PartialEq)]
struct SizeFilter {
    min: Option<f64>,
    max: Option<f64>,
    min_inclusive: bool,
    max_inclusive: bool,
}

impl SizeFilter {
    fn from_expression(size: &SizeExpression, domain: UnitDomain) -> Result<Self, SearchError> {
        // 复用 unit_value 归一化（与谓词同源），其产字符串，这里解析回数值比较。
        let to_num = |v: f64, u: SizeUnit| -> Result<f64, SearchError> {
            unit_value(v, u, domain).map(|s| s.parse::<f64>().unwrap_or(0.0))
        };
        Ok(match size {
            // 谓词 `>` / `<` 是严格比较；Between 谓词是 `>= && <=`（含界）。
            SizeExpression::GreaterThan { value, unit } => SizeFilter {
                min: Some(to_num(*value, *unit)?),
                max: None,
                min_inclusive: false,
                max_inclusive: false,
            },
            SizeExpression::LessThan { value, unit } => SizeFilter {
                min: None,
                max: Some(to_num(*value, *unit)?),
                min_inclusive: false,
                max_inclusive: false,
            },
            SizeExpression::Between { min, max, unit } => SizeFilter {
                min: Some(to_num(*min, *unit)?),
                max: Some(to_num(*max, *unit)?),
                min_inclusive: true,
                max_inclusive: true,
            },
        })
    }

    fn matches(&self, size_bytes: u64) -> bool {
        // 阈值经 unit_value 归一化为整数值的 f64；文件大小 < 2^52 字节（约 4 PB）
        // 在 f64 中可精确表示，故此处转换不会引入比较误差。
        #[allow(clippy::cast_precision_loss)]
        let v = size_bytes as f64;
        let lo = self
            .min
            .map_or(true, |m| if self.min_inclusive { v >= m } else { v > m });
        let hi = self
            .max
            .map_or(true, |m| if self.max_inclusive { v <= m } else { v < m });
        lo && hi
    }
}

/// 时间约束。`from`/`to` 端点的含界性由 `from_inclusive`/`to_inclusive` 决定，
/// 须与 `time_predicate` 的谓词严格性对齐：
/// Before → 上界严格（`< ...T00:00:00`），After → 下界严格（`> ...T23:59:59`），
/// Relative → 下含上严格（`>= today(from) && < today(to)`），
/// Absolute → 双含界（`>= fromT00:00:00 && <= toT23:59:59`）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct TimeFilter {
    from: Option<chrono::DateTime<chrono::Utc>>,
    to: Option<chrono::DateTime<chrono::Utc>>,
    from_inclusive: bool, // 谓词 >= 为 true，> 为 false
    to_inclusive: bool,   // 谓词 <= 为 true，< 为 false
}

impl TimeFilter {
    // 当前所有分支均不会失败，但保留 Result 以对齐谓词侧 API、并为后续日期校验留口（Task 4-6 接线）。
    #[allow(clippy::unnecessary_wraps)]
    fn from_expression(time: &TimeExpression) -> Result<Self, SearchError> {
        use chrono::{Duration, NaiveDate, NaiveTime, TimeZone, Utc};
        // 把 NaiveDate 落到当日起点/终点（UTC）。end_of_day=false → 00:00:00（含界下端，
        // 对应谓词 `T00:00:00`）；true → 23:59:59（含界上端，对应谓词 `T23:59:59`）。
        let day_bound = |date: NaiveDate, end_of_day: bool| -> chrono::DateTime<Utc> {
            // 23:59:59 = 当日零点 + 86399 秒，避免 from_hms_opt(..).unwrap() 的潜在 panic 面。
            let t = if end_of_day {
                NaiveTime::MIN + Duration::seconds(86_399)
            } else {
                NaiveTime::MIN
            };
            Utc.from_utc_datetime(&date.and_time(t))
        };
        Ok(match time {
            TimeExpression::Relative { value } => {
                // 注意：谓词侧用 `$time.today(N)`（按 **本地日** 边界，由 Spotlight 计算）。
                // 本 crate 的 chrono 未启用 `clock` 特性，无法取本地时区，这里以
                // `SystemTime::now()` 转成的 **UTC 当日零点** 为基准计算同样的天数偏移。
                // 非 UTC 时区下会与谓词产生最多约 1 天的边界偏差——见 BETA-15D 自查记录。
                let (from_days, to_days) = relative_time_bounds(*value);
                let now: chrono::DateTime<Utc> = std::time::SystemTime::now().into();
                let midnight = now.date_naive().and_time(NaiveTime::MIN);
                let to_utc = |days: i32| {
                    Utc.from_utc_datetime(&(midnight + Duration::days(i64::from(days))))
                };
                TimeFilter {
                    from: Some(to_utc(from_days)),
                    to: Some(to_utc(to_days)),
                    from_inclusive: true,
                    to_inclusive: false,
                }
            }
            TimeExpression::Absolute { from, to } => TimeFilter {
                from: Some(day_bound(*from, false)),
                to: Some(day_bound(*to, true)),
                from_inclusive: true,
                to_inclusive: true,
            },
            TimeExpression::Before { value } => TimeFilter {
                from: None,
                to: Some(day_bound(*value, false)),
                from_inclusive: true,
                to_inclusive: false,
            },
            TimeExpression::After { value } => TimeFilter {
                from: Some(day_bound(*value, true)),
                to: None,
                from_inclusive: false,
                to_inclusive: true,
            },
        })
    }

    fn matches(&self, t: Option<chrono::DateTime<chrono::Utc>>) -> bool {
        let Some(t) = t else { return false };
        let lo = self
            .from
            .map_or(true, |f| if self.from_inclusive { t >= f } else { t > f });
        let hi = self
            .to
            .map_or(true, |to| if self.to_inclusive { t <= to } else { t < to });
        lo && hi
    }
}

/// 时间约束作用的字段。
#[derive(Debug, Clone, PartialEq, Eq)]
enum TimeField {
    Modified(TimeFilter),
    Created(TimeFilter),
    Accessed(TimeFilter),
}

/// Q2（内容 CONTAINS）结果的 Rust 端二次过滤器：把无法进谓词的
/// 扩展名/大小/时间比较约束统一在此判定，语义须与 Q1 谓词等价。
#[derive(Debug, Clone, PartialEq, Default)]
struct PostFilter {
    extension: Option<ExtensionFilter>,
    size: Option<SizeFilter>,
    time: Vec<TimeField>,
}

impl PostFilter {
    fn is_empty(&self) -> bool {
        self.extension.is_none() && self.size.is_none() && self.time.is_empty()
    }

    fn matches(&self, result: &SearchResult) -> bool {
        if let Some(ext) = &self.extension {
            if !ext.matches(&result.path) {
                return false;
            }
        }
        if let Some(size) = &self.size {
            match result.metadata.size_bytes {
                Some(b) if size.matches(b) => {}
                _ => return false,
            }
        }
        for tf in &self.time {
            let ok = match tf {
                TimeField::Modified(f) => f.matches(result.metadata.modified_time),
                TimeField::Created(f) => f.matches(result.metadata.created_time),
                TimeField::Accessed(f) => f.matches(result.metadata.accessed_time),
            };
            if !ok {
                return false;
            }
        }
        true
    }
}

fn unit_value(value: f64, unit: SizeUnit, domain: UnitDomain) -> Result<String, SearchError> {
    let multiplier = match (domain, unit) {
        (UnitDomain::Bytes, SizeUnit::B) | (UnitDomain::Duration, SizeUnit::Sec) => 1.0,
        (UnitDomain::Bytes, SizeUnit::Kb) => 1_000.0,
        (UnitDomain::Bytes, SizeUnit::Mb) => 1_000_000.0,
        (UnitDomain::Bytes, SizeUnit::Gb) => 1_000_000_000.0,
        (UnitDomain::Duration, SizeUnit::Min) => 60.0,
        (UnitDomain::Duration, SizeUnit::Hour) => 3_600.0,
        _ => {
            return Err(SearchError::InvalidIntent {
                detail: format!("unit {unit:?} is not valid for {domain:?}"),
            });
        }
    };
    let normalized = value * multiplier;
    if !normalized.is_finite() || normalized < 0.0 {
        return Err(SearchError::InvalidIntent {
            detail: format!("invalid numeric value: {value}"),
        });
    }

    Ok(format!("{:.0}", normalized.round()))
}

fn file_type_extensions(file_type: FileType) -> &'static [&'static str] {
    // 单一信源在 common，避免三后端各持一份重复表（BETA-19 收拢）。
    scout_search_backend::extensions_for_file_type(file_type)
}

/// 转义 Spotlight 谓词字符串字面量。
#[must_use]
pub fn escape_predicate_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 转义 Spotlight glob 谓词字面量：在 `escape_predicate_string`（反斜杠 + 引号）基础上，
/// 额外把 glob 通配符 `*` `?` 转义为字面量，避免 keyword 被当通配展开。
#[must_use]
pub fn escape_glob_pattern(value: &str) -> String {
    escape_predicate_string(value)
        .replace('*', "\\*")
        .replace('?', "\\?")
}

/// 是否有可用的 `mdfind`（本机装了 Spotlight，绝大多数 macOS 机器默认自带）。
/// 供设置页「扫描本机 gguf」等不经 [`SearchBackend`] trait 面的场景做前置判断，
/// 同款用途参考 everything crate 的 `es_cli_available`。
#[must_use]
pub fn mdfind_available() -> bool {
    executable_exists(&PathBuf::from("mdfind"))
}

/// 按**扩展名**全盘查找文件（设置页「自动发现本机 gguf 模型」用，2026-08 加、
/// 与 everything crate 的 `find_files_by_extension` 同一用途、跨后端对齐 UX）。
/// `ext` 传裸扩展名（如 `gguf`，不含点）。Spotlight 默认索引整个卷（不同于 Windows
/// Search 只覆盖系统索引范围），一般不装额外软件即可扫到任意位置的文件。
/// mdfind 不可用 / 执行失败 → 返回空（调用方按"未发现"降级，不报错）。
#[must_use]
pub fn find_files_by_extension(ext: &str, limit: usize) -> Vec<PathBuf> {
    let mdfind_path = PathBuf::from("mdfind");
    if !executable_exists(&mdfind_path) {
        return Vec::new();
    }
    let g = escape_glob_pattern(ext);
    let query = SpotlightQuery {
        predicate: format!("kMDItemFSName == \"*.{g}\"cd"),
        only_in: Vec::new(),
        exclude_paths: Vec::new(),
        limit,
    };
    let cancel = CancellationToken::new();
    match run_mdfind(&mdfind_path, &query, DEFAULT_MDFIND_TIMEOUT, &cancel) {
        Ok(lines) => lines
            .into_iter()
            .filter(|l| !l.is_empty())
            .take(limit)
            .map(PathBuf::from)
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn run_mdfind(
    mdfind_path: &Path,
    query: &SpotlightQuery,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Vec<String>, SearchError> {
    let mut command = Command::new(mdfind_path);
    for path in &query.only_in {
        command.arg("-onlyin").arg(path);
    }
    command
        .arg(&query.predicate)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|error| SearchError::BackendUnavailable {
            reason: error.to_string(),
        })?;
    let start = Instant::now();

    loop {
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Vec::new());
        }

        if child.try_wait()?.is_some() {
            let output = child.wait_with_output()?;
            if output.status.success() {
                let stdout = String::from_utf8(output.stdout).map_err(|error| SearchError::Io {
                    detail: error.to_string(),
                })?;
                // macOS 26：mdfind 拒绝谓词时把 "Failed to create query" 打到 stdout 且 rc=0。
                if stdout.starts_with("Failed to create query") {
                    return Err(SearchError::Io {
                        detail: stdout.trim().to_owned(),
                    });
                }
                return Ok(stdout.lines().map(ToOwned::to_owned).collect());
            }

            return Err(SearchError::Io {
                detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }

        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SearchError::Timeout {
                elapsed_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
            });
        }

        thread::sleep(Duration::from_millis(10));
    }
}

/// 并发执行 Q1(comparison)与 Q2(string，若有)，合并去重，Q2 命中按 `post_filters` 过滤，
/// 最后排序 + 截断 limit。Q1 谓词已含全部约束，命中不再后置过滤。
fn run_translated(
    mdfind_path: &Path,
    timeout: Duration,
    query: &TranslatedQuery,
    sort: Option<SortOrder>,
    cancel: &CancellationToken,
) -> Result<Vec<SearchResult>, SearchError> {
    let q1 = &query.q1;
    let (lines1, lines2): (
        Result<Vec<String>, SearchError>,
        Result<Vec<String>, SearchError>,
    ) = std::thread::scope(|scope| {
        let handle2 = query
            .q2
            .as_ref()
            .map(|q2| scope.spawn(|| run_mdfind(mdfind_path, q2, timeout, cancel)));
        let lines1 = run_mdfind(mdfind_path, q1, timeout, cancel);
        let lines2 = match handle2 {
            Some(h) => h.join().unwrap_or_else(|_| {
                Err(SearchError::Io {
                    detail: "Q2 mdfind 线程 panic".to_owned(),
                })
            }),
            None => Ok(Vec::new()),
        };
        (lines1, lines2)
    });
    let lines1 = lines1?;
    let lines2 = lines2?;

    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut results: Vec<SearchResult> = Vec::new();
    collect_into(
        &lines1,
        None,
        &q1.exclude_paths,
        cancel,
        &mut seen,
        &mut results,
    );
    collect_into(
        &lines2,
        Some(&query.post_filters),
        &q1.exclude_paths,
        cancel,
        &mut seen,
        &mut results,
    );

    sort_results(&mut results, sort);
    results.truncate(q1.limit);
    Ok(results)
}

/// 把一批 mdfind 输出行转 `SearchResult` 并去重并入；`post` 为 Some 时按 `post_filters` 全过滤。
fn collect_into(
    lines: &[String],
    post: Option<&[PostFilter]>,
    exclude_paths: &[PathBuf],
    cancel: &CancellationToken,
    seen: &mut std::collections::HashSet<PathBuf>,
    results: &mut Vec<SearchResult>,
) {
    for line in lines {
        if cancel.is_cancelled() {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let path = PathBuf::from(line);
        if is_excluded(&path, exclude_paths) {
            continue;
        }
        if let Ok(result) = result_from_path(&path) {
            if let Some(filters) = post {
                if !filters.iter().all(|f| f.matches(&result)) {
                    continue;
                }
            }
            if seen.insert(result.path.clone()) {
                results.push(result);
            }
        }
    }
}

fn result_from_path(path: &Path) -> Result<SearchResult, SearchError> {
    let metadata = fs::metadata(path)?;
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();

    Ok(SearchResult {
        id: result_id(&canonical),
        path: canonical,
        name,
        source: BackendKind::Spotlight,
        match_type: MatchType::Filename,
        score: None,
        metadata: SearchResultMetadata {
            modified_time: metadata.modified().ok().map(DateTime::<Utc>::from),
            created_time: metadata.created().ok().map(DateTime::<Utc>::from),
            accessed_time: metadata.accessed().ok().map(DateTime::<Utc>::from),
            size_bytes: Some(metadata.len()),
            ..SearchResultMetadata::default()
        },
    })
}

fn executable_exists(path: &Path) -> bool {
    if path.components().count() > 1 {
        return path.is_file();
    }

    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join(path).is_file())
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use scout_platform_macos::MacOsLocationResolver;
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize)]
    struct Case {
        id: String,
        variant: String,
        intent: serde_json::Value,
    }

    fn resolver() -> MacOsLocationResolver {
        MacOsLocationResolver::with_home_and_screenshot_location(
            PathBuf::from("/Users/tester"),
            Some(PathBuf::from("/Users/tester/Desktop/Shots")),
        )
    }

    fn fixture_cases() -> Vec<Case> {
        serde_json::from_str(include_str!("../../common/tests/fixtures/cases.json")).unwrap()
    }

    #[test]
    fn translates_schema_search_cases_1_to_30() {
        let resolver = resolver();
        let cases = fixture_cases();

        for case in cases
            .iter()
            .filter(|case| case.id.parse::<u32>().is_ok_and(|id| id <= 30))
        {
            assert!(
                matches!(case.variant.as_str(), "FileSearch" | "MediaSearch"),
                "case {} should be search variant",
                case.id
            );
            let intent: SearchIntent = serde_json::from_value(case.intent.clone()).unwrap();
            let query = translate_intent(&intent, &resolver)
                .unwrap_or_else(|error| panic!("case {} translation failed: {error}", case.id));
            assert!(
                !query.q1.predicate.is_empty(),
                "case {} produced empty predicate",
                case.id
            );
            assert!(
                !query.q1.predicate.contains(" ; "),
                "case {} predicate looks shell-like: {}",
                case.id,
                query.q1.predicate
            );
        }
    }

    #[test]
    fn translates_location_hints_to_onlyin_paths() {
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search",
            "location": { "hint": "下载" }
        }))
        .unwrap();

        let query = translate_intent(&intent, &resolver()).unwrap();

        assert_eq!(
            query.q1.only_in,
            vec![PathBuf::from("/Users/tester/Downloads")]
        );
    }

    #[test]
    fn escapes_predicate_string_for_shell_injection_resistance() {
        let malicious = r#"预算"; rm -rf "$HOME"\done"#;
        let escaped = escape_predicate_string(malicious);

        assert!(escaped.contains("\\\""));
        assert!(escaped.contains("\\\\"));
        assert!(!escaped.contains("$HOME\""));

        let name_pred = name_glob_predicate(malicious);
        let content_pred = content_predicate(malicious);
        assert!(content_pred.contains("CONTAINS[cd]"));
        // 注入的 " 被转义，不能突破引号边界
        assert!(content_pred.contains("\\\""));
        assert!(!content_pred.contains("$HOME\""));
        // name_glob 走 escape_glob_pattern（在 escape_predicate_string 基础上再转义 * ?）
        assert!(name_pred.contains("kMDItemFSName == \"*"));
    }

    #[test]
    fn rejects_location_paths_with_newline_or_null_byte() {
        assert!(validate_search_path("/tmp/a\nb").is_err());
        assert!(validate_search_path("/tmp/a\0b").is_err());
    }

    #[test]
    fn unsupported_non_search_intents_are_reported() {
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "clarify",
            "reason": "unknown",
            "question": "?"
        }))
        .unwrap();

        let error = translate_intent(&intent, &resolver()).unwrap_err();

        assert!(matches!(error, SearchError::UnsupportedIntent { .. }));
    }

    #[cfg(unix)]
    use futures_executor::block_on;

    #[cfg(unix)]
    fn write_executable_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, body).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn dual_query_merges_q1_q2_dedups_and_postfilters_q2() {
        use futures_util::StreamExt;
        let root = std::env::temp_dir().join(format!("scout-b15d-dual-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let a = root.join("a.ppt");
        let b = root.join("b.txt");
        fs::write(&a, b"x").unwrap();
        fs::write(&b, b"x").unwrap();
        // fake mdfind：含 CONTAINS 的是 Q2 → 输出 a.ppt(重复) + b.txt(应被 ppt PostFilter 滤掉)；
        // 不含 CONTAINS 的是 Q1 → 输出 a.ppt
        let script = root.join("fake-mdfind.sh");
        write_executable_script(
            &script,
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *CONTAINS*) printf '%s\\n%s\\n' '{a}' '{b}';;\n  *) printf '%s\\n' '{a}';;\nesac\n",
                a = a.display(), b = b.display(),
            ),
        );
        let backend = SpotlightBackend::with_resolver(resolver()).with_mdfind_path(script);
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0", "intent": "file_search",
            "keywords": ["x"], "extensions": ["ppt"]
        }))
        .unwrap();
        let stream = block_on(backend.search(&intent, CancellationToken::new())).unwrap();
        let results: Vec<_> = block_on(stream.collect::<Vec<_>>())
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let names: Vec<_> = results.iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, vec!["a.ppt".to_string()]); // 去重 + b.txt 被 ppt PostFilter 滤掉
        let _ = fs::remove_dir_all(&root);
    }

    /// 验证 keyword + size 约束时，Q2（内容命中）结果会经过 size `PostFilter` 过滤：
    /// big.txt(5000 B > 1 KB) 保留，small.txt(10 B ≤ 1 KB) 被滤掉。
    /// Q1 返回空，所有候选均来自 Q2，以便孤立 size `PostFilter` 逻辑。
    #[cfg(unix)]
    #[test]
    fn dual_query_postfilters_q2_by_size() {
        use futures_util::StreamExt;
        let root = std::env::temp_dir().join(format!("scout-b15d-size-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let big = root.join("big.txt");
        let small = root.join("small.txt");
        // big.txt 5000 字节（> 1 KB = 1000 B），small.txt 10 字节（< 1 KB）
        fs::write(&big, vec![b'x'; 5000]).unwrap();
        fs::write(&small, b"0123456789").unwrap();
        // fake mdfind：含 CONTAINS 的是 Q2 → 输出两个文件；
        // 不含 CONTAINS 的是 Q1 → 返回空，所有候选均来自 Q2
        let script = root.join("fake-mdfind.sh");
        write_executable_script(
            &script,
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *CONTAINS*) printf '%s\\n%s\\n' '{big}' '{small}';;\n  *) ;;\nesac\n",
                big = big.display(),
                small = small.display(),
            ),
        );
        let backend = SpotlightBackend::with_resolver(resolver()).with_mdfind_path(script);
        // size: { type: "greater_than", value: 1.0, unit: "KB" }
        // SizeExpression::GreaterThan 对应 serde tag "greater_than"；SizeUnit::Kb 对应 "KB"
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search",
            "keywords": ["x"],
            "size": { "type": "greater_than", "value": 1.0, "unit": "KB" }
        }))
        .unwrap();
        let stream = block_on(backend.search(&intent, CancellationToken::new())).unwrap();
        let results: Vec<_> = block_on(stream.collect::<Vec<_>>())
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let names: Vec<_> = results.iter().map(|r| r.name.clone()).collect();
        // small.txt 应被 > 1 KB 的 size PostFilter 滤掉，只剩 big.txt
        assert_eq!(names, vec!["big.txt".to_string()]);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn run_mdfind_treats_failed_to_create_query_as_error() {
        let root = std::env::temp_dir().join(format!("scout-b15d-sentinel-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let script = root.join("fake-mdfind.sh");
        write_executable_script(
            &script,
            "#!/bin/sh\nprintf 'Failed to create query for ...\\n'\n",
        );
        let backend = SpotlightBackend::with_resolver(resolver()).with_mdfind_path(script);
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0", "intent": "file_search", "extensions": ["pdf"]
        }))
        .unwrap();
        let Err(err) = block_on(backend.search(&intent, CancellationToken::new())) else {
            panic!("expected Io error for stdout sentinel");
        };
        assert!(matches!(err, SearchError::Io { .. }));
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn mdfind_output_is_post_sorted_before_streaming() {
        use futures_util::StreamExt;

        let root =
            std::env::temp_dir().join(format!("scout-spotlight-sort-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let small = root.join("a.txt");
        let medium = root.join("b.txt");
        let large = root.join("c.txt");
        fs::write(&small, b"1").unwrap();
        fs::write(&medium, b"12").unwrap();
        fs::write(&large, b"123").unwrap();

        let script = root.join("fake-mdfind.sh");
        write_executable_script(
            &script,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' '{}' '{}' '{}'\n",
                medium.display(),
                small.display(),
                large.display()
            ),
        );
        let backend = SpotlightBackend::with_resolver(resolver()).with_mdfind_path(script);
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search",
            "sort": "size_desc"
        }))
        .unwrap();

        let stream = block_on(backend.search(&intent, CancellationToken::new())).unwrap();
        let results: Vec<_> = block_on(stream.collect::<Vec<_>>())
            .into_iter()
            .collect::<Result<_, _>>()
            .unwrap();
        let names: Vec<_> = results.iter().map(|result| result.name.as_str()).collect();

        assert_eq!(names, vec!["c.txt", "b.txt", "a.txt"]);

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_before_search_returns_empty_stream() {
        use futures_util::StreamExt;

        let root =
            std::env::temp_dir().join(format!("scout-spotlight-cancel-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let script = root.join("fake-mdfind.sh");
        write_executable_script(&script, "#!/bin/sh\nprintf '/tmp/a\\n'\n");

        let backend = SpotlightBackend::with_resolver(resolver()).with_mdfind_path(script);
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search"
        }))
        .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let stream = block_on(backend.search(&intent, cancel)).unwrap();
        let results = block_on(stream.collect::<Vec<_>>());

        assert!(results.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn singleton_group_predicates_byte_equal_to_scalar() {
        let g = KeywordGroup::singleton("工作汇报");
        assert_eq!(
            name_glob_predicate_expanded(&g),
            name_glob_predicate("工作汇报")
        );
        assert_eq!(
            content_predicate_expanded(&g),
            content_predicate("工作汇报")
        );
    }

    #[test]
    fn multi_group_predicates_or_join_all_members() {
        let g = KeywordGroup {
            head: "工作汇报".into(),
            synonyms: vec!["述职".into(), "年度总结".into()],
        };
        let name_pred = name_glob_predicate_expanded(&g);
        // 2 字段 (FSName/DisplayName) × 3 词 = 6 个 == glob 项
        assert_eq!(name_pred.matches("== \"*").count(), 6);
        let content_pred = content_predicate_expanded(&g);
        // 1 字段 (TextContent) × 3 词 = 3 个 CONTAINS[cd] 项
        assert_eq!(content_pred.matches("CONTAINS[cd]").count(), 3);
        for kw in ["工作汇报", "述职", "年度总结"] {
            assert!(name_pred.contains(kw));
            assert!(content_pred.contains(kw));
        }
    }

    #[test]
    fn multi_group_predicate_escapes_injection() {
        // synonym 含 " 字符，企图突破谓词引号边界
        let g = KeywordGroup {
            head: "x".into(),
            synonyms: vec!["a\" || (1==1) || \"".into()],
        };
        let content_pred = content_predicate_expanded(&g);
        assert!(content_pred.contains("\\\""), "双引号应被转义为 \\\"");
        assert!(!content_pred.contains("\" || (1==1) || \""));
        let name_pred = name_glob_predicate_expanded(&g);
        assert!(name_pred.contains("\\\""));
    }

    /// 2026-07-20：全局 `MatchMode` 控制多个 keyword 组之间的连接词——`All`（默认）组间
    /// `&&`，`Any` 组间 `||`，对 Q1（文件名）与 Q2（内容）两条谓词都生效。
    #[test]
    fn add_keyword_groups_to_builder_joins_by_match_mode() {
        let groups = vec![
            KeywordGroup::singleton("工作汇报"),
            KeywordGroup::singleton("季度"),
        ];

        let mut builder_all = QueryBuilder::new(None);
        add_keyword_groups_to_builder(&mut builder_all, &groups, MatchMode::All);
        let translated_all = builder_all.finish();
        // Q1（文件名）每个词组自身已带括号（跨 FSName/DisplayName OR），组间连接看得到 ") && ("。
        assert!(
            translated_all.q1.predicate.contains(") && ("),
            "All 模式 Q1 组间应为 &&: {}",
            translated_all.q1.predicate
        );
        // Q2（内容）singleton 组谓词本身不带括号，只需确认外层用 && 连接两词组、且不含 ||。
        let content_all = translated_all.q2.expect("应有 Q2").predicate;
        assert!(
            content_all.contains("工作汇报")
                && content_all.contains("季度")
                && content_all.contains(" && "),
            "All 模式 Q2 组间应为 &&: {content_all}"
        );
        assert!(
            !content_all.contains(" || "),
            "All 模式 Q2 不应含 ||: {content_all}"
        );

        let mut builder_any = QueryBuilder::new(None);
        add_keyword_groups_to_builder(&mut builder_any, &groups, MatchMode::Any);
        let translated_any = builder_any.finish();
        assert!(
            translated_any.q1.predicate.contains(") || ("),
            "Any 模式 Q1 组间应为 ||: {}",
            translated_any.q1.predicate
        );
        let content_any = translated_any.q2.expect("应有 Q2").predicate;
        assert!(
            content_any.contains("工作汇报")
                && content_any.contains("季度")
                && content_any.contains(" || "),
            "Any 模式 Q2 组间应为 ||: {content_any}"
        );
        assert!(
            !content_any.contains(" && "),
            "Any 模式 Q2 不应含 &&: {content_any}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn mdfind_timeout_is_reported() {
        let root =
            std::env::temp_dir().join(format!("scout-spotlight-timeout-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let script = root.join("slow-mdfind.sh");
        write_executable_script(&script, "#!/bin/sh\nsleep 1\n");

        let backend = SpotlightBackend::with_resolver(resolver())
            .with_mdfind_path(script)
            .with_timeout(Duration::from_millis(10));
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search"
        }))
        .unwrap();

        let Err(error) = block_on(backend.search(&intent, CancellationToken::new())) else {
            panic!("expected timeout");
        };

        assert!(matches!(error, SearchError::Timeout { .. }));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn escape_glob_pattern_escapes_metacharacters() {
        assert_eq!(escape_glob_pattern("a*b?c"), "a\\*b\\?c");
        assert_eq!(escape_glob_pattern("plain"), "plain");
        // 反斜杠先转义，避免与通配转义叠加产生歧义
        assert_eq!(escape_glob_pattern("a\\b"), "a\\\\b");
        // 双引号仍按谓词字面量转义（复用 escape_predicate_string 不丢）
        assert_eq!(escape_glob_pattern("a\"b"), "a\\\"b");
    }

    fn sample_result(
        path: &str,
        size: u64,
        modified: Option<chrono::DateTime<chrono::Utc>>,
    ) -> SearchResult {
        SearchResult {
            id: "t".into(),
            path: Path::new(path).to_path_buf(),
            name: Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            source: BackendKind::Spotlight,
            match_type: MatchType::Filename,
            score: None,
            metadata: SearchResultMetadata {
                modified_time: modified,
                created_time: None,
                accessed_time: None,
                size_bytes: Some(size),
                ..SearchResultMetadata::default()
            },
        }
    }

    #[test]
    fn extension_filter_matches_case_insensitive_and_negation() {
        let f = ExtensionFilter {
            extensions: vec!["pdf".into(), "ppt".into()],
            negate: false,
        };
        assert!(f.matches(Path::new("/x/A.PDF")));
        assert!(f.matches(Path::new("/x/b.ppt")));
        assert!(!f.matches(Path::new("/x/c.txt")));
        let neg = ExtensionFilter {
            extensions: vec!["tmp".into()],
            negate: true,
        };
        assert!(neg.matches(Path::new("/x/a.pdf")));
        assert!(!neg.matches(Path::new("/x/a.tmp")));
    }

    #[test]
    fn size_filter_matches_bytes_domain_bounds() {
        let gt = SizeFilter::from_expression(
            &SizeExpression::GreaterThan {
                value: 1.0,
                unit: SizeUnit::Mb,
            },
            UnitDomain::Bytes,
        )
        .unwrap();
        assert!(gt.matches(2_000_000));
        assert!(!gt.matches(500_000));
        assert!(!gt.matches(1_000_000)); // 严格 > ，等于不命中
        let bt = SizeFilter::from_expression(
            &SizeExpression::Between {
                min: 1.0,
                max: 2.0,
                unit: SizeUnit::Kb,
            },
            UnitDomain::Bytes,
        )
        .unwrap();
        assert!(bt.matches(1_000));
        assert!(bt.matches(2_000));
        assert!(!bt.matches(2_001));
        assert!(!bt.matches(999));
    }

    #[test]
    fn time_filter_before_after_absolute_bounds() {
        use chrono::{NaiveDate, TimeZone, Utc};
        let ymd = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).unwrap();
        let before = TimeFilter::from_expression(&TimeExpression::Before {
            value: ymd(2026, 1, 10),
        })
        .unwrap();
        let t_jan5 = Utc.with_ymd_and_hms(2026, 1, 5, 12, 0, 0).unwrap();
        let t_jan20 = Utc.with_ymd_and_hms(2026, 1, 20, 12, 0, 0).unwrap();
        assert!(before.matches(Some(t_jan5)));
        assert!(!before.matches(Some(t_jan20)));
        assert!(!before.matches(None)); // 无时间字段 → 不匹配
        let between = TimeFilter::from_expression(&TimeExpression::Absolute {
            from: ymd(2026, 1, 1),
            to: ymd(2026, 1, 31),
        })
        .unwrap();
        assert!(between.matches(Some(t_jan20)));
        assert!(!between.matches(Some(Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap())));
        // Before 上界严格：恰好落在 valueT00:00:00 不命中（对齐谓词 `<`）
        let exact = Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap();
        assert!(!before.matches(Some(exact)));
        // After 下界严格：恰好落在 valueT23:59:59 不命中（对齐谓词 `>`）
        let after = TimeFilter::from_expression(&TimeExpression::After {
            value: NaiveDate::from_ymd_opt(2026, 1, 10).unwrap(),
        })
        .unwrap();
        let after_exact = Utc.with_ymd_and_hms(2026, 1, 10, 23, 59, 59).unwrap();
        assert!(!after.matches(Some(after_exact)));
        assert!(after.matches(Some(Utc.with_ymd_and_hms(2026, 1, 11, 0, 0, 0).unwrap())));
    }

    #[test]
    fn post_filter_combines_all_constraints_against_result() {
        use chrono::{NaiveDate, TimeZone, Utc};
        let pf = PostFilter {
            extension: Some(ExtensionFilter {
                extensions: vec!["pdf".into()],
                negate: false,
            }),
            size: Some(
                SizeFilter::from_expression(
                    &SizeExpression::GreaterThan {
                        value: 1.0,
                        unit: SizeUnit::Kb,
                    },
                    UnitDomain::Bytes,
                )
                .unwrap(),
            ),
            time: vec![TimeField::Modified(
                TimeFilter::from_expression(&TimeExpression::Before {
                    value: NaiveDate::from_ymd_opt(2026, 1, 10).unwrap(),
                })
                .unwrap(),
            )],
        };
        assert!(!pf.is_empty());
        let mut r = sample_result(
            "/x/doc.pdf",
            5_000,
            Some(Utc.with_ymd_and_hms(2026, 1, 5, 0, 0, 0).unwrap()),
        );
        assert!(pf.matches(&r));
        r.path = Path::new("/x/doc.txt").to_path_buf();
        assert!(!pf.matches(&r)); // 扩展名不符 → 整体不匹配
    }

    #[test]
    fn query_builder_splits_into_q1_q2_and_postfilters() {
        let mut b = QueryBuilder::new(Some(10));
        b.and_cmp("(kMDItemFSName == \"*述职*\"cd)".into());
        b.and_str("kMDItemTextContent CONTAINS[cd] \"述职\"".into());
        b.and_cmp("(kMDItemFSName == \"*.ppt\"cd)".into());
        b.and_post_filter(PostFilter {
            extension: Some(ExtensionFilter {
                extensions: vec!["ppt".into()],
                negate: false,
            }),
            ..PostFilter::default()
        });
        let t = b.finish();
        assert_eq!(
            t.q1.predicate,
            "(kMDItemFSName == \"*述职*\"cd) && (kMDItemFSName == \"*.ppt\"cd)"
        );
        let q2 = t.q2.expect("有 str 谓词应产 Q2");
        assert_eq!(q2.predicate, "kMDItemTextContent CONTAINS[cd] \"述职\"");
        assert_eq!(t.post_filters.len(), 1);
        assert_eq!(t.q1.limit, 10);
        assert_eq!(q2.limit, 10);
    }

    #[test]
    fn query_builder_no_str_predicates_yields_no_q2() {
        let mut b = QueryBuilder::new(None);
        b.and_cmp("(kMDItemFSName == \"*.pdf\"cd)".into());
        let t = b.finish();
        assert!(t.q2.is_none());
        assert_eq!(t.q1.predicate, "(kMDItemFSName == \"*.pdf\"cd)");
    }

    #[test]
    fn query_builder_empty_yields_match_all_q1() {
        let t = QueryBuilder::new(None).finish();
        assert_eq!(t.q1.predicate, "kMDItemFSName == \"*\"");
        assert!(t.q2.is_none());
    }

    // ── BETA-15D Task 4 验收测试：Q1/Q2 拆分谓词形态 ──────────────────────────

    #[test]
    fn name_glob_predicate_globs_fsname_and_displayname() {
        assert_eq!(
            name_glob_predicate("述职"),
            "(kMDItemFSName == \"*述职*\"cd || kMDItemDisplayName == \"*述职*\"cd)"
        );
        assert_eq!(
            name_glob_predicate("a*b"),
            "(kMDItemFSName == \"*a\\*b*\"cd || kMDItemDisplayName == \"*a\\*b*\"cd)"
        );
    }

    #[test]
    fn content_predicate_uses_contains() {
        assert_eq!(
            content_predicate("述职"),
            "kMDItemTextContent CONTAINS[cd] \"述职\""
        );
    }

    #[test]
    fn file_search_keyword_plus_extension_splits_q1_q2_with_postfilter() {
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0", "intent": "file_search",
            "keywords": ["述职"], "extensions": ["ppt"]
        }))
        .unwrap();
        let t = translate_intent(&intent, &resolver()).unwrap();
        assert_eq!(
            t.q1.predicate,
            "(kMDItemFSName == \"*述职*\"cd || kMDItemDisplayName == \"*述职*\"cd) && (kMDItemFSName == \"*.ppt\"cd)"
        );
        assert_eq!(
            t.q2.unwrap().predicate,
            "kMDItemTextContent CONTAINS[cd] \"述职\""
        );
        assert_eq!(t.post_filters.len(), 1);
    }

    #[test]
    fn file_search_pure_extension_no_q2_no_postfilter() {
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0", "intent": "file_search", "extensions": ["pdf"]
        }))
        .unwrap();
        let t = translate_intent(&intent, &resolver()).unwrap();
        assert_eq!(t.q1.predicate, "(kMDItemFSName == \"*.pdf\"cd)");
        assert!(t.q2.is_none());
        assert!(t.post_filters.is_empty());
    }

    #[test]
    fn media_search_artist_to_q2_extension_to_q1_and_postfilter() {
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0", "intent": "media_search",
            "media_type": "audio", "artist": "周杰伦"
        }))
        .unwrap();
        let t = translate_intent(&intent, &resolver()).unwrap();
        // Audio → file_type Audio → 扩展名 glob 进 Q1
        assert!(t.q1.predicate.contains("kMDItemFSName == \"*.mp3\"cd"));
        // artist CONTAINS → Q2
        let q2 = t.q2.unwrap();
        assert!(q2
            .predicate
            .contains("kMDItemAuthors CONTAINS[cd] \"周杰伦\""));
        assert!(q2
            .predicate
            .contains("kMDItemMusicalGenre CONTAINS[cd] \"周杰伦\""));
        // Audio 默认扩展名集 → PostFilter
        assert!(t.post_filters.iter().any(|p| p.extension.is_some()));
    }

    #[test]
    fn media_search_duration_to_q1_cmp_no_postfilter() {
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0", "intent": "media_search", "media_type": "video",
            "duration": { "type": "greater_than", "value": 10.0, "unit": "m" }
        }))
        .unwrap();
        let t = translate_intent(&intent, &resolver()).unwrap();
        assert!(t.q1.predicate.contains("kMDItemDurationSeconds > 600"));
        assert!(t.q2.is_none()); // duration 无 Q2，是 known limitation 的根因
        assert!(t.post_filters.is_empty()); // 无 Q2 → post_filters 被 finish() 丢弃
    }
}
