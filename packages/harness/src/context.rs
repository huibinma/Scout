//! Context Memory — 多轮上下文、`target_ref` 解析、`refine` 合并（schema §3.4）。
//!
//! MVP-06 的产出。本模块只维护"最近一轮"的 intent + 结果（v1.0 容量 1），
//! 提供：
//!
//! - [`ContextMemory::record`] / [`ContextMemory::last_turn`] / [`ContextMemory::clear`]
//! - [`ContextMemory::resolve_target_ref`] —— 把 `TargetRef::LastResults { selector }`
//!   或 `TargetRef::Path` 解析成绝对路径列表
//! - [`ContextMemory::apply_refine`] —— 按 schema §3.4 合并语义生成新的基准 intent
//!
//! # 合并语义（与 schema §3.4 对齐）
//!
//! 1. **覆盖**：`delta` 中 Some 的字段完整覆盖基准对应字段。
//! 2. **清空**：`clear` 列表中的字段路径被移除（设为 `None`）。
//! 3. **冲突**：同一字段同时出现在 `clear` 与 `delta` 时，**以 `clear` 为准**
//!    （schema §5 运行时附加约束），delta 同名字段忽略并通过 [`RefineConflict`]
//!    回报上层（不阻止合并）。
//! 4. **类型约束**：base 必须是 `FileSearch` 或 `MediaSearch`；media 专有字段
//!    （artist/title/album/genre/quality/duration）不允许用于 FileSearch 基准。

#![allow(
    clippy::too_many_lines,
    clippy::doc_markdown,
    clippy::doc_link_with_quotes
)]

use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

use scout_search_backend::{
    ClearableField, FileSearch, MediaSearch, Refine, RefineDelta, SearchIntent, SearchResult,
    TargetRef, TargetSelector,
};

// ============================================================
// §1. LastTurn + ContextMemory
// ============================================================

/// 一轮搜索的快照：intent + 结果 + 记录时间。
#[derive(Debug, Clone)]
pub struct LastTurn {
    /// 该轮执行的 intent。
    pub intent: SearchIntent,
    /// 该轮 backend 返回的结果（顺序即 UI 展示顺序，1-based selector 与之对齐）。
    pub results: Vec<SearchResult>,
    /// 记录时间。用于未来的 TTL / 过期策略，v1.0 不主动过期。
    pub recorded_at: SystemTime,
}

/// 多轮上下文存储。v1.0 仅保留最近一轮。
///
/// Tool Loop（MVP-04）每次成功调用 backend 后通过 [`ContextMemory::record`]
/// 写入；Intent Router（MVP-05）解析 refine / file_action 时通过
/// [`ContextMemory::last_turn`] 与 [`ContextMemory::resolve_target_ref`]
/// / [`ContextMemory::apply_refine`] 消费。
#[derive(Debug, Default)]
pub struct ContextMemory {
    last: Option<LastTurn>,
}

impl ContextMemory {
    /// 创建空 Context Memory。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一轮。覆盖之前的快照（v1.0 容量 1）。
    pub fn record(&mut self, intent: SearchIntent, results: Vec<SearchResult>) {
        self.last = Some(LastTurn {
            intent,
            results,
            recorded_at: SystemTime::now(),
        });
    }

    /// 取最近一轮的引用。
    #[must_use]
    pub fn last_turn(&self) -> Option<&LastTurn> {
        self.last.as_ref()
    }

    /// 清空上下文（用户新开会话时调用）。
    pub fn clear(&mut self) {
        self.last = None;
    }

    /// 当前是否有可用上下文。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.last.is_none()
    }

    /// 解析 [`TargetRef`] 到绝对路径列表。
    ///
    /// `TargetRef::Path` 直接返回包装路径；`TargetRef::LastResults` 按 1-based
    /// 索引取最近一轮的结果。
    pub fn resolve_target_ref(
        &self,
        target_ref: &TargetRef,
    ) -> Result<Vec<PathBuf>, TargetRefError> {
        match target_ref {
            TargetRef::Path { value } => Ok(vec![PathBuf::from(value)]),
            TargetRef::Paths { values } => {
                if values.is_empty() {
                    return Err(TargetRefError::EmptyIndices);
                }
                Ok(values.iter().map(PathBuf::from).collect())
            }
            TargetRef::LastResults { selector } => self.resolve_last_results(selector),
        }
    }

    fn resolve_last_results(
        &self,
        selector: &TargetSelector,
    ) -> Result<Vec<PathBuf>, TargetRefError> {
        let last = self.last.as_ref().ok_or(TargetRefError::NoLastResults)?;
        let available = last.results.len();

        match selector {
            TargetSelector::All => Ok(last.results.iter().map(|r| r.path.clone()).collect()),
            TargetSelector::Index { value } => {
                let idx = pick_one_indexed(*value, available)?;
                Ok(vec![last.results[idx].path.clone()])
            }
            TargetSelector::Indices { values } => {
                if values.is_empty() {
                    return Err(TargetRefError::EmptyIndices);
                }
                let mut out = Vec::with_capacity(values.len());
                for value in values {
                    let idx = pick_one_indexed(*value, available)?;
                    out.push(last.results[idx].path.clone());
                }
                Ok(out)
            }
        }
    }

    /// 按 schema §3.4 合并语义把 [`Refine`] 应用到最近一轮 intent，生成新的基准 intent。
    ///
    /// 返回的新 intent 必定是 `FileSearch` 或 `MediaSearch`（即 refine 的合法基准）。
    /// 若 `delta` 与 `clear` 在同一字段冲突，以 `clear` 为准；冲突详情通过
    /// [`RefineOutcome::conflicts`] 回报。
    pub fn apply_refine(&self, refine: &Refine) -> Result<RefineOutcome, RefineMergeError> {
        let last = self.last.as_ref().ok_or(RefineMergeError::NoLastIntent)?;
        let clear_set: Vec<ClearableField> = refine.clear.clone().unwrap_or_default();

        match &last.intent {
            SearchIntent::FileSearch(fs) => {
                let mut base = fs.clone();
                let conflicts = apply_to_file_search(&mut base, &refine.delta, &clear_set)?;
                if let Some(lang) = refine.language {
                    base.language = Some(lang);
                }
                Ok(RefineOutcome {
                    intent: SearchIntent::FileSearch(base),
                    conflicts,
                })
            }
            SearchIntent::MediaSearch(ms) => {
                let mut base = ms.clone();
                let conflicts = apply_to_media_search(&mut base, &refine.delta, &clear_set)?;
                if let Some(lang) = refine.language {
                    base.language = Some(lang);
                }
                Ok(RefineOutcome {
                    intent: SearchIntent::MediaSearch(base),
                    conflicts,
                })
            }
            other => Err(RefineMergeError::InvalidBase {
                intent_kind: intent_kind_name(other),
            }),
        }
    }
}

// ============================================================
// §2. 错误与冲突类型
// ============================================================

/// `resolve_target_ref` 的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetRefError {
    /// 上下文中无最近一轮结果。
    NoLastResults,
    /// `Index` / `Indices` 中的某个 1-based 索引超出可用范围。
    IndexOutOfRange {
        /// 用户请求的 1-based 索引。
        requested: u32,
        /// 上一轮可用的结果数。
        available: usize,
    },
    /// `Indices.values` 为空（schema 应当在校验层阻止，此处兜底）。
    EmptyIndices,
}

impl fmt::Display for TargetRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoLastResults => f.write_str("no last results recorded in context memory"),
            Self::IndexOutOfRange {
                requested,
                available,
            } => write!(
                f,
                "target_ref index {requested} out of range (available {available})"
            ),
            Self::EmptyIndices => f.write_str("target_ref indices list is empty"),
        }
    }
}

impl std::error::Error for TargetRefError {}

/// `apply_refine` 的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefineMergeError {
    /// 上下文中无 last intent。
    NoLastIntent,
    /// 基准 intent 不是 `FileSearch` / `MediaSearch`（v1.0 仅这两个可作为 refine 基准）。
    InvalidBase {
        /// 当前基准 intent 的类型名。
        intent_kind: &'static str,
    },
    /// `delta` 中出现了基准 intent 不支持的字段（如 file_search 基准上设 artist）。
    FieldNotApplicable {
        /// 字段名。
        field: &'static str,
        /// 基准 intent 类型名。
        base: &'static str,
    },
}

impl fmt::Display for RefineMergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoLastIntent => f.write_str("no last intent recorded in context memory"),
            Self::InvalidBase { intent_kind } => {
                write!(
                    f,
                    "refine base intent is {intent_kind}, must be file_search or media_search"
                )
            }
            Self::FieldNotApplicable { field, base } => {
                write!(
                    f,
                    "refine delta field `{field}` not applicable to base intent `{base}`"
                )
            }
        }
    }
}

impl std::error::Error for RefineMergeError {}

/// `delta` 与 `clear` 冲突的记录。Tracing 应当上报此条目（schema §5 运行时约束）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefineConflict {
    /// 同时出现在 `clear` 与 `delta` 的字段。
    pub field: ClearableField,
}

/// `apply_refine` 的成功输出：合并后的 intent + 冲突记录。
#[derive(Debug, Clone, PartialEq)]
pub struct RefineOutcome {
    /// 合并后的基准 intent（必为 FileSearch / MediaSearch）。
    pub intent: SearchIntent,
    /// `clear` 与 `delta` 同名冲突列表（取 clear，忽略 delta 同名字段）。
    pub conflicts: Vec<RefineConflict>,
}

// ============================================================
// §3. 私有合并辅助
// ============================================================

fn intent_kind_name(intent: &SearchIntent) -> &'static str {
    match intent {
        SearchIntent::FileSearch(_) => "file_search",
        SearchIntent::MediaSearch(_) => "media_search",
        SearchIntent::FileAction(_) => "file_action",
        SearchIntent::Refine(_) => "refine",
        SearchIntent::Clarify(_) => "clarify",
    }
}

fn pick_one_indexed(value: u32, available: usize) -> Result<usize, TargetRefError> {
    if value == 0 || (value as usize) > available {
        return Err(TargetRefError::IndexOutOfRange {
            requested: value,
            available,
        });
    }
    Ok(value as usize - 1)
}

/// 把 RefineDelta 应用到 FileSearch 基准。
///
/// - 先按 `clear_set` 清空字段；
/// - 然后按 `delta` Some 字段覆盖（跳过已清空的同名字段，记入 conflicts）；
/// - media-only 字段（artist/title/album/genre/quality/duration）出现在 delta 时报错。
fn apply_to_file_search(
    base: &mut FileSearch,
    delta: &RefineDelta,
    clear_set: &[ClearableField],
) -> Result<Vec<RefineConflict>, RefineMergeError> {
    reject_media_only_in_delta(delta, "file_search")?;

    // 1. clear
    if clear_set.contains(&ClearableField::Location) {
        base.location = None;
    }
    if clear_set.contains(&ClearableField::Extensions) {
        base.extensions = None;
    }
    if clear_set.contains(&ClearableField::FileType) {
        base.file_type = None;
    }
    if clear_set.contains(&ClearableField::Keywords) {
        base.keywords = None;
    }
    if clear_set.contains(&ClearableField::ModifiedTime) {
        base.modified_time = None;
    }
    if clear_set.contains(&ClearableField::CreatedTime) {
        base.created_time = None;
    }
    if clear_set.contains(&ClearableField::AccessedTime) {
        base.accessed_time = None;
    }
    if clear_set.contains(&ClearableField::Size) {
        base.size = None;
    }
    if clear_set.contains(&ClearableField::ExcludeExtensions) {
        base.exclude_extensions = None;
    }
    if clear_set.contains(&ClearableField::ExcludeFileType) {
        base.exclude_file_type = None;
    }

    // 2. delta（同名 clear 字段冲突时取 clear，记录到 conflicts）
    let mut conflicts = Vec::new();
    apply_optional(
        &mut base.location,
        delta.location.clone(),
        ClearableField::Location,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.extensions,
        delta.extensions.clone(),
        ClearableField::Extensions,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.file_type,
        delta.file_type.clone(),
        ClearableField::FileType,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.keywords,
        delta.keywords.clone(),
        ClearableField::Keywords,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.modified_time,
        delta.modified_time,
        ClearableField::ModifiedTime,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.created_time,
        delta.created_time,
        ClearableField::CreatedTime,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.accessed_time,
        delta.accessed_time,
        ClearableField::AccessedTime,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.size,
        delta.size,
        ClearableField::Size,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.exclude_extensions,
        delta.exclude_extensions.clone(),
        ClearableField::ExcludeExtensions,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.exclude_file_type,
        delta.exclude_file_type.clone(),
        ClearableField::ExcludeFileType,
        clear_set,
        &mut conflicts,
    );
    // sort / limit 不在 clear 白名单中，直接覆盖；无冲突可能
    if let Some(sort) = delta.sort {
        base.sort = Some(sort);
    }
    if let Some(limit) = delta.limit {
        base.limit = Some(limit);
    }

    Ok(conflicts)
}

/// 把 RefineDelta 应用到 MediaSearch 基准。
#[allow(clippy::unnecessary_wraps)] // 对称于 apply_to_file_search 的返回签名
fn apply_to_media_search(
    base: &mut MediaSearch,
    delta: &RefineDelta,
    clear_set: &[ClearableField],
) -> Result<Vec<RefineConflict>, RefineMergeError> {
    // 1. clear
    if clear_set.contains(&ClearableField::Location) {
        base.location = None;
    }
    if clear_set.contains(&ClearableField::Extensions) {
        base.extensions = None;
    }
    if clear_set.contains(&ClearableField::FileType) {
        base.file_type = None;
    }
    if clear_set.contains(&ClearableField::Keywords) {
        base.keywords = None;
    }
    if clear_set.contains(&ClearableField::ModifiedTime) {
        base.modified_time = None;
    }
    if clear_set.contains(&ClearableField::CreatedTime) {
        base.created_time = None;
    }
    if clear_set.contains(&ClearableField::AccessedTime) {
        base.accessed_time = None;
    }
    if clear_set.contains(&ClearableField::Size) {
        base.size = None;
    }
    if clear_set.contains(&ClearableField::ExcludeExtensions) {
        base.exclude_extensions = None;
    }
    if clear_set.contains(&ClearableField::ExcludeFileType) {
        base.exclude_file_type = None;
    }
    if clear_set.contains(&ClearableField::Artist) {
        base.artist = None;
    }
    if clear_set.contains(&ClearableField::Title) {
        base.title = None;
    }
    if clear_set.contains(&ClearableField::Album) {
        base.album = None;
    }
    if clear_set.contains(&ClearableField::Genre) {
        base.genre = None;
    }
    if clear_set.contains(&ClearableField::Quality) {
        base.quality = None;
    }
    if clear_set.contains(&ClearableField::Duration) {
        base.duration = None;
    }

    // 2. delta
    let mut conflicts = Vec::new();
    apply_optional(
        &mut base.location,
        delta.location.clone(),
        ClearableField::Location,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.extensions,
        delta.extensions.clone(),
        ClearableField::Extensions,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.file_type,
        delta.file_type.clone(),
        ClearableField::FileType,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.keywords,
        delta.keywords.clone(),
        ClearableField::Keywords,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.modified_time,
        delta.modified_time,
        ClearableField::ModifiedTime,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.created_time,
        delta.created_time,
        ClearableField::CreatedTime,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.accessed_time,
        delta.accessed_time,
        ClearableField::AccessedTime,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.size,
        delta.size,
        ClearableField::Size,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.exclude_extensions,
        delta.exclude_extensions.clone(),
        ClearableField::ExcludeExtensions,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.exclude_file_type,
        delta.exclude_file_type.clone(),
        ClearableField::ExcludeFileType,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.artist,
        delta.artist.clone(),
        ClearableField::Artist,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.title,
        delta.title.clone(),
        ClearableField::Title,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.album,
        delta.album.clone(),
        ClearableField::Album,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.genre,
        delta.genre.clone(),
        ClearableField::Genre,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.quality,
        delta.quality,
        ClearableField::Quality,
        clear_set,
        &mut conflicts,
    );
    apply_optional(
        &mut base.duration,
        delta.duration,
        ClearableField::Duration,
        clear_set,
        &mut conflicts,
    );
    if let Some(sort) = delta.sort {
        base.sort = Some(sort);
    }
    if let Some(limit) = delta.limit {
        base.limit = Some(limit);
    }

    Ok(conflicts)
}

fn apply_optional<T>(
    target: &mut Option<T>,
    delta_value: Option<T>,
    field: ClearableField,
    clear_set: &[ClearableField],
    conflicts: &mut Vec<RefineConflict>,
) {
    let Some(value) = delta_value else { return };
    if clear_set.contains(&field) {
        // 冲突：以 clear 为准，忽略 delta；记录冲突供 tracing 上报
        conflicts.push(RefineConflict { field });
        return;
    }
    *target = Some(value);
}

fn reject_media_only_in_delta(
    delta: &RefineDelta,
    base: &'static str,
) -> Result<(), RefineMergeError> {
    if delta.artist.is_some() {
        return Err(RefineMergeError::FieldNotApplicable {
            field: "artist",
            base,
        });
    }
    if delta.title.is_some() {
        return Err(RefineMergeError::FieldNotApplicable {
            field: "title",
            base,
        });
    }
    if delta.album.is_some() {
        return Err(RefineMergeError::FieldNotApplicable {
            field: "album",
            base,
        });
    }
    if delta.genre.is_some() {
        return Err(RefineMergeError::FieldNotApplicable {
            field: "genre",
            base,
        });
    }
    if delta.quality.is_some() {
        return Err(RefineMergeError::FieldNotApplicable {
            field: "quality",
            base,
        });
    }
    if delta.duration.is_some() {
        return Err(RefineMergeError::FieldNotApplicable {
            field: "duration",
            base,
        });
    }
    Ok(())
}

// ============================================================
// §4. 单元测试
// ============================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use scout_search_backend::{
        BackendKind, BaseRef, FileType, Language, Location, MatchType, MediaSearch, MediaType,
        RelativeTime, SchemaVersion, SearchResult, SearchResultMetadata, SizeExpression, SizeUnit,
        SortOrder, TimeExpression,
    };

    fn mk_result(idx: usize) -> SearchResult {
        SearchResult {
            id: format!("id-{idx}"),
            path: PathBuf::from(format!("/tmp/synthetic-{idx}.txt")),
            name: format!("synthetic-{idx}.txt"),
            source: BackendKind::Spotlight,
            match_type: MatchType::Filename,
            score: None,
            metadata: SearchResultMetadata::default(),
        }
    }

    fn mk_file_search() -> FileSearch {
        FileSearch {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            keywords: None,
            extensions: Some(vec!["pptx".to_owned()]),
            file_type: Some(vec![FileType::Presentation]),
            location: Some(Location {
                hint: Some("下载".to_owned()),
                include: None,
                exclude: None,
            }),
            modified_time: Some(TimeExpression::Relative {
                value: RelativeTime::Yesterday,
            }),
            created_time: None,
            accessed_time: None,
            size: None,
            exclude_extensions: None,
            exclude_file_type: None,
            sort: Some(SortOrder::ModifiedDesc),
            limit: Some(50),
        }
    }

    fn mk_media_search() -> MediaSearch {
        MediaSearch {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
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
        }
    }

    // ---- ContextMemory 基本行为 ----

    #[test]
    fn empty_memory_returns_none() {
        let mem = ContextMemory::new();
        assert!(mem.is_empty());
        assert!(mem.last_turn().is_none());
    }

    #[test]
    fn record_overwrites_previous_turn() {
        let mut mem = ContextMemory::new();
        mem.record(
            SearchIntent::FileSearch(mk_file_search()),
            vec![mk_result(1)],
        );
        mem.record(
            SearchIntent::FileSearch(mk_file_search()),
            vec![mk_result(2)],
        );
        assert_eq!(mem.last_turn().unwrap().results.len(), 1);
        assert_eq!(mem.last_turn().unwrap().results[0].id, "id-2".to_owned());
    }

    #[test]
    fn clear_resets_memory() {
        let mut mem = ContextMemory::new();
        mem.record(
            SearchIntent::FileSearch(mk_file_search()),
            vec![mk_result(1)],
        );
        mem.clear();
        assert!(mem.is_empty());
    }

    // ---- target_ref 解析 ----

    #[test]
    fn resolve_target_ref_path_returns_value() {
        let mem = ContextMemory::new();
        let target = TargetRef::Path {
            value: "/Users/r/budget.pdf".to_owned(),
        };
        let resolved = mem.resolve_target_ref(&target).unwrap();
        assert_eq!(resolved, vec![PathBuf::from("/Users/r/budget.pdf")]);
    }

    #[test]
    fn resolve_target_ref_last_results_index() {
        let mut mem = ContextMemory::new();
        mem.record(
            SearchIntent::FileSearch(mk_file_search()),
            vec![mk_result(1), mk_result(2), mk_result(3)],
        );

        let target = TargetRef::LastResults {
            selector: TargetSelector::Index { value: 2 },
        };
        let resolved = mem.resolve_target_ref(&target).unwrap();
        assert_eq!(resolved, vec![PathBuf::from("/tmp/synthetic-2.txt")]);
    }

    #[test]
    fn resolve_target_ref_last_results_indices() {
        let mut mem = ContextMemory::new();
        mem.record(
            SearchIntent::FileSearch(mk_file_search()),
            vec![mk_result(1), mk_result(2), mk_result(3)],
        );

        let target = TargetRef::LastResults {
            selector: TargetSelector::Indices { values: vec![1, 3] },
        };
        let resolved = mem.resolve_target_ref(&target).unwrap();
        assert_eq!(
            resolved,
            vec![
                PathBuf::from("/tmp/synthetic-1.txt"),
                PathBuf::from("/tmp/synthetic-3.txt"),
            ]
        );
    }

    #[test]
    fn resolve_target_ref_last_results_all() {
        let mut mem = ContextMemory::new();
        mem.record(
            SearchIntent::FileSearch(mk_file_search()),
            vec![mk_result(1), mk_result(2)],
        );
        let target = TargetRef::LastResults {
            selector: TargetSelector::All,
        };
        let resolved = mem.resolve_target_ref(&target).unwrap();
        assert_eq!(resolved.len(), 2);
    }

    #[test]
    fn resolve_target_ref_no_last_results() {
        let mem = ContextMemory::new();
        let target = TargetRef::LastResults {
            selector: TargetSelector::Index { value: 1 },
        };
        assert_eq!(
            mem.resolve_target_ref(&target),
            Err(TargetRefError::NoLastResults)
        );
    }

    #[test]
    fn resolve_target_ref_index_out_of_range() {
        let mut mem = ContextMemory::new();
        mem.record(
            SearchIntent::FileSearch(mk_file_search()),
            vec![mk_result(1)],
        );
        let target = TargetRef::LastResults {
            selector: TargetSelector::Index { value: 5 },
        };
        let err = mem.resolve_target_ref(&target).unwrap_err();
        assert_eq!(
            err,
            TargetRefError::IndexOutOfRange {
                requested: 5,
                available: 1
            }
        );

        let target_zero = TargetRef::LastResults {
            selector: TargetSelector::Index { value: 0 },
        };
        assert!(matches!(
            mem.resolve_target_ref(&target_zero),
            Err(TargetRefError::IndexOutOfRange { requested: 0, .. })
        ));
    }

    #[test]
    fn resolve_target_ref_empty_indices() {
        let mut mem = ContextMemory::new();
        mem.record(
            SearchIntent::FileSearch(mk_file_search()),
            vec![mk_result(1)],
        );
        let target = TargetRef::LastResults {
            selector: TargetSelector::Indices { values: Vec::new() },
        };
        assert_eq!(
            mem.resolve_target_ref(&target),
            Err(TargetRefError::EmptyIndices)
        );
    }

    #[test]
    fn resolve_target_ref_paths_returns_all() {
        let mem = ContextMemory::new();
        let target = TargetRef::Paths {
            values: vec!["/tmp/a.pdf".to_owned(), "/tmp/b.pdf".to_owned()],
        };
        let got = mem.resolve_target_ref(&target).unwrap();
        assert_eq!(
            got,
            vec![PathBuf::from("/tmp/a.pdf"), PathBuf::from("/tmp/b.pdf")]
        );
    }

    #[test]
    fn resolve_target_ref_paths_empty_errs() {
        let mem = ContextMemory::new();
        let target = TargetRef::Paths { values: vec![] };
        assert!(matches!(
            mem.resolve_target_ref(&target),
            Err(TargetRefError::EmptyIndices)
        ));
    }

    // ---- refine 合并 — schema §7.5 / §7.8 契约用例 ----

    /// §7.5 #31：用户说"只看下载目录里的"。基准是 #1 file_search "昨天编辑过的 ppt"。
    /// 验证 delta.location 覆盖基准 location。
    #[test]
    fn refine_case_31_overwrite_location() {
        let mut mem = ContextMemory::new();
        let mut base = mk_file_search();
        base.location = Some(Location {
            hint: Some("桌面".to_owned()),
            include: None,
            exclude: None,
        });
        mem.record(SearchIntent::FileSearch(base), Vec::new());

        let refine = Refine {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            base_ref: BaseRef::LastIntent,
            delta: RefineDelta {
                location: Some(Location {
                    hint: Some("下载".to_owned()),
                    include: None,
                    exclude: None,
                }),
                ..RefineDelta::default()
            },
            clear: None,
        };
        let out = mem.apply_refine(&refine).unwrap();
        let SearchIntent::FileSearch(merged) = out.intent else {
            panic!("expected FileSearch")
        };
        assert_eq!(merged.location.unwrap().hint, Some("下载".to_owned()));
        assert!(out.conflicts.is_empty());
    }

    /// §7.5 #32：用户说"只看 pdf"。delta.extensions 覆盖基准。
    #[test]
    fn refine_case_32_overwrite_extensions_and_filetype() {
        let mut mem = ContextMemory::new();
        mem.record(SearchIntent::FileSearch(mk_file_search()), Vec::new());

        let refine = Refine {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            base_ref: BaseRef::LastIntent,
            delta: RefineDelta {
                extensions: Some(vec!["pdf".to_owned()]),
                file_type: Some(vec![FileType::Document]),
                ..RefineDelta::default()
            },
            clear: None,
        };
        let out = mem.apply_refine(&refine).unwrap();
        let SearchIntent::FileSearch(merged) = out.intent else {
            panic!("expected FileSearch")
        };
        assert_eq!(merged.extensions.as_deref(), Some(&["pdf".to_owned()][..]));
        assert_eq!(merged.file_type, Some(vec![FileType::Document]));
    }

    /// §7.5 #33：用户说"排除视频"。delta.exclude_file_type 追加到基准。
    #[test]
    fn refine_case_33_exclude_file_type() {
        let mut mem = ContextMemory::new();
        mem.record(SearchIntent::FileSearch(mk_file_search()), Vec::new());

        let refine = Refine {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            base_ref: BaseRef::LastIntent,
            delta: RefineDelta {
                exclude_file_type: Some(vec![FileType::Video]),
                ..RefineDelta::default()
            },
            clear: None,
        };
        let out = mem.apply_refine(&refine).unwrap();
        let SearchIntent::FileSearch(merged) = out.intent else {
            panic!("expected FileSearch")
        };
        assert_eq!(merged.exclude_file_type, Some(vec![FileType::Video]));
    }

    /// §7.5 #35：用户说"按大小倒序"。delta.sort 覆盖基准 sort。
    #[test]
    fn refine_case_35_overwrite_sort() {
        let mut mem = ContextMemory::new();
        mem.record(SearchIntent::FileSearch(mk_file_search()), Vec::new());

        let refine = Refine {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            base_ref: BaseRef::LastIntent,
            delta: RefineDelta {
                sort: Some(SortOrder::SizeDesc),
                ..RefineDelta::default()
            },
            clear: None,
        };
        let out = mem.apply_refine(&refine).unwrap();
        let SearchIntent::FileSearch(merged) = out.intent else {
            panic!("expected FileSearch")
        };
        assert_eq!(merged.sort, Some(SortOrder::SizeDesc));
    }

    /// §7.8 #43：clear=["location"] 移除基准 location。
    #[test]
    fn refine_case_43_clear_location() {
        let mut mem = ContextMemory::new();
        mem.record(SearchIntent::FileSearch(mk_file_search()), Vec::new());

        let refine = Refine {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            base_ref: BaseRef::LastIntent,
            delta: RefineDelta::default(),
            clear: Some(vec![ClearableField::Location]),
        };
        let out = mem.apply_refine(&refine).unwrap();
        let SearchIntent::FileSearch(merged) = out.intent else {
            panic!("expected FileSearch")
        };
        assert!(merged.location.is_none());
    }

    /// §7.8 #45：refine 加 exclude_file_type；通过 Context Memory 合并后基准 intent 保留该字段。
    #[test]
    fn refine_case_45_exclude_file_type_persists_into_base() {
        let mut mem = ContextMemory::new();
        // 基准：下载目录 + size > 100MB
        let mut base = mk_file_search();
        base.size = Some(SizeExpression::GreaterThan {
            value: 100.0,
            unit: SizeUnit::Mb,
        });
        base.sort = Some(SortOrder::SizeDesc);
        base.modified_time = None;
        base.extensions = None;
        base.file_type = None;
        mem.record(SearchIntent::FileSearch(base), Vec::new());

        let refine = Refine {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            base_ref: BaseRef::LastIntent,
            delta: RefineDelta {
                exclude_file_type: Some(vec![FileType::Archive]),
                ..RefineDelta::default()
            },
            clear: None,
        };
        let out = mem.apply_refine(&refine).unwrap();
        let SearchIntent::FileSearch(merged) = out.intent else {
            panic!("expected FileSearch")
        };
        assert_eq!(merged.exclude_file_type, Some(vec![FileType::Archive]));
        // 基准其他字段保留
        assert_eq!(
            merged.location.as_ref().unwrap().hint,
            Some("下载".to_owned())
        );
        assert!(matches!(
            merged.size,
            Some(SizeExpression::GreaterThan { .. })
        ));
        assert_eq!(merged.sort, Some(SortOrder::SizeDesc));
    }

    // ---- refine 冲突 ----

    #[test]
    fn refine_clear_wins_when_field_in_both_clear_and_delta() {
        let mut mem = ContextMemory::new();
        mem.record(SearchIntent::FileSearch(mk_file_search()), Vec::new());

        let refine = Refine {
            schema_version: SchemaVersion::V1,
            language: None,
            base_ref: BaseRef::LastIntent,
            delta: RefineDelta {
                location: Some(Location {
                    hint: Some("桌面".to_owned()),
                    include: None,
                    exclude: None,
                }),
                ..RefineDelta::default()
            },
            clear: Some(vec![ClearableField::Location]),
        };
        let out = mem.apply_refine(&refine).unwrap();
        let SearchIntent::FileSearch(merged) = out.intent else {
            panic!("expected FileSearch")
        };
        // clear 胜出：location 应该是 None
        assert!(merged.location.is_none());
        // 冲突被记录
        assert_eq!(
            out.conflicts,
            vec![RefineConflict {
                field: ClearableField::Location
            }]
        );
    }

    // ---- refine 错误分支 ----

    #[test]
    fn refine_without_last_intent_errors() {
        let mem = ContextMemory::new();
        let refine = Refine {
            schema_version: SchemaVersion::V1,
            language: None,
            base_ref: BaseRef::LastIntent,
            delta: RefineDelta::default(),
            clear: None,
        };
        assert_eq!(
            mem.apply_refine(&refine),
            Err(RefineMergeError::NoLastIntent)
        );
    }

    #[test]
    fn refine_with_clarify_base_errors() {
        use scout_search_backend::{Clarify, ClarifyReason};
        let mut mem = ContextMemory::new();
        mem.record(
            SearchIntent::Clarify(Clarify {
                schema_version: SchemaVersion::V1,
                language: Some(Language::Zh),
                reason: ClarifyReason::AmbiguousTime,
                question: "什么时候?".to_owned(),
                options: None,
            }),
            Vec::new(),
        );
        let refine = Refine {
            schema_version: SchemaVersion::V1,
            language: None,
            base_ref: BaseRef::LastIntent,
            delta: RefineDelta::default(),
            clear: None,
        };
        let err = mem.apply_refine(&refine).unwrap_err();
        assert!(matches!(
            err,
            RefineMergeError::InvalidBase {
                intent_kind: "clarify"
            }
        ));
    }

    #[test]
    fn refine_media_only_field_on_file_search_base_errors() {
        let mut mem = ContextMemory::new();
        mem.record(SearchIntent::FileSearch(mk_file_search()), Vec::new());
        let refine = Refine {
            schema_version: SchemaVersion::V1,
            language: None,
            base_ref: BaseRef::LastIntent,
            delta: RefineDelta {
                artist: Some("周华健".to_owned()),
                ..RefineDelta::default()
            },
            clear: None,
        };
        let err = mem.apply_refine(&refine).unwrap_err();
        assert_eq!(
            err,
            RefineMergeError::FieldNotApplicable {
                field: "artist",
                base: "file_search"
            }
        );
    }

    // ---- media_search 基准的 refine ----

    #[test]
    fn refine_overrides_artist_on_media_search_base() {
        let mut mem = ContextMemory::new();
        mem.record(SearchIntent::MediaSearch(mk_media_search()), Vec::new());
        let refine = Refine {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            base_ref: BaseRef::LastIntent,
            delta: RefineDelta {
                artist: Some("罗大佑".to_owned()),
                ..RefineDelta::default()
            },
            clear: None,
        };
        let out = mem.apply_refine(&refine).unwrap();
        let SearchIntent::MediaSearch(merged) = out.intent else {
            panic!("expected MediaSearch")
        };
        assert_eq!(merged.artist, Some("罗大佑".to_owned()));
    }

    #[test]
    fn refine_language_overrides_base_language() {
        let mut mem = ContextMemory::new();
        let mut base = mk_file_search();
        base.language = Some(Language::Zh);
        mem.record(SearchIntent::FileSearch(base), Vec::new());

        let refine = Refine {
            schema_version: SchemaVersion::V1,
            language: Some(Language::En),
            base_ref: BaseRef::LastIntent,
            delta: RefineDelta::default(),
            clear: None,
        };
        let out = mem.apply_refine(&refine).unwrap();
        let SearchIntent::FileSearch(merged) = out.intent else {
            panic!("expected FileSearch")
        };
        assert_eq!(merged.language, Some(Language::En));
    }
}
