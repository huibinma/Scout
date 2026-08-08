//! `SearchableTool` 子 trait：在 `Tool` 之上加 `search()` dispatch，让 `dyn Tool`
//! 无法访问的 `backend.search()` 通过 `dyn SearchableTool` 可达。
//!
//! Tool trait 故意保留"最小公约数"设计（见 `lib.rs` §2 注释），不在其上加 `invoke()`。
//! 本 trait 专门承载 Search 类工具的流式调用接口；FileActionTool 走另外的子 trait
//! （未来引入）。

use std::sync::Arc;

use scout_search_backend::{
    BackendSearchFuture, CancellationToken, ExpandedSearchIntent, SearchBackend, SearchIntent,
};

use crate::{SearchTool, Tool};

/// Search 类工具的 dispatch trait。
///
/// 任何实现 [`SearchBackend`] 的具体类型，通过 [`SearchTool`] 包装后即得到
/// [`SearchableTool`] 实现，可经 `Arc<dyn SearchableTool>` 跨边界 dispatch。
pub trait SearchableTool: Tool {
    /// 异步流式调用底层 backend 的 `search()`。
    ///
    /// 与 [`SearchBackend::search`] 同形状；语义由 backend 决定。
    fn search<'a>(
        &'a self,
        intent: &'a SearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a>;

    /// 接受同义词扩展后的搜索意图，委托给 backend 的 `search_expanded()`。
    ///
    /// 支持同义词的 backend 覆盖 `SearchBackend::search_expanded`；
    /// 其余 backend 走默认 fallback（等同于 `search(&expanded.base)`）。
    fn search_expanded<'a>(
        &'a self,
        expanded: &'a ExpandedSearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a>;
}

impl<B: SearchBackend + 'static> SearchableTool for SearchTool<B> {
    fn search<'a>(
        &'a self,
        intent: &'a SearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        self.backend().search(intent, cancel)
    }

    fn search_expanded<'a>(
        &'a self,
        expanded: &'a ExpandedSearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        self.backend().search_expanded(expanded, cancel)
    }
}

/// 便于 `Arc<dyn SearchableTool>` 在外部代码中跨 await 边界传递的类型别名。
pub type SharedSearchableTool = Arc<dyn SearchableTool>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::SupportedIntent;
    use futures_util::StreamExt;
    use scout_search_backend::{
        backend_stream_from_results, BackendKind, FileSearch, ImplementationStatus, SchemaVersion,
    };

    #[derive(Debug)]
    struct FakeBackend;

    impl SearchBackend for FakeBackend {
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
            Box::pin(async move { Ok(backend_stream_from_results(Vec::new(), cancel)) })
        }
    }

    fn file_search() -> SearchIntent {
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

    use futures_executor::block_on;

    #[test]
    fn searchable_tool_via_dyn_dispatch_returns_backend_stream() {
        let tool: Arc<dyn SearchableTool> = Arc::new(SearchTool::new(
            "search.fake",
            "Fake",
            FakeBackend,
            vec![SupportedIntent::FileSearch],
            "fake backend for dispatch test",
        ));

        let intent = file_search();
        let stream = block_on(tool.search(&intent, CancellationToken::new()))
            .expect("search() should succeed");

        let results: Vec<_> = block_on(stream.collect());
        assert!(results.is_empty(), "empty stub backend yields zero results");
    }
}
