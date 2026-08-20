//! Intent Router：按 `SearchIntent` 选择可用工具。

use crate::{SearchableTool, SupportedIntent, Tool, ToolRegistry};
use scout_search_backend::{BackendKind, ExpandedSearchIntent, SearchIntent};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

/// Intent Router 路由错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteError {
    /// 没有可用 backend 支持该 intent。
    NoBackend,
    /// `clarify` intent 只需要向用户提问，不应路由到 backend。
    ClarifyNotRoutable,
    /// 当前 intent 还没有可执行路由。
    UnsupportedIntent {
        /// 详细原因。
        detail: String,
    },
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoBackend => f.write_str("no available backend for intent"),
            Self::ClarifyNotRoutable => f.write_str("clarify intent is not routable"),
            Self::UnsupportedIntent { detail } => write!(f, "unsupported intent: {detail}"),
        }
    }
}

impl Error for RouteError {}

/// 按 intent 从 [`ToolRegistry`] 中选择工具。
///
/// 路由顺序由 [`ToolRegistry`] 的 id 升序遍历保证稳定；stub 和不可用工具由
/// [`ToolRegistry::available_tools_supporting`] 自动剔除。
#[derive(Debug, Clone, Copy)]
pub struct IntentRouter<'a> {
    registry: &'a ToolRegistry,
}

impl<'a> IntentRouter<'a> {
    /// 创建指向指定注册表的路由器。
    #[must_use]
    pub const fn new(registry: &'a ToolRegistry) -> Self {
        Self { registry }
    }

    /// 为 intent 选择第一个可用工具。
    pub fn route(&self, intent: &SearchIntent) -> Result<&'a dyn Tool, RouteError> {
        let supported_intent = SupportedIntent::from_intent(intent);
        if supported_intent == SupportedIntent::Clarify {
            return Err(RouteError::ClarifyNotRoutable);
        }

        self.registry
            .available_tools_supporting(supported_intent)
            .into_iter()
            .next()
            .ok_or(RouteError::NoBackend)
    }

    /// 为 intent 选择第一个可用的 search-typed 工具，用于流式调用。
    ///
    /// 与 [`Self::route`] 的区别：返回 `Arc<dyn SearchableTool>` 而非 `&dyn Tool`，
    /// 调用方可直接 `.search().await` 流式拿结果，无需再做 dispatch。
    pub fn route_search(
        &self,
        intent: &SearchIntent,
    ) -> Result<Arc<dyn SearchableTool>, RouteError> {
        let supported_intent = SupportedIntent::from_intent(intent);
        if supported_intent == SupportedIntent::Clarify {
            return Err(RouteError::ClarifyNotRoutable);
        }

        // 候选按 id 升序（BTreeMap），已剔除 stub 与不可用工具。
        let candidates = self
            .registry
            .available_search_tools_supporting(supported_intent);

        // 能力感知路由：需要正文内容 / 媒体元数据匹配的查询，优先选内容型后端
        // （Spotlight / WindowsSearch 索引正文与媒体标签；内置原生索引只索引文件名+路径，
        // 对这类查询会退化）。纯文件名 / 路径 / 大小 / 扩展名查询不触发，沿用 id 序首位
        // （Windows 上内置原生索引因 id 靠前被优先选中，更快）。内容型后端不可用时回落首位。
        if requires_content_or_metadata(intent) {
            if let Some(rich) = candidates
                .iter()
                .find(|tool| backend_indexes_content(tool.capability().backend_kind))
            {
                return Ok(Arc::clone(rich));
            }
        }

        candidates.into_iter().next().ok_or(RouteError::NoBackend)
    }

    /// 为**同义词扩展后**的意图选择工具。
    ///
    /// 与 [`Self::route_search`] 的区别：路由依据扩展后的关键词组——即使 parser 未抽出
    /// keyword、由 gazetteer（BETA-15E）兜底注入的内容词，也能把查询导向内容型后端。
    /// 否则纯文件名查询命中的 base intent 无 keyword，会错误落到只索引文件名的内置原生索引。
    pub fn route_search_expanded(
        &self,
        expanded: &ExpandedSearchIntent,
    ) -> Result<Arc<dyn SearchableTool>, RouteError> {
        let supported_intent = SupportedIntent::from_intent(&expanded.base);
        if supported_intent == SupportedIntent::Clarify {
            return Err(RouteError::ClarifyNotRoutable);
        }

        let candidates = self
            .registry
            .available_search_tools_supporting(supported_intent);

        // base 自身需要内容/元数据，或扩展产生了任何内容关键词组 → 优先内容型后端。
        let needs_content = expanded_needs_content(expanded);
        if needs_content {
            if let Some(rich) = candidates
                .iter()
                .find(|tool| backend_indexes_content(tool.capability().backend_kind))
            {
                return Ok(Arc::clone(rich));
            }
        }

        candidates.into_iter().next().ok_or(RouteError::NoBackend)
    }

    /// 返回**有序候选列表**，供 fallback chain 逐个回退使用。
    ///
    /// 排序与 [`Self::route_search_expanded`] 一致：base 需要内容/元数据，或扩展产生
    /// 任何内容关键词组 → 内容型后端排首位；否则沿用 id 序。区别仅在于返回全部候选而非首位。
    pub fn route_search_chain(
        &self,
        expanded: &ExpandedSearchIntent,
    ) -> Result<Vec<Arc<dyn SearchableTool>>, RouteError> {
        let supported_intent = SupportedIntent::from_intent(&expanded.base);
        if supported_intent == SupportedIntent::Clarify {
            return Err(RouteError::ClarifyNotRoutable);
        }

        let mut candidates = self
            .registry
            .available_search_tools_supporting(supported_intent);
        if candidates.is_empty() {
            return Err(RouteError::NoBackend);
        }

        let needs_content = expanded_needs_content(expanded);
        if needs_content {
            if let Some(pos) = candidates
                .iter()
                .position(|tool| backend_indexes_content(tool.capability().backend_kind))
            {
                let rich = candidates.remove(pos);
                candidates.insert(0, rich);
            }
        }
        Ok(candidates)
    }

    /// 返回**该一起查询的后端集合**（BETA-04 fan-out 多源）。
    ///
    /// 与 [`Self::route_search_chain`]（fallback 选一个）不同：本方法返回的后端应被
    /// **同时查询、结果归一化合并**。规则：
    /// - 内容/媒体查询（base 需内容 或 扩展产生内容词组）→ **全部 content-capable 可用后端**
    ///   （Spotlight / `WindowsSearch` / `NativeIndex`），让本地索引与系统搜索一起命中；
    /// - 纯文件名/扩展名查询 → **单个首选**（id 序首位，通常是内置原生索引快通道）。
    ///
    /// 内容查询但无任何 content-capable 后端时，回落单个首位（best-effort）。
    pub fn route_search_fanout(
        &self,
        expanded: &ExpandedSearchIntent,
    ) -> Result<Vec<Arc<dyn SearchableTool>>, RouteError> {
        let supported_intent = SupportedIntent::from_intent(&expanded.base);
        if supported_intent == SupportedIntent::Clarify {
            return Err(RouteError::ClarifyNotRoutable);
        }

        let candidates = self
            .registry
            .available_search_tools_supporting(supported_intent);
        if candidates.is_empty() {
            return Err(RouteError::NoBackend);
        }

        if expanded_needs_content(expanded) {
            let mut selected: Vec<Arc<dyn SearchableTool>> = candidates
                .iter()
                .filter(|tool| backend_indexes_content(tool.capability().backend_kind))
                .map(Arc::clone)
                .collect();
            // 有内容关键词时，并列纯文件名后端（内置原生索引）做全盘召回——有关键词时其文件名
            // 匹配是合理召回、不会 match-all（无关键词查询才会，见 search.rs 注释），故无关键词
            // 的纯类型查询不并列、维持现状。无纯文件名后端（如 macOS）时此追加为空 → 零变化。
            if !expanded.keyword_groups.is_empty() {
                selected.extend(
                    candidates
                        .iter()
                        .filter(|tool| !backend_indexes_content(tool.capability().backend_kind))
                        .map(Arc::clone),
                );
            }
            if !selected.is_empty() {
                return Ok(selected);
            }
        }

        let first = candidates.into_iter().next().ok_or(RouteError::NoBackend)?;
        Ok(vec![first])
    }

    /// 返回**纯文件名后端**（如内置原生索引），供内容 fan-out 零结果时按文件名兜底召回。
    ///
    /// [`Self::route_search_fanout`] 的内容分支：**有内容关键词时已并列纳入内置原生索引**（全盘文件名
    /// 召回）；但**无关键词的纯类型/扩展名查询**仍只纳 content-capable 后端、不含内置原生索引。本方法
    /// 返回被排除的纯文件名后端，供调用方在内容轮**零结果时**按文件名补一轮（见
    /// [`crate::run_fanout_merge_with_fallback`]）——覆盖「无关键词查询」或「content 与并列内置原生索引
    /// 都零结果」的盲区。
    ///
    /// 仅当存在纯文件名后端时非空（Windows 有内置原生索引；macOS 仅 Spotlight/本地索引，返回空 → 无兜底）。
    /// Clarify 不可路由 → 空。
    #[must_use]
    pub fn route_filename_fallback(
        &self,
        expanded: &ExpandedSearchIntent,
    ) -> Vec<Arc<dyn SearchableTool>> {
        let supported_intent = SupportedIntent::from_intent(&expanded.base);
        if supported_intent == SupportedIntent::Clarify {
            return Vec::new();
        }
        self.registry
            .available_search_tools_supporting(supported_intent)
            .into_iter()
            .filter(|tool| !backend_indexes_content(tool.capability().backend_kind))
            .collect()
    }
}

/// 扩展后的意图是否需要内容/元数据匹配：base 自身需要，或扩展产生了任何非空内容关键词组。
fn expanded_needs_content(expanded: &ExpandedSearchIntent) -> bool {
    requires_content_or_metadata(&expanded.base)
        || expanded
            .keyword_groups
            .iter()
            .any(|group| !group.head.trim().is_empty())
}

/// 该 intent 是否需要正文内容或媒体元数据匹配（而非纯文件属性）。
fn requires_content_or_metadata(intent: &SearchIntent) -> bool {
    match intent {
        SearchIntent::FileSearch(search) => has_nonempty_keywords(search.keywords.as_deref()),
        SearchIntent::MediaSearch(search) => {
            has_nonempty_keywords(search.keywords.as_deref())
                || search.artist.is_some()
                || search.title.is_some()
                || search.album.is_some()
                || search.genre.is_some()
                || search.duration.is_some()
        }
        SearchIntent::Refine(_) | SearchIntent::FileAction(_) | SearchIntent::Clarify(_) => false,
    }
}

fn has_nonempty_keywords(keywords: Option<&[String]>) -> bool {
    keywords.is_some_and(|words| words.iter().any(|word| !word.trim().is_empty()))
}

/// 该后端是否索引正文内容与媒体元数据。内置原生索引仅文件名/路径，返回 `false`。
/// SemanticIndex（BETA-15B 语义召回臂）也索引正文，须放行进内容 fanout。
const fn backend_indexes_content(kind: Option<BackendKind>) -> bool {
    matches!(
        kind,
        Some(
            BackendKind::Spotlight
                | BackendKind::WindowsSearch
                | BackendKind::NativeIndex
                | BackendKind::SemanticIndex
        )
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::{ToolCapability, ToolKind};
    use scout_search_backend::{
        BackendKind, Clarify, ClarifyReason, FileSearch, ImplementationStatus, SchemaVersion,
    };

    #[derive(Debug)]
    struct FakeTool {
        id: &'static str,
        status: ImplementationStatus,
        available: bool,
        capability: ToolCapability,
    }

    impl FakeTool {
        fn search(
            id: &'static str,
            status: ImplementationStatus,
            available: bool,
            backend_kind: BackendKind,
            supported_intents: Vec<SupportedIntent>,
        ) -> Self {
            Self {
                id,
                status,
                available,
                capability: ToolCapability::for_search(id, backend_kind, supported_intents),
            }
        }
    }

    impl Tool for FakeTool {
        fn id(&self) -> &str {
            self.id
        }

        fn name(&self) -> &str {
            self.id
        }

        fn kind(&self) -> ToolKind {
            ToolKind::Search
        }

        fn capability(&self) -> &ToolCapability {
            &self.capability
        }

        fn implementation_status(&self) -> ImplementationStatus {
            self.status
        }

        fn is_available(&self) -> bool {
            self.available
        }
    }

    fn file_search_intent() -> SearchIntent {
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

    #[test]
    fn routes_to_real_backend_when_stub_also_registered() {
        let mut registry = ToolRegistry::new();
        registry
            .register(FakeTool::search(
                "search.stub",
                ImplementationStatus::Stub,
                true,
                BackendKind::NativeIndex,
                vec![SupportedIntent::FileSearch],
            ))
            .unwrap();
        registry
            .register(FakeTool::search(
                "search.real",
                ImplementationStatus::Real,
                true,
                BackendKind::Spotlight,
                vec![SupportedIntent::FileSearch],
            ))
            .unwrap();

        let router = IntentRouter::new(&registry);
        let tool = router.route(&file_search_intent()).unwrap();
        assert_eq!(tool.id(), "search.real");
    }

    #[test]
    fn routes_first_available_real_by_id_order() {
        let mut registry = ToolRegistry::new();
        registry
            .register(FakeTool::search(
                "search.zeta",
                ImplementationStatus::Real,
                true,
                BackendKind::NativeFileIndex,
                vec![SupportedIntent::FileSearch],
            ))
            .unwrap();
        registry
            .register(FakeTool::search(
                "search.alpha",
                ImplementationStatus::Real,
                true,
                BackendKind::Spotlight,
                vec![SupportedIntent::FileSearch],
            ))
            .unwrap();

        let router = IntentRouter::new(&registry);
        let tool = router.route(&file_search_intent()).unwrap();
        assert_eq!(tool.id(), "search.alpha");
    }

    #[test]
    fn no_backend_when_none_available() {
        let registry = ToolRegistry::new();
        let router = IntentRouter::new(&registry);
        let err = router.route(&file_search_intent()).unwrap_err();
        assert_eq!(err, RouteError::NoBackend);
    }

    #[test]
    fn clarify_is_not_routable() {
        let registry = ToolRegistry::new();
        let router = IntentRouter::new(&registry);
        let intent = SearchIntent::Clarify(Clarify {
            schema_version: SchemaVersion::V1,
            language: None,
            reason: ClarifyReason::AmbiguousAction,
            question: "要打开还是查找？".to_owned(),
            options: None,
        });

        let err = router.route(&intent).unwrap_err();
        assert_eq!(err, RouteError::ClarifyNotRoutable);
    }

    // ===== Task 3 新加测试：route_search 走 Arc<dyn SearchableTool> =====

    use crate::SearchTool;
    use futures_util::FutureExt;
    use scout_search_backend::{
        backend_stream_from_results, BackendSearchFuture, CancellationToken, SearchBackend,
    };

    #[derive(Debug)]
    struct FakeRealBackend;

    impl SearchBackend for FakeRealBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Spotlight
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
            async move { Ok(backend_stream_from_results(Vec::new(), cancel)) }.boxed()
        }
    }

    #[test]
    fn route_search_returns_real_backend_arc() {
        let mut registry = ToolRegistry::new();
        registry
            .register_search(SearchTool::new(
                "search.real",
                "Real",
                FakeRealBackend,
                vec![SupportedIntent::FileSearch],
                "real fake backend",
            ))
            .unwrap();

        let router = IntentRouter::new(&registry);
        let tool = router.route_search(&file_search_intent()).unwrap();
        assert_eq!(tool.id(), "search.real");
    }

    #[test]
    fn route_search_no_backend_when_none_available() {
        let registry = ToolRegistry::new();
        let router = IntentRouter::new(&registry);
        let err = router.route_search(&file_search_intent()).unwrap_err();
        assert_eq!(err, RouteError::NoBackend);
    }

    #[test]
    fn route_search_clarify_returns_clarify_not_routable() {
        let registry = ToolRegistry::new();
        let router = IntentRouter::new(&registry);
        let intent = SearchIntent::Clarify(Clarify {
            schema_version: SchemaVersion::V1,
            language: None,
            reason: ClarifyReason::AmbiguousAction,
            question: "要打开还是查找？".to_owned(),
            options: None,
        });
        let err = router.route_search(&intent).unwrap_err();
        assert_eq!(err, RouteError::ClarifyNotRoutable);
    }

    // ===== 能力感知路由：内容/元数据查询优先内容型后端 =====

    #[derive(Debug)]
    struct FakeKindBackend(BackendKind);

    impl SearchBackend for FakeKindBackend {
        fn kind(&self) -> BackendKind {
            self.0
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
            async move { Ok(backend_stream_from_results(Vec::new(), cancel)) }.boxed()
        }
    }

    /// 同时注册内置原生索引（id 靠前）与 WindowsSearch（内容型）两个真实后端。
    fn registry_native_file_index_and_windows() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry
            .register_search(SearchTool::new(
                "search.native-file-index",
                "NativeFileIndex",
                FakeKindBackend(BackendKind::NativeFileIndex),
                vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
                "native-file-index",
            ))
            .unwrap();
        registry
            .register_search(SearchTool::new(
                "search.windows_search",
                "WindowsSearch",
                FakeKindBackend(BackendKind::WindowsSearch),
                vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
                "windows search",
            ))
            .unwrap();
        registry
    }

    fn file_search_extensions_only() -> SearchIntent {
        SearchIntent::FileSearch(FileSearch {
            schema_version: SchemaVersion::V1,
            language: None,
            keywords: None,
            extensions: Some(vec!["pdf".to_owned()]),
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
    fn keyword_query_prefers_content_backend_over_id_order() {
        // file_search_intent() 带 keyword "budget" → 需内容匹配 → 选 WindowsSearch，
        // 即使内置原生索引在 id 序中更靠前。
        let registry = registry_native_file_index_and_windows();
        let router = IntentRouter::new(&registry);
        let tool = router.route_search(&file_search_intent()).unwrap();
        assert_eq!(tool.id(), "search.windows_search");
    }

    #[test]
    fn attribute_only_query_keeps_id_order_native_file_index_first() {
        // 纯扩展名查询 → 不需内容 → 沿用 id 序首位（内置原生索引）。
        let registry = registry_native_file_index_and_windows();
        let router = IntentRouter::new(&registry);
        let tool = router.route_search(&file_search_extensions_only()).unwrap();
        assert_eq!(tool.id(), "search.native-file-index");
    }

    #[test]
    fn keyword_query_falls_back_when_no_content_backend() {
        // 只有内置原生索引可用时，内容查询回落到内置原生索引（best-effort，不报错）。
        let mut registry = ToolRegistry::new();
        registry
            .register_search(SearchTool::new(
                "search.native-file-index",
                "NativeFileIndex",
                FakeKindBackend(BackendKind::NativeFileIndex),
                vec![SupportedIntent::FileSearch],
                "native-file-index",
            ))
            .unwrap();
        let router = IntentRouter::new(&registry);
        let tool = router.route_search(&file_search_intent()).unwrap();
        assert_eq!(tool.id(), "search.native-file-index");
    }

    #[test]
    fn expanded_with_injected_keyword_group_routes_to_content_backend() {
        use scout_search_backend::{ExpandedSearchIntent, KeywordGroup};
        // base 无 keyword（模拟 parser 对自然中文 query 未抽出名词短语），但 gazetteer
        // 注入了内容词组 → 应路由到内容型 WindowsSearch，而非 id 靠前的内置原生索引。
        let registry = registry_native_file_index_and_windows();
        let router = IntentRouter::new(&registry);
        let expanded = ExpandedSearchIntent {
            base: file_search_extensions_only(),
            keyword_groups: vec![KeywordGroup {
                head: "工作汇报".to_owned(),
                synonyms: vec!["述职".to_owned()],
            }],
            match_mode: scout_search_backend::MatchMode::default(),
        };
        let tool = router.route_search_expanded(&expanded).unwrap();
        assert_eq!(tool.id(), "search.windows_search");
    }

    #[test]
    fn expanded_without_keyword_groups_keeps_id_order() {
        use scout_search_backend::ExpandedSearchIntent;
        // 无任何关键词组（纯文件名/扩展名查询）→ 沿用 id 序首位（内置原生索引，快）。
        let registry = registry_native_file_index_and_windows();
        let router = IntentRouter::new(&registry);
        let expanded = ExpandedSearchIntent {
            base: file_search_extensions_only(),
            keyword_groups: vec![],
            match_mode: scout_search_backend::MatchMode::default(),
        };
        let tool = router.route_search_expanded(&expanded).unwrap();
        assert_eq!(tool.id(), "search.native-file-index");
    }

    // ===== fallback chain：route_search_chain 返回有序候选列表 =====

    /// 将 `SearchIntent` 包装为未扩词的 `ExpandedSearchIntent`（恒等映射）。
    fn expanded_of(intent: SearchIntent) -> ExpandedSearchIntent {
        ExpandedSearchIntent::identity(intent)
    }

    #[test]
    fn route_search_chain_returns_all_candidates_ordered() {
        let registry = registry_native_file_index_and_windows();
        let router = IntentRouter::new(&registry);
        // 纯扩展名查询（无 keywords）→ 不触发内容排序，沿 id 序返回全部 2 个候选。
        let expanded = expanded_of(file_search_extensions_only());
        let chain = router.route_search_chain(&expanded).unwrap();
        assert_eq!(chain.len(), 2, "应返回全部可用候选");
    }

    #[test]
    fn route_search_chain_content_query_puts_rich_backend_first() {
        let registry = registry_native_file_index_and_windows();
        let router = IntentRouter::new(&registry);
        // file_search_intent() 含 keyword "budget" → 内容查询 → 内容型后端排首位。
        let expanded = expanded_of(file_search_intent());
        let chain = router.route_search_chain(&expanded).unwrap();
        assert_eq!(chain.len(), 2, "全部候选仍在，未丢失");
        assert!(
            backend_indexes_content(chain[0].capability().backend_kind),
            "内容查询时内容型后端应排首位"
        );
    }

    #[test]
    fn route_search_chain_no_backend_when_empty() {
        let registry = ToolRegistry::new();
        let router = IntentRouter::new(&registry);
        let expanded = expanded_of(file_search_extensions_only());
        assert_eq!(
            router.route_search_chain(&expanded).unwrap_err(),
            RouteError::NoBackend
        );
    }

    #[test]
    fn route_search_chain_clarify_returns_clarify_not_routable() {
        let registry = ToolRegistry::new();
        let router = IntentRouter::new(&registry);
        let intent = SearchIntent::Clarify(Clarify {
            schema_version: SchemaVersion::V1,
            language: None,
            reason: ClarifyReason::AmbiguousAction,
            question: "要打开还是查找？".to_owned(),
            options: None,
        });
        let expanded = expanded_of(intent);
        assert_eq!(
            router.route_search_chain(&expanded).unwrap_err(),
            RouteError::ClarifyNotRoutable
        );
    }

    // ===== BETA-04 fan-out：route_search_fanout 返回该一起查询的后端集合 =====

    /// 内置原生索引（filename）+ WindowsSearch（content）+ NativeIndex（content）三后端。
    fn registry_three_backends() -> ToolRegistry {
        let mut registry = registry_native_file_index_and_windows();
        // id 故意选 "search.other_content"（字母序排在 "native-file-index" 之后）——
        // 下面 `fanout_attribute_only_returns_single_primary` 依赖 id 序首位恰好落在
        // 内置原生索引，用以验证"纯属性查询选中单个最快后端"这一路由行为。
        registry
            .register_search(SearchTool::new(
                "search.other_content",
                "OtherContentIndex",
                FakeKindBackend(BackendKind::NativeIndex),
                vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
                "other content index",
            ))
            .unwrap();
        registry
    }

    #[test]
    fn fanout_content_query_returns_all_content_backends() {
        // 有 keyword 的内容查询（keyword_groups 非空）→ content-capable + 纯文件名后端并列。
        // identity 展开：keywords=["budget"] → keyword_groups=[singleton("budget")]（非空），
        // 故内置原生索引也被纳入（有关键词时文件名匹配合理、不会 match-all）。
        let registry = registry_three_backends();
        let router = IntentRouter::new(&registry);
        let expanded = expanded_of(file_search_intent());
        let fanout = router.route_search_fanout(&expanded).unwrap();
        assert_eq!(fanout.len(), 3, "content-capable(2) + 内置原生索引(1) 并列");
        assert!(
            fanout
                .iter()
                .any(|t| backend_indexes_content(t.capability().backend_kind)),
            "应含 content 后端"
        );
        assert!(
            fanout.iter().any(|t| t.id() == "search.native-file-index"),
            "有关键词查询应并列内置原生索引"
        );
    }

    #[test]
    fn fanout_attribute_only_returns_single_primary() {
        // 纯扩展名查询 → 单个首选（id 序首位内置原生索引）。
        let registry = registry_three_backends();
        let router = IntentRouter::new(&registry);
        let expanded = expanded_of(file_search_extensions_only());
        let fanout = router.route_search_fanout(&expanded).unwrap();
        assert_eq!(fanout.len(), 1);
        assert_eq!(fanout[0].id(), "search.native-file-index");
    }

    #[test]
    fn fanout_content_query_falls_back_when_no_content_backend() {
        // 只有内置原生索引（filename）可用时，内容查询回落单个首位（best-effort）。
        let mut registry = ToolRegistry::new();
        registry
            .register_search(SearchTool::new(
                "search.native-file-index",
                "NativeFileIndex",
                FakeKindBackend(BackendKind::NativeFileIndex),
                vec![SupportedIntent::FileSearch],
                "native-file-index",
            ))
            .unwrap();
        let router = IntentRouter::new(&registry);
        let fanout = router
            .route_search_fanout(&expanded_of(file_search_intent()))
            .unwrap();
        assert_eq!(fanout.len(), 1);
        assert_eq!(fanout[0].id(), "search.native-file-index");
    }

    #[test]
    fn fanout_no_backend_and_clarify_errors() {
        let registry = ToolRegistry::new();
        let router = IntentRouter::new(&registry);
        assert_eq!(
            router
                .route_search_fanout(&expanded_of(file_search_extensions_only()))
                .unwrap_err(),
            RouteError::NoBackend
        );
        let clarify = SearchIntent::Clarify(Clarify {
            schema_version: SchemaVersion::V1,
            language: None,
            reason: ClarifyReason::AmbiguousAction,
            question: "?".to_owned(),
            options: None,
        });
        assert_eq!(
            router
                .route_search_fanout(&expanded_of(clarify))
                .unwrap_err(),
            RouteError::ClarifyNotRoutable
        );
    }

    #[test]
    fn fanout_keyword_query_includes_native_file_index_for_full_recall() {
        use scout_search_backend::{ExpandedSearchIntent, KeywordGroup};
        // 有内容关键词组 → content 后端(windows + local) 并列内置原生索引(filename) 做全盘召回。
        let registry = registry_three_backends();
        let router = IntentRouter::new(&registry);
        let expanded = ExpandedSearchIntent {
            base: file_search_extensions_only(),
            keyword_groups: vec![KeywordGroup {
                head: "工作汇报".to_owned(),
                synonyms: vec!["述职".to_owned()],
            }],
            match_mode: scout_search_backend::MatchMode::default(),
        };
        let fanout = router.route_search_fanout(&expanded).unwrap();
        assert_eq!(
            fanout.len(),
            3,
            "content(windows+local) + 内置原生索引并列，实际 {fanout:?}"
        );
        assert!(
            fanout.iter().any(|t| t.id() == "search.native-file-index"),
            "有关键词查询应并列内置原生索引"
        );
        assert!(
            fanout
                .iter()
                .any(|t| backend_indexes_content(t.capability().backend_kind)),
            "应仍含 content 后端"
        );
    }

    #[test]
    fn fanout_keyword_query_content_only_without_filename_backend() {
        use scout_search_backend::{ExpandedSearchIntent, KeywordGroup};
        // macOS 形态：无纯文件名后端 → 即使有关键词，并列集 = content only（零行为变化）。
        let mut registry = ToolRegistry::new();
        registry
            .register_search(SearchTool::new(
                "search.windows_search",
                "WindowsSearch",
                FakeKindBackend(BackendKind::WindowsSearch),
                vec![SupportedIntent::FileSearch],
                "ws",
            ))
            .unwrap();
        registry
            .register_search(SearchTool::new(
                "search.local",
                "LocalIndex",
                FakeKindBackend(BackendKind::NativeIndex),
                vec![SupportedIntent::FileSearch],
                "local",
            ))
            .unwrap();
        let router = IntentRouter::new(&registry);
        let expanded = ExpandedSearchIntent {
            base: file_search_extensions_only(),
            keyword_groups: vec![KeywordGroup {
                head: "工作汇报".to_owned(),
                synonyms: vec![],
            }],
            match_mode: scout_search_backend::MatchMode::default(),
        };
        let fanout = router.route_search_fanout(&expanded).unwrap();
        assert_eq!(
            fanout.len(),
            2,
            "macOS 形态应返回两个 content 后端（防空集令 all() 漏判），实际 {fanout:?}"
        );
        assert!(
            fanout
                .iter()
                .all(|t| backend_indexes_content(t.capability().backend_kind)),
            "无 filename 后端时并列集应只含 content 后端，实际 {fanout:?}"
        );
    }

    // ===== route_filename_fallback：内容 fan-out 之外的纯文件名后端 =====

    #[test]
    fn filename_fallback_returns_only_non_content_backends() {
        // 内置原生索引（filename）+ WindowsSearch + NativeIndex（content）→ 只返回内置原生索引。
        let registry = registry_three_backends();
        let router = IntentRouter::new(&registry);
        let fallback = router.route_filename_fallback(&expanded_of(file_search_intent()));
        assert_eq!(fallback.len(), 1, "应只含纯文件名后端内置原生索引");
        assert_eq!(fallback[0].id(), "search.native-file-index");
        assert!(!backend_indexes_content(
            fallback[0].capability().backend_kind
        ));
    }

    #[test]
    fn semantic_backend_counts_as_content() {
        assert!(backend_indexes_content(Some(BackendKind::SemanticIndex)));
    }

    #[test]
    fn filename_fallback_empty_when_all_content_backends() {
        // 仅 WindowsSearch + NativeIndex（均 content，模拟 macOS Spotlight+Local）→ 无文件名兜底。
        let mut registry = ToolRegistry::new();
        registry
            .register_search(SearchTool::new(
                "search.windows_search",
                "WindowsSearch",
                FakeKindBackend(BackendKind::WindowsSearch),
                vec![SupportedIntent::FileSearch],
                "windows search",
            ))
            .unwrap();
        registry
            .register_search(SearchTool::new(
                "search.local",
                "LocalIndex",
                FakeKindBackend(BackendKind::NativeIndex),
                vec![SupportedIntent::FileSearch],
                "local index",
            ))
            .unwrap();
        let router = IntentRouter::new(&registry);
        let fallback = router.route_filename_fallback(&expanded_of(file_search_intent()));
        assert!(fallback.is_empty(), "全 content 后端时无文件名兜底");
    }

    #[test]
    fn filename_fallback_clarify_is_empty() {
        let registry = registry_three_backends();
        let router = IntentRouter::new(&registry);
        let clarify = SearchIntent::Clarify(Clarify {
            schema_version: SchemaVersion::V1,
            language: None,
            reason: ClarifyReason::AmbiguousAction,
            question: "?".to_owned(),
            options: None,
        });
        assert!(router
            .route_filename_fallback(&expanded_of(clarify))
            .is_empty());
    }
}
