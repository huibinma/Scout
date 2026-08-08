use crate::{SupportedIntent, ToolKind, ToolRegistry};
use scout_search_backend::{BackendKind, FileActionKind, ImplementationStatus};
use serde::{Deserialize, Serialize};

/// 每个 Search 工具的状态摘要。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackendSummary {
    /// 工具唯一标识。
    pub id: String,
    /// 人类可读名称。
    pub name: String,
    /// 后端种类（Search 类工具特有）。
    pub backend_kind: Option<BackendKind>,
    /// 当前是否可用。
    pub is_available: bool,
    /// 实现状态（Real/Stub）。
    pub implementation_status: ImplementationStatus,
}

/// 能力发现服务。
///
/// 负责查询当前注册工具集的整体能力并集，为 UI 或路由逻辑提供依据。
pub struct CapabilityDiscovery<'a> {
    registry: &'a ToolRegistry,
}

impl std::fmt::Debug for CapabilityDiscovery<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityDiscovery").finish()
    }
}

impl<'a> CapabilityDiscovery<'a> {
    /// 绑定一个注册表。
    #[must_use]
    pub fn new(registry: &'a ToolRegistry) -> Self {
        Self { registry }
    }

    /// 当前生产链整体支持的 Intent 集合（剔除 Stub）。
    #[must_use]
    pub fn supported_intents(&self) -> Vec<SupportedIntent> {
        let mut intents = std::collections::BTreeSet::new();
        for tool in self.registry.production_tools() {
            for intent in &tool.capability().supported_intents {
                intents.insert(*intent);
            }
        }
        intents.into_iter().collect()
    }

    /// 当前生产链中仅可用工具支持的 Intent 集合（剔除 Stub 和当前不可用工具）。
    #[must_use]
    pub fn available_intents(&self) -> Vec<SupportedIntent> {
        let mut intents = std::collections::BTreeSet::new();
        for tool in self.registry.production_tools() {
            if tool.is_available() {
                for intent in &tool.capability().supported_intents {
                    intents.insert(*intent);
                }
            }
        }
        intents.into_iter().collect()
    }

    /// 当前生产链支持的所有文件操作并集。
    #[must_use]
    pub fn supported_actions(&self) -> Vec<FileActionKind> {
        let mut actions = Vec::new();
        for tool in self.registry.production_tools() {
            for action in &tool.capability().supported_actions {
                if !actions.contains(action) {
                    actions.push(*action);
                }
            }
        }
        actions
    }

    /// 每个 Search 工具的状态摘要列表，按 id 升序排列。
    #[must_use]
    pub fn backend_summary(&self) -> Vec<BackendSummary> {
        let mut summaries: Vec<_> = self
            .registry
            .tools_by_kind(ToolKind::Search)
            .into_iter()
            .map(|tool| BackendSummary {
                id: tool.id().to_string(),
                name: tool.name().to_string(),
                backend_kind: tool.capability().backend_kind,
                is_available: tool.is_available(),
                implementation_status: tool.implementation_status(),
            })
            .collect();
        summaries.sort_by(|a, b| a.id.cmp(&b.id));
        summaries
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::print_stdout
    )]
    use super::*;
    use crate::{SearchTool, SupportedIntent};
    use scout_search_backend::{
        backend_stream_from_results, BackendSearchFuture, CancellationToken, SearchBackend,
        SearchIntent,
    };

    #[derive(Debug)]
    struct MockBackend {
        kind: BackendKind,
        status: ImplementationStatus,
        available: bool,
    }

    impl SearchBackend for MockBackend {
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
        ) -> BackendSearchFuture<'a> {
            Box::pin(async move { Ok(backend_stream_from_results(Vec::new(), cancel)) })
        }
    }

    #[test]
    fn discovery_filters_stub_and_reports_correctly() {
        let mut registry = ToolRegistry::new();

        // 1. Real + Available + FileSearch
        registry
            .register(SearchTool::new(
                "search.spotlight",
                "Spotlight",
                MockBackend {
                    kind: BackendKind::Spotlight,
                    status: ImplementationStatus::Real,
                    available: true,
                },
                vec![SupportedIntent::FileSearch],
                "desc",
            ))
            .unwrap();

        // 2. Real + Unavailable + MediaSearch
        registry
            .register(SearchTool::new(
                "search.everything",
                "Everything",
                MockBackend {
                    kind: BackendKind::Everything,
                    status: ImplementationStatus::Real,
                    available: false,
                },
                vec![SupportedIntent::MediaSearch],
                "desc",
            ))
            .unwrap();

        // 3. Stub + Available + Clarify
        registry
            .register(SearchTool::new(
                "search.stub",
                "Stub",
                MockBackend {
                    kind: BackendKind::WindowsSearch,
                    status: ImplementationStatus::Stub,
                    available: true,
                },
                vec![SupportedIntent::Clarify],
                "desc",
            ))
            .unwrap();

        let discovery = CapabilityDiscovery::new(&registry);

        // supported_intents 应该包含 1 和 2 的，不包含 3
        let supported = discovery.supported_intents();
        assert_eq!(supported.len(), 2);
        assert!(supported.contains(&SupportedIntent::FileSearch));
        assert!(supported.contains(&SupportedIntent::MediaSearch));
        assert!(!supported.contains(&SupportedIntent::Clarify));

        // available_intents 应该仅包含 1
        let available = discovery.available_intents();
        assert_eq!(available.len(), 1);
        assert!(available.contains(&SupportedIntent::FileSearch));

        // backend_summary 应该按 id 排序包含所有 1, 2, 3
        let summary = discovery.backend_summary();
        assert_eq!(summary.len(), 3);
        assert_eq!(summary[0].id, "search.everything");
        assert_eq!(summary[1].id, "search.spotlight");
        assert_eq!(summary[2].id, "search.stub");
    }
}
