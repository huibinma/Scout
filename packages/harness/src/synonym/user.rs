//! BETA-11D 用户级持久化同义词词典：YAML 模型 + lint + 运行时可变 CRUD。
//!
//! 与系统词典（[`crate::synonym::yaml`]）的**唯一 schema 差异**：用户词典**允许组内跨语言 alias**
//! （目标 case「友商竞争分析 → AWS / Azure / 产品分析」需要），其余 lint 规则全部沿用系统层原语。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use scout_search_backend::{ExpandedSearchIntent, SearchIntent};

use crate::synonym::expander::SynonymExpander;
use crate::synonym::yaml::{
    classify, expand_with_view, DictView, KeywordLang, YamlSynonymExpander, MAX_ALIASES_PER_GROUP,
};

const SUPPORTED_VERSION: u32 = 1;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum UserDictError {
    #[error("解析用户词典 YAML 失败: {0}")]
    Parse(String),
    #[error("用户词典 version={got}，仅支持 version=1")]
    UnsupportedVersion { got: u32 },
    #[error("组 head={head:?}: aliases 数量 {n} 超过上限 {MAX_ALIASES_PER_GROUP}")]
    TooManyAliases { head: String, n: usize },
    #[error("组 head={head:?} 内出现重复词 {dup:?}")]
    DuplicateWithinGroup { head: String, dup: String },
    #[error("词 {word:?} 在多个组中作为 head 或 alias 重复出现")]
    DuplicateAcrossGroups { word: String },
    #[error("head 不能为空")]
    EmptyHead,
    #[error("不允许标识符样的词 {word:?}（含连字符/下划线/点等分隔符）")]
    IdentifierLike { word: String },
    #[error("词 {word:?} 无法被索引（混合语言单词 / 纯符号 / 纯数字）")]
    Unindexable { word: String },
}

/// 用户词典单组（YAML 序列化形态）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserGroup {
    pub head: String,
    #[serde(default)]
    pub aliases: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawUserDict {
    version: u32,
    #[serde(default)]
    groups: Vec<UserGroup>,
}

#[derive(Serialize)]
struct OutUserDict<'a> {
    version: u32,
    groups: &'a [UserGroup],
}

/// 运行时可变的用户词典：`groups` 为权威态，索引每次变更后重建。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct UserIndex {
    groups: Vec<UserGroup>,
    zh_index: HashMap<String, Arc<[String]>>,
    en_index: HashMap<String, Arc<[String]>>,
}

impl UserIndex {
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// 从 YAML 文本解析 + 全量 lint。任一组非法则整体拒绝。
    pub fn from_yaml_str(yaml: &str) -> Result<Self, UserDictError> {
        let raw: RawUserDict =
            serde_yaml::from_str(yaml).map_err(|e| UserDictError::Parse(e.to_string()))?;
        if raw.version != SUPPORTED_VERSION {
            return Err(UserDictError::UnsupportedVersion { got: raw.version });
        }
        Self::from_groups(raw.groups)
    }

    /// 从组列表构造（lint + 建索引）。
    pub fn from_groups(groups: Vec<UserGroup>) -> Result<Self, UserDictError> {
        // 归一化：trim head 和每个 alias，丢弃 trim 后为空的 alias。
        let groups: Vec<UserGroup> = groups
            .into_iter()
            .map(|g| UserGroup {
                head: g.head.trim().to_owned(),
                aliases: g
                    .aliases
                    .into_iter()
                    .map(|a| a.trim().to_owned())
                    .filter(|a| !a.is_empty())
                    .collect(),
            })
            .collect();
        lint_groups(&groups)?;
        let (zh_index, en_index) = build_user_indices(&groups);
        Ok(Self {
            groups,
            zh_index,
            en_index,
        })
    }

    #[must_use]
    pub fn groups(&self) -> &[UserGroup] {
        &self.groups
    }

    /// 序列化为 YAML 文本（持久化用）。
    #[must_use]
    pub fn to_yaml_str(&self) -> String {
        serde_yaml::to_string(&OutUserDict {
            version: SUPPORTED_VERSION,
            groups: &self.groups,
        })
        .unwrap_or_else(|_| "version: 1\ngroups: []\n".to_owned())
    }

    /// 添加或合并到同名 head 组（合并去重保序）。校验失败则不改原状态。
    ///
    /// 跨组重复词及（merge 分支）与 head 同名的 alias 由 lint 拦截，调用方得到 `Err` 而非静默错误。
    pub fn add_or_merge(&mut self, head: &str, aliases: Vec<String>) -> Result<(), UserDictError> {
        let head = head.trim().to_owned();
        // 先归一化 incoming：trim、去空、组内自去重（两分支一致行为）。
        let mut seen = std::collections::HashSet::new();
        let incoming: Vec<String> = aliases
            .into_iter()
            .map(|a| a.trim().to_owned())
            .filter(|a| !a.is_empty())
            .filter(|a| seen.insert(a.clone()))
            .collect();
        let mut candidate = self.groups.clone();
        if let Some(g) = candidate.iter_mut().find(|g| g.head == head) {
            for a in incoming {
                if a != g.head && !g.aliases.contains(&a) {
                    g.aliases.push(a);
                }
            }
        } else {
            candidate.push(UserGroup {
                head,
                aliases: incoming,
            });
        }
        self.replace_groups(candidate)
    }

    /// 替换某 head 组的 aliases（head 不存在则等价新增）。
    ///
    /// 跨组重复词或 alias 等于 head 由 lint 拦截，调用方得到 `Err` 而非静默错误。
    pub fn update(&mut self, head: &str, aliases: Vec<String>) -> Result<(), UserDictError> {
        let head = head.trim().to_owned();
        let aliases: Vec<String> = aliases
            .into_iter()
            .map(|a| a.trim().to_owned())
            .filter(|a| !a.is_empty())
            .collect();
        let mut candidate = self.groups.clone();
        match candidate.iter_mut().find(|g| g.head == head) {
            Some(g) => g.aliases = aliases,
            None => candidate.push(UserGroup { head, aliases }),
        }
        self.replace_groups(candidate)
    }

    /// 删除某 head 组，返回是否删到。
    #[allow(clippy::expect_used)]
    pub fn remove(&mut self, head: &str) -> bool {
        if !self.groups.iter().any(|g| g.head == head) {
            return false;
        }
        let candidate: Vec<UserGroup> = self
            .groups
            .iter()
            .filter(|g| g.head != head)
            .cloned()
            .collect();
        self.replace_groups(candidate)
            .expect("remove: 在已合法词典上做删除不应触发 lint 失败");
        true
    }

    /// 用候选组列表替换全量（先 lint，成功才提交——失败原子回滚）。
    fn replace_groups(&mut self, candidate: Vec<UserGroup>) -> Result<(), UserDictError> {
        let next = Self::from_groups(candidate)?;
        *self = next;
        Ok(())
    }

    /// 按语言查 keyword 所在组的全体成员（含 head）。供 `LayeredSynonymExpander` 实现 `DictView` 使用。
    pub(crate) fn lookup(&self, lang: KeywordLang, keyword: &str) -> Option<Arc<[String]>> {
        match lang {
            KeywordLang::Zh => self.zh_index.get(keyword).cloned(),
            KeywordLang::En => self.en_index.get(keyword).cloned(),
            KeywordLang::Skip => None,
        }
    }

    /// 所有索引键（zh + en）。每个词由 `classify` 路由到唯一 index，故两 index 间无重复键。
    pub(crate) fn keys(&self) -> impl Iterator<Item = &String> {
        self.zh_index.keys().chain(self.en_index.keys())
    }
}

// ─── LayeredSynonymExpander ───────────────────────────────────────────────────

/// 双层同义词扩展器：用户层（可变）覆盖系统层（只读）。冲突 keyword 用用户组替换系统组。
#[derive(Debug)]
pub struct LayeredSynonymExpander {
    system: YamlSynonymExpander,
    user: Arc<RwLock<UserIndex>>,
}

impl LayeredSynonymExpander {
    #[must_use]
    pub fn new(system: YamlSynonymExpander, user: Arc<RwLock<UserIndex>>) -> Self {
        Self { system, user }
    }
}

impl DictView for LayeredSynonymExpander {
    #[allow(clippy::print_stderr)]
    fn lookup(&self, lang: KeywordLang, keyword: &str) -> Option<Arc<[String]>> {
        // 用户层优先（替换语义）。
        if let Ok(user) = self.user.read() {
            if let Some(hit) = user.lookup(lang, keyword) {
                return Some(hit);
            }
        } else {
            eprintln!("synonym: 用户词典锁中毒，本次降级为系统层");
        }
        self.system.lookup(lang, keyword)
    }

    #[allow(clippy::print_stderr)]
    fn all_keys(&self) -> Vec<String> {
        let mut keys = self.system.all_keys();
        if let Ok(user) = self.user.read() {
            keys.extend(user.keys().cloned());
        } else {
            eprintln!("synonym: 用户词典锁中毒，本次降级为系统层");
        }
        // HashMap 键序不定；sort+dedup 得键集并集
        keys.sort();
        keys.dedup();
        keys
    }

    #[allow(clippy::print_stderr)]
    fn multiword_keys(&self) -> Vec<String> {
        let mut keys = self.system.multiword_keys();
        if let Ok(user) = self.user.read() {
            keys.extend(user.keys().filter(|k| k.contains(' ')).cloned());
        } else {
            eprintln!("synonym: 用户词典锁中毒，本次降级为系统层");
        }
        keys.sort();
        keys.dedup();
        keys
    }

    fn is_type_or_media_key(&self, key: &str) -> bool {
        // 用户教学的同义词均为内容词（不带 domain），类型 / 媒体守护只看系统层。
        self.system.is_type_or_media_key(key)
    }
}

impl SynonymExpander for LayeredSynonymExpander {
    fn expand(&self, intent: SearchIntent, query: &str) -> ExpandedSearchIntent {
        expand_with_view(self, intent, query)
    }
}

// ─── lint helpers ─────────────────────────────────────────────────────────────

/// lint 一组用户词条（污染防护，复用系统层原语）。
fn lint_groups(groups: &[UserGroup]) -> Result<(), UserDictError> {
    let mut seen_words: HashSet<&str> = HashSet::new();
    for g in groups {
        if g.head.trim().is_empty() {
            return Err(UserDictError::EmptyHead);
        }
        if g.aliases.len() > MAX_ALIASES_PER_GROUP {
            return Err(UserDictError::TooManyAliases {
                head: g.head.clone(),
                n: g.aliases.len(),
            });
        }
        let members: Vec<&str> = std::iter::once(g.head.as_str())
            .chain(g.aliases.iter().map(String::as_str))
            .collect();
        let mut intra: HashSet<&str> = HashSet::new();
        for w in &members {
            if is_identifier_like(w) {
                return Err(UserDictError::IdentifierLike { word: (*w).into() });
            }
            if classify(w) == KeywordLang::Skip {
                return Err(UserDictError::Unindexable { word: (*w).into() });
            }
            if !intra.insert(*w) {
                return Err(UserDictError::DuplicateWithinGroup {
                    head: g.head.clone(),
                    dup: (*w).into(),
                });
            }
        }
        for w in &members {
            if !seen_words.insert(*w) {
                return Err(UserDictError::DuplicateAcrossGroups { word: (*w).into() });
            }
        }
    }
    Ok(())
}

/// 标识符样：含连字符 / 下划线 / 点（如 `synthetic-place`）。对齐 BETA-11 §4.2 `NoopExpand` 规则。
fn is_identifier_like(word: &str) -> bool {
    word.chars().any(|c| matches!(c, '-' | '_' | '.'))
}

type LangIndex = HashMap<String, Arc<[String]>>;

/// 按各成员语言建立双向索引（与系统层 `build_index` 同语义，但允许组内跨语言成员）。
///
/// `classify` 返回 `Skip` 的词不会进入任何索引（经 Fix-1 lint 后，存储词典中该分支不可达）。
fn build_user_indices(groups: &[UserGroup]) -> (LangIndex, LangIndex) {
    let mut zh = HashMap::new();
    let mut en = HashMap::new();
    for g in groups {
        let members: Arc<[String]> = std::iter::once(g.head.clone())
            .chain(g.aliases.iter().cloned())
            .collect::<Vec<String>>()
            .into();
        for w in members.iter() {
            match classify(w) {
                KeywordLang::Zh => {
                    zh.insert(w.clone(), Arc::clone(&members));
                }
                KeywordLang::En => {
                    en.insert(w.clone(), Arc::clone(&members));
                }
                KeywordLang::Skip => {}
            }
        }
    }
    (zh, en)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    #[test]
    fn parses_cross_language_group() {
        let yaml =
            "version: 1\ngroups:\n  - head: 友商竞争分析\n    aliases: [AWS, Azure, 产品分析]\n";
        let idx = UserIndex::from_yaml_str(yaml).unwrap();
        assert_eq!(idx.groups().len(), 1);
        assert_eq!(idx.groups()[0].head, "友商竞争分析");
        // 中文 head 入 zh_index，英文 alias 入 en_index，均指向同组。
        assert!(idx.zh_index.contains_key("友商竞争分析"));
        assert!(idx.en_index.contains_key("AWS"));
    }

    #[test]
    fn rejects_too_many_aliases() {
        let yaml = "version: 1\ngroups:\n  - head: h\n    aliases: [a1,a2,a3,a4,a5,a6,a7,a8,a9]\n";
        assert_eq!(
            UserIndex::from_yaml_str(yaml),
            Err(UserDictError::TooManyAliases {
                head: "h".into(),
                n: 9
            })
        );
    }

    #[test]
    fn rejects_identifier_like() {
        let yaml = "version: 1\ngroups:\n  - head: 报告\n    aliases: [synthetic-place]\n";
        assert_eq!(
            UserIndex::from_yaml_str(yaml),
            Err(UserDictError::IdentifierLike {
                word: "synthetic-place".into()
            })
        );
    }

    #[test]
    fn rejects_duplicate_across_groups() {
        let yaml = "version: 1\ngroups:\n  - head: 报告\n    aliases: [汇报]\n  - head: 文档\n    aliases: [汇报]\n";
        assert_eq!(
            UserIndex::from_yaml_str(yaml),
            Err(UserDictError::DuplicateAcrossGroups {
                word: "汇报".into()
            })
        );
    }

    #[test]
    fn yaml_roundtrip() {
        let yaml = "version: 1\ngroups:\n  - head: 报告\n    aliases: [汇报, report]\n";
        let idx = UserIndex::from_yaml_str(yaml).unwrap();
        let out = idx.to_yaml_str();
        let reparsed = UserIndex::from_yaml_str(&out).unwrap();
        assert_eq!(reparsed.groups(), idx.groups());
    }

    #[test]
    fn rejects_empty_head() {
        let yaml = "version: 1\ngroups:\n  - head: ''\n    aliases: [汇报]\n";
        assert_eq!(
            UserIndex::from_yaml_str(yaml),
            Err(UserDictError::EmptyHead)
        );
    }

    #[test]
    fn rejects_duplicate_within_group() {
        let yaml = "version: 1\ngroups:\n  - head: 报告\n    aliases: [报告]\n";
        assert!(matches!(
            UserIndex::from_yaml_str(yaml),
            Err(UserDictError::DuplicateWithinGroup { .. })
        ));
    }

    #[test]
    fn rejects_unindexable_mixed_token() {
        // 产品AWS 同时含 CJK 和 ASCII 字母 → classify 返回 Skip → Unindexable
        let yaml = "version: 1\ngroups:\n  - head: 报告\n    aliases: [产品AWS]\n";
        assert!(matches!(
            UserIndex::from_yaml_str(yaml),
            Err(UserDictError::Unindexable { .. })
        ));
    }

    #[test]
    fn layered_user_only_term_expands_via_gazetteer() {
        use crate::synonym::yaml::YamlSynonymExpander;
        use scout_search_backend::{FileSearch, SchemaVersion, SearchIntent};
        use std::path::Path;

        let system = YamlSynonymExpander::from_str(
            "version: 1\nlanguage: zh\ngroups: []\n",
            Path::new("zh.yaml"),
            "version: 1\nlanguage: en\ngroups: []\n",
            Path::new("en.yaml"),
        )
        .unwrap();
        let user = Arc::new(RwLock::new(
            UserIndex::from_yaml_str(
                "version: 1\ngroups:\n  - head: 友商竞争分析\n    aliases: [AWS, Azure]\n",
            )
            .unwrap(),
        ));
        let exp = LayeredSynonymExpander::new(system, user);
        let intent = SearchIntent::FileSearch(FileSearch {
            schema_version: SchemaVersion::V1,
            language: None,
            keywords: None, // keywords: None 强制走 gazetteer（扫 query 原文）路径
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
        let out = exp.expand(intent, "友商竞争分析");
        assert_eq!(out.keyword_groups.len(), 1);
        let g = &out.keyword_groups[0];
        assert_eq!(g.head, "友商竞争分析");
        assert!(g.synonyms.contains(&"AWS".to_owned()));
        assert!(g.synonyms.contains(&"Azure".to_owned()));
    }

    #[test]
    fn layered_empty_user_matches_system_only() {
        use crate::synonym::expander::SynonymExpander;
        use crate::synonym::yaml::YamlSynonymExpander;
        use scout_search_backend::{FileSearch, SchemaVersion, SearchIntent};
        use std::path::Path;

        let zh = "version: 1\nlanguage: zh\ngroups:\n  - head: 工作汇报\n    aliases: [述职]\n";
        let en = "version: 1\nlanguage: en\ngroups: []\n";
        let intent = || {
            SearchIntent::FileSearch(FileSearch {
                schema_version: SchemaVersion::V1,
                language: None,
                keywords: Some(vec!["工作汇报".to_owned()]),
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
        };
        let sys_only =
            YamlSynonymExpander::from_str(zh, Path::new("zh.yaml"), en, Path::new("en.yaml"))
                .unwrap()
                .expand(intent(), "工作汇报");
        let layered = LayeredSynonymExpander::new(
            YamlSynonymExpander::from_str(zh, Path::new("zh.yaml"), en, Path::new("en.yaml"))
                .unwrap(),
            Arc::new(RwLock::new(UserIndex::empty())),
        )
        .expand(intent(), "工作汇报");
        assert_eq!(
            layered.keyword_groups, sys_only.keyword_groups,
            "用户层空时与系统层逐字节一致"
        );
    }

    #[test]
    fn add_or_merge_dedups_same_head() {
        let mut idx = UserIndex::empty();
        idx.add_or_merge("报告", vec!["汇报".into()]).unwrap();
        idx.add_or_merge("报告", vec!["汇报".into(), "周报".into()])
            .unwrap();
        assert_eq!(idx.groups().len(), 1, "同 head 合并不新增组");
        assert_eq!(
            idx.groups()[0].aliases,
            vec!["汇报", "周报"],
            "合并去重保序"
        );
    }

    #[test]
    fn add_rejects_too_many_after_merge() {
        let mut idx = UserIndex::empty();
        idx.add_or_merge(
            "h",
            vec!["a1".into(), "a2".into(), "a3".into(), "a4".into()],
        )
        .unwrap();
        let err = idx
            .add_or_merge(
                "h",
                vec![
                    "a5".into(),
                    "a6".into(),
                    "a7".into(),
                    "a8".into(),
                    "a9".into(),
                ],
            )
            .unwrap_err();
        assert!(matches!(err, UserDictError::TooManyAliases { .. }));
        // 失败后原状态不变（仍 4 个 alias）。
        assert_eq!(idx.groups()[0].aliases.len(), 4);
    }

    #[test]
    fn update_replaces_aliases() {
        let mut idx = UserIndex::empty();
        idx.add_or_merge("报告", vec!["汇报".into()]).unwrap();
        idx.update("报告", vec!["周报".into(), "月报".into()])
            .unwrap();
        assert_eq!(idx.groups()[0].aliases, vec!["周报", "月报"]);
    }

    #[test]
    fn remove_drops_group() {
        let mut idx = UserIndex::empty();
        idx.add_or_merge("报告", vec!["汇报".into()]).unwrap();
        assert!(idx.remove("报告"));
        assert!(idx.groups().is_empty());
        assert!(!idx.remove("不存在"));
    }
}
