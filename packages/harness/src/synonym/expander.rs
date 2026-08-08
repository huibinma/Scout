//! `SynonymExpander` trait + `NoopExpander` 恒等实现。

use scout_search_backend::{ExpandedSearchIntent, SearchIntent};

/// 把 `SearchIntent` 扩展为 `ExpandedSearchIntent`。
///
/// 实现需保证：未命中词典的 keyword 产出 singleton group（`synonyms` 为空），
/// 后端拿到 singleton 时行为与原 `SearchBackend::search(base)` byte-equal。
pub trait SynonymExpander: Send + Sync + std::fmt::Debug {
    /// `query` 为原始自然语言查询串，供兼底 gazetteer 使用（parser 无 keyword 时扫词典）。
    fn expand(&self, intent: SearchIntent, query: &str) -> ExpandedSearchIntent;
}

/// 恒等实现：把 intent 包成全 singleton 的 `ExpandedSearchIntent`。
/// 用于测试 / 关闭场景 / 词典加载失败的 fallback。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopExpander;

impl SynonymExpander for NoopExpander {
    fn expand(&self, intent: SearchIntent, _query: &str) -> ExpandedSearchIntent {
        ExpandedSearchIntent::identity(intent)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use scout_search_backend::{Clarify, ClarifyReason, FileSearch, SchemaVersion, SearchIntent};

    /// 构造含指定 keywords 的 `FileSearch` intent（最小有效字段）。
    fn intent_with_keywords(kws: Vec<&str>) -> SearchIntent {
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

    #[test]
    fn noop_returns_identity_singleton_groups() {
        let intent = intent_with_keywords(vec!["工作汇报", "ppt"]);
        let expanded = NoopExpander.expand(intent, "");
        assert_eq!(expanded.keyword_groups.len(), 2);
        assert!(expanded.is_identity());
        assert_eq!(expanded.keyword_groups[0].head, "工作汇报");
        assert!(expanded.keyword_groups[0].synonyms.is_empty());
        assert_eq!(expanded.keyword_groups[1].head, "ppt");
    }

    #[test]
    fn noop_on_clarify_intent_produces_empty_groups() {
        // Clarify 是非 search variant，应返回空 keyword_groups
        let clarify = SearchIntent::Clarify(Clarify {
            schema_version: SchemaVersion::V1,
            language: None,
            reason: ClarifyReason::AmbiguousType,
            question: "你想搜索哪种类型的文件？".to_owned(),
            options: None,
        });
        let expanded = NoopExpander.expand(clarify, "");
        assert!(expanded.keyword_groups.is_empty());
    }
}
