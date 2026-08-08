//! `scout-search-backend` — Search Intent v1.0 类型与 `SearchBackend` trait。
//!
//! 本 crate 提供 Search Intent JSON 的 Rust 类型，与
//! [docs/search-intent-schema.md](../../../../docs/search-intent-schema.md) v1.0 严格对齐。
//!
//! # 设计要点
//!
//! - **internally-tagged enum**：[`SearchIntent`] 用 `intent` 字段作 tag，五个变体平铺顶层字段。
//! - **`deny_unknown_fields`**：所有结构体显式拒绝未知字段，与 schema 的 `additionalProperties: false` 对齐。
//! - **缺失 = `None`**：`Option<T>` 字段在 JSON 中可以缺失（视为 `None`）也可以显式 `null`。
//! - **运行时约束（如 `limit ≤ 500`、`question.minLength ≥ 1`、`file_action` 条件校验）不在类型层表达**，
//!   由 PROTO-03 引入 `jsonschema` crate 跨语言校验；PROTO-04 再补 Rust 侧的 `validate()` 兜底函数。
//!
//! # 用例覆盖
//!
//! 见 `tests/cases.rs` —— 反序列化 [schema §7](../../../../docs/search-intent-schema.md) 全部 47 条用例。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, Ordering as AtomicOrdering},
    Arc,
};

use chrono::{DateTime, NaiveDate, Utc};
use futures_core::Stream;
use serde::{Deserialize, Serialize};

// ============================================================
// §0. SearchBackend trait + 归一化结果类型
// ============================================================

/// 搜索后端身份。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// macOS Spotlight。
    Spotlight,
    /// Windows Search / `SystemIndex`。
    WindowsSearch,
    /// Everything 可选加速后端。
    Everything,
    /// `Scout` 未来自建索引。
    NativeIndex,
    /// BETA-15B 语义向量索引（本地嵌入 + 暴力 cosine 最近邻）。
    SemanticIndex,
}

/// 后端实现状态。生产 fallback 链必须排除 [`ImplementationStatus::Stub`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImplementationStatus {
    /// 真实可用实现。
    Real,
    /// 仅用于开发 / 测试枚举的占位实现。
    Stub,
}

/// 结果命中来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchType {
    /// 文件名命中。
    Filename,
    /// 文件内容命中。
    Content,
    /// 元数据命中。
    Metadata,
    /// OCR 文本命中。
    Ocr,
    /// 语义向量近邻命中（BETA-15B）。
    Semantic,
}

/// 归一化文件元数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SearchResultMetadata {
    /// 修改时间。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_time: Option<DateTime<Utc>>,
    /// 创建时间。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_time: Option<DateTime<Utc>>,
    /// 最后访问时间。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessed_time: Option<DateTime<Utc>>,
    /// 文件大小，单位 byte。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// 音频艺术家 / 作者。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    /// 媒体标题。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 音频专辑。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// 媒体时长，单位秒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
}

/// 搜索结果。`id` 在 v0.1 仅保证本次查询内稳定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchResult {
    /// 结果 ID。原型期由后端按规范化绝对路径生成。
    pub id: String,
    /// 绝对路径（OS native）。
    pub path: PathBuf,
    /// 文件名（含扩展名）。
    pub name: String,
    /// 返回此结果的后端。
    pub source: BackendKind,
    /// 命中类型。
    pub match_type: MatchType,
    /// 相关性分数。后端无分数时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// 归一化元数据。
    #[serde(default)]
    pub metadata: SearchResultMetadata,
}

/// 提取 `SearchIntent` 中声明的排序方式。
#[must_use]
pub const fn intent_sort_order(intent: &SearchIntent) -> Option<SortOrder> {
    match intent {
        SearchIntent::FileSearch(search) => search.sort,
        SearchIntent::MediaSearch(search) => search.sort,
        SearchIntent::Refine(refine) => refine.delta.sort,
        SearchIntent::FileAction(_) | SearchIntent::Clarify(_) => None,
    }
}

/// 按 `SearchResultMetadata` 与文件名执行客户端 post-sort。
///
/// [`SortOrder::RelevanceDesc`] 保留后端默认顺序。
pub fn sort_results(results: &mut [SearchResult], sort: Option<SortOrder>) {
    let Some(sort) = sort else {
        return;
    };

    match sort {
        SortOrder::RelevanceDesc => {}
        SortOrder::ModifiedDesc => {
            results.sort_by(|left, right| {
                compare_option_desc(left.metadata.modified_time, right.metadata.modified_time)
            });
        }
        SortOrder::ModifiedAsc => {
            results.sort_by(|left, right| {
                compare_option_asc(left.metadata.modified_time, right.metadata.modified_time)
            });
        }
        SortOrder::CreatedDesc => {
            results.sort_by(|left, right| {
                compare_option_desc(left.metadata.created_time, right.metadata.created_time)
            });
        }
        SortOrder::CreatedAsc => {
            results.sort_by(|left, right| {
                compare_option_asc(left.metadata.created_time, right.metadata.created_time)
            });
        }
        SortOrder::AccessedDesc => {
            results.sort_by(|left, right| {
                compare_option_desc(left.metadata.accessed_time, right.metadata.accessed_time)
            });
        }
        SortOrder::SizeDesc => {
            results.sort_by(|left, right| {
                compare_option_desc(left.metadata.size_bytes, right.metadata.size_bytes)
            });
        }
        SortOrder::SizeAsc => {
            results.sort_by(|left, right| {
                compare_option_asc(left.metadata.size_bytes, right.metadata.size_bytes)
            });
        }
        SortOrder::NameAsc => results.sort_by_key(|result| result.name.to_lowercase()),
        SortOrder::NameDesc => {
            results.sort_by_key(|result| std::cmp::Reverse(result.name.to_lowercase()));
        }
    }
}

fn compare_option_asc<T: Ord>(left: Option<T>, right: Option<T>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn compare_option_desc<T: Ord>(left: Option<T>, right: Option<T>) -> std::cmp::Ordering {
    compare_option_asc(right, left)
}

/// `SearchBackend` 错误类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchError {
    /// 后端整体不可用。
    BackendUnavailable { reason: String },
    /// 系统权限不足。
    PermissionDenied { path: Option<PathBuf> },
    /// intent 本身不合法。
    InvalidIntent { detail: String },
    /// intent 合法，但当前后端能力不支持。
    UnsupportedIntent { detail: String },
    /// 超过 deadline。
    Timeout { elapsed_ms: u64 },
    /// 其他 IO 错误。
    Io { detail: String },
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BackendUnavailable { reason } => write!(f, "backend unavailable: {reason}"),
            Self::PermissionDenied { path } => match path {
                Some(path) => write!(f, "permission denied: {}", path.display()),
                None => f.write_str("permission denied"),
            },
            Self::InvalidIntent { detail } => write!(f, "invalid intent: {detail}"),
            Self::UnsupportedIntent { detail } => write!(f, "unsupported intent: {detail}"),
            Self::Timeout { elapsed_ms } => write!(f, "search timeout after {elapsed_ms} ms"),
            Self::Io { detail } => write!(f, "io error: {detail}"),
        }
    }
}

impl Error for SearchError {}

impl From<std::io::Error> for SearchError {
    fn from(error: std::io::Error) -> Self {
        Self::Io {
            detail: error.to_string(),
        }
    }
}

/// 搜索后端返回的异步结果流。
pub type BackendStream = Pin<Box<dyn Stream<Item = Result<SearchResult, SearchError>> + Send>>;

/// `SearchBackend::search` 返回的 boxed future。
///
/// 这里不用 `async fn` in trait，是为了保留 `Box<dyn SearchBackend>` 的 object safety。
pub type BackendSearchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<BackendStream, SearchError>> + Send + 'a>>;

/// 搜索取消信号。
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    /// 创建一个未取消的新信号。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 请求取消搜索。
    pub fn cancel(&self) {
        self.cancelled.store(true, AtomicOrdering::SeqCst);
    }

    /// 当前是否已经取消。
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(AtomicOrdering::SeqCst)
    }
}

/// 所有搜索后端实现此 trait。v0.2 对外只暴露异步流式接口。
pub trait SearchBackend: fmt::Debug + Send + Sync {
    /// 后端身份。
    fn kind(&self) -> BackendKind;

    /// 实现状态。真实后端默认返回 [`ImplementationStatus::Real`]。
    fn implementation_status(&self) -> ImplementationStatus {
        ImplementationStatus::Real
    }

    /// 当前环境下是否可用。
    fn is_available(&self) -> bool;

    /// 执行一次搜索。`intent` 应已通过 schema 校验；返回值是可取消的异步结果流。
    fn search<'a>(
        &'a self,
        intent: &'a SearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a>;

    /// 接受同义词扩展后的搜索意图。默认实现 fallback 到 `search(&expanded.base)`,
    /// 丢弃 group 信息。支持同义词的后端覆盖此方法。
    fn search_expanded<'a>(
        &'a self,
        expanded: &'a crate::ExpandedSearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        self.search(&expanded.base, cancel)
    }
}

/// 从完整结果集构造 backend stream；若取消信号已触发则停止继续产出。
#[must_use]
pub fn backend_stream_from_results(
    results: Vec<SearchResult>,
    cancel: CancellationToken,
) -> BackendStream {
    Box::pin(futures_util::stream::unfold(
        (results.into_iter(), cancel),
        |(mut iter, cancel)| async move {
            if cancel.is_cancelled() {
                return None;
            }
            iter.next().map(|result| (Ok(result), (iter, cancel)))
        },
    ))
}

/// 从错误构造只产出一次错误的 backend stream。
#[must_use]
pub fn backend_stream_from_error(error: SearchError) -> BackendStream {
    Box::pin(futures_util::stream::once(async move { Err(error) }))
}

/// 搜索后端注册表。
#[derive(Debug, Default)]
pub struct BackendRegistry {
    backends: Vec<Box<dyn SearchBackend>>,
}

impl BackendRegistry {
    /// 创建空注册表。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            backends: Vec::new(),
        }
    }

    /// 注册一个后端。
    pub fn register<B>(&mut self, backend: B)
    where
        B: SearchBackend + 'static,
    {
        self.backends.push(Box::new(backend));
    }

    /// 全部注册后端，包含测试 stub。
    #[must_use]
    pub fn all_backends(&self) -> Vec<&dyn SearchBackend> {
        self.backends
            .iter()
            .map(std::convert::AsRef::as_ref)
            .collect()
    }

    /// 生产 fallback 链，自动剔除 stub。
    #[must_use]
    pub fn production_backends(&self) -> Vec<&dyn SearchBackend> {
        self.backends
            .iter()
            .filter(|backend| backend.implementation_status() == ImplementationStatus::Real)
            .map(std::convert::AsRef::as_ref)
            .collect()
    }

    /// 选择第一个生产可用后端。
    #[must_use]
    pub fn select_available(&self) -> Option<&dyn SearchBackend> {
        self.backends
            .iter()
            .filter(|backend| backend.implementation_status() == ImplementationStatus::Real)
            .find(|backend| backend.is_available())
            .map(std::convert::AsRef::as_ref)
    }
}

/// 位置 hint 解析错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocationResolveError {
    /// 当前系统无法确定用户 home 目录。
    HomeDirUnavailable,
    /// hint 不在当前 resolver 支持列表中。
    UnsupportedHint { hint: String },
    /// 平台 API 或命令读取失败。
    Platform { detail: String },
}

impl fmt::Display for LocationResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HomeDirUnavailable => f.write_str("home directory unavailable"),
            Self::UnsupportedHint { hint } => write!(f, "unsupported location hint: {hint}"),
            Self::Platform { detail } => write!(f, "platform location resolver failed: {detail}"),
        }
    }
}

impl Error for LocationResolveError {}

/// 把自然语言位置 hint 解析成一个或多个绝对路径。
pub trait LocationResolver: fmt::Debug + Send + Sync {
    /// 解析单个 hint。返回空数组不合法；无法识别时返回 [`LocationResolveError::UnsupportedHint`]。
    fn resolve_hint(&self, hint: &str) -> Result<Vec<PathBuf>, LocationResolveError>;
}

// ============================================================
// §1. 顶层公共字段：SchemaVersion / Language
// ============================================================

/// Schema 版本。v1.0 仅含 `"1.0"` 一个值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchemaVersion {
    /// `"1.0"`
    #[serde(rename = "1.0")]
    V1,
}

/// 用户输入语言（来自规则解析器或模型识别）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    /// 中文
    Zh,
    /// 英文
    En,
    /// 中英混合
    Mixed,
    /// 未识别（默认）
    Unknown,
}

// ============================================================
// §2. 高层枚举：FileType / SortOrder
// ============================================================

/// 高层文件类型；由 backend 展开为扩展名集合（见 schema §4.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileType {
    Document,
    Spreadsheet,
    Presentation,
    Image,
    Screenshot,
    Video,
    Audio,
    Archive,
    Code,
    Executable,
}

/// 把 location 路径字符串校验为绝对路径（拒绝含 null 字节 / 换行的注入），三后端共用。
///
/// # Errors
/// 路径含 null/换行，或不是绝对路径时返回 [`SearchError::InvalidIntent`]。
pub fn validate_absolute_search_path(path: &str) -> Result<PathBuf, SearchError> {
    if path.contains('\0') || path.contains('\n') {
        return Err(SearchError::InvalidIntent {
            detail: "location path contains null byte or newline".to_owned(),
        });
    }

    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(SearchError::InvalidIntent {
            detail: format!("location path must be absolute: {}", path.display()),
        });
    }

    Ok(path)
}

/// 判断路径是否落在任一排除根目录之下，三后端共用。
#[must_use]
pub fn is_path_excluded(path: &std::path::Path, excluded_roots: &[PathBuf]) -> bool {
    excluded_roots.iter().any(|root| path.starts_with(root))
}

/// 由路径生成稳定的结果 ID（对路径取 hash 后十六进制），各后端共用以保持 ID 口径一致。
#[must_use]
pub fn result_id_for_path(path: &std::path::Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// `FileType` → 该类型的扩展名集合（小写、无点）。后端把 `file_type` 展开为扩展名过滤时用，
/// 跨范畴均衡查询拆分子查询时也用——三后端历史上各持一份**完全相同**的副本，此处收拢为单一信源。
#[must_use]
pub fn extensions_for_file_type(file_type: FileType) -> &'static [&'static str] {
    match file_type {
        FileType::Document => &[
            "doc", "docx", "pdf", "txt", "md", "html", "rtf", "pages", "odt",
        ],
        FileType::Spreadsheet => &["xls", "xlsx", "csv", "numbers", "ods"],
        FileType::Presentation => &["ppt", "pptx", "key", "odp"],
        FileType::Image | FileType::Screenshot => &[
            "jpg", "jpeg", "png", "gif", "heic", "heif", "webp", "bmp", "tiff", "svg",
        ],
        FileType::Video => &["mp4", "mov", "avi", "mkv", "webm", "m4v", "wmv", "flv"],
        FileType::Audio => &[
            "mp3", "flac", "wav", "m4a", "ape", "ogg", "aac", "wma", "aiff",
        ],
        FileType::Archive => &["zip", "rar", "7z", "tar", "gz", "bz2", "xz"],
        FileType::Code => &[
            "py", "js", "ts", "rs", "go", "java", "c", "cpp", "h", "hpp", "swift", "kt",
        ],
        FileType::Executable => &["exe", "msi", "dmg", "pkg", "app", "deb", "rpm"],
    }
}

/// 媒体大类推导出的高层文件类型：显式 `file_type` 优先，否则由 `media_type` 派生。
/// everything / windows-search 翻译媒体查询时共用——历史上各持一份完全相同副本，此处收拢为单一信源。
#[must_use]
pub fn media_derived_file_types(search: &MediaSearch) -> Option<Vec<FileType>> {
    let derived = match search.media_type {
        MediaType::Audio => Some(FileType::Audio),
        MediaType::Image | MediaType::Screenshot => Some(FileType::Image),
        MediaType::Video => Some(FileType::Video),
    };
    search
        .file_type
        .clone()
        .or_else(|| derived.map(|ft| vec![ft]))
}

/// 后端翻译时共用的"通用约束"载体：把一次查询里与具体后端语法无关的字段（关键词 / 扩展名 /
/// 类型 / 路径 / 时间 / 大小 / 排除项）打包，交由各后端的 `add_common_constraints` 落成自家查询语法。
/// 三后端历史上各持一份**完全相同**的定义，此处收拢为单一信源。
#[derive(Debug, Clone, Copy)]
pub struct CommonConstraints<'a> {
    /// 关键词（调用方决定：原始列表或 `None` + groups）
    pub keywords: Option<&'a [String]>,
    /// 扩展名白名单
    pub extensions: Option<&'a [String]>,
    /// 高层文件类型
    pub file_type: Option<&'a [FileType]>,
    /// 路径线索 / 约束
    pub location: Option<&'a Location>,
    /// 修改时间
    pub modified_time: Option<&'a TimeExpression>,
    /// 创建时间
    pub created_time: Option<&'a TimeExpression>,
    /// 最后访问时间
    pub accessed_time: Option<&'a TimeExpression>,
    /// 文件大小约束
    pub size: Option<&'a SizeExpression>,
    /// 排除的扩展名
    pub exclude_extensions: Option<&'a [String]>,
    /// 排除的高层类型
    pub exclude_file_type: Option<&'a [FileType]>,
}

/// 媒体查询的通用约束（`keywords` 由调用方决定：原始列表或 `None` + groups；
/// `file_types` 由调用方用 [`media_derived_file_types`] 算好并保活后传入）。
/// everything / windows-search 共用，收拢为单一信源。
#[must_use]
pub fn media_common_constraints<'a>(
    search: &'a MediaSearch,
    keywords: Option<&'a [String]>,
    file_types: Option<&'a [FileType]>,
) -> CommonConstraints<'a> {
    CommonConstraints {
        keywords,
        extensions: search.extensions.as_deref(),
        file_type: file_types,
        location: search.location.as_ref(),
        modified_time: search.modified_time.as_ref(),
        created_time: search.created_time.as_ref(),
        accessed_time: search.accessed_time.as_ref(),
        size: search.size.as_ref(),
        exclude_extensions: search.exclude_extensions.as_deref(),
        exclude_file_type: search.exclude_file_type.as_deref(),
    }
}

/// 相对时间 → `(起始天偏移, 结束天偏移)`（以"今天"为 0 的天数区间，左闭右开语义由各后端落地）。
/// spotlight / windows-search 把 `RelativeTime` 落成各自时间语法前先取此统一边界——
/// 历史上各持一份完全相同副本，此处收拢为单一信源。
#[must_use]
pub fn relative_time_bounds(value: RelativeTime) -> (i32, i32) {
    match value {
        RelativeTime::Today => (0, 1),
        RelativeTime::Yesterday => (-1, 0),
        RelativeTime::Last3Days => (-3, 1),
        RelativeTime::Last7Days | RelativeTime::ThisWeek => (-7, 1),
        RelativeTime::Last14Days => (-14, 1),
        RelativeTime::Last30Days => (-30, 1),
        RelativeTime::LastWeek => (-14, -7),
        RelativeTime::ThisMonth => (-31, 1),
        RelativeTime::LastMonth => (-62, -31),
        RelativeTime::ThisYear => (-366, 1),
        RelativeTime::LastYear => (-732, -366),
    }
}

/// `file_type` 字段的 serde：内部表示为 `Option<Vec<FileType>>`，但 JSON wire 格式
/// **同时接受标量与数组**，且**单元素序列化回标量**——保持与 schema v1.0 单值 `file_type`
/// 的 wire 兼容（旧 fixtures / v1 `LoRA` 数据集 / evals 一律不变），同时支持跨范畴多类型
/// （BETA-18，如 `["image","video"]`）。空数组规整为 `None`。
pub(crate) mod file_type_set {
    use super::FileType;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    /// 反序列化辅助：标量 `"document"` 或数组 `["image","video"]` 二选一。
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ScalarOrVec {
        Scalar(FileType),
        Vec(Vec<FileType>),
    }

    // serde `with` 的 serialize 签名固定为 `&Option<T>`，无法改为 `Option<&T>`。
    #[allow(clippy::ref_option)]
    pub(crate) fn serialize<S>(
        value: &Option<Vec<FileType>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            None => serializer.serialize_none(),
            // 单元素回写为标量，保持与单值 schema 的 byte-equal。
            Some(v) if v.len() == 1 => v[0].serialize(serializer),
            Some(v) => v.serialize(serializer),
        }
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Option<Vec<FileType>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt = Option::<ScalarOrVec>::deserialize(deserializer)?;
        Ok(opt.and_then(|v| {
            let vec = match v {
                ScalarOrVec::Scalar(ft) => vec![ft],
                ScalarOrVec::Vec(v) => v,
            };
            // 空数组规整为 None，避免下游需区分 Some(空) 与 None。
            if vec.is_empty() {
                None
            } else {
                Some(vec)
            }
        }))
    }
}

/// 结果排序方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    RelevanceDesc,
    ModifiedDesc,
    ModifiedAsc,
    CreatedDesc,
    CreatedAsc,
    AccessedDesc,
    SizeDesc,
    SizeAsc,
    NameAsc,
    NameDesc,
}

// ============================================================
// §3. TimeExpression
// ============================================================

/// 相对时间语义（schema §4.1）。
///
/// 注：含数字的变体必须 `#[serde(rename)]` —— serde 的 `snake_case` 不在数字前加下划线，
/// 会把 `Last3Days` 序列化为 `last3_days`，但 schema 要求 `last_3_days`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelativeTime {
    Today,
    Yesterday,
    #[serde(rename = "last_3_days")]
    Last3Days,
    #[serde(rename = "last_7_days")]
    Last7Days,
    #[serde(rename = "last_14_days")]
    Last14Days,
    #[serde(rename = "last_30_days")]
    Last30Days,
    ThisWeek,
    LastWeek,
    ThisMonth,
    LastMonth,
    ThisYear,
    LastYear,
}

/// 时间表达式（schema §4.1）。模型只输出语义；具体边界由程序按本地时区/locale 计算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimeExpression {
    /// 相对时间，如 `yesterday` / `last_7_days`。
    Relative {
        /// 见 [`RelativeTime`]
        value: RelativeTime,
    },
    /// 绝对时间闭区间 `[from, to]`，ISO 8601 日期。
    Absolute {
        /// 起始日期
        from: NaiveDate,
        /// 结束日期
        to: NaiveDate,
    },
    /// 严格早于该日期。
    Before {
        /// 阈值日期
        value: NaiveDate,
    },
    /// 严格晚于该日期。
    After {
        /// 阈值日期
        value: NaiveDate,
    },
}

// ============================================================
// §4. SizeExpression
// ============================================================

/// 大小 / 时长单位（schema §4.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SizeUnit {
    /// Byte
    #[serde(rename = "B")]
    B,
    /// Kilobyte（十进制 1000 倍）
    #[serde(rename = "KB")]
    Kb,
    /// Megabyte
    #[serde(rename = "MB")]
    Mb,
    /// Gigabyte
    #[serde(rename = "GB")]
    Gb,
    /// 秒（duration 用）
    #[serde(rename = "s")]
    Sec,
    /// 分钟
    #[serde(rename = "m")]
    Min,
    /// 小时
    #[serde(rename = "h")]
    Hour,
}

/// 大小或时长表达式（schema §4.2）。`media_search.duration` 复用此结构（单位用 s/m/h）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SizeExpression {
    /// `value > X`
    GreaterThan {
        /// 阈值；schema 要求 > 0（运行时校验）
        value: f64,
        /// 单位
        unit: SizeUnit,
    },
    /// `value < X`
    LessThan {
        /// 阈值
        value: f64,
        /// 单位
        unit: SizeUnit,
    },
    /// `min ≤ value ≤ max`
    Between {
        /// 下界
        min: f64,
        /// 上界
        max: f64,
        /// 单位
        unit: SizeUnit,
    },
}

// ============================================================
// §5. Location
// ============================================================

/// 路径线索 / 约束（schema §4.3）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Location {
    /// 自然语言线索（如 "下载"、"desktop"）。由 resolver 解析为 `include`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// 解析后的包含路径数组。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    /// 排除路径数组。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
}

// ============================================================
// §6. TargetRef / TargetSelector
// ============================================================

/// 上一轮结果的指代选择器（schema §4.6）。v1.0 不支持 `filter` 变体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TargetSelector {
    /// 单个 1-based 索引
    Index {
        /// 1-based
        value: u32,
    },
    /// 多个 1-based 索引（schema 要求 `minItems: 1`，运行时校验）
    Indices {
        /// 1-based 索引列表
        values: Vec<u32>,
    },
    /// 上一轮全部结果
    All,
}

/// 文件操作的目标指代（schema §4.6）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum TargetRef {
    /// 上一轮搜索结果（由 Context Memory 保存）
    LastResults {
        /// 选择器
        selector: TargetSelector,
    },
    /// 直接指定绝对路径
    Path {
        /// 绝对路径
        value: String,
    },
    /// 直接指定一组绝对路径。确认流的自包含 pending 用（多目标 copy/move），
    /// 由 wiring 层在首次下发时把已解析的 N 个绝对路径写入，规避确认前 context 漂移。
    /// parser 不产此变体。
    Paths {
        /// 绝对路径列表
        values: Vec<String>,
    },
}

// ============================================================
// §7. SearchIntent 顶层 — internally-tagged enum
// ============================================================

/// Search Intent v1.0 顶层类型。
///
/// 通过 `intent` 字段区分五个变体；每个变体的字段平铺到 JSON 顶层（serde internally-tagged enum）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "intent", rename_all = "snake_case")]
pub enum SearchIntent {
    /// 通用文件搜索
    FileSearch(FileSearch),
    /// 媒体专项搜索（音频 / 图片 / 视频 / 截图）
    MediaSearch(MediaSearch),
    /// 对已选中文件执行操作
    FileAction(FileAction),
    /// 在上一轮结果上二次筛选
    Refine(Refine),
    /// 模糊查询时的澄清问题
    Clarify(Clarify),
}

impl SearchIntent {
    /// 返回 *Search variant 的 keywords 切片（FileSearch / MediaSearch）。其他 variant 返回 None。
    #[must_use]
    pub fn search_keywords(&self) -> Option<&[String]> {
        match self {
            Self::FileSearch(s) => s.keywords.as_deref(),
            Self::MediaSearch(s) => s.keywords.as_deref(),
            _ => None,
        }
    }
}

// ============================================================
// §8. FileSearch
// ============================================================

/// `file_search` intent（schema §3.1）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSearch {
    /// Schema 版本（必为 `"1.0"`）
    pub schema_version: SchemaVersion,
    /// 用户输入语言
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    /// 文件名 / 内容关键词
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keywords: Option<Vec<String>>,
    /// 扩展名（不含点）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    /// 高层文件类型（BETA-18：支持跨范畴多类型；wire 接受标量或数组，单值回写为标量）
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "file_type_set"
    )]
    pub file_type: Option<Vec<FileType>>,
    /// 路径线索 / 约束
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    /// 修改时间
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_time: Option<TimeExpression>,
    /// 创建时间
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_time: Option<TimeExpression>,
    /// 最后访问时间
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessed_time: Option<TimeExpression>,
    /// 文件大小约束
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<SizeExpression>,
    /// 排除的扩展名
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_extensions: Option<Vec<String>>,
    /// 排除的高层类型
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_file_type: Option<Vec<FileType>>,
    /// 排序顺序
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<SortOrder>,
    /// 返回上限（默认 50，最大 500；运行时校验）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

// ============================================================
// §9. MediaSearch
// ============================================================

/// 媒体类型（schema §3.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaType {
    /// 音频
    Audio,
    /// 图片
    Image,
    /// 视频
    Video,
    /// 截图（image 子集，启发式过滤）
    Screenshot,
}

/// 音频质量启发式。v1.0 由扩展名启发，MVP 后由 metadata reader 升级（见 schema §8.2 #15）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    /// 无损（flac/wav/aiff/ape 等）
    Lossless,
    /// 高品质
    High,
    /// 标准
    Standard,
    /// 低品质
    Low,
}

/// `media_search` intent（schema §3.2）。包含 [`FileSearch`] 的所有字段，外加媒体专有字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaSearch {
    /// Schema 版本（必为 `"1.0"`）
    pub schema_version: SchemaVersion,
    /// 用户输入语言
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    /// 媒体大类（必填）
    pub media_type: MediaType,
    /// 演唱者 / 作者
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    /// 标题
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 专辑
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// 流派
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    /// 音频质量
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<Quality>,
    /// 时长约束（单位用 s/m/h）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<SizeExpression>,
    // 以下与 FileSearch 相同
    /// 关键词
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keywords: Option<Vec<String>>,
    /// 扩展名
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    /// 高层文件类型（BETA-18：支持跨范畴多类型；wire 接受标量或数组，单值回写为标量）
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "file_type_set"
    )]
    pub file_type: Option<Vec<FileType>>,
    /// 路径线索 / 约束
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    /// 修改时间
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_time: Option<TimeExpression>,
    /// 创建时间
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_time: Option<TimeExpression>,
    /// 最后访问时间
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessed_time: Option<TimeExpression>,
    /// 文件大小约束
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<SizeExpression>,
    /// 排除的扩展名
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_extensions: Option<Vec<String>>,
    /// 排除的高层类型
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_file_type: Option<Vec<FileType>>,
    /// 排序顺序
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<SortOrder>,
    /// 返回上限
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

// ============================================================
// §10. FileAction
// ============================================================

/// 文件操作类型（schema §3.3）。
///
/// 权限分级（与计划书 §8.1 对应）：`open` L3、`locate` L1、`copy`/`move`/`rename` L4、`delete` L5。
/// `delete` 在 MVP 必须被运行时拒绝（schema 允许但 Policy Engine 阻止）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileActionKind {
    /// 打开
    Open,
    /// 在文件管理器中显示
    Locate,
    /// 复制
    Copy,
    /// 移动
    Move,
    /// 重命名
    Rename,
    /// 删除（MVP 不开放）
    Delete,
}

/// `file_action` intent（schema §3.3）。
///
/// 运行时条件校验（未在类型层表达）：
/// - `Copy`/`Move`：`destination` 必填且非空
/// - `Rename`：`new_name` 必填且非空
/// - `Copy`/`Move`/`Rename`/`Delete`：`requires_confirmation` 必须为 `true`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileAction {
    /// Schema 版本（必为 `"1.0"`）
    pub schema_version: SchemaVersion,
    /// 用户输入语言
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    /// 操作类型
    pub action: FileActionKind,
    /// 目标文件指代
    pub target_ref: TargetRef,
    /// 目标路径（`copy`/`move` 必填）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    /// 新文件名（`rename` 必填）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_name: Option<String>,
    /// 是否需要用户确认
    pub requires_confirmation: bool,
}

// ============================================================
// §11. Refine
// ============================================================

/// Refine 基准引用。v1.0 仅支持 `last_intent`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseRef {
    /// 上一次 intent
    LastIntent,
}

/// `refine.clear` 中允许的字段路径（schema §3.4 清空字段白名单）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClearableField {
    Location,
    Extensions,
    FileType,
    Keywords,
    ModifiedTime,
    CreatedTime,
    AccessedTime,
    Size,
    ExcludeExtensions,
    ExcludeFileType,
    Artist,
    Title,
    Album,
    Genre,
    Quality,
    Duration,
}

/// Refine 的字段增量（schema §3.4）。所有字段都是可选；语义是"覆盖"基准对应字段。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RefineDelta {
    /// 关键词
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keywords: Option<Vec<String>>,
    /// 扩展名
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    /// 高层文件类型（BETA-18：支持跨范畴多类型；wire 接受标量或数组，单值回写为标量）
    #[serde(skip_serializing_if = "Option::is_none", with = "file_type_set")]
    pub file_type: Option<Vec<FileType>>,
    /// 路径线索 / 约束
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    /// 修改时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_time: Option<TimeExpression>,
    /// 创建时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_time: Option<TimeExpression>,
    /// 访问时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accessed_time: Option<TimeExpression>,
    /// 文件大小
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<SizeExpression>,
    /// 排除扩展名
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_extensions: Option<Vec<String>>,
    /// 排除高层类型
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_file_type: Option<Vec<FileType>>,
    /// 演唱者
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    /// 标题
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 专辑
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// 流派
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    /// 音频质量
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<Quality>,
    /// 时长
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<SizeExpression>,
    /// 排序
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<SortOrder>,
    /// 返回上限
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// `refine` intent（schema §3.4）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refine {
    /// Schema 版本（必为 `"1.0"`）
    pub schema_version: SchemaVersion,
    /// 用户输入语言
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    /// 基准引用
    pub base_ref: BaseRef,
    /// 字段增量
    pub delta: RefineDelta,
    /// 清空字段列表
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear: Option<Vec<ClearableField>>,
}

// ============================================================
// §12. Clarify
// ============================================================

/// Clarify 触发原因（schema §3.5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClarifyReason {
    /// 时间表达模糊
    AmbiguousTime,
    /// 位置表达模糊或无法解析
    AmbiguousLocation,
    /// 类型表达模糊
    AmbiguousType,
    /// 操作意图模糊
    AmbiguousAction,
    /// 高风险写操作需要确认
    UnsafeAction,
    /// 其他未分类
    Unknown,
}

/// `clarify` intent（schema §3.5）。
///
/// 运行时校验：`question` 必须非空（schema `minLength: 1`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clarify {
    /// Schema 版本（必为 `"1.0"`）
    pub schema_version: SchemaVersion,
    /// 用户输入语言
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    /// 触发原因
    pub reason: ClarifyReason,
    /// 给用户的问题文本
    pub question: String,
    /// 建议选项（UI 可渲染为快捷按钮）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
}

// ============================================================
// §13. ExpandedSearchIntent / KeywordGroup（同义词扩展载体类型）
// ============================================================

pub mod expanded;
pub use expanded::{ExpandedSearchIntent, KeywordGroup, MatchMode};

// ============================================================
// §14. 单元测试：枚举名 ↔ JSON 字符串映射
// ============================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn schema_version_roundtrip() {
        let json = "\"1.0\"";
        let v: SchemaVersion = serde_json::from_str(json).unwrap();
        assert_eq!(v, SchemaVersion::V1);
        assert_eq!(serde_json::to_string(&v).unwrap(), json);
    }

    /// BETA-18：`file_type` 接受标量或数组；单值回写为标量（wire 兼容）、多值为数组、空数组规整 None。
    #[test]
    fn file_type_scalar_or_vec_serde() {
        fn ft(json: &str) -> Option<Vec<FileType>> {
            let fs: FileSearch =
                serde_json::from_str(&format!(r#"{{"schema_version":"1.0","file_type":{json}}}"#))
                    .unwrap();
            fs.file_type
        }
        // 标量入 → 单元素 Vec。
        assert_eq!(ft(r#""document""#), Some(vec![FileType::Document]));
        // 数组入 → Vec。
        assert_eq!(
            ft(r#"["image","video"]"#),
            Some(vec![FileType::Image, FileType::Video])
        );
        // 空数组 → None。
        assert_eq!(ft("[]"), None);
        assert_eq!(ft("null"), None);

        // 序列化：单值回标量、多值数组。
        let one = FileSearch {
            schema_version: SchemaVersion::V1,
            language: None,
            keywords: None,
            extensions: None,
            file_type: Some(vec![FileType::Document]),
            location: None,
            modified_time: None,
            created_time: None,
            accessed_time: None,
            size: None,
            exclude_extensions: None,
            exclude_file_type: None,
            sort: None,
            limit: None,
        };
        let v = serde_json::to_value(&one).unwrap();
        assert_eq!(v["file_type"], serde_json::json!("document"));
        let mut multi = one.clone();
        multi.file_type = Some(vec![FileType::Image, FileType::Video]);
        let v = serde_json::to_value(&multi).unwrap();
        assert_eq!(v["file_type"], serde_json::json!(["image", "video"]));
    }

    #[test]
    fn language_roundtrip() {
        for (s, expected) in [
            ("zh", Language::Zh),
            ("en", Language::En),
            ("mixed", Language::Mixed),
            ("unknown", Language::Unknown),
        ] {
            let json = format!("\"{s}\"");
            let v: Language = serde_json::from_str(&json).unwrap();
            assert_eq!(v, expected);
            assert_eq!(serde_json::to_string(&v).unwrap(), json);
        }
    }

    #[test]
    fn size_unit_special_renames() {
        // 大写单位
        let v: SizeUnit = serde_json::from_str("\"MB\"").unwrap();
        assert_eq!(v, SizeUnit::Mb);
        assert_eq!(serde_json::to_string(&v).unwrap(), "\"MB\"");
        // 小写时长单位
        let v: SizeUnit = serde_json::from_str("\"s\"").unwrap();
        assert_eq!(v, SizeUnit::Sec);
        assert_eq!(serde_json::to_string(&v).unwrap(), "\"s\"");
    }

    #[test]
    fn time_expression_variants() {
        let v: TimeExpression =
            serde_json::from_str(r#"{"type":"relative","value":"yesterday"}"#).unwrap();
        assert!(matches!(
            v,
            TimeExpression::Relative {
                value: RelativeTime::Yesterday
            }
        ));

        let v: TimeExpression =
            serde_json::from_str(r#"{"type":"absolute","from":"2025-01-01","to":"2025-12-31"}"#)
                .unwrap();
        let TimeExpression::Absolute { from, to } = v else {
            panic!("expected Absolute");
        };
        assert_eq!(from, NaiveDate::from_ymd_opt(2025, 1, 1).unwrap());
        assert_eq!(to, NaiveDate::from_ymd_opt(2025, 12, 31).unwrap());
    }

    #[test]
    fn target_selector_all_unit_variant() {
        let json = r#"{"type":"all"}"#;
        let v: TargetSelector = serde_json::from_str(json).unwrap();
        assert_eq!(v, TargetSelector::All);
        assert_eq!(serde_json::to_string(&v).unwrap(), json);
    }

    #[test]
    fn target_ref_path_variant() {
        let json = r#"{"source":"path","value":"/Users/r/x.pdf"}"#;
        let v: TargetRef = serde_json::from_str(json).unwrap();
        assert!(matches!(v, TargetRef::Path { ref value } if value == "/Users/r/x.pdf"));
    }

    #[test]
    fn file_search_minimal_roundtrip() {
        let json = r#"{"schema_version":"1.0","intent":"file_search","keywords":["预算"]}"#;
        let v: SearchIntent = serde_json::from_str(json).unwrap();
        let SearchIntent::FileSearch(fs) = v else {
            panic!("expected FileSearch")
        };
        assert_eq!(fs.keywords.as_deref(), Some(&["预算".to_owned()][..]));
        assert_eq!(fs.schema_version, SchemaVersion::V1);
    }

    #[test]
    fn deny_unknown_fields_on_file_search() {
        let json = r#"{"schema_version":"1.0","intent":"file_search","unknown_field":42}"#;
        let err = serde_json::from_str::<SearchIntent>(json).unwrap_err();
        assert!(err.to_string().contains("unknown_field"));
    }

    #[test]
    fn unknown_intent_rejected() {
        let json = r#"{"schema_version":"1.0","intent":"unknown_intent"}"#;
        assert!(serde_json::from_str::<SearchIntent>(json).is_err());
    }

    #[test]
    fn wrong_schema_version_rejected() {
        let json = r#"{"schema_version":"2.0","intent":"file_search"}"#;
        assert!(serde_json::from_str::<SearchIntent>(json).is_err());
    }

    fn result_for_sort(
        name: &str,
        modified: &str,
        created: &str,
        accessed: &str,
        size: u64,
    ) -> SearchResult {
        SearchResult {
            id: name.to_owned(),
            path: PathBuf::from(format!("/tmp/{name}")),
            name: name.to_owned(),
            source: BackendKind::Spotlight,
            match_type: MatchType::Filename,
            score: None,
            metadata: SearchResultMetadata {
                modified_time: Some(parse_time(modified)),
                created_time: Some(parse_time(created)),
                accessed_time: Some(parse_time(accessed)),
                size_bytes: Some(size),
                ..SearchResultMetadata::default()
            },
        }
    }

    fn parse_time(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn post_sort_covers_all_sort_orders() {
        let base = vec![
            result_for_sort(
                "bravo.txt",
                "2026-01-02T00:00:00Z",
                "2025-01-02T00:00:00Z",
                "2024-01-02T00:00:00Z",
                20,
            ),
            result_for_sort(
                "alpha.txt",
                "2026-01-01T00:00:00Z",
                "2025-01-01T00:00:00Z",
                "2024-01-01T00:00:00Z",
                10,
            ),
            result_for_sort(
                "charlie.txt",
                "2026-01-03T00:00:00Z",
                "2025-01-03T00:00:00Z",
                "2024-01-03T00:00:00Z",
                30,
            ),
        ];

        for (sort, expected) in [
            (
                SortOrder::ModifiedDesc,
                vec!["charlie.txt", "bravo.txt", "alpha.txt"],
            ),
            (
                SortOrder::ModifiedAsc,
                vec!["alpha.txt", "bravo.txt", "charlie.txt"],
            ),
            (
                SortOrder::CreatedDesc,
                vec!["charlie.txt", "bravo.txt", "alpha.txt"],
            ),
            (
                SortOrder::CreatedAsc,
                vec!["alpha.txt", "bravo.txt", "charlie.txt"],
            ),
            (
                SortOrder::AccessedDesc,
                vec!["charlie.txt", "bravo.txt", "alpha.txt"],
            ),
            (
                SortOrder::SizeDesc,
                vec!["charlie.txt", "bravo.txt", "alpha.txt"],
            ),
            (
                SortOrder::SizeAsc,
                vec!["alpha.txt", "bravo.txt", "charlie.txt"],
            ),
            (
                SortOrder::NameAsc,
                vec!["alpha.txt", "bravo.txt", "charlie.txt"],
            ),
            (
                SortOrder::NameDesc,
                vec!["charlie.txt", "bravo.txt", "alpha.txt"],
            ),
            (
                SortOrder::RelevanceDesc,
                vec!["bravo.txt", "alpha.txt", "charlie.txt"],
            ),
        ] {
            let mut results = base.clone();
            sort_results(&mut results, Some(sort));
            let names: Vec<_> = results.iter().map(|result| result.name.as_str()).collect();
            assert_eq!(names, expected, "{sort:?}");
        }
    }

    #[derive(Debug)]
    struct TestBackend {
        kind: BackendKind,
        status: ImplementationStatus,
        available: bool,
    }

    impl SearchBackend for TestBackend {
        fn kind(&self) -> BackendKind {
            self.kind
        }

        fn implementation_status(&self) -> ImplementationStatus {
            self.status
        }

        fn is_available(&self) -> bool {
            self.available
        }

        fn search<'a>(
            &'a self,
            _intent: &'a SearchIntent,
            _cancel: CancellationToken,
        ) -> BackendSearchFuture<'a> {
            Box::pin(async {
                Ok(backend_stream_from_error(SearchError::BackendUnavailable {
                    reason: "test backend".to_owned(),
                }))
            })
        }
    }

    #[test]
    fn production_chain_excludes_stub_backends() {
        let mut registry = BackendRegistry::new();
        registry.register(TestBackend {
            kind: BackendKind::WindowsSearch,
            status: ImplementationStatus::Stub,
            available: true,
        });
        registry.register(TestBackend {
            kind: BackendKind::Spotlight,
            status: ImplementationStatus::Real,
            available: true,
        });

        let production = registry.production_backends();

        assert_eq!(production.len(), 1);
        assert_eq!(production[0].kind(), BackendKind::Spotlight);
        assert_eq!(
            production[0].implementation_status(),
            ImplementationStatus::Real
        );
        assert_eq!(
            registry.select_available().map(SearchBackend::kind),
            Some(BackendKind::Spotlight)
        );
    }
}

// ============================================================
// §15. 单元测试：search_expanded default method
// ============================================================

#[cfg(test)]
mod search_expanded_default_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::{ExpandedSearchIntent, KeywordGroup};
    use std::sync::Mutex;

    /// 构造最小有效 `FileSearch` intent（与 harness expander 测试一致）。
    fn minimal_file_search_intent(kws: Vec<&str>) -> SearchIntent {
        SearchIntent::FileSearch(FileSearch {
            schema_version: SchemaVersion::V1,
            language: None,
            keywords: Some(kws.into_iter().map(str::to_owned).collect()),
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

    /// 记录传入 `search()` 的 intent 以便断言。
    #[derive(Debug, Default)]
    struct FakeBackend {
        called_with: Mutex<Option<SearchIntent>>,
    }

    impl SearchBackend for FakeBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Spotlight
        }

        fn is_available(&self) -> bool {
            true
        }

        fn search<'a>(
            &'a self,
            intent: &'a SearchIntent,
            cancel: CancellationToken,
        ) -> BackendSearchFuture<'a> {
            *self.called_with.lock().unwrap() = Some(intent.clone());
            Box::pin(async move { Ok(backend_stream_from_results(Vec::new(), cancel)) })
        }
    }

    #[tokio::test]
    async fn default_search_expanded_falls_back_to_search() {
        let intent = minimal_file_search_intent(vec!["工作汇报"]);

        let expanded = ExpandedSearchIntent {
            base: intent.clone(),
            keyword_groups: vec![KeywordGroup {
                head: "工作汇报".into(),
                synonyms: vec!["述职".into()],
            }],
            match_mode: MatchMode::default(),
        };

        let backend = FakeBackend::default();
        // 默认实现应将 expanded.base 传给 search()
        let _ = backend
            .search_expanded(&expanded, CancellationToken::new())
            .await;

        assert_eq!(backend.called_with.lock().unwrap().as_ref(), Some(&intent));
    }
}
