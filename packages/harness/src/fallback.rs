//! Fallback Chain：按确定性顺序尝试可用搜索工具。

use crate::{CapabilityDiscovery, SupportedIntent, Tool, ToolRegistry};
use scout_search_backend::BackendKind;
use std::error::Error;
use std::fmt;

/// fallback 链执行错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackError<E> {
    /// 没有任何可用候选工具。
    NoCandidates,
    /// 所有候选工具均失败，保留完整错误链。
    AllFailed(Vec<(String, E)>),
}

impl<E: fmt::Display> fmt::Display for FallbackError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCandidates => f.write_str("fallback chain has no candidates"),
            Self::AllFailed(errors) => {
                write!(f, "all fallback candidates failed: {}", errors.len())
            }
        }
    }
}

impl<E> Error for FallbackError<E> where E: fmt::Debug + fmt::Display {}

/// 按能力发现结果和注册表状态生成 fallback 候选链。
///
/// 排序规则固定为：系统索引（Spotlight / Windows Search）优先，其次自建索引
/// （`NativeIndex` / `SemanticIndex`）；同一后端等级内按工具 id 升序。
#[derive(Debug, Clone, Copy)]
pub struct FallbackChain<'a> {
    registry: &'a ToolRegistry,
    discovery: &'a CapabilityDiscovery<'a>,
}

impl<'a> FallbackChain<'a> {
    /// 创建 fallback 链。
    #[must_use]
    pub const fn new(registry: &'a ToolRegistry, discovery: &'a CapabilityDiscovery<'a>) -> Self {
        Self {
            registry,
            discovery,
        }
    }

    /// 返回指定 intent 的候选工具序列。
    #[must_use]
    pub fn candidates(&self, intent: SupportedIntent) -> Vec<&'a dyn Tool> {
        if !self.discovery.available_intents().contains(&intent) {
            return Vec::new();
        }

        let mut candidates = self.registry.available_tools_supporting(intent);
        candidates.retain(|tool| tool.capability().backend_kind.is_some());
        candidates.sort_by(|left, right| {
            backend_priority(left.capability().backend_kind)
                .cmp(&backend_priority(right.capability().backend_kind))
                .then_with(|| left.id().cmp(right.id()))
        });
        candidates
    }

    /// 依次尝试候选工具，首个成功值直接返回。
    ///
    /// 每次失败都会记录 `(tool_id, error)`；全部失败时返回完整错误链。
    pub fn try_each<T, E, F>(
        &self,
        intent: SupportedIntent,
        mut attempt: F,
    ) -> Result<T, FallbackError<E>>
    where
        F: FnMut(&dyn Tool) -> Result<T, E>,
    {
        let candidates = self.candidates(intent);
        if candidates.is_empty() {
            return Err(FallbackError::NoCandidates);
        }

        let mut errors = Vec::new();
        for tool in candidates {
            match attempt(tool) {
                Ok(value) => return Ok(value),
                Err(error) => errors.push((tool.id().to_owned(), error)),
            }
        }
        Err(FallbackError::AllFailed(errors))
    }
}

const fn backend_priority(kind: Option<BackendKind>) -> u8 {
    match kind {
        Some(BackendKind::Spotlight | BackendKind::WindowsSearch) => 0,
        // NativeFileIndex 替代原 Everything 的优先级档位：内容型系统索引之后、
        // 自建正文索引之前——只索引文件名，但比正文索引更快、覆盖面更全。
        Some(BackendKind::NativeFileIndex) => 1,
        // SemanticIndex 与 NativeIndex 同级（均为本地自建正文索引）。
        Some(BackendKind::NativeIndex | BackendKind::SemanticIndex) => 2,
        None => u8::MAX,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::{ToolCapability, ToolKind};
    use scout_search_backend::ImplementationStatus;
    use std::cell::Cell;

    #[derive(Debug)]
    struct FakeTool {
        id: &'static str,
        status: ImplementationStatus,
        available: bool,
        capability: ToolCapability,
    }

    impl FakeTool {
        fn new(
            id: &'static str,
            backend_kind: BackendKind,
            status: ImplementationStatus,
            available: bool,
        ) -> Self {
            Self {
                id,
                status,
                available,
                capability: ToolCapability::for_search(
                    id,
                    backend_kind,
                    vec![SupportedIntent::FileSearch],
                ),
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

    fn chain_with(registry: &ToolRegistry) -> FallbackChain<'_> {
        let discovery = Box::leak(Box::new(CapabilityDiscovery::new(registry)));
        FallbackChain::new(registry, discovery)
    }

    #[test]
    fn single_spotlight_available_yields_one_candidate() {
        let mut registry = ToolRegistry::new();
        registry
            .register(FakeTool::new(
                "search.spotlight",
                BackendKind::Spotlight,
                ImplementationStatus::Real,
                true,
            ))
            .unwrap();

        let candidates = chain_with(&registry).candidates(SupportedIntent::FileSearch);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id(), "search.spotlight");
    }

    #[test]
    fn spotlight_precedes_native_index_when_both_available() {
        let mut registry = ToolRegistry::new();
        registry
            .register(FakeTool::new(
                "search.native-index",
                BackendKind::NativeIndex,
                ImplementationStatus::Real,
                true,
            ))
            .unwrap();
        registry
            .register(FakeTool::new(
                "search.spotlight",
                BackendKind::Spotlight,
                ImplementationStatus::Real,
                true,
            ))
            .unwrap();

        let ids: Vec<_> = chain_with(&registry)
            .candidates(SupportedIntent::FileSearch)
            .into_iter()
            .map(Tool::id)
            .collect();

        assert_eq!(ids, vec!["search.spotlight", "search.native-index"]);
    }

    #[test]
    fn stub_backend_is_excluded_by_capability_discovery() {
        let mut registry = ToolRegistry::new();
        registry
            .register(FakeTool::new(
                "search.stub",
                BackendKind::Spotlight,
                ImplementationStatus::Stub,
                true,
            ))
            .unwrap();

        let candidates = chain_with(&registry).candidates(SupportedIntent::FileSearch);
        assert!(candidates.is_empty());
    }

    #[test]
    fn first_failure_falls_back_to_second_success() {
        let mut registry = ToolRegistry::new();
        registry
            .register(FakeTool::new(
                "search.spotlight",
                BackendKind::Spotlight,
                ImplementationStatus::Real,
                true,
            ))
            .unwrap();
        registry
            .register(FakeTool::new(
                "search.native-index",
                BackendKind::NativeIndex,
                ImplementationStatus::Real,
                true,
            ))
            .unwrap();
        let seen = Cell::new(0usize);

        let value = chain_with(&registry)
            .try_each(SupportedIntent::FileSearch, |tool| {
                seen.set(seen.get() + 1);
                if tool.id() == "search.spotlight" {
                    Err("spotlight failed")
                } else {
                    Ok(tool.id().to_owned())
                }
            })
            .unwrap();

        assert_eq!(value, "search.native-index");
        assert_eq!(seen.get(), 2);
    }

    #[test]
    fn all_failures_return_complete_error_chain() {
        let mut registry = ToolRegistry::new();
        registry
            .register(FakeTool::new(
                "search.spotlight",
                BackendKind::Spotlight,
                ImplementationStatus::Real,
                true,
            ))
            .unwrap();
        registry
            .register(FakeTool::new(
                "search.native-index",
                BackendKind::NativeIndex,
                ImplementationStatus::Real,
                true,
            ))
            .unwrap();

        let error = chain_with(&registry)
            .try_each(SupportedIntent::FileSearch, |tool| {
                Err::<(), _>(format!("{} failed", tool.id()))
            })
            .unwrap_err();

        assert_eq!(
            error,
            FallbackError::AllFailed(vec![
                (
                    "search.spotlight".to_owned(),
                    "search.spotlight failed".to_owned(),
                ),
                (
                    "search.native-index".to_owned(),
                    "search.native-index failed".to_owned(),
                ),
            ])
        );
    }
}
