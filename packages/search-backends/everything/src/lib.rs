//! Everything 后端 v0.1。
//!
//! 本 crate 优先通过 voidtools `es.exe` CLI 执行搜索，并用 [`EverythingExecutor`] 隔离真实
//! 进程调用。Windows 上的默认执行器 [`EsCliExecutor`] 经 [`run_es_cli`] spawn `es.exe`（结构化
//! 参数、不过 shell），结果经 `-export-txt -utf8-bom` 导出为 UTF-8 文件再读回（规避 stdout
//! 代码页对 CJK 文件名的破坏），支持取消 / 超时。已在 Windows 11 真机上端到端实测（MVP-12）。
//! [`SearchBackend::search_expanded`] 覆盖同义词扩展：组内同义词在文件名层面 `|` OR 展开、
//! 组间 AND（BETA-15C）。执行层逻辑与已实测的 Spotlight / Windows Search 后端同构。

use std::fmt;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use scout_platform_windows::WindowsLocationResolver;
use scout_search_backend::{
    backend_stream_from_results, intent_sort_order, media_common_constraints,
    media_derived_file_types, sort_results, BackendKind, BackendSearchFuture, BackendStream,
    CancellationToken, CommonConstraints, ExpandedSearchIntent, FileSearch, FileType,
    ImplementationStatus, KeywordGroup, LocationResolver, MatchMode, MatchType, MediaSearch,
    MediaType, Quality, RelativeTime, SearchBackend, SearchError, SearchIntent, SearchResult,
    SearchResultMetadata, SizeExpression, SizeUnit, SortOrder, TimeExpression,
};
// 跨后端共用的小工具收拢在 common，后端按原名别名引入，调用点零改动。
use scout_search_backend::{
    is_path_excluded as is_excluded, result_id_for_path as result_id,
    validate_absolute_search_path as validate_search_path,
};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

/// 已构造的 `es.exe` 命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EverythingCommand {
    /// `es.exe` 可执行文件路径。
    pub program: PathBuf,
    /// 结构化参数列表。不会经过 shell。
    pub args: Vec<String>,
    /// 结果端排除路径。
    pub exclude_paths: Vec<PathBuf>,
    /// 返回上限。
    pub limit: usize,
}

/// Everything 执行器返回的 boxed future。
pub type EverythingFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, SearchError>> + Send + 'a>>;

/// Everything 执行器抽象。
pub trait EverythingExecutor: fmt::Debug + Send + Sync {
    /// 当前环境下 `es.exe` 是否存在且可调用。
    fn is_available(&self, program: &Path) -> bool;

    /// 执行 `es.exe` 并返回 stdout 文本。
    fn execute<'a>(
        &'a self,
        command: &'a EverythingCommand,
        cancel: CancellationToken,
    ) -> EverythingFuture<'a>;
}

/// 平台默认 `es.exe` 执行器。
#[derive(Debug, Clone, Copy)]
pub struct EsCliExecutor;

#[cfg(target_os = "windows")]
impl EverythingExecutor for EsCliExecutor {
    fn is_available(&self, program: &Path) -> bool {
        executable_exists(program)
    }

    fn execute<'a>(
        &'a self,
        command: &'a EverythingCommand,
        cancel: CancellationToken,
    ) -> EverythingFuture<'a> {
        // 在 `async` 块内阻塞调用同步进程执行（与 Spotlight / Windows Search 后端同构）。
        Box::pin(async move { run_es_cli(command, &cancel) })
    }
}

/// Everything `es.exe` 执行的默认超时。
#[cfg(target_os = "windows")]
const DEFAULT_EXEC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// 同步执行 `es.exe`，返回结果路径文本（每行一条）；支持取消与超时（kill 子进程）。
///
/// **编码**：`es.exe` 的 **stdout 按控制台代码页输出**（中文 Windows 为 GBK/936），CJK 文件名会乱码——
/// 真机实测 `from_utf8_lossy(stdout)` 把「述职报告」毁成 `�����`。改用 `-export-txt <tmp> -utf8-bom`
/// 让 es 写出**带 BOM 的 UTF-8 文件**（实测正确编码 CJK），再读回去 BOM 即得干净 UTF-8。
/// 文件搜索是中文用户的主场景，故此处必须走 export 路径而非 stdout。
#[cfg(target_os = "windows")]
fn run_es_cli(
    command: &EverythingCommand,
    cancel: &CancellationToken,
) -> Result<String, SearchError> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    if cancel.is_cancelled() {
        return Ok(String::new());
    }

    // es 把结果导出到独立临时文件（UTF-8 + BOM），规避 stdout 代码页乱码。
    // 退出/出错/取消都要清理临时文件：guard 在作用域结束时尽力删除。
    let export_path = unique_export_path();
    let _guard = TempFileGuard(export_path.clone());

    let mut cmd = Command::new(&command.program);
    cmd.args(&command.args)
        .arg("-export-txt")
        .arg(&export_path)
        .arg("-utf8-bom")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // CREATE_NO_WINDOW：避免每次搜索 spawn es.exe 时闪现控制台黑框。
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|error| SearchError::BackendUnavailable {
            reason: format!("failed to spawn {}: {error}", command.program.display()),
        })?;
    let start = Instant::now();

    loop {
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(String::new());
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
            if output.status.success() {
                return Ok(read_export_file(&export_path));
            }
            return Err(SearchError::Io {
                detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
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

/// es 导出临时文件的 RAII 清理 guard：作用域结束时尽力删除（成功/出错/取消/超时均覆盖）。
#[cfg(target_os = "windows")]
struct TempFileGuard(PathBuf);

#[cfg(target_os = "windows")]
impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 生成进程内唯一的导出临时文件路径（进程 id + 单调计数器，避免并发查询互相覆盖）。
#[cfg(target_os = "windows")]
fn unique_export_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("scout-es-{}-{}.txt", std::process::id(), seq))
}

/// 读取 es 导出的 UTF-8(+BOM) 文件，剥除 BOM 返回纯文本。
/// 0 结果时 es 可能不创建文件——文件缺失视为空结果。
#[cfg(target_os = "windows")]
fn read_export_file(path: &Path) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    let without_bom = bytes
        .strip_prefix(&[0xEF, 0xBB, 0xBF])
        .unwrap_or(bytes.as_slice());
    String::from_utf8_lossy(without_bom).into_owned()
}

#[cfg(not(target_os = "windows"))]
impl EverythingExecutor for EsCliExecutor {
    fn is_available(&self, _program: &Path) -> bool {
        false
    }

    fn execute<'a>(
        &'a self,
        _command: &EverythingCommand,
        _cancel: CancellationToken,
    ) -> EverythingFuture<'a> {
        Box::pin(async {
            Err(SearchError::BackendUnavailable {
                reason: "Everything ES CLI is only enabled on Windows".to_owned(),
            })
        })
    }
}

/// Everything `es.exe` 搜索后端。
#[derive(Debug)]
pub struct EverythingBackend<E = EsCliExecutor, R = WindowsLocationResolver> {
    es_path: PathBuf,
    executor: E,
    resolver: R,
}

impl EverythingBackend<EsCliExecutor, WindowsLocationResolver> {
    /// 创建默认 Everything 后端。
    pub fn new() -> Result<Self, SearchError> {
        let resolver =
            WindowsLocationResolver::new().map_err(|error| SearchError::BackendUnavailable {
                reason: error.to_string(),
            })?;

        Ok(Self::with_executor_and_resolver(
            EsCliExecutor,
            resolver,
            resolve_es_path(),
        ))
    }
}

impl<E, R> EverythingBackend<E, R> {
    /// 使用指定 executor、resolver 与 `es.exe` 路径创建后端。
    #[must_use]
    pub const fn with_executor_and_resolver(executor: E, resolver: R, es_path: PathBuf) -> Self {
        Self {
            es_path,
            executor,
            resolver,
        }
    }
}

impl<E, R> SearchBackend for EverythingBackend<E, R>
where
    E: EverythingExecutor,
    R: LocationResolver,
{
    fn kind(&self) -> BackendKind {
        BackendKind::Everything
    }

    fn implementation_status(&self) -> ImplementationStatus {
        #[cfg(target_os = "windows")]
        {
            if self.is_available() {
                ImplementationStatus::Real
            } else {
                ImplementationStatus::Stub
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            ImplementationStatus::Stub
        }
    }

    fn is_available(&self) -> bool {
        self.executor.is_available(&self.es_path)
    }

    fn search<'a>(
        &'a self,
        intent: &'a SearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        Box::pin(async move {
            let command = translate_intent(intent, &self.resolver, &self.es_path)?;
            let output = self.executor.execute(&command, cancel.clone()).await?;
            Ok(output_to_stream(
                &output,
                &command,
                intent_sort_order(intent),
                cancel,
            ))
        })
    }

    /// 同义词扩展搜索（BETA-15C）。每个 keyword 组的同义词在文件名层面用 `|` OR 展开
    /// （`<head|syn1|syn2>`），组间靠 `es.exe` 默认的空格 AND；其余约束（扩展名 / 时间 /
    /// 大小 / 路径 / 媒体元数据）与 `search` 一致。Everything 是纯文件名引擎，故扩展只作用于
    /// 文件名，不引入内容字段（与能力路由「内容查询走 Windows Search」分工一致）。
    fn search_expanded<'a>(
        &'a self,
        expanded: &'a ExpandedSearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        Box::pin(async move {
            let command = translate_intent_expanded(expanded, &self.resolver, &self.es_path)?;
            let output = self.executor.execute(&command, cancel.clone()).await?;
            Ok(output_to_stream(
                &output,
                &command,
                intent_sort_order(&expanded.base),
                cancel,
            ))
        })
    }
}

/// 把 `es.exe` 的 stdout 文本过滤排除路径、排序、截断 limit 后构造结果流。
/// 由 `search` 与 `search_expanded` 共用。
fn output_to_stream(
    output: &str,
    command: &EverythingCommand,
    sort: Option<SortOrder>,
    cancel: CancellationToken,
) -> BackendStream {
    let mut results = Vec::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        if cancel.is_cancelled() {
            break;
        }
        let path = PathBuf::from(line.trim());
        if is_excluded(&path, &command.exclude_paths) {
            continue;
        }
        results.push(result_from_path(path));
    }
    sort_results(&mut results, sort);
    results.truncate(command.limit);
    backend_stream_from_results(results, cancel)
}

/// 把 `SearchIntent` 翻译为 `es.exe` 结构化参数。
pub fn translate_intent<R>(
    intent: &SearchIntent,
    resolver: &R,
    es_path: &Path,
) -> Result<EverythingCommand, SearchError>
where
    R: LocationResolver,
{
    match intent {
        SearchIntent::FileSearch(search) => translate_file_search(search, resolver, es_path),
        SearchIntent::MediaSearch(search) => translate_media_search(search, resolver, es_path),
        SearchIntent::Refine(_) | SearchIntent::FileAction(_) | SearchIntent::Clarify(_) => {
            Err(SearchError::UnsupportedIntent {
                detail: "EverythingBackend only accepts merged file_search/media_search intents"
                    .to_owned(),
            })
        }
    }
}

fn translate_file_search<R>(
    search: &FileSearch,
    resolver: &R,
    es_path: &Path,
) -> Result<EverythingCommand, SearchError>
where
    R: LocationResolver,
{
    let mut builder = CommandBuilder::new(es_path.to_path_buf(), search.limit);
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
    es_path: &Path,
) -> Result<EverythingCommand, SearchError>
where
    R: LocationResolver,
{
    let mut builder = CommandBuilder::new(es_path.to_path_buf(), search.limit);
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
    builder: &mut CommandBuilder,
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
        builder.term(artist);
    }
    if let Some(title) = search.title.as_deref() {
        builder.term(title);
    }
    if let Some(album) = search.album.as_deref() {
        builder.term(album);
    }
    if let Some(genre) = search.genre.as_deref() {
        builder.term(genre);
    }
    if search.quality == Some(Quality::Lossless) && search.extensions.is_none() {
        builder.extension_filter(&["flac", "wav", "aiff", "ape"], false);
    }
    if let Some(duration) = search.duration.as_ref() {
        builder.size_filter("length", duration, UnitDomain::Duration)?;
    }
    Ok(())
}

/// 把同义词扩展后的意图翻译为 `es.exe` 结构化参数（BETA-15C）。
pub fn translate_intent_expanded<R>(
    expanded: &ExpandedSearchIntent,
    resolver: &R,
    es_path: &Path,
) -> Result<EverythingCommand, SearchError>
where
    R: LocationResolver,
{
    match &expanded.base {
        SearchIntent::FileSearch(search) => translate_file_search_expanded(
            search,
            &expanded.keyword_groups,
            expanded.match_mode,
            resolver,
            es_path,
        ),
        SearchIntent::MediaSearch(search) => translate_media_search_expanded(
            search,
            &expanded.keyword_groups,
            expanded.match_mode,
            resolver,
            es_path,
        ),
        SearchIntent::Refine(_) | SearchIntent::FileAction(_) | SearchIntent::Clarify(_) => {
            Err(SearchError::UnsupportedIntent {
                detail: "EverythingBackend only accepts merged file_search/media_search intents"
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
    es_path: &Path,
) -> Result<EverythingCommand, SearchError>
where
    R: LocationResolver,
{
    let mut builder = CommandBuilder::new(es_path.to_path_buf(), search.limit);
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
    es_path: &Path,
) -> Result<EverythingCommand, SearchError>
where
    R: LocationResolver,
{
    let mut builder = CommandBuilder::new(es_path.to_path_buf(), search.limit);
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

/// 每个 keyword 组按全局 `match_mode` 译成 `es.exe` term：
/// - [`MatchMode::All`]（默认，全部复合条件命中）：组内同义词 `|` OR 展开成单个 term，
///   组间靠默认空格 AND（与旧行为一致）。singleton 组退化为裸词，与 `search` 路径的
///   `term(head)` byte-equal。
/// - [`MatchMode::Any`]（任一条件命中）：所有词组的全部词项摊平进**单个** `<...>` OR
///   term——`es.exe` 多个独立参数默认空格 AND，要表达「组间 OR」必须合并成一个 term。
fn add_keyword_groups(
    builder: &mut CommandBuilder,
    groups: &[KeywordGroup],
    match_mode: MatchMode,
) {
    match match_mode {
        MatchMode::All => {
            for group in groups.iter().filter(|group| !group.head.is_empty()) {
                builder.keyword_group_term(&group.all());
            }
        }
        MatchMode::Any => builder.keyword_groups_term_any(groups),
    }
}

fn add_common_constraints<R>(
    builder: &mut CommandBuilder,
    resolver: &R,
    constraints: CommonConstraints<'_>,
) -> Result<(), SearchError>
where
    R: LocationResolver,
{
    if let Some(keywords) = constraints.keywords {
        for keyword in keywords.iter().filter(|keyword| !keyword.is_empty()) {
            builder.term(keyword);
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
        builder.time_filter("dm", time);
    }
    if let Some(time) = constraints.created_time {
        builder.time_filter("dc", time);
    }
    if let Some(time) = constraints.accessed_time {
        builder.time_filter("da", time);
    }
    if let Some(size) = constraints.size {
        builder.size_filter("size", size, UnitDomain::Bytes)?;
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
    builder: &mut CommandBuilder,
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
struct CommandBuilder {
    program: PathBuf,
    args: Vec<String>,
    exclude_paths: Vec<PathBuf>,
    limit: usize,
}

impl CommandBuilder {
    fn new(program: PathBuf, limit: Option<u32>) -> Self {
        let limit = limit
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(DEFAULT_LIMIT)
            .min(MAX_LIMIT);
        Self {
            program,
            // `-n <limit>` 限制结果数。`es.exe` 默认即输出完整路径，无需额外标志；
            // 早期误加的 `-path` 会被当成「在某路径下搜索」从而吞掉真正的搜索项
            // （真机实测 `es -n N -path ext:pdf` 返回 0 结果）。
            args: vec!["-n".to_owned(), limit.to_string()],
            exclude_paths: Vec::new(),
            limit,
        }
    }

    fn finish(mut self, sort: Option<SortOrder>) -> EverythingCommand {
        if let Some(sort_arg) = sort_arg(sort) {
            self.args.push("-sort".to_owned());
            self.args.push(sort_arg.to_owned());
        }
        EverythingCommand {
            program: self.program,
            args: self.args,
            exclude_paths: self.exclude_paths,
            limit: self.limit,
        }
    }

    fn term(&mut self, value: &str) {
        if !value.contains('\0') && !value.contains('\n') {
            self.args.push(value.to_owned());
        }
    }

    /// 一个同义词组：组内所有词用 `|` OR 连成单个 term。多词时用 `<...>` 分组隔离，
    /// 避免与组间空格 AND 混淆 `|` 优先级（实测 `a|b ext:pdf` 会被解析为
    /// `a OR (b AND ext:pdf)`，`<a|b> ext:pdf` 才是 `(a OR b) AND ext:pdf`）。
    /// 单词组退化为裸词，与 `term` byte-equal（保持 singleton 不变量）。
    /// 含 `\0` / `\n` 的词被跳过；全部被跳过则不产出 term。
    fn keyword_group_term(&mut self, terms: &[&str]) {
        let safe: Vec<&str> = terms
            .iter()
            .copied()
            .filter(|term| !term.contains('\0') && !term.contains('\n'))
            .collect();
        match safe.as_slice() {
            [] => {}
            [single] => self.args.push((*single).to_owned()),
            many => self.args.push(format!("<{}>", many.join("|"))),
        }
    }

    /// [`MatchMode::Any`] 用：把所有词组的全部词项（不分组边界）摊平进单个 `<...>` OR
    /// term——`es.exe` 无「组间 OR」原生语法，多个独立参数默认空格 AND，故需合并成一个
    /// term 才能表达「任一条件命中」。单个有效词项退化为裸词（与 `term` byte-equal）。
    fn keyword_groups_term_any(&mut self, groups: &[KeywordGroup]) {
        let all_terms: Vec<&str> = groups
            .iter()
            .filter(|group| !group.head.is_empty())
            .flat_map(KeywordGroup::all)
            .filter(|term| !term.contains('\0') && !term.contains('\n'))
            .collect();
        match all_terms.as_slice() {
            [] => {}
            [single] => self.args.push((*single).to_owned()),
            many => self.args.push(format!("<{}>", many.join("|"))),
        }
    }

    fn extension_filter<S>(&mut self, extensions: &[S], negate: bool)
    where
        S: AsRef<str>,
    {
        // es.exe 多个独立 `ext:` 参数是**空格 AND**（实测 `ext:ppt ext:pdf` 命中 0，
        // 因无文件同时是两种扩展名）；正确的「任一扩展名」是分号列表 `ext:ppt;pdf`（OR）。
        // 故把所有扩展名合并为单个 `ext:a;b;c` term。修复多扩展名（用户多类型 / file_type
        // 展开为多扩展名）此前被 AND 致空结果的 bug（BETA-18 真机暴露）。
        let exts: Vec<String> = extensions
            .iter()
            .filter_map(|extension| normalized_extension(extension.as_ref()))
            .collect();
        if exts.is_empty() {
            return;
        }
        let joined = exts.join(";");
        if negate {
            // `!ext:a;b` = NOT(ext∈{a,b})，排除两者并集，与逐个 `!ext:` AND 语义等价。
            self.args.push(format!("!ext:{joined}"));
        } else {
            self.args.push(format!("ext:{joined}"));
        }
    }

    /// 把目录约束为「递归限定在该目录及其所有子目录下」。
    ///
    /// Everything 的 `parent:` / `infolder:` 只匹配**直接子项**（非递归）——真机实测
    /// （MVP-26）：对含子目录的 `location.include` 或解析后的目录 hint（如「下载里的 pdf」，
    /// 文件落在 `Downloads\某子目录\`）会**漏掉子目录中的全部文件**，与 Windows Search
    /// `SCOPE` / Spotlight 的递归语义不一致。
    ///
    /// 改为把目录路径作为**全路径子串** term 匹配（`es.exe` 对含 `:`/`\` 的裸 term 按路径
    /// 子串处理）——递归覆盖所有层级；并在末尾补一个路径分隔符作为**右边界**，避免
    /// `…\Downloads` 误匹配兄弟目录 `…\Downloads2`（真机实测裸路径子串会 leak 到兄弟目录，
    /// 加尾分隔符后不再 leak）。term 含空格时由 `es.exe` 作为单参数保留，不被空格 AND 拆分。
    fn path_under<P>(&mut self, path: P)
    where
        P: Into<PathBuf>,
    {
        let mut scope = path.into().to_string_lossy().into_owned();
        if !scope.ends_with(std::path::MAIN_SEPARATOR) {
            scope.push(std::path::MAIN_SEPARATOR);
        }
        self.term(&scope);
    }

    fn time_filter(&mut self, field: &'static str, time: &TimeExpression) {
        match time {
            TimeExpression::Relative { value } => self
                .args
                .push(format!("{field}:{}", relative_time_filter(*value))),
            TimeExpression::Absolute { from, to } => {
                self.args.push(format!("{field}:{from}..{to}"));
            }
            TimeExpression::Before { value } => self.args.push(format!("{field}:<{value}")),
            TimeExpression::After { value } => self.args.push(format!("{field}:>{value}")),
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
                self.args
                    .push(format!("{field}:>{}", unit_value(*value, *unit, domain)?));
            }
            SizeExpression::LessThan { value, unit } => {
                self.args
                    .push(format!("{field}:<{}", unit_value(*value, *unit, domain)?));
            }
            SizeExpression::Between { min, max, unit } => self.args.push(format!(
                "{field}:{}..{}",
                unit_value(*min, *unit, domain)?,
                unit_value(*max, *unit, domain)?
            )),
        }
        Ok(())
    }
}

fn sort_arg(sort: Option<SortOrder>) -> Option<&'static str> {
    match sort {
        Some(SortOrder::ModifiedDesc) => Some("dm-descending"),
        Some(SortOrder::ModifiedAsc) => Some("dm-ascending"),
        Some(SortOrder::CreatedDesc) => Some("dc-descending"),
        Some(SortOrder::CreatedAsc) => Some("dc-ascending"),
        Some(SortOrder::AccessedDesc) => Some("da-descending"),
        Some(SortOrder::SizeDesc) => Some("size-descending"),
        Some(SortOrder::SizeAsc) => Some("size-ascending"),
        Some(SortOrder::NameAsc) => Some("name-ascending"),
        Some(SortOrder::NameDesc) => Some("name-descending"),
        Some(SortOrder::RelevanceDesc) | None => None,
    }
}

fn relative_time_filter(value: RelativeTime) -> &'static str {
    match value {
        RelativeTime::Today => "today",
        RelativeTime::Yesterday => "yesterday",
        RelativeTime::Last3Days => "last3days",
        RelativeTime::Last7Days | RelativeTime::ThisWeek => "last7days",
        RelativeTime::Last14Days => "last14days",
        RelativeTime::Last30Days | RelativeTime::ThisMonth => "last30days",
        RelativeTime::LastWeek => "lastweek",
        RelativeTime::LastMonth => "lastmonth",
        RelativeTime::ThisYear => "thisyear",
        RelativeTime::LastYear => "lastyear",
    }
}

fn normalized_extension(extension: &str) -> Option<String> {
    let normalized = extension
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    (!normalized.is_empty()).then_some(normalized)
}

#[derive(Debug, Clone, Copy)]
enum UnitDomain {
    Bytes,
    Duration,
}

fn unit_value(value: f64, unit: SizeUnit, domain: UnitDomain) -> Result<String, SearchError> {
    let suffix = match (domain, unit) {
        (UnitDomain::Bytes, SizeUnit::B) => "b",
        (UnitDomain::Bytes, SizeUnit::Kb) => "kb",
        (UnitDomain::Bytes, SizeUnit::Mb) => "mb",
        (UnitDomain::Bytes, SizeUnit::Gb) => "gb",
        (UnitDomain::Duration, SizeUnit::Sec) => "s",
        (UnitDomain::Duration, SizeUnit::Min) => "m",
        (UnitDomain::Duration, SizeUnit::Hour) => "h",
        _ => {
            return Err(SearchError::InvalidIntent {
                detail: format!("unit {unit:?} is not valid for {domain:?}"),
            });
        }
    };
    if !value.is_finite() || value < 0.0 {
        return Err(SearchError::InvalidIntent {
            detail: format!("invalid numeric value: {value}"),
        });
    }
    Ok(format!("{value:.0}{suffix}"))
}

fn file_type_extensions(file_type: FileType) -> &'static [&'static str] {
    // 单一信源在 common，避免三后端各持一份重复表（BETA-19 收拢）。
    scout_search_backend::extensions_for_file_type(file_type)
}

fn result_from_path(path: PathBuf) -> SearchResult {
    let metadata = fs::metadata(&path).ok();
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();

    SearchResult {
        id: result_id(&path),
        path,
        name,
        source: BackendKind::Everything,
        match_type: MatchType::Filename,
        score: None,
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

fn executable_exists(path: &Path) -> bool {
    if path.components().count() > 1 {
        return path.is_file();
    }

    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join(path).is_file())
    })
}

/// es.exe 候选路径，按优先级：PATH（裸名 `es.exe`）→ winget 安装目录 → Everything 默认安装目录。
/// `es.exe`（Everything CLI）独立于 Everything 主程序，常经 winget 包 `voidtools.Everything.Cli`
/// 安装到 `%LOCALAPPDATA%` 下、不进 PATH。env 由参数注入以便跨平台测试；`None` 则跳过对应候选。
fn es_path_candidates(localappdata: Option<&Path>, program_files: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::from("es.exe")];
    if let Some(local) = localappdata {
        candidates.push(
            local
                .join("Microsoft")
                .join("WinGet")
                .join("Packages")
                .join("voidtools.Everything.Cli_Microsoft.Winget.Source_8wekyb3d8bbwe")
                .join("es.exe"),
        );
    }
    if let Some(pf) = program_files {
        candidates.push(pf.join("Everything").join("es.exe"));
    }
    candidates
}

/// 解析实际可用的 es.exe 路径：遍历 [`es_path_candidates`] 取第一个 [`executable_exists`] 者；
/// 都不存在则回退裸名 `es.exe`（与原实现一致——`is_available` 据此报 false）。读取 `LOCALAPPDATA`
/// / `ProgramFiles` 环境变量构造候选（非 Windows 通常无这些变量 → 仅裸名）。
fn resolve_es_path() -> PathBuf {
    let localappdata = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from);
    let candidates = es_path_candidates(localappdata.as_deref(), program_files.as_deref());
    candidates
        .into_iter()
        .find(|p| executable_exists(p))
        .unwrap_or_else(|| PathBuf::from("es.exe"))
}

/// 2026-07-06（cycle 9 真机反馈·模型本地发现）：按**完整文件名**全盘查找文件。
///
/// 复用本 crate 的 es.exe 两段式定位（PATH 裸名 → winget Links / `ProgramFiles` 兜底）与
/// UTF-8 导出解码（`-export-txt -utf8-bom`，规避 stdout 代码页毁 CJK 路径）。`wfn:`
/// （wholefilename）精确整名匹配、大小写不敏感。es.exe 不可用 / 执行失败 / 非 Windows
/// → 返回空（调用方按"未发现"降级，不报错）。
///
/// 给桌面端「本地已有 .gguf 模型发现」用；不进 [`SearchBackend`] trait 面。
#[cfg(target_os = "windows")]
#[must_use]
pub fn find_files_named(filename: &str, limit: usize) -> Vec<PathBuf> {
    let program = resolve_es_path();
    if !executable_exists(&program) {
        return Vec::new();
    }
    let command = EverythingCommand {
        program,
        args: vec![
            "-n".to_owned(),
            limit.to_string(),
            format!("wfn:{filename}"),
        ],
        exclude_paths: Vec::new(),
        limit,
    };
    let cancel = CancellationToken::new();
    match run_es_cli(&command, &cancel) {
        Ok(text) => text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// 非 Windows 平台：Everything 不可用，恒返回空。
#[cfg(not(target_os = "windows"))]
#[must_use]
pub fn find_files_named(_filename: &str, _limit: usize) -> Vec<PathBuf> {
    Vec::new()
}

/// 2026-07-07（设置页自动发现模型）：按**扩展名**全盘查找文件（`ext:<ext>`）。
///
/// 与 [`find_files_named`] 同款 es.exe 定位 + UTF-8 导出解码；用于「自动发现本机 gguf
/// 模型」——列出候选交用户选用为语义 / 生成模型路径覆盖（不加载、不复制）。`ext`
/// 传裸扩展名（如 `gguf`，不含点）。es.exe 不可用 / 失败 / 非 Windows → 返回空。
#[cfg(target_os = "windows")]
#[must_use]
pub fn find_files_by_extension(ext: &str, limit: usize) -> Vec<PathBuf> {
    let program = resolve_es_path();
    if !executable_exists(&program) {
        return Vec::new();
    }
    let command = EverythingCommand {
        program,
        args: vec!["-n".to_owned(), limit.to_string(), format!("ext:{ext}")],
        exclude_paths: Vec::new(),
        limit,
    };
    let cancel = CancellationToken::new();
    match run_es_cli(&command, &cancel) {
        Ok(text) => text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// 非 Windows 平台：Everything 不可用，恒返回空。
#[cfg(not(target_os = "windows"))]
#[must_use]
pub fn find_files_by_extension(_ext: &str, _limit: usize) -> Vec<PathBuf> {
    Vec::new()
}

/// es.exe 是否可用（两段式定位命中任一候选）。调用方据此区分
/// [`find_files_named`] 返回空是"没找到"还是"Everything 不可用"。
#[cfg(target_os = "windows")]
#[must_use]
pub fn es_cli_available() -> bool {
    executable_exists(&resolve_es_path())
}

/// 非 Windows 平台：恒 false。
#[cfg(not(target_os = "windows"))]
#[must_use]
pub const fn es_cli_available() -> bool {
    false
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
        available: bool,
        stdout: String,
    }

    impl EverythingExecutor for MockExecutor {
        fn is_available(&self, _program: &Path) -> bool {
            self.available
        }

        fn execute<'a>(
            &'a self,
            _command: &EverythingCommand,
            _cancel: CancellationToken,
        ) -> EverythingFuture<'a> {
            Box::pin(async move { Ok(self.stdout.clone()) })
        }
    }

    use futures_executor::block_on;

    fn fixture_cases() -> Vec<Case> {
        serde_json::from_str(include_str!("../../common/tests/fixtures/cases.json")).unwrap()
    }

    #[test]
    fn translates_schema_search_cases_1_to_30_to_structured_args() {
        let cases = fixture_cases();
        let es_path = PathBuf::from("es.exe");

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
            let command = translate_intent(&intent, &MockResolver, &es_path)
                .unwrap_or_else(|error| panic!("case {} translation failed: {error}", case.id));
            assert_eq!(command.program, es_path);
            assert_eq!(command.args[0], "-n");
            assert!(
                command
                    .args
                    .iter()
                    .all(|arg| !arg.contains('\n') && !arg.contains('\0')),
                "case {} has invalid command arg: {:?}",
                case.id,
                command.args
            );
        }
    }

    #[test]
    fn malicious_keyword_is_single_argument_not_shell_script() {
        let malicious = r#"预算"; rm -rf "%USERPROFILE%""#;
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search",
            "keywords": [malicious]
        }))
        .unwrap();

        let command = translate_intent(&intent, &MockResolver, Path::new("es.exe")).unwrap();

        assert!(command.args.iter().any(|arg| arg == malicious));
        assert!(!command.args.iter().any(|arg| arg == "rm" || arg == "-rf"));
    }

    #[test]
    fn location_include_is_recursive_path_scope_not_nonrecursive_parent() {
        // MVP-26 真机回归守护：location.include 必须递归限定（覆盖子目录），
        // 不能再用 Everything 非递归的 parent:/infolder:。
        let dir = std::env::temp_dir().join("scout-scope-probe");
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search",
            "extensions": ["pdf"],
            "location": { "include": [dir.to_string_lossy()] }
        }))
        .unwrap();

        let command = translate_intent(&intent, &MockResolver, Path::new("es.exe")).unwrap();

        // 不再使用非递归谓词。
        assert!(
            command
                .args
                .iter()
                .all(|arg| !arg.starts_with("parent:") && !arg.starts_with("infolder:")),
            "应避免非递归 parent:/infolder: 谓词: {:?}",
            command.args
        );
        // 改为「全路径子串 + 尾分隔符」作为独立的递归 scope term。
        let expected = {
            let mut s = dir.to_string_lossy().into_owned();
            if !s.ends_with(std::path::MAIN_SEPARATOR) {
                s.push(std::path::MAIN_SEPARATOR);
            }
            s
        };
        assert!(
            command.args.iter().any(|arg| arg == &expected),
            "应有递归路径 scope term {expected:?}: {:?}",
            command.args
        );
    }

    #[test]
    fn cross_category_file_type_emits_single_semicolon_ext_term() {
        // BETA-18：多 file_type → 扩展名并集应为**单个**分号列表 `ext:a;b;...`（OR）；
        // 多个独立 `ext:` 是空格 AND（命中 0，真机暴露）。
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search",
            "file_type": ["presentation", "document"]
        }))
        .unwrap();
        let command = translate_intent(&intent, &MockResolver, Path::new("es.exe")).unwrap();
        let ext_args: Vec<&String> = command
            .args
            .iter()
            .filter(|arg| arg.starts_with("ext:"))
            .collect();
        assert_eq!(
            ext_args.len(),
            1,
            "应只有一个 ext: term（分号 OR），实得 {:?}",
            command.args
        );
        let term = ext_args[0];
        assert!(term.contains("ppt"), "应含 presentation 扩展名 ppt: {term}");
        assert!(term.contains("pdf"), "应含 document 扩展名 pdf: {term}");
        assert!(term.contains(';'), "多扩展名应分号连接: {term}");
    }

    #[test]
    fn rejects_location_paths_with_newline_or_null_byte() {
        assert!(validate_search_path("/tmp/a\nb").is_err());
        assert!(validate_search_path("/tmp/a\0b").is_err());
    }

    #[test]
    fn mock_executor_drives_search_results() {
        let backend = EverythingBackend::with_executor_and_resolver(
            MockExecutor {
                available: true,
                stdout: "/Users/tester/report.pdf\n/Users/tester/image.png\n".to_owned(),
            },
            MockResolver,
            PathBuf::from("es.exe"),
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

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].source, BackendKind::Everything);
        assert_eq!(results[0].name, "report.pdf");
    }

    #[test]
    fn mock_executor_results_are_post_sorted() {
        let root =
            std::env::temp_dir().join(format!("scout-everything-sort-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let small = root.join("a.txt");
        let medium = root.join("b.txt");
        let large = root.join("c.txt");
        std::fs::write(&small, b"1").unwrap();
        std::fs::write(&medium, b"12").unwrap();
        std::fs::write(&large, b"123").unwrap();

        let backend = EverythingBackend::with_executor_and_resolver(
            MockExecutor {
                available: true,
                stdout: format!(
                    "{}\n{}\n{}\n",
                    medium.display(),
                    small.display(),
                    large.display()
                ),
            },
            MockResolver,
            PathBuf::from("es.exe"),
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

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn implementation_status_is_stub_on_non_windows_or_missing_es() {
        let backend = EverythingBackend::with_executor_and_resolver(
            MockExecutor {
                available: false,
                stdout: String::new(),
            },
            MockResolver,
            PathBuf::from("es.exe"),
        );

        assert_eq!(backend.implementation_status(), ImplementationStatus::Stub);
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

        let error = translate_intent(&intent, &MockResolver, Path::new("es.exe")).unwrap_err();

        assert!(matches!(error, SearchError::UnsupportedIntent { .. }));
    }

    #[test]
    fn expanded_keyword_group_ors_all_synonyms() {
        let base: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search",
            "keywords": ["工作汇报"]
        }))
        .unwrap();
        let expanded = ExpandedSearchIntent {
            base,
            keyword_groups: vec![KeywordGroup {
                head: "工作汇报".to_owned(),
                synonyms: vec!["述职".to_owned(), "工作总结".to_owned()],
            }],
            match_mode: MatchMode::default(),
        };

        let command =
            translate_intent_expanded(&expanded, &MockResolver, Path::new("es.exe")).unwrap();

        // 组内同义词用 `|` OR 连成单个 `<...>` term，组内三词全在。
        let group_arg = command
            .args
            .iter()
            .find(|arg| arg.starts_with('<') && arg.ends_with('>'))
            .expect("应有 <a|b|c> 分组 term");
        assert_eq!(group_arg, "<工作汇报|述职|工作总结>");
        // 仍不过 shell：参数内无换行 / null。
        assert!(command
            .args
            .iter()
            .all(|arg| !arg.contains('\n') && !arg.contains('\0')));
    }

    /// 2026-07-20：`MatchMode::Any` 下多个词组应摊平进单个 `<...>` OR term，而非默认空格
    /// AND（`es.exe` 无原生「组间 OR」语法，必须合并成一个 term 才能表达任一条件命中）。
    #[test]
    fn expanded_any_mode_flattens_groups_into_single_or_term() {
        let base: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search",
            "keywords": ["工作汇报", "季度"]
        }))
        .unwrap();
        let expanded = ExpandedSearchIntent {
            base,
            keyword_groups: vec![
                KeywordGroup::singleton("工作汇报"),
                KeywordGroup::singleton("季度"),
            ],
            match_mode: MatchMode::Any,
        };

        let command =
            translate_intent_expanded(&expanded, &MockResolver, Path::new("es.exe")).unwrap();

        // 两个词组摊平进同一个 <a|b> term，而不是两个独立参数（独立参数会被 es.exe 按空格 AND）。
        assert!(
            command.args.iter().any(|arg| arg == "<工作汇报|季度>"),
            "Any 模式应把词组摊平进单个 OR term: {:?}",
            command.args
        );
        assert!(
            !command.args.iter().any(|arg| arg == "工作汇报"),
            "不应再有独立的裸词参数（会被空格 AND）: {:?}",
            command.args
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
        let expanded = ExpandedSearchIntent {
            base: base.clone(),
            keyword_groups: vec![KeywordGroup::singleton("报告")],
            match_mode: MatchMode::default(),
        };

        let plain = translate_intent(&base, &MockResolver, Path::new("es.exe")).unwrap();
        let via_expanded =
            translate_intent_expanded(&expanded, &MockResolver, Path::new("es.exe")).unwrap();

        // 未扩词时，expanded 路径与普通翻译产出 byte-equal 的命令。
        assert_eq!(plain, via_expanded);
    }

    #[test]
    fn search_expanded_drives_results_through_executor() {
        let backend = EverythingBackend::with_executor_and_resolver(
            MockExecutor {
                available: true,
                stdout: "/Users/tester/述职2024.docx\n".to_owned(),
            },
            MockResolver,
            PathBuf::from("es.exe"),
        );
        let base: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search",
            "keywords": ["工作汇报"]
        }))
        .unwrap();
        let expanded = ExpandedSearchIntent {
            base,
            keyword_groups: vec![KeywordGroup {
                head: "工作汇报".to_owned(),
                synonyms: vec!["述职".to_owned()],
            }],
            match_mode: MatchMode::default(),
        };

        let stream =
            block_on(backend.search_expanded(&expanded, CancellationToken::new())).unwrap();
        let results: Vec<_> = block_on(stream.collect::<Vec<_>>())
            .into_iter()
            .collect::<Result<_, _>>()
            .unwrap();

        // 同义词 expanded 路径命中含 synonym 的文件（模拟「搜工作汇报 → 命中 述职」）。
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "述职2024.docx");
        assert_eq!(results[0].source, BackendKind::Everything);
    }

    #[test]
    fn es_path_candidates_prioritize_path_then_winget_then_program_files() {
        let local = PathBuf::from(r"C:\Users\me\AppData\Local");
        let pf = PathBuf::from(r"C:\Program Files");
        let cands = es_path_candidates(Some(local.as_path()), Some(pf.as_path()));
        // PATH 裸名优先（cands[0]）
        assert_eq!(cands[0], PathBuf::from("es.exe"));
        // 含 winget 安装路径（voidtools.Everything.Cli 包目录）
        assert!(
            cands.iter().any(|p| {
                p.to_string_lossy()
                    .contains("voidtools.Everything.Cli_Microsoft.Winget.Source_8wekyb3d8bbwe")
                    && p.ends_with("es.exe")
            }),
            "应含 winget es.exe 候选: {cands:?}"
        );
        // 含 Everything 默认安装目录（Program Files\Everything\es.exe）
        assert!(
            cands.iter().any(|p| {
                let s = p.to_string_lossy();
                s.contains("Program Files") && s.contains("Everything") && p.ends_with("es.exe")
            }),
            "应含 Everything 默认安装目录候选: {cands:?}"
        );
    }

    #[test]
    fn es_path_candidates_without_env_only_path() {
        let cands = es_path_candidates(None, None);
        assert_eq!(cands, vec![PathBuf::from("es.exe")]);
    }

    #[test]
    fn resolve_es_path_falls_back_to_bare_name_when_none_exist() {
        // 候选均不存在时回退裸名「es.exe」（is_available 据此报 false，行为同原实现）。
        // 非 Windows 开发机上 winget/ProgramFiles 候选与 PATH 中的 es.exe 通常都不存在。
        assert_eq!(resolve_es_path(), PathBuf::from("es.exe"));
    }
}
