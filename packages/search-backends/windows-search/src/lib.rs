//! Windows Search 后端 v0.1。
//!
//! 本 crate 将 `SearchIntent` 翻译为 Windows Search `SystemIndex` SQL，并把真实执行收敛到
//! [`WindowsSearchExecutor`]。Windows 上的默认执行器 [`PlatformWindowsSearchExecutor`] 经
//! `Search.CollatorDSO` OLE DB provider（固定 `PowerShell` + ADODB 脚本，SQL 经环境变量传入）
//! 执行查询，已在 Windows 11 真机上实测端到端跑通（MVP-11）。

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use scout_platform_windows::WindowsLocationResolver;
use scout_search_backend::{
    backend_stream_from_results, intent_sort_order, media_common_constraints,
    media_derived_file_types, relative_time_bounds, sort_results, BackendKind, BackendSearchFuture,
    BackendStream, CancellationToken, CommonConstraints, ExpandedSearchIntent, FileSearch,
    FileType, ImplementationStatus, KeywordGroup, LocationResolver, MatchMode, MatchType,
    MediaSearch, MediaType, Quality, SearchBackend, SearchError, SearchIntent, SearchResult,
    SearchResultMetadata, SizeExpression, SizeUnit, SortOrder, TimeExpression,
};
// 跨后端共用的小工具收拢在 common，后端按原名别名引入，调用点零改动。
use scout_search_backend::{
    is_path_excluded as is_excluded, result_id_for_path as result_id,
    validate_absolute_search_path as validate_search_path,
};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

/// Windows Search SQL 参数值。
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    /// 文本参数。
    Text(String),
    /// 数值参数。
    Number(f64),
    /// 相对天数偏移（相对执行时刻）。`Search.CollatorDSO` 不支持 `DATEADD`/`GETDATE`，
    /// 故翻译层只记录偏移量，由执行器在运行期解析为绝对 ISO 日期字面量。
    RelativeDay(i32),
}

/// 已翻译的 `SystemIndex` SQL 查询。
#[derive(Debug, Clone, PartialEq)]
pub struct WindowsSearchQuery {
    /// 参数化 SQL 文本。用户输入只能进入 [`WindowsSearchQuery::params`]。
    pub sql: String,
    /// SQL 参数，按 `?` 出现顺序绑定。
    pub params: Vec<SqlValue>,
    /// 结果端排除路径。
    pub exclude_paths: Vec<PathBuf>,
    /// 返回上限。
    pub limit: usize,
}

/// Windows Search 原始行。
#[derive(Debug, Clone, PartialEq)]
pub struct WindowsSearchRow {
    /// 真实文件系统路径（由 `System.ItemUrl` 还原，非本地化的 `System.ItemPathDisplay`）。
    pub path: PathBuf,
    /// 文件名（可由 `path` 派生）。
    pub name: Option<String>,
    /// 已从 `SystemIndex` 映射出的元数据。
    pub metadata: SearchResultMetadata,
}

/// Windows Search 执行器返回的 boxed future。
pub type WindowsSearchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<WindowsSearchRow>, SearchError>> + Send + 'a>>;

/// Windows Search 执行器抽象。
pub trait WindowsSearchExecutor: fmt::Debug + Send + Sync {
    /// 当前环境下是否能执行 Windows Search。
    fn is_available(&self) -> bool;

    /// 执行参数化 SQL 查询。
    fn execute<'a>(
        &'a self,
        query: &'a WindowsSearchQuery,
        cancel: CancellationToken,
    ) -> WindowsSearchFuture<'a>;
}

/// 平台默认 Windows Search 执行器。
#[derive(Debug, Clone, Copy)]
pub struct PlatformWindowsSearchExecutor;

#[cfg(target_os = "windows")]
impl WindowsSearchExecutor for PlatformWindowsSearchExecutor {
    fn is_available(&self) -> bool {
        true
    }

    fn execute<'a>(
        &'a self,
        query: &'a WindowsSearchQuery,
        cancel: CancellationToken,
    ) -> WindowsSearchFuture<'a> {
        // 经 `Search.CollatorDSO` OLE DB provider 执行参数化 SQL。该 provider 不支持
        // 参数标记 `?`，故先把已转义的 `params` 内联进 SQL（见 [`inline_params`]），再交
        // 由固定 PowerShell + ADODB 脚本执行（SQL 经环境变量传入，脚本本身不插值用户数据）。
        Box::pin(async move { run_windows_search(query, &cancel) })
    }
}

/// Windows Search 执行的默认超时（含 `PowerShell` + COM 预热开销）。
#[cfg(target_os = "windows")]
const DEFAULT_EXEC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// 把参数化 SQL 中的 `?` 占位符替换为已转义的字面量。
///
/// 文本参数用单引号包裹并将内部 `'` 加倍（SQL 字符串字面量转义）；数值参数按整数形式
/// 输出（本后端的数值均为整数语义：相对天数、字节阈值等）。用户输入中的 `LIKE` 通配符
/// 在翻译层已由 [`escape_like_pattern`] 处理，本函数只负责字符串字面量边界安全。
#[cfg(target_os = "windows")]
fn inline_params(sql: &str, params: &[SqlValue]) -> Result<String, SearchError> {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(sql.len() + params.len() * 8);
    let mut iter = params.iter();
    for ch in sql.chars() {
        if ch == '?' {
            let value = iter.next().ok_or_else(|| SearchError::InvalidIntent {
                detail: "SQL placeholder count exceeds parameter count".to_owned(),
            })?;
            match value {
                SqlValue::Text(text) => {
                    out.push('\'');
                    out.push_str(&text.replace('\'', "''"));
                    out.push('\'');
                }
                SqlValue::Number(num) => {
                    let _ = write!(out, "{num:.0}");
                }
                SqlValue::RelativeDay(days) => {
                    // 相对偏移在执行时刻解析为绝对本地时间 ISO 字面量，复刻原
                    // `DATEADD('day', n, GETDATE())` 语义。已实测 provider 接受
                    // `'YYYY-MM-DDTHH:MM:SS'` 形态。本地时区锚点带亚天级偏差（与
                    // Spotlight 后端 BETA-15D 同类已知限制）。
                    let moment = chrono::Local::now() + chrono::Duration::days(i64::from(*days));
                    out.push('\'');
                    out.push_str(&moment.format("%Y-%m-%dT%H:%M:%S").to_string());
                    out.push('\'');
                }
            }
        } else {
            out.push(ch);
        }
    }
    if iter.next().is_some() {
        return Err(SearchError::InvalidIntent {
            detail: "more parameters than SQL placeholders".to_owned(),
        });
    }
    Ok(out)
}

/// 固定 `PowerShell` 脚本：打开 `Search.CollatorDSO` 连接，执行 `$env:SCOUT_WS_SQL`，
/// 逐行输出 `System.ItemPathDisplay`（UTF-8），至多 `$env:SCOUT_WS_LIMIT` 行。
#[cfg(target_os = "windows")]
const WINDOWS_SEARCH_PS_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$sql = $env:SCOUT_WS_SQL
$limit = [int]$env:SCOUT_WS_LIMIT
$conn = New-Object -ComObject ADODB.Connection
try {
    $conn.Open("Provider=Search.CollatorDSO;Extended Properties='Application=Windows';")
    $rs = $conn.Execute($sql)
    $count = 0
    while (-not $rs.EOF -and $count -lt $limit) {
        [Console]::Out.WriteLine($rs.Fields.Item('System.ItemUrl').Value)
        $count++
        $rs.MoveNext()
    }
    $rs.Close()
} finally {
    if ($conn.State -ne 0) { $conn.Close() }
}
"#;

/// 同步执行 Windows Search 查询（在 `async` 块内被阻塞调用，与 Spotlight 后端同构）。
#[cfg(target_os = "windows")]
fn run_windows_search(
    query: &WindowsSearchQuery,
    cancel: &CancellationToken,
) -> Result<Vec<WindowsSearchRow>, SearchError> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    if cancel.is_cancelled() {
        return Ok(Vec::new());
    }

    let sql = inline_params(&query.sql, &query.params)?;

    let mut command = Command::new("powershell.exe");
    command
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-Command")
        .arg(WINDOWS_SEARCH_PS_SCRIPT)
        .env("SCOUT_WS_SQL", &sql)
        .env("SCOUT_WS_LIMIT", query.limit.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // CREATE_NO_WINDOW：避免每次搜索 spawn powershell.exe 时闪现控制台黑框
    // （GUI app 调用控制台子进程的典型副作用）。
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command
        .spawn()
        .map_err(|error| SearchError::BackendUnavailable {
            reason: format!("failed to spawn powershell.exe: {error}"),
        })?;
    let start = Instant::now();

    loop {
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Vec::new());
        }

        if child
            .try_wait()
            .map_err(|error| SearchError::Io {
                detail: error.to_string(),
            })?
            .is_some()
        {
            let output = child.wait_with_output().map_err(|error| SearchError::Io {
                detail: error.to_string(),
            })?;
            if !output.status.success() {
                return Err(SearchError::Io {
                    detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                });
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            let rows = stdout
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .filter_map(path_from_item_url)
                .map(row_from_path)
                .collect();
            return Ok(rows);
        }

        if start.elapsed() >= DEFAULT_EXEC_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SearchError::Timeout {
                elapsed_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
            });
        }

        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 把 `System.ItemUrl` 还原为真实文件路径。
///
/// 形如 `file:C:/Users/.../name.pdf`（正斜杠、实测未百分号编码）。非 `file:` 项
/// （邮件 `mapi:` 等）返回 `None` 被跳过——本后端只处理文件系统条目。
#[cfg(target_os = "windows")]
fn path_from_item_url(item_url: &str) -> Option<PathBuf> {
    let rest = item_url.strip_prefix("file:")?;
    Some(PathBuf::from(rest.replace('/', "\\")))
}

/// 由路径构造 [`WindowsSearchRow`]，并用文件系统元数据补全大小 / 时间（供排序）。
#[cfg(target_os = "windows")]
fn row_from_path(path: PathBuf) -> WindowsSearchRow {
    let metadata = std::fs::metadata(&path).ok();
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned);

    WindowsSearchRow {
        path,
        name,
        metadata: metadata.map_or_else(SearchResultMetadata::default, |metadata| {
            SearchResultMetadata {
                modified_time: metadata.modified().ok().map(chrono::DateTime::from),
                created_time: metadata.created().ok().map(chrono::DateTime::from),
                accessed_time: metadata.accessed().ok().map(chrono::DateTime::from),
                size_bytes: Some(metadata.len()),
                ..SearchResultMetadata::default()
            }
        }),
    }
}

#[cfg(not(target_os = "windows"))]
impl WindowsSearchExecutor for PlatformWindowsSearchExecutor {
    fn is_available(&self) -> bool {
        false
    }

    fn execute<'a>(
        &'a self,
        _query: &WindowsSearchQuery,
        _cancel: CancellationToken,
    ) -> WindowsSearchFuture<'a> {
        Box::pin(async {
            Err(SearchError::BackendUnavailable {
                reason: "Windows Search is only available on Windows".to_owned(),
            })
        })
    }
}

/// Windows Search `SystemIndex` 后端。
#[derive(Debug)]
pub struct WindowsSearchBackend<E = PlatformWindowsSearchExecutor, R = WindowsLocationResolver> {
    executor: E,
    resolver: R,
}

impl WindowsSearchBackend<PlatformWindowsSearchExecutor, WindowsLocationResolver> {
    /// 创建默认 Windows Search 后端。
    pub fn new() -> Result<Self, SearchError> {
        let resolver =
            WindowsLocationResolver::new().map_err(|error| SearchError::BackendUnavailable {
                reason: error.to_string(),
            })?;

        Ok(Self::with_executor_and_resolver(
            PlatformWindowsSearchExecutor,
            resolver,
        ))
    }
}

impl<E, R> WindowsSearchBackend<E, R> {
    /// 使用指定 executor 与 resolver 创建后端，便于测试注入。
    #[must_use]
    pub const fn with_executor_and_resolver(executor: E, resolver: R) -> Self {
        Self { executor, resolver }
    }
}

impl<E, R> SearchBackend for WindowsSearchBackend<E, R>
where
    E: WindowsSearchExecutor,
    R: LocationResolver,
{
    fn kind(&self) -> BackendKind {
        BackendKind::WindowsSearch
    }

    fn implementation_status(&self) -> ImplementationStatus {
        ImplementationStatus::Real
    }

    fn is_available(&self) -> bool {
        self.executor.is_available()
    }

    fn search<'a>(
        &'a self,
        intent: &'a SearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        Box::pin(async move {
            let query = translate_intent(intent, &self.resolver)?;
            let rows = self.executor.execute(&query, cancel.clone()).await?;
            Ok(rows_to_stream(
                rows,
                &query,
                intent_sort_order(intent),
                cancel,
            ))
        })
    }

    /// 同义词扩展搜索（BETA-15C）。每个 keyword 组的同义词 OR 展开到
    /// 文件名 + 内容字段，组间 AND；其余约束（扩展名 / 时间 / 大小 / 路径 / 媒体元数据）
    /// 与 `search` 一致。支持「搜工作汇报 → 命中 述职 / 工作总结」这类意图理解。
    fn search_expanded<'a>(
        &'a self,
        expanded: &'a ExpandedSearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        Box::pin(async move {
            let query = translate_intent_expanded(expanded, &self.resolver)?;
            let rows = self.executor.execute(&query, cancel.clone()).await?;
            Ok(rows_to_stream(
                rows,
                &query,
                intent_sort_order(&expanded.base),
                cancel,
            ))
        })
    }
}

/// 把执行器返回的行过滤排除路径、排序、截断 limit 后构造结果流。
/// 由 `search` 与 `search_expanded` 共用。
fn rows_to_stream(
    rows: Vec<WindowsSearchRow>,
    query: &WindowsSearchQuery,
    sort: Option<SortOrder>,
    cancel: CancellationToken,
) -> BackendStream {
    let mut results = Vec::new();
    for row in rows {
        if cancel.is_cancelled() {
            break;
        }
        if is_excluded(&row.path, &query.exclude_paths) {
            continue;
        }
        results.push(result_from_row(row));
    }
    sort_results(&mut results, sort);
    results.truncate(query.limit);
    backend_stream_from_results(results, cancel)
}

/// 把 `SearchIntent` 翻译为 Windows Search SQL。
pub fn translate_intent<R>(
    intent: &SearchIntent,
    resolver: &R,
) -> Result<WindowsSearchQuery, SearchError>
where
    R: LocationResolver,
{
    match intent {
        SearchIntent::FileSearch(search) => translate_file_search(search, resolver),
        SearchIntent::MediaSearch(search) => translate_media_search(search, resolver),
        SearchIntent::Refine(_) | SearchIntent::FileAction(_) | SearchIntent::Clarify(_) => {
            Err(SearchError::UnsupportedIntent {
                detail: "WindowsSearchBackend only accepts merged file_search/media_search intents"
                    .to_owned(),
            })
        }
    }
}

fn translate_file_search<R>(
    search: &FileSearch,
    resolver: &R,
) -> Result<WindowsSearchQuery, SearchError>
where
    R: LocationResolver,
{
    let mut builder = SqlBuilder::new(search.limit);
    add_common_constraints(
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
    Ok(builder.finish(search.sort))
}

fn translate_media_search<R>(
    search: &MediaSearch,
    resolver: &R,
) -> Result<WindowsSearchQuery, SearchError>
where
    R: LocationResolver,
{
    let mut builder = SqlBuilder::new(search.limit);
    let file_types = media_derived_file_types(search);
    add_common_constraints(
        &mut builder,
        resolver,
        media_common_constraints(search, search.keywords.as_deref(), file_types.as_deref()),
    )?;
    add_media_constraints(&mut builder, resolver, search)?;
    Ok(builder.finish(search.sort))
}

/// 计算 `media_type` 派生的兜底 `file_type`（显式 `file_type` 优先；BETA-18 多值）。
/// 返回 owned `Vec`，调用方须以 `let` 绑定保活后 `as_deref()` 传入 [`CommonConstraints`]。
/// 媒体专属约束（截屏目录 hint + artist/title/album/genre/quality/duration）。
/// `translate_media_search` 与其 expanded 版共用，保证语义一致。
fn add_media_constraints<R>(
    builder: &mut SqlBuilder,
    resolver: &R,
    search: &MediaSearch,
) -> Result<(), SearchError>
where
    R: LocationResolver,
{
    if search.media_type == MediaType::Screenshot && search.location.is_none() {
        add_resolved_hint(builder, resolver, "截屏")?;
    }
    if let Some(artist) = search.artist.as_deref() {
        builder.text_like_any(
            &["System.Music.Artist", "System.Author"],
            &contains_pattern(artist),
        );
    }
    if let Some(title) = search.title.as_deref() {
        builder.text_like_any(
            &["System.Title", "System.ItemNameDisplay"],
            &contains_pattern(title),
        );
    }
    if let Some(album) = search.album.as_deref() {
        builder.text_like("System.Music.AlbumTitle", contains_pattern(album));
    }
    if let Some(genre) = search.genre.as_deref() {
        builder.text_like("System.Music.Genre", contains_pattern(genre));
    }
    if search.quality == Some(Quality::Lossless) && search.extensions.is_none() {
        builder.extension_filter(&["flac", "wav", "aiff", "ape"], false);
    }
    if let Some(duration) = search.duration.as_ref() {
        builder.size_filter("System.Media.Duration", duration, UnitDomain::Duration)?;
    }
    Ok(())
}

/// 把同义词扩展后的意图翻译为 Windows Search SQL（BETA-15C）。
pub fn translate_intent_expanded<R>(
    expanded: &ExpandedSearchIntent,
    resolver: &R,
) -> Result<WindowsSearchQuery, SearchError>
where
    R: LocationResolver,
{
    match &expanded.base {
        SearchIntent::FileSearch(search) => translate_file_search_expanded(
            search,
            &expanded.keyword_groups,
            expanded.match_mode,
            resolver,
        ),
        SearchIntent::MediaSearch(search) => translate_media_search_expanded(
            search,
            &expanded.keyword_groups,
            expanded.match_mode,
            resolver,
        ),
        SearchIntent::Refine(_) | SearchIntent::FileAction(_) | SearchIntent::Clarify(_) => {
            Err(SearchError::UnsupportedIntent {
                detail: "WindowsSearchBackend only accepts merged file_search/media_search intents"
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
) -> Result<WindowsSearchQuery, SearchError>
where
    R: LocationResolver,
{
    let mut builder = SqlBuilder::new(search.limit);
    add_keyword_groups(&mut builder, groups, match_mode);
    add_common_constraints(
        &mut builder,
        resolver,
        CommonConstraints {
            keywords: None, // 已由 groups 处理
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
    Ok(builder.finish(search.sort))
}

fn translate_media_search_expanded<R>(
    search: &MediaSearch,
    groups: &[KeywordGroup],
    match_mode: MatchMode,
    resolver: &R,
) -> Result<WindowsSearchQuery, SearchError>
where
    R: LocationResolver,
{
    let mut builder = SqlBuilder::new(search.limit);
    add_keyword_groups(&mut builder, groups, match_mode);
    let file_types = media_derived_file_types(search);
    add_common_constraints(
        &mut builder,
        resolver,
        media_common_constraints(search, None, file_types.as_deref()),
    )?;
    add_media_constraints(&mut builder, resolver, search)?;
    Ok(builder.finish(search.sort))
}

/// 每个 keyword 组：组内同义词 × 各字段 OR 成一个谓词块；组间按全局 `match_mode` 取
/// AND（[`MatchMode::All`]，全部复合条件命中）或 OR（[`MatchMode::Any`]，任一条件命中）。
/// 多组的组间连接词合并为**单个**谓词块推给 `SqlBuilder`，与其余约束（扩展名 / 时间 /
/// 大小 / 路径）始终保持 AND——只有关键词组之间的语义受 `match_mode` 影响。
fn add_keyword_groups(builder: &mut SqlBuilder, groups: &[KeywordGroup], match_mode: MatchMode) {
    let mut group_predicates: Vec<String> = Vec::new();
    for group in groups.iter().filter(|group| !group.head.is_empty()) {
        if let Some(predicate) = builder.keyword_group_predicate(
            &[
                "System.ItemNameDisplay",
                "System.FileName",
                "System.Search.Contents",
            ],
            &group.all(),
        ) {
            group_predicates.push(predicate);
        }
    }
    if group_predicates.is_empty() {
        return;
    }
    let combined = if group_predicates.len() == 1 {
        group_predicates.into_iter().next().unwrap_or_default()
    } else {
        let joiner = match match_mode {
            MatchMode::All => " AND ",
            MatchMode::Any => " OR ",
        };
        format!("({})", group_predicates.join(joiner))
    };
    builder.push(combined);
}

fn add_common_constraints<R>(
    builder: &mut SqlBuilder,
    resolver: &R,
    constraints: CommonConstraints<'_>,
) -> Result<(), SearchError>
where
    R: LocationResolver,
{
    if let Some(keywords) = constraints.keywords {
        for keyword in keywords.iter().filter(|keyword| !keyword.is_empty()) {
            builder.text_like_any(
                &[
                    "System.ItemNameDisplay",
                    "System.FileName",
                    "System.Search.Contents",
                ],
                &contains_pattern(keyword),
            );
        }
    }

    if let Some(extensions) = constraints.extensions {
        if !extensions.is_empty() {
            builder.extension_filter(extensions, false);
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
            builder.extension_filter(&exts, false);
        }
    }

    if let Some(time) = constraints.modified_time {
        builder.time_filter("System.DateModified", time);
    }
    if let Some(time) = constraints.created_time {
        builder.time_filter("System.DateCreated", time);
    }
    if let Some(time) = constraints.accessed_time {
        builder.time_filter("System.DateAccessed", time);
    }
    if let Some(size) = constraints.size {
        builder.size_filter("System.Size", size, UnitDomain::Bytes)?;
    }

    if let Some(location) = constraints.location {
        if let Some(hint) = location.hint.as_deref() {
            add_resolved_hint(builder, resolver, hint)?;
        }
        if let Some(includes) = location.include.as_ref() {
            for path in includes {
                builder.path_under(validate_search_path(path)?);
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
            builder.extension_filter(exclude_extensions, true);
        }
    }
    if let Some(exclude_file_type) = constraints.exclude_file_type {
        for file_type in exclude_file_type {
            builder.extension_filter(file_type_extensions(*file_type), true);
        }
    }

    Ok(())
}

fn add_resolved_hint<R>(
    builder: &mut SqlBuilder,
    resolver: &R,
    hint: &str,
) -> Result<(), SearchError>
where
    R: LocationResolver,
{
    for path in resolver
        .resolve_hint(hint)
        .map_err(|error| SearchError::UnsupportedIntent {
            detail: error.to_string(),
        })?
    {
        builder.path_under(path);
    }
    Ok(())
}

#[derive(Debug)]
struct SqlBuilder {
    predicates: Vec<String>,
    params: Vec<SqlValue>,
    exclude_paths: Vec<PathBuf>,
    limit: usize,
}

impl SqlBuilder {
    fn new(limit: Option<u32>) -> Self {
        let limit = limit
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(DEFAULT_LIMIT)
            .min(MAX_LIMIT);
        Self {
            predicates: Vec::new(),
            params: Vec::new(),
            exclude_paths: Vec::new(),
            limit,
        }
    }

    fn finish(self, sort: Option<SortOrder>) -> WindowsSearchQuery {
        let where_clause = if self.predicates.is_empty() {
            "System.ItemPathDisplay IS NOT NULL".to_owned()
        } else {
            format!(
                "System.ItemPathDisplay IS NOT NULL AND {}",
                self.predicates.join(" AND ")
            )
        };
        let order_by = order_by_clause(sort);
        // SELECT System.ItemUrl：`System.ItemPathDisplay` 返回的是本地化显示路径
        // （如 `C:\用户\alice\下载\...`），无法用于文件系统访问 / 文件操作；
        // `System.ItemUrl`（`file:C:/Users/...`）携带真实路径，由执行器还原。
        let sql = format!("SELECT System.ItemUrl FROM SYSTEMINDEX WHERE {where_clause}{order_by}");
        WindowsSearchQuery {
            sql,
            params: self.params,
            exclude_paths: self.exclude_paths,
            limit: self.limit,
        }
    }

    fn push(&mut self, predicate: String) {
        self.predicates.push(predicate);
    }

    fn text_like(&mut self, field: &'static str, value: String) {
        self.push(format!("{field} LIKE ?"));
        self.params.push(SqlValue::Text(value));
    }

    fn text_like_any(&mut self, fields: &[&'static str], value: &str) {
        let predicates = fields
            .iter()
            .map(|field| {
                self.params.push(SqlValue::Text(value.to_owned()));
                format!("{field} LIKE ?")
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        self.push(format!("({predicates})"));
    }

    /// 一个同义词组 → 组内所有词 × 所有字段 OR 成单个谓词块字符串（不直接 push，交调用方
    /// 按全局 `match_mode` 决定组间怎么连接）。参数按词序追加，与最终拼回的谓词文本中
    /// `?` 占位符顺序一致。每个词经 [`contains_pattern`] 做 `%...%` 包裹与 `LIKE` 通配符转义。
    fn keyword_group_predicate(
        &mut self,
        fields: &[&'static str],
        terms: &[&str],
    ) -> Option<String> {
        let mut predicates = Vec::with_capacity(terms.len() * fields.len());
        for &term in terms {
            let pattern = contains_pattern(term);
            for field in fields {
                self.params.push(SqlValue::Text(pattern.clone()));
                predicates.push(format!("{field} LIKE ?"));
            }
        }
        if predicates.is_empty() {
            None
        } else {
            Some(format!("({})", predicates.join(" OR ")))
        }
    }

    fn extension_filter<S>(&mut self, extensions: &[S], negate: bool)
    where
        S: AsRef<str>,
    {
        let operator = if negate { "<>" } else { "=" };
        let joiner = if negate { " AND " } else { " OR " };
        let predicates = extensions
            .iter()
            .filter_map(|extension| normalized_extension(extension.as_ref()))
            .map(|extension| {
                self.params.push(SqlValue::Text(extension));
                format!("System.FileExtension {operator} ?")
            })
            .collect::<Vec<_>>()
            .join(joiner);
        if !predicates.is_empty() {
            self.push(format!("({predicates})"));
        }
    }

    fn path_under<P>(&mut self, path: P)
    where
        P: Into<PathBuf>,
    {
        let path = path.into();
        // 用 Windows Search 的 `SCOPE` 谓词按真实路径递归限定目录。不能用
        // `System.ItemPathDisplay LIKE`——它是本地化显示路径（如 `文档`/`下载`），
        // 对真实路径前缀匹配不到（真机实测：位于已索引目录下仍返回 0 行）。
        self.push("SCOPE = ?".to_owned());
        self.params
            .push(SqlValue::Text(format!("file:{}", path.to_string_lossy())));
    }

    fn time_filter(&mut self, field: &'static str, time: &TimeExpression) {
        match time {
            TimeExpression::Relative { value } => {
                let (from, to) = relative_time_bounds(*value);
                self.push(format!("{field} >= ?"));
                self.params.push(SqlValue::RelativeDay(from));
                self.push(format!("{field} < ?"));
                self.params.push(SqlValue::RelativeDay(to));
            }
            TimeExpression::Absolute { from, to } => {
                self.push(format!("{field} >= ?"));
                self.params.push(SqlValue::Text(format!("{from}T00:00:00")));
                self.push(format!("{field} <= ?"));
                self.params.push(SqlValue::Text(format!("{to}T23:59:59")));
            }
            TimeExpression::Before { value } => {
                self.push(format!("{field} < ?"));
                self.params
                    .push(SqlValue::Text(format!("{value}T00:00:00")));
            }
            TimeExpression::After { value } => {
                self.push(format!("{field} > ?"));
                self.params
                    .push(SqlValue::Text(format!("{value}T23:59:59")));
            }
        }
    }

    fn size_filter(
        &mut self,
        field: &'static str,
        size: &SizeExpression,
        domain: UnitDomain,
    ) -> Result<(), SearchError> {
        match size {
            SizeExpression::GreaterThan { value, unit } => {
                self.push(format!("{field} > ?"));
                self.params
                    .push(SqlValue::Number(unit_value(*value, *unit, domain)?));
            }
            SizeExpression::LessThan { value, unit } => {
                self.push(format!("{field} < ?"));
                self.params
                    .push(SqlValue::Number(unit_value(*value, *unit, domain)?));
            }
            SizeExpression::Between { min, max, unit } => {
                self.push(format!("{field} >= ?"));
                self.params
                    .push(SqlValue::Number(unit_value(*min, *unit, domain)?));
                self.push(format!("{field} <= ?"));
                self.params
                    .push(SqlValue::Number(unit_value(*max, *unit, domain)?));
            }
        }
        Ok(())
    }
}

fn order_by_clause(sort: Option<SortOrder>) -> &'static str {
    match sort.unwrap_or(SortOrder::RelevanceDesc) {
        SortOrder::ModifiedDesc => " ORDER BY System.DateModified DESC",
        SortOrder::ModifiedAsc => " ORDER BY System.DateModified ASC",
        SortOrder::CreatedDesc => " ORDER BY System.DateCreated DESC",
        SortOrder::CreatedAsc => " ORDER BY System.DateCreated ASC",
        SortOrder::AccessedDesc => " ORDER BY System.DateAccessed DESC",
        SortOrder::SizeDesc => " ORDER BY System.Size DESC",
        SortOrder::SizeAsc => " ORDER BY System.Size ASC",
        SortOrder::NameAsc => " ORDER BY System.ItemNameDisplay ASC",
        SortOrder::NameDesc => " ORDER BY System.ItemNameDisplay DESC",
        SortOrder::RelevanceDesc => "",
    }
}

fn contains_pattern(value: &str) -> String {
    format!("%{}%", escape_like_pattern(value))
}

fn normalized_extension(extension: &str) -> Option<String> {
    let normalized = extension
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    (!normalized.is_empty()).then_some(format!(".{normalized}"))
}

/// 转义 SQL `LIKE` 模式中的通配符。
#[must_use]
pub fn escape_like_pattern(value: &str) -> String {
    value
        .replace('[', "[[]")
        .replace('%', "[%]")
        .replace('_', "[_]")
}

/// 按**扩展名**全盘查找文件（设置页「自动发现本机 gguf 模型」用，2026-08 加、
/// 与 `scout-native-index` crate 的 `find_files_by_extension` 同一用途、跨后端对齐 UX）。
/// `ext` 传裸扩展名（如 `gguf`，不含点）。**已知限制**：`SystemIndex` 只覆盖系统
/// 「索引选项」纳入的目录（通常是用户库文件夹），不像内置原生索引 / Spotlight 那样
/// 默认全盘覆盖——常用目录之外（如另一块盘、临时下载目录）的文件扫不到，调用方
/// 应告知用户这一点而非当作与内置原生索引等价的全盘扫描。执行失败 / 非 Windows →
/// 返回空（调用方按"未发现"降级，不报错）。
#[cfg(target_os = "windows")]
#[must_use]
pub fn find_files_by_extension(ext: &str, limit: usize) -> Vec<PathBuf> {
    let normalized = normalized_extension(ext).unwrap_or_else(|| format!(".{ext}"));
    let query = WindowsSearchQuery {
        sql: "SELECT System.ItemUrl FROM SYSTEMINDEX WHERE System.FileExtension = ?".to_owned(),
        params: vec![SqlValue::Text(normalized)],
        exclude_paths: Vec::new(),
        limit,
    };
    let cancel = CancellationToken::new();
    match run_windows_search(&query, &cancel) {
        Ok(rows) => rows.into_iter().map(|row| row.path).collect(),
        Err(_) => Vec::new(),
    }
}

/// 非 Windows 平台：`SystemIndex` 不可用，恒返回空。
#[cfg(not(target_os = "windows"))]
#[must_use]
pub fn find_files_by_extension(_ext: &str, _limit: usize) -> Vec<PathBuf> {
    Vec::new()
}

#[derive(Debug, Clone, Copy)]
enum UnitDomain {
    Bytes,
    Duration,
}

fn unit_value(value: f64, unit: SizeUnit, domain: UnitDomain) -> Result<f64, SearchError> {
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
    Ok(normalized.round())
}

fn file_type_extensions(file_type: FileType) -> &'static [&'static str] {
    // 单一信源在 common，避免三后端各持一份重复表（BETA-19 收拢）。
    scout_search_backend::extensions_for_file_type(file_type)
}

fn result_from_row(row: WindowsSearchRow) -> SearchResult {
    let name = row
        .name
        .or_else(|| {
            row.path
                .file_name()
                .and_then(|name| name.to_str())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_default();

    SearchResult {
        id: result_id(&row.path),
        path: row.path,
        name,
        source: BackendKind::WindowsSearch,
        match_type: MatchType::Filename,
        score: None,
        metadata: row.metadata,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use serde::Deserialize;

    use super::*;
    use futures_util::StreamExt;

    #[derive(Debug, Deserialize)]
    struct Case {
        id: String,
        variant: String,
        intent: serde_json::Value,
    }

    #[derive(Debug, Clone)]
    struct MockResolver;

    impl LocationResolver for MockResolver {
        fn resolve_hint(
            &self,
            hint: &str,
        ) -> Result<Vec<PathBuf>, scout_search_backend::LocationResolveError> {
            let path = match hint {
                "下载" | "downloads" => "/Users/tester/Downloads",
                "桌面" | "desktop" => "/Users/tester/Desktop",
                "文稿" | "documents" => "/Users/tester/Documents",
                "截屏" | "screenshots" => "/Users/tester/Pictures/Screenshots",
                _ => "/Users/tester",
            };
            Ok(vec![PathBuf::from(path)])
        }
    }

    #[derive(Debug, Clone)]
    struct MockExecutor {
        rows: Vec<WindowsSearchRow>,
    }

    impl WindowsSearchExecutor for MockExecutor {
        fn is_available(&self) -> bool {
            true
        }

        fn execute<'a>(
            &'a self,
            _query: &WindowsSearchQuery,
            _cancel: CancellationToken,
        ) -> WindowsSearchFuture<'a> {
            Box::pin(async move { Ok(self.rows.clone()) })
        }
    }

    use futures_executor::block_on;

    fn fixture_cases() -> Vec<Case> {
        serde_json::from_str(include_str!("../../common/tests/fixtures/cases.json")).unwrap()
    }

    #[test]
    fn translates_schema_search_cases_1_to_30_to_parameterized_sql() {
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
            let query = translate_intent(&intent, &MockResolver)
                .unwrap_or_else(|error| panic!("case {} translation failed: {error}", case.id));
            assert!(
                query.sql.starts_with("SELECT System.ItemUrl"),
                "case {} produced unexpected SQL: {}",
                case.id,
                query.sql
            );
            assert!(
                query.sql.contains("FROM SYSTEMINDEX WHERE"),
                "case {} should target SystemIndex",
                case.id
            );
            assert!(
                !query.sql.contains("预算")
                    && !query.sql.contains("周华健")
                    && !query.sql.contains("Eric Clapton")
                    && !query.sql.contains("budget"),
                "case {} leaked user input into SQL: {}",
                case.id,
                query.sql
            );
        }
    }

    #[test]
    fn keeps_malicious_keyword_in_params_not_sql() {
        let malicious = r"预算'; DROP TABLE SYSTEMINDEX; -- % _ [";
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search",
            "keywords": [malicious]
        }))
        .unwrap();

        let query = translate_intent(&intent, &MockResolver).unwrap();

        assert!(!query.sql.contains("DROP TABLE"));
        assert!(!query.sql.contains(malicious));
        assert!(query.params.iter().any(|param| matches!(
            param,
            SqlValue::Text(value) if value.contains("DROP TABLE")
                && value.contains("[%]")
                && value.contains("[_]")
                && value.contains("[[]")
        )));
    }

    #[test]
    fn rejects_location_paths_with_newline_or_null_byte() {
        assert!(validate_search_path("/tmp/a\nb").is_err());
        assert!(validate_search_path("/tmp/a\0b").is_err());
    }

    #[test]
    fn mock_executor_drives_search_results() {
        let backend = WindowsSearchBackend::with_executor_and_resolver(
            MockExecutor {
                rows: vec![WindowsSearchRow {
                    path: PathBuf::from("/Users/tester/report.pdf"),
                    name: Some("report.pdf".to_owned()),
                    metadata: SearchResultMetadata::default(),
                }],
            },
            MockResolver,
        );
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search",
            "extensions": ["pdf"]
        }))
        .unwrap();

        let stream = block_on(backend.search(&intent, CancellationToken::new())).unwrap();
        let results: Vec<_> = block_on(stream.collect::<Vec<_>>())
            .into_iter()
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source, BackendKind::WindowsSearch);
        assert_eq!(results[0].name, "report.pdf");
    }

    #[test]
    fn mock_executor_results_are_post_sorted() {
        let backend = WindowsSearchBackend::with_executor_and_resolver(
            MockExecutor {
                rows: vec![
                    WindowsSearchRow {
                        path: PathBuf::from("/Users/tester/b.txt"),
                        name: Some("b.txt".to_owned()),
                        metadata: SearchResultMetadata {
                            size_bytes: Some(20),
                            ..SearchResultMetadata::default()
                        },
                    },
                    WindowsSearchRow {
                        path: PathBuf::from("/Users/tester/a.txt"),
                        name: Some("a.txt".to_owned()),
                        metadata: SearchResultMetadata {
                            size_bytes: Some(10),
                            ..SearchResultMetadata::default()
                        },
                    },
                    WindowsSearchRow {
                        path: PathBuf::from("/Users/tester/c.txt"),
                        name: Some("c.txt".to_owned()),
                        metadata: SearchResultMetadata {
                            size_bytes: Some(30),
                            ..SearchResultMetadata::default()
                        },
                    },
                ],
            },
            MockResolver,
        );
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

        let error = translate_intent(&intent, &MockResolver).unwrap_err();

        assert!(matches!(error, SearchError::UnsupportedIntent { .. }));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn inline_params_escapes_text_and_formats_numbers() {
        let sql = "WHERE name LIKE ? AND size > ?";
        let params = vec![
            SqlValue::Text("o'brien%".to_owned()),
            SqlValue::Number(100_000.0),
        ];
        let inlined = inline_params(sql, &params).unwrap();
        // 单引号加倍；数值无小数点；用户值不以裸文本泄漏。
        assert_eq!(inlined, "WHERE name LIKE 'o''brien%' AND size > 100000");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn inline_params_relative_day_becomes_quoted_iso_literal() {
        let inlined = inline_params("dm >= ?", &[SqlValue::RelativeDay(-7)]).unwrap();
        // 形如 dm >= 'YYYY-MM-DDTHH:MM:SS'
        assert!(inlined.starts_with("dm >= '"));
        assert!(inlined.ends_with('\''));
        assert!(inlined.contains('T'));
        assert_eq!(inlined.matches('\'').count(), 2);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn inline_params_rejects_placeholder_param_count_mismatch() {
        assert!(inline_params("a = ?", &[]).is_err());
        assert!(inline_params("a = 1", &[SqlValue::Number(1.0)]).is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn path_from_item_url_restores_real_path_and_skips_non_file() {
        assert_eq!(
            path_from_item_url("file:C:/Users/alice/Downloads/报告.pdf"),
            Some(PathBuf::from(r"C:\Users\alice\Downloads\报告.pdf"))
        );
        assert_eq!(path_from_item_url("mapi:0123456789ABCDEF"), None);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn location_include_scopes_via_scope_predicate_not_localized_display_path() {
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search",
            "extensions": ["pdf"],
            "location": { "include": ["C:\\corpus"] }
        }))
        .unwrap();

        let query = translate_intent(&intent, &MockResolver).unwrap();

        assert!(
            query.sql.contains("SCOPE = ?"),
            "location.include 应经 SCOPE 谓词限定: {}",
            query.sql
        );
        assert!(
            !query.sql.contains("ItemPathDisplay LIKE"),
            "不得用本地化的 ItemPathDisplay 做路径限定: {}",
            query.sql
        );
        assert!(query
            .params
            .iter()
            .any(|param| matches!(param, SqlValue::Text(value) if value == "file:C:\\corpus")));
    }

    #[test]
    fn expanded_keyword_group_ors_all_synonyms() {
        let base: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search",
            "keywords": ["工作汇报"]
        }))
        .unwrap();
        let expanded = scout_search_backend::ExpandedSearchIntent {
            base,
            keyword_groups: vec![KeywordGroup {
                head: "工作汇报".to_owned(),
                synonyms: vec!["述职".to_owned(), "工作总结".to_owned()],
            }],
            match_mode: MatchMode::default(),
        };

        let query = translate_intent_expanded(&expanded, &MockResolver).unwrap();

        // 组内三个同义词都应作为 LIKE 参数出现（%...% 包裹）。
        for term in ["工作汇报", "述职", "工作总结"] {
            assert!(
                query
                    .params
                    .iter()
                    .any(|param| matches!(param, SqlValue::Text(value) if value.contains(term))),
                "扩展查询缺同义词 {term}: {:?}",
                query.params
            );
        }
        // 组内 OR 连接。
        assert!(query.sql.contains(" OR "), "应含组内 OR: {}", query.sql);
        // 用户词不泄漏进 SQL 文本（仍走参数化）。
        assert!(
            !query.sql.contains("述职"),
            "同义词不应进 SQL 文本: {}",
            query.sql
        );
    }

    /// 2026-07-20：全局 `MatchMode` 控制多个 keyword 组之间的连接词——`All` 组间 AND
    /// （默认，全部复合条件命中），`Any` 组间 OR（任一条件命中）。其余约束（此处的
    /// `extensions`）应始终与关键词组保持 AND，不受 `match_mode` 影响。
    #[test]
    fn expanded_multi_group_joiner_follows_match_mode() {
        let base: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search",
            "keywords": ["工作汇报", "季度"],
            "extensions": ["pdf"]
        }))
        .unwrap();
        let groups = vec![
            KeywordGroup::singleton("工作汇报"),
            KeywordGroup::singleton("季度"),
        ];

        let expanded_all = scout_search_backend::ExpandedSearchIntent {
            base: base.clone(),
            keyword_groups: groups.clone(),
            match_mode: MatchMode::All,
        };
        let query_all = translate_intent_expanded(&expanded_all, &MockResolver).unwrap();
        assert!(
            query_all.sql.contains(") AND ("),
            "All 模式组间应为 AND: {}",
            query_all.sql
        );

        let expanded_any = scout_search_backend::ExpandedSearchIntent {
            base,
            keyword_groups: groups,
            match_mode: MatchMode::Any,
        };
        let query_any = translate_intent_expanded(&expanded_any, &MockResolver).unwrap();
        assert!(
            query_any.sql.contains(") OR ("),
            "Any 模式组间应为 OR: {}",
            query_any.sql
        );
        // extensions 约束不受 match_mode 影响，始终与关键词组块整体 AND。
        assert!(
            query_any.sql.contains(") AND (System.FileExtension = ?)"),
            "扩展名约束应与关键词组块 AND 连接（不随 match_mode 变 OR）: {}",
            query_any.sql
        );
    }

    #[test]
    fn expanded_singleton_group_matches_plain_keyword_translation() {
        let base: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search",
            "keywords": ["报告"]
        }))
        .unwrap();
        let expanded = scout_search_backend::ExpandedSearchIntent {
            base: base.clone(),
            keyword_groups: vec![KeywordGroup::singleton("报告")],
            match_mode: MatchMode::default(),
        };

        let plain = translate_intent(&base, &MockResolver).unwrap();
        let via_expanded = translate_intent_expanded(&expanded, &MockResolver).unwrap();

        // 未扩词时，expanded 路径与普通翻译产出等价 SQL + 参数。
        assert_eq!(plain.sql, via_expanded.sql);
        assert_eq!(plain.params, via_expanded.params);
    }
}
