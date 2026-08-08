//! 经过同义词扩展后的搜索意图。组间 AND、组内 OR。

use crate::SearchIntent;
use serde::{Deserialize, Serialize};

/// 多个 keyword 组之间的复合匹配模式（全局用户可配置，2026-07-20 拍板）。
///
/// 组内恒为 OR（同义词互为等价，不受此设置影响）；本枚举只决定**组间**的连接方式：
/// - `All`：组间 AND，要求每个复合条件都命中——严格，未命中即 0 结果，不再像 BETA-57
///   旧行为那样静默放宽到 OR（用户反馈"返回大量不符合要求的结果"正是该静默放宽所致）。
/// - `Any`：组间 OR，任一条件命中即可——用于用户主动要广召回时手动切换。
///
/// 四个检索后端（local-index / windows-search / everything / spotlight）统一读取此字段，
/// 保证同一次全局配置下所有后端的复合条件语义一致。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    /// 全部复合条件命中（组间 AND）。默认。
    #[default]
    All,
    /// 任一条件命中（组间 OR）。
    Any,
}

/// 单个 keyword 的同义词组。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeywordGroup {
    /// parser 原始词（lookup key）。
    pub head: String,
    /// 不含 head 的等价别名。与 head OR 拼起。
    pub synonyms: Vec<String>,
}

impl KeywordGroup {
    /// 构造一个仅含 head 的组（未扩词）。
    #[must_use]
    pub fn singleton(head: impl Into<String>) -> Self {
        Self {
            head: head.into(),
            synonyms: Vec::new(),
        }
    }

    /// 组内所有词（head 在首位 + synonyms 顺序保留）。
    #[must_use]
    pub fn all(&self) -> Vec<&str> {
        let mut out = Vec::with_capacity(1 + self.synonyms.len());
        out.push(self.head.as_str());
        out.extend(self.synonyms.iter().map(String::as_str));
        out
    }

    /// 是否未扩词。
    #[must_use]
    pub fn is_singleton(&self) -> bool {
        self.synonyms.is_empty()
    }
}

/// 扩展后的搜索意图。保留原 `SearchIntent` 不动，附带按 keyword 顺序对齐的同义词组。
/// 注：`SearchIntent` 含 `f64` 字段（`SizeExpression`），故只能 `PartialEq`，不实现 `Eq`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExpandedSearchIntent {
    pub base: SearchIntent,
    /// 与 `base` 中 *Search variant 的 keywords 顺序对齐；非 search variant 时为空。
    pub keyword_groups: Vec<KeywordGroup>,
    /// 组间复合匹配模式（全局配置注入，默认 `All`）。见 [`MatchMode`]。
    #[serde(default)]
    pub match_mode: MatchMode,
}

impl ExpandedSearchIntent {
    /// 构造一个未扩词的 expanded（恒等映射），`match_mode` 默认 [`MatchMode::All`]。
    #[must_use]
    pub fn identity(base: SearchIntent) -> Self {
        let keyword_groups = base
            .search_keywords()
            .map(|kws| {
                kws.iter()
                    .map(|kw| KeywordGroup::singleton(kw.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            base,
            keyword_groups,
            match_mode: MatchMode::default(),
        }
    }

    /// 链式设置 `match_mode`（全局配置读取后注入）。
    #[must_use]
    pub const fn with_match_mode(mut self, match_mode: MatchMode) -> Self {
        self.match_mode = match_mode;
        self
    }

    /// 是否所有组都未扩词。
    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.keyword_groups.iter().all(KeywordGroup::is_singleton)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn singleton_group_has_no_synonyms() {
        let g = KeywordGroup::singleton("工作汇报");
        assert_eq!(g.head, "工作汇报");
        assert!(g.synonyms.is_empty());
        assert!(g.is_singleton());
        assert_eq!(g.all(), vec!["工作汇报"]);
    }

    #[test]
    fn group_all_preserves_head_then_synonyms_order() {
        let g = KeywordGroup {
            head: "工作汇报".into(),
            synonyms: vec!["述职".into(), "年度总结".into()],
        };
        assert_eq!(g.all(), vec!["工作汇报", "述职", "年度总结"]);
        assert!(!g.is_singleton());
    }
}
