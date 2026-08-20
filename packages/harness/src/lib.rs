//! `scout-harness` — Agent Harness 核心组件。
//!
//! 本 crate 是 MVP-01 的产出，作为 M1 子阶段后续 task（Schema Validator / Policy /
//! Tool Loop / Intent Router / Context Memory / Streaming / Tracing / Capability /
//! Fallback / FileActionTool）的前置。
//!
//! # 设计要点
//!
//! - [`Tool`] 是所有可调用工具的最小公约数；任何 [`SearchBackend`] 通过 [`SearchTool`]
//!   适配为 [`Tool`]；未来的 FileActionTool（MVP-10A）也将实现 [`Tool`]。
//! - [`ToolRegistry`] 按 id 索引，支持按 [`ToolKind`] 过滤、按 [`SupportedIntent`]
//!   检索能力匹配的工具子集。
//! - **生产 fallback 链必须排除 [`ImplementationStatus::Stub`]**
//!   ——这是 ROADMAP §6.1 / §6.2 的硬指标。[`ToolRegistry::production_tools`]
//!   与 [`ToolRegistry::production_tools_supporting`] 自动剔除 stub。
//!
//! # 与 `BackendRegistry` 的关系
//!
//! `BackendRegistry`（位于 `scout-search-backend`）只管 [`SearchBackend`]，
//! 是 PROTO-04 的产出。`ToolRegistry` 高一层，管所有工具种类。两者并存：
//! - 仅在 backend 选择层（如 CLI 直接调单一 backend）使用 `BackendRegistry`；
//! - 在 Harness 调度层（Intent Router / Fallback Chain / Policy Engine 之上）
//!   使用 `ToolRegistry`。

pub mod intent_router;
pub mod policy;
pub mod streaming;
pub mod tool_loop;

pub use intent_router::{IntentRouter, RouteError};
pub use policy::{PermissionLevel, PolicyAction, PolicyDecision, PolicyEngine};
pub use scout_search_backend::ImplementationStatus;
use scout_search_backend::{
    BackendKind, BackendStream, CancellationToken, FileActionKind, SearchBackend, SearchError,
    SearchIntent,
};
pub use streaming::{IntoStream, ResultEvent, ResultStream, StreamCancellation, StreamSink};
pub use tool_loop::{LoopError, LoopOutcome, LoopStep, ToolLoopController};

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

// M1 子阶段各模块（MVP-02 ~ MVP-10A）。Tool Registry 已在本文件落地（MVP-01）。
pub mod context;
pub mod file_action_tool;

// BETA-11 同义词关键词扩展。
pub mod synonym;
pub use synonym::{
    ExpanderError, LayeredSynonymExpander, NoopExpander, SynonymExpander, UserDictError, UserGroup,
    UserIndex, YamlSynonymExpander,
};

// ============================================================
// §1. ToolKind / SupportedIntent / ToolCapability
// ============================================================

/// 工具种类。每个 [`Tool`] 必定属于其中一种。
///
/// 设计为非穷举（`#[non_exhaustive]`），后续阶段可在不破坏 API 的前提下加入
/// 索引器 / OCR / 模型推理等工具类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ToolKind {
    /// `SearchBackend` 适配而来的搜索工具。
    Search,
    /// 文件操作工具（MVP-10A 引入）。
    FileAction,
}

/// `SearchIntent` 五个变体的轻量枚举（不带数据，仅用于声明工具能力）。
///
/// 与 [`SearchIntent`] 一一对应：
/// - [`SupportedIntent::FileSearch`] ↔ `SearchIntent::FileSearch`
/// - [`SupportedIntent::MediaSearch`] ↔ `SearchIntent::MediaSearch`
/// - [`SupportedIntent::FileAction`] ↔ `SearchIntent::FileAction`
/// - [`SupportedIntent::Refine`] ↔ `SearchIntent::Refine`
/// - [`SupportedIntent::Clarify`] ↔ `SearchIntent::Clarify`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SupportedIntent {
    /// 通用文件搜索
    FileSearch,
    /// 媒体专项搜索
    MediaSearch,
    /// 文件操作
    FileAction,
    /// 二次筛选
    Refine,
    /// 澄清问题
    Clarify,
}

impl SupportedIntent {
    /// 从一个具体 [`SearchIntent`] 提取它对应的能力枚举。
    #[must_use]
    pub const fn from_intent(intent: &SearchIntent) -> Self {
        match intent {
            SearchIntent::FileSearch(_) => Self::FileSearch,
            SearchIntent::MediaSearch(_) => Self::MediaSearch,
            SearchIntent::FileAction(_) => Self::FileAction,
            SearchIntent::Refine(_) => Self::Refine,
            SearchIntent::Clarify(_) => Self::Clarify,
        }
    }
}

/// 工具能力描述。Capability Discovery（MVP-09）会消费此结构生成 UI 提示，
/// Intent Router（MVP-05）会按 [`SupportedIntent`] 做路由匹配。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCapability {
    /// 简短中文描述（UI 展示用，如 `"macOS Spotlight 系统搜索"`）。
    pub description: String,
    /// 此工具支持的 intent 变体（Search 类工具填）。
    pub supported_intents: Vec<SupportedIntent>,
    /// 此工具支持的文件操作（FileAction 类工具填，MVP-10A 引入）。
    pub supported_actions: Vec<FileActionKind>,
    /// 可选 backend 身份（Search 类工具填）。
    pub backend_kind: Option<BackendKind>,
}

impl ToolCapability {
    /// 构造一个 Search 类工具的能力描述（便利构造器）。
    #[must_use]
    pub fn for_search(
        description: impl Into<String>,
        backend_kind: BackendKind,
        supported_intents: Vec<SupportedIntent>,
    ) -> Self {
        Self {
            description: description.into(),
            supported_intents,
            supported_actions: Vec::new(),
            backend_kind: Some(backend_kind),
        }
    }

    /// 构造一个 `FileAction` 类工具的能力描述。
    #[must_use]
    pub fn for_file_action(
        description: impl Into<String>,
        supported_actions: Vec<FileActionKind>,
    ) -> Self {
        Self {
            description: description.into(),
            supported_intents: vec![SupportedIntent::FileAction],
            supported_actions,
            backend_kind: None,
        }
    }
}

// ============================================================
// §2. Tool trait
// ============================================================

/// 所有可调用工具实现此 trait。
///
/// [`Tool`] 故意不暴露 `invoke()` —— Search / `FileAction` 调用形状不同，由具体
/// 工具类型在外部直接调用（如 [`SearchTool::invoke`]）。`Tool` 仅承担"身份 +
/// 元信息 + 可用性"的最小公约数职责。
///
/// `Send + Sync` 是为了让 [`ToolRegistry`] 可跨线程共享（Tool Loop / Streaming
/// 阶段会有并发调用）。
pub trait Tool: fmt::Debug + Send + Sync {
    /// 全局稳定 id，推荐 `"<kind>.<backend>"` 形式
    /// （如 `"search.spotlight"` / `"file_action.locate"`）。
    fn id(&self) -> &str;

    /// 人类可读名称（UI 展示用）。
    fn name(&self) -> &str;

    /// 工具种类。
    fn kind(&self) -> ToolKind;

    /// 能力描述。
    fn capability(&self) -> &ToolCapability;

    /// 实现状态。默认返回 [`ImplementationStatus::Real`]；stub / 占位实现必须 override。
    fn implementation_status(&self) -> ImplementationStatus {
        ImplementationStatus::Real
    }

    /// 当前环境下是否可用（如检测到 es.exe 存在 / 系统索引就绪 / 必要权限已授予）。
    fn is_available(&self) -> bool;
}

// ============================================================
// §3. SearchTool 适配器
// ============================================================

/// 把任意 [`SearchBackend`] 包成 [`Tool`]，以便注册到 [`ToolRegistry`]。
///
/// `Arc` 包住 backend，便于 Tool Loop（MVP-04）与 Streaming（MVP-07）后续在
/// 任务间共享 backend 句柄而不必复制。
pub struct SearchTool<B: SearchBackend + 'static> {
    id: String,
    name: String,
    capability: ToolCapability,
    backend: Arc<B>,
}

impl<B: SearchBackend + 'static> fmt::Debug for SearchTool<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SearchTool")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("backend_kind", &self.capability.backend_kind)
            .field(
                "implementation_status",
                &self.backend.implementation_status(),
            )
            .finish()
    }
}

impl<B: SearchBackend + 'static> SearchTool<B> {
    /// 构造一个 [`SearchTool`]。
    ///
    /// `id` 推荐用 `"search.<backend>"` 形式（如 `"search.spotlight"`、
    /// `"search.windows"`、`"search.native-file-index"`）。
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        backend: B,
        supported_intents: Vec<SupportedIntent>,
        description: impl Into<String>,
    ) -> Self {
        let backend = Arc::new(backend);
        let capability = ToolCapability::for_search(description, backend.kind(), supported_intents);
        Self {
            id: id.into(),
            name: name.into(),
            capability,
            backend,
        }
    }

    /// 借用底层 backend（测试 / 调试用）。
    #[must_use]
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// 调用底层 backend 的异步流式 search。
    pub async fn invoke(
        &self,
        intent: &SearchIntent,
        cancel: CancellationToken,
    ) -> Result<BackendStream, SearchError> {
        self.backend.search(intent, cancel).await
    }
}

impl<B: SearchBackend + 'static> Tool for SearchTool<B> {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Search
    }

    fn capability(&self) -> &ToolCapability {
        &self.capability
    }

    fn implementation_status(&self) -> ImplementationStatus {
        self.backend.implementation_status()
    }

    fn is_available(&self) -> bool {
        self.backend.is_available()
    }
}

// ============================================================
// §4. ToolRegistry
// ============================================================

/// 重复注册同 id 工具时的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateToolId(pub String);

impl fmt::Display for DuplicateToolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "duplicate tool id: {}", self.0)
    }
}

impl std::error::Error for DuplicateToolId {}

/// 内部 newtype：让通用 `tools` 表持有 `Arc<dyn SearchableTool>` 的一份引用，
/// 复用同一份堆分配，避免 `register_search` 时同一个 backend 被装入两个独立 Box。
///
/// 仅 [`ToolRegistry::register_search`] 构造它；外部不可见。
pub(crate) struct SearchableToolHandle(Arc<dyn crate::searchable_tool::SearchableTool>);

impl fmt::Debug for SearchableToolHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SearchableToolHandle")
            .field(&self.0.id())
            .finish()
    }
}

impl Tool for SearchableToolHandle {
    fn id(&self) -> &str {
        self.0.id()
    }
    fn name(&self) -> &str {
        self.0.name()
    }
    fn kind(&self) -> ToolKind {
        self.0.kind()
    }
    fn capability(&self) -> &ToolCapability {
        self.0.capability()
    }
    fn implementation_status(&self) -> ImplementationStatus {
        self.0.implementation_status()
    }
    fn is_available(&self) -> bool {
        self.0.is_available()
    }
}

/// 工具注册表。
///
/// - 按 id 索引（[`BTreeMap`]，遍历顺序稳定 = id 升序，便于跨平台一致性测试）。
/// - 生产链自动剔除 [`ImplementationStatus::Stub`]。
/// - 支持按 [`ToolKind`] / [`SupportedIntent`] 检索。
/// - Search 类工具走 [`Self::register_search`]，同时入通用表与 search-typed 表，
///   后者用 `Arc<dyn SearchableTool>` 让外部可直接调 `.search().await`。
#[derive(Debug, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Box<dyn Tool>>,
    searchable: BTreeMap<String, Arc<dyn crate::searchable_tool::SearchableTool>>,
}

impl ToolRegistry {
    /// 创建空注册表。
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
            searchable: BTreeMap::new(),
        }
    }

    /// 注册一个工具。id 必须全局唯一。
    ///
    /// **Search 类工具请改用 [`Self::register_search`]**：本方法不会填充
    /// `searchable` 表，会导致 [`Self::find_search_tool`] /
    /// [`Self::available_search_tools_supporting`] 找不到该工具。
    pub fn register<T: Tool + 'static>(&mut self, tool: T) -> Result<(), DuplicateToolId> {
        self.register_boxed(Box::new(tool))
    }

    /// 注册一个已经装箱的工具（dyn 调度场景用）。
    pub fn register_boxed(&mut self, tool: Box<dyn Tool>) -> Result<(), DuplicateToolId> {
        if self.tools.contains_key(tool.id()) {
            return Err(DuplicateToolId(tool.id().to_owned()));
        }
        self.tools.insert(tool.id().to_owned(), tool);
        Ok(())
    }

    /// Search 类工具的专用注册：同时入 `tools` 表（通过 [`SearchableToolHandle`]）
    /// 和 `searchable` 表，共享同一份 Arc。
    pub fn register_search<B: SearchBackend + 'static>(
        &mut self,
        tool: SearchTool<B>,
    ) -> Result<(), DuplicateToolId> {
        let id = tool.id().to_owned();
        if self.tools.contains_key(&id) {
            return Err(DuplicateToolId(id));
        }
        let arc: Arc<dyn crate::searchable_tool::SearchableTool> = Arc::new(tool);
        self.searchable.insert(id.clone(), Arc::clone(&arc));
        self.tools.insert(id, Box::new(SearchableToolHandle(arc)));
        Ok(())
    }

    /// 按 id 查找 search-typed 工具；未注册或非 Search 类返回 `None`。
    #[must_use]
    pub fn find_search_tool(
        &self,
        id: &str,
    ) -> Option<Arc<dyn crate::searchable_tool::SearchableTool>> {
        self.searchable.get(id).cloned()
    }

    /// 生产链中支持指定 intent 且当前可用的 search-typed 工具子集。
    ///
    /// 三层过滤：[`ImplementationStatus::Real`] + `is_available()` + 支持该 intent。
    /// 排序按工具 id 升序（BTreeMap 自然有序），确保跨平台一致性。
    #[must_use]
    pub fn available_search_tools_supporting(
        &self,
        intent: SupportedIntent,
    ) -> Vec<Arc<dyn crate::searchable_tool::SearchableTool>> {
        self.searchable
            .values()
            .filter(|tool| tool.implementation_status() == ImplementationStatus::Real)
            .filter(|tool| tool.is_available())
            .filter(|tool| tool.capability().supported_intents.contains(&intent))
            .cloned()
            .collect()
    }

    /// 按 id 查找；未注册时返回 `None`。
    #[must_use]
    pub fn find_by_id(&self, id: &str) -> Option<&dyn Tool> {
        self.tools.get(id).map(AsRef::as_ref)
    }

    /// 全部已注册工具（含 stub）。遍历顺序按 id 升序。
    #[must_use]
    pub fn all_tools(&self) -> Vec<&dyn Tool> {
        self.tools.values().map(AsRef::as_ref).collect()
    }

    /// 按 [`ToolKind`] 过滤（不剔除 stub）。
    #[must_use]
    pub fn tools_by_kind(&self, kind: ToolKind) -> Vec<&dyn Tool> {
        self.tools
            .values()
            .filter(|tool| tool.kind() == kind)
            .map(AsRef::as_ref)
            .collect()
    }

    /// 生产 fallback 链：剔除 stub，保留 [`ImplementationStatus::Real`]。
    ///
    /// 这是 ROADMAP §6.1 / §6.2 出场指标"Stub backend 不进入生产 fallback 链"
    /// 在 Harness 层的实现入口。
    #[must_use]
    pub fn production_tools(&self) -> Vec<&dyn Tool> {
        self.tools
            .values()
            .filter(|tool| tool.implementation_status() == ImplementationStatus::Real)
            .map(AsRef::as_ref)
            .collect()
    }

    /// 生产链中支持指定 intent 的工具子集（Intent Router 使用）。
    #[must_use]
    pub fn production_tools_supporting(&self, intent: SupportedIntent) -> Vec<&dyn Tool> {
        self.tools
            .values()
            .filter(|tool| tool.implementation_status() == ImplementationStatus::Real)
            .filter(|tool| tool.capability().supported_intents.contains(&intent))
            .map(AsRef::as_ref)
            .collect()
    }

    /// 生产链中支持指定 intent 且当前可用的工具子集（Fallback Chain 的源数据）。
    #[must_use]
    pub fn available_tools_supporting(&self, intent: SupportedIntent) -> Vec<&dyn Tool> {
        self.tools
            .values()
            .filter(|tool| tool.implementation_status() == ImplementationStatus::Real)
            .filter(|tool| tool.is_available())
            .filter(|tool| tool.capability().supported_intents.contains(&intent))
            .map(AsRef::as_ref)
            .collect()
    }

    /// 当前注册工具总数（含 stub）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// 注册表是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

pub mod schema_validator;
pub use schema_validator::{SchemaError, SchemaValidator, ValidationError};

pub mod tracing;
pub use tracing::{
    anonymize_path, JsonLinesHook, NoopHook, ToolCallEvent, ToolErrorEvent, ToolResultEvent,
    Tracer, TracingHook,
};

pub mod capability;
pub use capability::{BackendSummary, CapabilityDiscovery};

pub mod fallback;
pub use fallback::{FallbackChain, FallbackError};

pub mod fallback_chain;
pub use fallback_chain::{run_fallback_chain, BackendSwitch, ChainOutcome, SwitchReason};

pub mod fanout_merge;
pub use fanout_merge::{
    filter_results_for_intent, fuse_fanout_merge, fuse_fanout_rrf, run_fanout_merge,
    run_fanout_merge_rrf, run_fanout_merge_with_fallback, FanoutOutcome,
};
pub use scout_result_normalizer::MergedResult;

pub mod audit;
pub use audit::{
    AuditEntry, AuditLog, AuditOperation, AuditResult, InMemoryAuditLog, JsonlAuditLog,
};

pub mod searchable_tool;
pub use searchable_tool::{SearchableTool, SharedSearchableTool};

// ============================================================
// §5. 单元测试
// ============================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use scout_search_backend::SchemaVersion;
    use scout_search_backend::{FileSearch, SearchIntent};

    // 一个最小可控的 backend，用来构造各种 ImplementationStatus / is_available 组合。
    #[derive(Debug)]
    struct FakeBackend {
        kind: BackendKind,
        status: ImplementationStatus,
        available: bool,
    }

    impl SearchBackend for FakeBackend {
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
            cancel: CancellationToken,
        ) -> scout_search_backend::BackendSearchFuture<'a> {
            Box::pin(async move {
                Ok(scout_search_backend::backend_stream_from_results(
                    Vec::new(),
                    cancel,
                ))
            })
        }
    }

    fn make_search_tool(
        id: &str,
        backend_kind: BackendKind,
        status: ImplementationStatus,
        available: bool,
    ) -> SearchTool<FakeBackend> {
        SearchTool::new(
            id,
            id,
            FakeBackend {
                kind: backend_kind,
                status,
                available,
            },
            vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
            "fake backend for unit test",
        )
    }

    #[test]
    fn register_and_find_by_id() {
        let mut registry = ToolRegistry::new();
        registry
            .register(make_search_tool(
                "search.spotlight",
                BackendKind::Spotlight,
                ImplementationStatus::Real,
                true,
            ))
            .unwrap();

        let tool = registry.find_by_id("search.spotlight").unwrap();
        assert_eq!(tool.id(), "search.spotlight");
        assert_eq!(tool.kind(), ToolKind::Search);
        assert_eq!(tool.capability().backend_kind, Some(BackendKind::Spotlight));
        assert!(registry.find_by_id("search.does-not-exist").is_none());
    }

    #[test]
    fn duplicate_id_rejected() {
        let mut registry = ToolRegistry::new();
        registry
            .register(make_search_tool(
                "search.spotlight",
                BackendKind::Spotlight,
                ImplementationStatus::Real,
                true,
            ))
            .unwrap();

        let err = registry
            .register(make_search_tool(
                "search.spotlight",
                BackendKind::Spotlight,
                ImplementationStatus::Real,
                true,
            ))
            .unwrap_err();
        assert_eq!(err, DuplicateToolId("search.spotlight".to_owned()));
    }

    #[test]
    fn all_tools_and_tools_by_kind() {
        let mut registry = ToolRegistry::new();
        registry
            .register(make_search_tool(
                "search.spotlight",
                BackendKind::Spotlight,
                ImplementationStatus::Real,
                true,
            ))
            .unwrap();
        registry
            .register(make_search_tool(
                "search.windows",
                BackendKind::WindowsSearch,
                ImplementationStatus::Stub,
                false,
            ))
            .unwrap();

        assert_eq!(registry.len(), 2);
        assert!(!registry.is_empty());
        assert_eq!(registry.all_tools().len(), 2);
        assert_eq!(registry.tools_by_kind(ToolKind::Search).len(), 2);
        assert_eq!(registry.tools_by_kind(ToolKind::FileAction).len(), 0);
    }

    /// MVP-01 关键验收：生产 fallback 链必须排除 stub backend。
    ///
    /// 对应 ROADMAP §6.1 / §6.2 出场指标"Stub backend 不进入生产 fallback 链"。
    #[test]
    fn production_chain_excludes_stub_tools() {
        let mut registry = ToolRegistry::new();
        registry
            .register(make_search_tool(
                "search.spotlight",
                BackendKind::Spotlight,
                ImplementationStatus::Real,
                true,
            ))
            .unwrap();
        registry
            .register(make_search_tool(
                "search.windows",
                BackendKind::WindowsSearch,
                ImplementationStatus::Stub,
                true,
            ))
            .unwrap();
        registry
            .register(make_search_tool(
                "search.native-file-index",
                BackendKind::NativeFileIndex,
                ImplementationStatus::Stub,
                true,
            ))
            .unwrap();

        let production = registry.production_tools();
        assert_eq!(production.len(), 1);
        assert_eq!(production[0].id(), "search.spotlight");
        assert_eq!(
            production[0].implementation_status(),
            ImplementationStatus::Real
        );

        // 即使按 intent 过滤，stub 同样被剔除。
        let supporting = registry.production_tools_supporting(SupportedIntent::FileSearch);
        assert_eq!(supporting.len(), 1);
        assert_eq!(supporting[0].id(), "search.spotlight");
    }

    #[test]
    fn available_tools_supporting_filters_unavailable() {
        let mut registry = ToolRegistry::new();
        // Real + available + 支持 file_search → 入选
        registry
            .register(make_search_tool(
                "search.spotlight",
                BackendKind::Spotlight,
                ImplementationStatus::Real,
                true,
            ))
            .unwrap();
        // Real + 不可用 → 不入选
        registry
            .register(make_search_tool(
                "search.windows",
                BackendKind::WindowsSearch,
                ImplementationStatus::Real,
                false,
            ))
            .unwrap();
        // Stub → 不入选
        registry
            .register(make_search_tool(
                "search.native-file-index",
                BackendKind::NativeFileIndex,
                ImplementationStatus::Stub,
                true,
            ))
            .unwrap();

        let available = registry.available_tools_supporting(SupportedIntent::FileSearch);
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].id(), "search.spotlight");
    }

    #[test]
    fn supported_intent_from_intent_covers_all_variants() {
        let file_search = SearchIntent::FileSearch(FileSearch {
            schema_version: SchemaVersion::V1,
            language: None,
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
        assert_eq!(
            SupportedIntent::from_intent(&file_search),
            SupportedIntent::FileSearch
        );
    }

    #[test]
    fn tool_capability_constructors() {
        let cap = ToolCapability::for_search(
            "Spotlight",
            BackendKind::Spotlight,
            vec![SupportedIntent::FileSearch],
        );
        assert_eq!(cap.backend_kind, Some(BackendKind::Spotlight));
        assert_eq!(cap.supported_intents, vec![SupportedIntent::FileSearch]);
        assert!(cap.supported_actions.is_empty());

        let cap = ToolCapability::for_file_action(
            "本地文件操作",
            vec![FileActionKind::Open, FileActionKind::Locate],
        );
        assert_eq!(cap.backend_kind, None);
        assert_eq!(cap.supported_intents, vec![SupportedIntent::FileAction]);
        assert_eq!(cap.supported_actions.len(), 2);
    }

    // ===== Task 2 新加测试：register_search + searchable 表 =====

    #[test]
    fn register_search_indexes_both_tables() {
        let mut registry = ToolRegistry::new();
        registry
            .register_search(make_search_tool(
                "search.spotlight",
                BackendKind::Spotlight,
                ImplementationStatus::Real,
                true,
            ))
            .unwrap();

        // 通用 tools 表能找到（通过 SearchableToolHandle 转包）
        let via_tools = registry.find_by_id("search.spotlight").unwrap();
        assert_eq!(via_tools.id(), "search.spotlight");
        assert_eq!(via_tools.kind(), ToolKind::Search);
        assert_eq!(
            via_tools.capability().backend_kind,
            Some(BackendKind::Spotlight)
        );

        // search-typed 表也能找到
        let via_searchable = registry.find_search_tool("search.spotlight").unwrap();
        assert_eq!(via_searchable.id(), "search.spotlight");
    }

    #[test]
    fn register_search_rejects_duplicate_id() {
        let mut registry = ToolRegistry::new();
        registry
            .register_search(make_search_tool(
                "search.spotlight",
                BackendKind::Spotlight,
                ImplementationStatus::Real,
                true,
            ))
            .unwrap();
        let err = registry
            .register_search(make_search_tool(
                "search.spotlight",
                BackendKind::Spotlight,
                ImplementationStatus::Real,
                true,
            ))
            .unwrap_err();
        assert_eq!(err, DuplicateToolId("search.spotlight".to_owned()));
    }

    #[test]
    fn available_search_tools_supporting_filters_stub_and_unavailable() {
        let mut registry = ToolRegistry::new();
        // Real + available → 入选
        registry
            .register_search(make_search_tool(
                "search.spotlight",
                BackendKind::Spotlight,
                ImplementationStatus::Real,
                true,
            ))
            .unwrap();
        // Real + 不可用 → 不入选
        registry
            .register_search(make_search_tool(
                "search.windows",
                BackendKind::WindowsSearch,
                ImplementationStatus::Real,
                false,
            ))
            .unwrap();
        // Stub → 不入选
        registry
            .register_search(make_search_tool(
                "search.native-file-index",
                BackendKind::NativeFileIndex,
                ImplementationStatus::Stub,
                true,
            ))
            .unwrap();

        let available = registry.available_search_tools_supporting(SupportedIntent::FileSearch);
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].id(), "search.spotlight");
    }
}
