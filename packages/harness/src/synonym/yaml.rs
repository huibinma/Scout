//! YAML 词典加载 + lint + 同义词扩展。

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

use scout_search_backend::{ExpandedSearchIntent, KeywordGroup, SearchIntent};

use crate::synonym::expander::SynonymExpander;

const SUPPORTED_VERSION: u32 = 1;
pub(crate) const MAX_ALIASES_PER_GROUP: usize = 8;

#[derive(Debug, Error)]
pub enum ExpanderError {
    #[error("读取词典文件 {path:?} 失败: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("解析 YAML {path:?} 失败: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("词典 {path:?} version={got},仅支持 version=1")]
    UnsupportedVersion { path: PathBuf, got: u32 },
    #[error("词典 {path:?} language={got},此文件期望 language={expected}")]
    LanguageMismatch {
        path: PathBuf,
        expected: &'static str,
        got: String,
    },
    #[error("词典 {path:?} 组 head={head:?}: aliases 数量 {n} > 上限 {MAX_ALIASES_PER_GROUP}")]
    TooManyAliases {
        path: PathBuf,
        head: String,
        n: usize,
    },
    #[error("词典 {path:?} 组 head={head:?} 含跨语言 alias {alias:?}(expected {expected})")]
    CrossLanguageAlias {
        path: PathBuf,
        head: String,
        alias: String,
        expected: &'static str,
    },
    #[error("词典 {path:?} 组 head={head:?} 内出现重复词 {dup:?}")]
    DuplicateWithinGroup {
        path: PathBuf,
        head: String,
        dup: String,
    },
    #[error("词典 {path:?} 词 {word:?} 在多个组中作为 head 或 alias 出现")]
    DuplicateAcrossGroups { path: PathBuf, word: String },
}

#[derive(Debug, Deserialize)]
struct RawDict {
    version: u32,
    language: String,
    groups: Vec<RawGroup>,
}

#[derive(Debug, Deserialize)]
struct RawGroup {
    head: String,
    aliases: Vec<String>,
    #[serde(default)]
    domain: Option<String>,
}

/// 解析后的词典。
#[derive(Debug, Clone)]
pub(crate) struct ParsedDict {
    #[allow(dead_code)]
    pub(crate) language: &'static str,
    pub(crate) groups: Vec<ParsedGroup>,
}

/// 词典中单个同义词组。
#[derive(Debug, Clone)]
pub(crate) struct ParsedGroup {
    pub(crate) head: String,
    pub(crate) aliases: Vec<String>,
    /// 词典作者标注的语义域：`office`/`personal`/`document`/`design` 为内容主题桶；
    /// `file_type`/`media` 为类型 / 媒体词（不应被 gazetteer 当内容词召回）。`None` 视为内容词。
    pub(crate) domain: Option<String>,
}

/// 该 domain 是否为「类型 / 媒体」域（非内容词）。`gazetteer` 内容词提取据此与
/// parser 重解析守护并用：任一判定为类型词即跳过，只收紧不放宽。
pub(crate) fn is_type_or_media_domain(domain: Option<&str>) -> bool {
    matches!(domain, Some("file_type" | "media"))
}

/// 从 YAML 字符串解析并 lint 词典。
///
/// - `expected_lang`：期望的语言标签（`"zh"` 或 `"en"`）。
/// - 构造期 fail-fast：任何 lint 错误立即返回 `Err`。
pub(crate) fn parse_dict_str(
    yaml: &str,
    path: &Path,
    expected_lang: &'static str,
) -> Result<ParsedDict, ExpanderError> {
    let raw: RawDict = serde_yaml::from_str(yaml).map_err(|source| ExpanderError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    if raw.version != SUPPORTED_VERSION {
        return Err(ExpanderError::UnsupportedVersion {
            path: path.to_path_buf(),
            got: raw.version,
        });
    }
    if raw.language != expected_lang {
        return Err(ExpanderError::LanguageMismatch {
            path: path.to_path_buf(),
            expected: expected_lang,
            got: raw.language,
        });
    }

    let mut seen_words: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut groups = Vec::with_capacity(raw.groups.len());
    for g in raw.groups {
        if g.aliases.len() > MAX_ALIASES_PER_GROUP {
            return Err(ExpanderError::TooManyAliases {
                path: path.to_path_buf(),
                head: g.head,
                n: g.aliases.len(),
            });
        }
        let all: Vec<&str> = std::iter::once(g.head.as_str())
            .chain(g.aliases.iter().map(String::as_str))
            .collect();
        let mut intra: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for w in &all {
            if !intra.insert(*w) {
                return Err(ExpanderError::DuplicateWithinGroup {
                    path: path.to_path_buf(),
                    head: g.head.clone(),
                    dup: (*w).into(),
                });
            }
            if has_cross_language_char(w, expected_lang) {
                return Err(ExpanderError::CrossLanguageAlias {
                    path: path.to_path_buf(),
                    head: g.head.clone(),
                    alias: (*w).into(),
                    expected: expected_lang,
                });
            }
        }
        for w in &all {
            if !seen_words.insert((*w).into()) {
                return Err(ExpanderError::DuplicateAcrossGroups {
                    path: path.to_path_buf(),
                    word: (*w).into(),
                });
            }
        }
        groups.push(ParsedGroup {
            head: g.head,
            aliases: g.aliases,
            domain: g.domain,
        });
    }
    Ok(ParsedDict {
        language: expected_lang,
        groups,
    })
}

fn has_cross_language_char(word: &str, expected_lang: &str) -> bool {
    let has_ascii_alpha = word.chars().any(|c| c.is_ascii_alphabetic());
    let has_cjk = word.chars().any(is_cjk);
    match expected_lang {
        "zh" => has_ascii_alpha,
        "en" => has_cjk,
        _ => false,
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF |   // CJK Unified Ideographs
        0x3400..=0x4DBF |   // CJK Extension A
        0x20000..=0x2A6DF | // CJK Extension B
        0x3000..=0x303F |   // CJK 符号与标点
        0xFF00..=0xFFEF     // 全角形式
    )
}

// ─── 运行期 cap ───────────────────────────────────────────────────────────────

const RUNTIME_KEYWORD_CAP: usize = 32;

/// 截断扩展结果至 `cap` 词总量。
/// 返回 (截断后的 groups, 是否触发截断 warn)。
pub(crate) fn cap_keyword_groups(
    mut groups: Vec<KeywordGroup>,
    cap: usize,
) -> (Vec<KeywordGroup>, bool) {
    let total: usize = groups.iter().map(|g| 1 + g.synonyms.len()).sum();
    if total <= cap {
        return (groups, false);
    }
    let mut budget = cap;
    let mut new_groups = Vec::with_capacity(groups.len());
    for g in groups.drain(..) {
        if budget == 0 {
            break;
        }
        // head 必保
        let head = g.head;
        budget = budget.saturating_sub(1);
        let take = budget.min(g.synonyms.len());
        let synonyms: Vec<String> = g.synonyms.into_iter().take(take).collect();
        budget -= take;
        new_groups.push(KeywordGroup { head, synonyms });
    }
    (new_groups, true)
}

// ─── YamlSynonymExpander ──────────────────────────────────────────────────────

/// 扩展算法所需的词典只读视图。`YamlSynonymExpander`（系统层）与 `LayeredSynonymExpander`
/// （双层）各自实现，使一套扩展算法复用于两种词典形态。
pub(crate) trait DictView {
    /// 按语言查 keyword 所在组的全体成员（含 head）。
    /// 实现者应返回 `Arc::clone`（仅引用计数加一），不应新建 allocation。
    fn lookup(&self, lang: KeywordLang, keyword: &str) -> Option<Arc<[String]>>;
    /// 全部索引键（zh + en），供 gazetteer 扫描。顺序不影响结果（下游确定性排序）。
    ///
    /// 返回 owned `Vec<String>` 是有意为之：`LayeredSynonymExpander` 的用户层存于
    /// `RwLock` 读锁后，无法跨 guard 生命期返回借用迭代器，故 trait 统一返回 owned 键。
    /// 每次 `expand` 分配数百个小 String，相比 `query.find` + parser 重解析的 per-key
    /// 成本可忽略不计。
    fn all_keys(&self) -> Vec<String>;
    /// 含空格的多词键，供多词覆盖。顺序不影响结果（覆盖循环按最长优先取 best，与 key 枚举顺序无关）。
    fn multiword_keys(&self) -> Vec<String>;
    /// 该键是否属 `file_type`/`media` domain（类型 / 媒体词）。gazetteer 内容词提取据此跳过，
    /// 与 `is_pure_content_term` 的 parser 重解析守护并用（任一为真即跳过，只收紧不放宽）。
    fn is_type_or_media_key(&self, key: &str) -> bool;
}

/// YAML 词典实现的 `SynonymExpander`。
#[derive(Debug)]
pub struct YamlSynonymExpander {
    /// head/alias -> 该组所有成员（含 head）。
    zh_index: HashMap<String, Arc<[String]>>,
    en_index: HashMap<String, Arc<[String]>>,
    /// 属 `file_type`/`media` domain 的全部成员词（head + alias，zh+en 合并）。
    /// gazetteer 内容词提取据此跳过类型 / 媒体词，补 parser 重解析对短英文类型词（如
    /// `document`/`file`）的假阳漏洞。
    type_media_keys: HashSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeywordLang {
    Zh,
    En,
    /// 混合（含 ASCII 字母 + 含 CJK）、纯符号/数字等。不扩词。
    Skip,
}

impl YamlSynonymExpander {
    /// 从两份 YAML 文件加载（zh + en）。
    pub fn from_paths(zh: &Path, en: &Path) -> Result<Self, ExpanderError> {
        let zh_yaml = std::fs::read_to_string(zh).map_err(|source| ExpanderError::Io {
            path: zh.to_path_buf(),
            source,
        })?;
        let en_yaml = std::fs::read_to_string(en).map_err(|source| ExpanderError::Io {
            path: en.to_path_buf(),
            source,
        })?;
        Self::from_str(&zh_yaml, zh, &en_yaml, en)
    }

    /// 从字符串构造（测试友好）。
    pub fn from_str(
        zh_yaml: &str,
        zh_path: &Path,
        en_yaml: &str,
        en_path: &Path,
    ) -> Result<Self, ExpanderError> {
        let zh = parse_dict_str(zh_yaml, zh_path, "zh")?;
        let en = parse_dict_str(en_yaml, en_path, "en")?;
        let mut type_media_keys = collect_type_media_keys(&zh.groups);
        type_media_keys.extend(collect_type_media_keys(&en.groups));
        Ok(Self {
            zh_index: build_index(&zh.groups),
            en_index: build_index(&en.groups),
            type_media_keys,
        })
    }
}

impl DictView for YamlSynonymExpander {
    fn lookup(&self, lang: KeywordLang, keyword: &str) -> Option<Arc<[String]>> {
        match lang {
            KeywordLang::Zh => self.zh_index.get(keyword).cloned(),
            KeywordLang::En => self.en_index.get(keyword).cloned(),
            KeywordLang::Skip => None,
        }
    }
    fn all_keys(&self) -> Vec<String> {
        self.zh_index
            .keys()
            .chain(self.en_index.keys())
            .cloned()
            .collect()
    }
    fn multiword_keys(&self) -> Vec<String> {
        self.zh_index
            .keys()
            .chain(self.en_index.keys())
            .filter(|k| k.contains(' '))
            .cloned()
            .collect()
    }
    fn is_type_or_media_key(&self, key: &str) -> bool {
        self.type_media_keys.contains(key)
    }
}

/// 重解析守护：候选词若被 parser 分类为类型/媒体/扩展名信号则非纯内容词。
/// 以 parser 为类型判定单一信源 —— 内容名词短语（工作汇报/报告/合同…）解析后
/// 无 `file_type`/`extensions` 且为 `FileSearch` 变体；类型词（如"幻灯片"判为
/// presentation）、媒体词（如"截图"判为 media 搜索）会被排除。
fn is_pure_content_term(term: &str) -> bool {
    match scout_intent_parser::parse(term) {
        SearchIntent::FileSearch(fs) => fs.file_type.is_none() && fs.extensions.is_none(),
        _ => false,
    }
}

/// 裸内容词兜底：parser 无 keyword 且 gazetteer 词典无命中时，若整条查询本身就是一个
/// 纯内容词查询（无 `file_type` / `extensions` / `location` / 已有 keyword），则把它（剥离前导
/// 搜索动词后）作为内容关键词注入。让「英语」「合同」「简历」这类非词典裸词也能走内容搜索，
/// 而非退化成 match-all。
///
/// 守护策略：先以**整条查询**重解析判定——含类型 / 扩展名 / 媒体 / 位置信号即放弃（交回原
/// 路径），避免把「找一份ppt」「下载里的东西」误当关键词；通过后再剥离动词得到内容部分。
fn bare_content_keyword(query: &str) -> Option<String> {
    let trimmed = query.trim();
    if trimmed.is_empty() || !is_bare_content_query(trimmed) {
        return None;
    }
    let candidate = strip_leading_search_verbs(trimmed);
    (!candidate.is_empty()).then(|| candidate.to_owned())
}

/// 整条查询是否为「纯内容词」FileSearch（无任何类型 / 扩展名 / 位置 / 已有 keyword 信号）。
fn is_bare_content_query(query: &str) -> bool {
    matches!(
        scout_intent_parser::parse(query),
        SearchIntent::FileSearch(fs)
            if fs.file_type.is_none()
                && fs.extensions.is_none()
                && fs.location.is_none()
                && fs.keywords.as_ref().map_or(true, Vec::is_empty)
    )
}

/// 剥离前导搜索动词，返回内容部分。中文动词按长优先逐个剥；英文动词需后接空白以免误伤
/// 形如 `findings` 的内容词。
fn strip_leading_search_verbs(query: &str) -> &str {
    const ZH_VERBS: &[&str] = &["搜索", "查找", "查询", "找", "搜", "查"];
    const EN_VERBS: &[&str] = &["find ", "search ", "look for "];
    let mut s = query.trim();
    loop {
        let stripped = ZH_VERBS
            .iter()
            .find_map(|verb| s.strip_prefix(verb))
            .or_else(|| EN_VERBS.iter().find_map(|verb| s.strip_prefix(verb)));
        match stripped {
            Some(rest) => s = rest.trim_start(),
            None => break,
        }
    }
    s
}

// ─── 共享扩展算法自由函数 ──────────────────────────────────────────────────────

fn expand_one(view: &dyn DictView, keyword: &str) -> KeywordGroup {
    let lang = classify(keyword);
    match view.lookup(lang, keyword) {
        None => KeywordGroup::singleton(keyword),
        Some(group_members) => {
            let synonyms: Vec<String> = group_members
                .iter()
                .filter(|w| w.as_str() != keyword)
                .cloned()
                .collect();
            KeywordGroup {
                head: keyword.to_string(),
                synonyms,
            }
        }
    }
}

fn gazetteer_lookup_multi(view: &dyn DictView, query: &str) -> Vec<String> {
    let mut cands: Vec<(usize, usize, usize, String)> = Vec::new();
    for key in view.all_keys() {
        let Some(pos) = query.find(key.as_str()) else {
            continue;
        };
        // 类型词守护：domain 标注（权威）或 parser 重解析任一判为类型 / 媒体词即跳过。
        if view.is_type_or_media_key(&key) || !is_pure_content_term(&key) {
            continue;
        }
        cands.push((key.chars().count(), pos, pos + key.len(), key));
    }
    cands.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut chosen: Vec<(usize, usize, String)> = Vec::new();
    for (_, start, end, key) in cands {
        let overlaps = chosen.iter().any(|(s, e, _)| start < *e && *s < end);
        if !overlaps {
            chosen.push((start, end, key));
        }
    }
    chosen.sort_by_key(|(s, _, _)| *s);
    chosen.into_iter().map(|(_, _, k)| k).collect()
}

fn apply_multiword_override(view: &dyn DictView, query: &str, groups: &mut [KeywordGroup]) {
    let lower = query.to_lowercase();
    let multiword: Vec<String> = view.multiword_keys();
    for slot in groups.iter_mut() {
        let kw = slot.head.to_lowercase();
        let mut best: Option<(usize, usize, String)> = None; // (字符长, 首现位置, key)
        for key in &multiword {
            let key_l = key.to_lowercase();
            // 词边界匹配：keyword 必须是多词键中的**完整单词**之一（而非任意子串），
            // 避免 "over" 误配 "cover letter" 等（词典增长后的潜在 bug）。
            if !key_l.split_ascii_whitespace().any(|w| w == kw.as_str()) {
                continue;
            }
            let Some(pos) = lower.find(&*key_l) else {
                continue;
            };
            // 类型词守护：domain 标注（权威）或 parser 重解析任一判为类型 / 媒体词即跳过。
            if view.is_type_or_media_key(key) || !is_pure_content_term(key) {
                continue;
            }
            let len = key.chars().count();
            let better = match best {
                None => true,
                Some((blen, bpos, _)) => len > blen || (len == blen && pos < bpos),
            };
            if better {
                best = Some((len, pos, key.clone()));
            }
        }
        if let Some((_, _, key)) = best {
            *slot = expand_one(view, &key);
        }
    }
}

/// 双视图共享的扩展算法主体（原 `YamlSynonymExpander::expand` 逻辑，逐字节等价）。
pub(crate) fn expand_with_view(
    view: &dyn DictView,
    intent: SearchIntent,
    query: &str,
) -> ExpandedSearchIntent {
    let gaz = gazetteer_lookup_multi(view, query);
    let groups =
        if let Some(merged) = merge_or_group(gaz.iter().map(|k| expand_one(view, k)).collect()) {
            vec![merged]
        } else {
            match intent.search_keywords() {
                Some(kws) if !kws.is_empty() => {
                    let mut gs = kws
                        .iter()
                        .map(|kw| expand_one(view, kw))
                        .collect::<Vec<_>>();
                    apply_multiword_override(view, query, &mut gs);
                    dedup_groups_by_head(&mut gs);
                    gs
                }
                _ => bare_content_keyword(query)
                    .map(|matched| vec![expand_one(view, &matched)])
                    .unwrap_or_default(),
            }
        };
    let (groups, _warn_truncated) = cap_keyword_groups(groups, RUNTIME_KEYWORD_CAP);
    ExpandedSearchIntent {
        base: intent,
        keyword_groups: groups,
        match_mode: scout_search_backend::MatchMode::default(),
    }
}

impl SynonymExpander for YamlSynonymExpander {
    fn expand(&self, intent: SearchIntent, query: &str) -> ExpandedSearchIntent {
        expand_with_view(self, intent, query)
    }
}

/// 按 head 去重 keyword 组，保留首现顺序（多词键覆盖后两 token 可能映射到同一组）。
fn dedup_groups_by_head(groups: &mut Vec<KeywordGroup>) {
    let mut seen = std::collections::HashSet::new();
    groups.retain(|g| seen.insert(g.head.clone()));
}

/// 把多个 keyword 组合并成单个 OR 组：首组 head 作合并 head，其余组的 head + 全部 synonyms
/// 按序去重并入 synonyms（排除与 head 重复者）。空输入返回 `None`；单元素输入返回该组本身
/// （保证单概念与历史行为一致）。
fn merge_or_group(groups: Vec<KeywordGroup>) -> Option<KeywordGroup> {
    let mut iter = groups.into_iter();
    let first = iter.next()?;
    let head = first.head;
    let mut synonyms = first.synonyms;
    for g in iter {
        synonyms.push(g.head);
        synonyms.extend(g.synonyms);
    }
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(head.clone());
    synonyms.retain(|s| seen.insert(s.clone()));
    Some(KeywordGroup { head, synonyms })
}

/// 收集所有 `file_type`/`media` domain 组的成员词（head + alias），供类型词守护查表。
fn collect_type_media_keys(groups: &[ParsedGroup]) -> HashSet<String> {
    let mut keys = HashSet::new();
    for g in groups {
        if is_type_or_media_domain(g.domain.as_deref()) {
            keys.insert(g.head.clone());
            keys.extend(g.aliases.iter().cloned());
        }
    }
    keys
}

/// 从 `ParsedGroup` 列表建立双向（head + alias）倒排索引。
fn build_index(groups: &[ParsedGroup]) -> HashMap<String, Arc<[String]>> {
    let mut idx = HashMap::new();
    for g in groups {
        let members: Arc<[String]> = std::iter::once(g.head.clone())
            .chain(g.aliases.iter().cloned())
            .collect::<Vec<String>>()
            .into();
        for w in members.iter() {
            idx.insert(w.clone(), Arc::clone(&members));
        }
    }
    idx
}

/// 判断 keyword 的主要语言，用于选择索引。
pub(crate) fn classify(keyword: &str) -> KeywordLang {
    let has_ascii_alpha = keyword.chars().any(|c| c.is_ascii_alphabetic());
    let has_cjk = keyword.chars().any(is_cjk);
    // 标识符样符号：连字符、下划线、其它 ASCII 标点（非字母数字非空白）。
    // 用于侦测 `synthetic-place` 这类带分隔符的标识符 keyword。
    let has_other_ascii = keyword
        .chars()
        .any(|c| c.is_ascii() && !c.is_ascii_alphanumeric() && !c.is_whitespace());
    let only_digits_or_symbols = !has_ascii_alpha && !has_cjk;

    if only_digits_or_symbols {
        return KeywordLang::Skip;
    }
    if has_ascii_alpha && (has_cjk || has_other_ascii) {
        return KeywordLang::Skip;
    }
    if has_cjk && !has_ascii_alpha {
        return KeywordLang::Zh;
    }
    if has_ascii_alpha && !has_cjk {
        return KeywordLang::En;
    }
    KeywordLang::Skip
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic)]
    use super::*;
    use std::path::PathBuf;

    fn p() -> PathBuf {
        PathBuf::from("/tmp/test.yaml")
    }

    #[test]
    fn accepts_well_formed_zh_dict() {
        let yaml = r"
version: 1
language: zh
groups:
  - head: 工作汇报
    aliases: [述职, 年度总结]
  - head: 截图
    aliases: [截屏, 屏幕截图]
";
        let d = parse_dict_str(yaml, &p(), "zh").unwrap();
        assert_eq!(d.language, "zh");
        assert_eq!(d.groups.len(), 2);
        assert_eq!(d.groups[0].head, "工作汇报");
        assert_eq!(d.groups[0].aliases, vec!["述职", "年度总结"]);
    }

    #[test]
    fn rejects_too_many_aliases() {
        let yaml = r"
version: 1
language: zh
groups:
  - head: 工作汇报
    aliases: [a1, a2, a3, a4, a5, a6, a7, a8, a9]
";
        match parse_dict_str(yaml, &p(), "zh") {
            Err(ExpanderError::TooManyAliases { n, .. }) => assert_eq!(n, 9),
            other => panic!("expected TooManyAliases, got {other:?}"),
        }
    }

    #[test]
    fn rejects_cross_language_alias_in_zh_dict() {
        let yaml = r"
version: 1
language: zh
groups:
  - head: 合同
    aliases: [协议, contract]
";
        match parse_dict_str(yaml, &p(), "zh") {
            Err(ExpanderError::CrossLanguageAlias { alias, .. }) => assert_eq!(alias, "contract"),
            other => panic!("expected CrossLanguageAlias, got {other:?}"),
        }
    }

    #[test]
    fn rejects_cjk_in_en_dict() {
        let yaml = r"
version: 1
language: en
groups:
  - head: slides
    aliases: [slideshow, 幻灯片]
";
        match parse_dict_str(yaml, &p(), "en") {
            Err(ExpanderError::CrossLanguageAlias { alias, .. }) => assert_eq!(alias, "幻灯片"),
            other => panic!("expected CrossLanguageAlias, got {other:?}"),
        }
    }

    #[test]
    fn rejects_duplicate_within_group() {
        let yaml = r"
version: 1
language: zh
groups:
  - head: 截图
    aliases: [截图, 截屏]
";
        match parse_dict_str(yaml, &p(), "zh") {
            Err(ExpanderError::DuplicateWithinGroup { dup, .. }) => assert_eq!(dup, "截图"),
            other => panic!("expected DuplicateWithinGroup, got {other:?}"),
        }
    }

    #[test]
    fn rejects_duplicate_across_groups() {
        let yaml = r"
version: 1
language: zh
groups:
  - head: 截图
    aliases: [截屏]
  - head: 屏幕快照
    aliases: [截屏]
";
        match parse_dict_str(yaml, &p(), "zh") {
            Err(ExpanderError::DuplicateAcrossGroups { word, .. }) => assert_eq!(word, "截屏"),
            other => panic!("expected DuplicateAcrossGroups, got {other:?}"),
        }
    }

    #[test]
    fn rejects_wrong_version() {
        let yaml = r"
version: 2
language: zh
groups: []
";
        match parse_dict_str(yaml, &p(), "zh") {
            Err(ExpanderError::UnsupportedVersion { got, .. }) => assert_eq!(got, 2),
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod expand_tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use scout_search_backend::{FileSearch, SchemaVersion, SearchIntent};
    use std::path::PathBuf;

    fn zh_yaml() -> &'static str {
        r"
version: 1
language: zh
groups:
  - head: 工作汇报
    aliases: [述职, 年度总结]
    domain: office
  - head: 合同
    aliases: [协议]
    domain: personal
  - head: 截图
    aliases: [截屏, 屏幕截图]
    domain: media
"
    }

    fn en_yaml() -> &'static str {
        r"
version: 1
language: en
groups:
  - head: slides
    aliases: [slideshow, presentation]
  - head: cover letter
    aliases: [application]
  - head: style guide
    aliases: [branding, guidelines]
  - head: document
    aliases: [doc, file]
    domain: file_type
"
    }

    fn expander() -> YamlSynonymExpander {
        YamlSynonymExpander::from_str(
            zh_yaml(),
            &PathBuf::from("zh.yaml"),
            en_yaml(),
            &PathBuf::from("en.yaml"),
        )
        .unwrap()
    }

    /// 构造含指定 keywords 的 `FileSearch` intent（最小有效字段）。
    fn intent_with(kws: Vec<&str>) -> SearchIntent {
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
    fn is_pure_content_term_separates_content_from_type_media() {
        // 内容名词短语（parser 不分类为类型/媒体）→ true
        assert!(is_pure_content_term("工作汇报"));
        assert!(is_pure_content_term("述职"));
        assert!(is_pure_content_term("报告"));
        assert!(is_pure_content_term("合同"));
        // 类型/媒体词（parser 消费为 file_type/media_type/ext）→ false
        assert!(!is_pure_content_term("幻灯片")); // file_type=presentation
        assert!(!is_pure_content_term("视频")); // file_type=video
        assert!(!is_pure_content_term("文档")); // file_type=document
        assert!(!is_pure_content_term("截图")); // media_search/screenshot
    }

    #[test]
    fn gazetteer_lookup_multi_picks_nonoverlapping_content_terms() {
        let e = expander();
        // 单内容词 → 单元素 vec
        assert_eq!(
            gazetteer_lookup_multi(&e, "找一份工作汇报相关的ppt"),
            vec!["工作汇报".to_string()]
        );
        // 媒体词被 is_pure_content_term 守护跳过 → 空
        assert!(gazetteer_lookup_multi(&e, "找一张截图").is_empty());
        // 无词典词 → 空
        assert!(gazetteer_lookup_multi(&e, "随便找点东西").is_empty());
    }

    #[test]
    fn gazetteer_skips_file_type_domain_words() {
        // 跟进项①：`document`（head）与 `file`（alias）属 file_type domain，但 parser 重解析
        // 二者均不带 file_type/extensions 信号 → is_pure_content_term 误判为 true（假阳），仅靠
        // parser 会把它们当内容词注入、污染召回（如 `file` 子串还会命中 filename/profile）。
        // domain 标注权威 → is_type_or_media_key 补上这一漏洞，类型词不应被当内容词。
        let e = expander();
        assert!(
            gazetteer_lookup_multi(&e, "the document I wrote").is_empty(),
            "document 是 file_type domain 词，不应被当内容词"
        );
        assert!(
            gazetteer_lookup_multi(&e, "find the file").is_empty(),
            "file 是 file_type domain 词，不应被当内容词"
        );
        // 对照：`doc` 因 parser 已识别为 file_type+扩展名，本就不漏（domain 守护与 parser 守护并存）。
        assert!(gazetteer_lookup_multi(&e, "report.docx").is_empty());
    }

    #[test]
    fn expand_merges_multiple_gazetteer_content_words_into_single_or_group() {
        // 跟进项②：query 含两个词典内容词（工作汇报 + 合同）→ gazetteer 各自命中 →
        // merge_or_group 合并为**单个 OR 组**（召回核心：跨内容词取并集 OR，而非分裂成组间 AND）。
        // 端到端走 expand，补此前只有「单内容词命中」与「合并掉修饰语」的覆盖缺口。
        let e = expander();
        let expanded = e.expand(intent_with(vec![]), "工作汇报和合同的资料");
        assert_eq!(
            expanded.keyword_groups.len(),
            1,
            "两个内容词应并入单个 OR 组"
        );
        let g = &expanded.keyword_groups[0];
        assert_eq!(g.head, "工作汇报", "首现内容词作合并 head");
        let all = g.all();
        // 第二个内容词及双方同义词全部并入同一 OR 组
        assert!(all.contains(&"合同"), "第二内容词应进同组");
        assert!(all.contains(&"协议"), "合同 的同义词应进同组");
        assert!(all.contains(&"述职"), "工作汇报 的同义词应进同组");
    }

    #[test]
    fn classify_pure_chinese_is_zh() {
        assert_eq!(classify("工作汇报"), KeywordLang::Zh);
    }

    #[test]
    fn classify_pure_english_is_en() {
        assert_eq!(classify("slides"), KeywordLang::En);
    }

    #[test]
    fn classify_hyphenated_identifier_is_skip() {
        assert_eq!(classify("synthetic-place"), KeywordLang::Skip);
        assert_eq!(classify("synthetic-place-笔记"), KeywordLang::Skip);
    }

    #[test]
    fn classify_digits_only_is_skip() {
        assert_eq!(classify("2024"), KeywordLang::Skip);
    }

    #[test]
    fn zh_keyword_expands_with_head_first() {
        let e = expander();
        let out = e.expand(intent_with(vec!["工作汇报"]), "");
        assert_eq!(out.keyword_groups.len(), 1);
        let g = &out.keyword_groups[0];
        assert_eq!(g.head, "工作汇报");
        assert_eq!(g.synonyms, vec!["述职", "年度总结"]);
    }

    #[test]
    fn zh_alias_as_input_makes_alias_the_head() {
        let e = expander();
        let out = e.expand(intent_with(vec!["述职"]), "");
        let g = &out.keyword_groups[0];
        assert_eq!(g.head, "述职");
        // synonyms 按词典原顺序（含 head，不含命中词本身）
        assert_eq!(g.synonyms, vec!["工作汇报", "年度总结"]);
    }

    #[test]
    fn en_keyword_expands_via_en_index() {
        let e = expander();
        let out = e.expand(intent_with(vec!["slides"]), "");
        let g = &out.keyword_groups[0];
        assert_eq!(g.head, "slides");
        assert_eq!(g.synonyms, vec!["slideshow", "presentation"]);
    }

    #[test]
    fn miss_in_dict_returns_singleton() {
        let e = expander();
        let out = e.expand(intent_with(vec!["完全不存在的词"]), "");
        assert!(out.keyword_groups[0].is_singleton());
    }

    #[test]
    fn mixed_identifier_is_singleton() {
        let e = expander();
        let out = e.expand(intent_with(vec!["synthetic-place"]), "");
        assert!(out.keyword_groups[0].is_singleton());
    }

    #[test]
    fn multi_keyword_intent_preserves_order() {
        // 跟进项③：本测试守的是 **parser-keyword 路径**（非 gazetteer 合并路径）。query 传空串
        // → gazetteer_lookup_multi 零命中 → 走 `match intent.search_keywords()` 分支，parser 的
        // 每个 keyword 各自 expand_one 成独立组、保持入参顺序（与
        // `expand_merges_multiple_gazetteer_content_words_into_single_or_group` 的「命中词典→并入
        // 单 OR 组」形成对照）。命中词典内容词会被 gazetteer 合并，故此处用空 query 显式绕开。
        let e = expander();
        let out = e.expand(
            intent_with(vec!["工作汇报", "synthetic-place", "slides"]),
            "",
        );
        assert_eq!(out.keyword_groups.len(), 3);
        assert_eq!(out.keyword_groups[0].head, "工作汇报");
        assert_eq!(out.keyword_groups[0].synonyms.len(), 2);
        assert!(out.keyword_groups[1].is_singleton());
        assert_eq!(out.keyword_groups[2].head, "slides");
        assert_eq!(out.keyword_groups[2].synonyms.len(), 2);
    }

    #[test]
    fn expand_gazetteer_injects_when_no_keyword() {
        // 直接构造「parser 未抽出 keyword」的 intent（空 keywords），验证 gazetteer 兼底分支
        // 扫词典注入内容词。BETA-13（parser G1 关键词抽取）后，parser 对多数自然中文 query
        // 已能抽出 keyword（如「找一份工作汇报相关的ppt」→ keywords=["工作汇报"]，走 expand 的
        // parser-has-keyword 分支），原「parser 无 keyword」前提对自然 query 不再可达；故此
        // 单元测试改为直接驱动 expand 的兼底路径，不依赖 parser 抽取行为。
        let e = expander();
        let intent = intent_with(vec![]);
        let expanded = e.expand(intent, "工作汇报相关的内容");
        assert_eq!(expanded.keyword_groups.len(), 1);
        assert_eq!(expanded.keyword_groups[0].head, "工作汇报");
        assert!(expanded.keyword_groups[0]
            .synonyms
            .contains(&"述职".to_string()));
    }

    #[test]
    fn expand_gazetteer_overrides_even_when_parser_has_keyword() {
        // BETA-13 召回修复：parser 有 keyword 时，gazetteer 仍扫 query 核心内容词并替代，
        // 让结果带同义词扩展（修复前此路径不走 gazetteer、无同义词 → 召回崩）。
        let e = expander();
        let intent = scout_intent_parser::parse("找文件名包含工作汇报的ppt");
        assert_eq!(intent.search_keywords().map(<[String]>::len), Some(1));
        let expanded = e.expand(intent, "找文件名包含工作汇报的ppt");
        assert_eq!(expanded.keyword_groups.len(), 1);
        assert_eq!(expanded.keyword_groups[0].head, "工作汇报");
        // 关键：现在走 gazetteer → 带同义词「述职」（修复前不带）
        assert!(expanded.keyword_groups[0]
            .synonyms
            .contains(&"述职".to_string()));
    }

    #[test]
    fn expand_gazetteer_drops_modifier_keyword_into_single_or_group() {
        // 崩塌模式 1（合同+乙方）：parser 多抽修饰语 → 组间 AND 碾压。gazetteer 命中核心词
        // 「工作汇报」→ 替代成单 OR 组、甩掉修饰语「张三」（不再两组 AND）。
        let e = expander();
        let intent = intent_with(vec!["工作汇报", "张三"]);
        let expanded = e.expand(intent, "工作汇报相关的，张三那份");
        assert_eq!(expanded.keyword_groups.len(), 1, "应合并为单 OR 组");
        assert_eq!(expanded.keyword_groups[0].head, "工作汇报");
        assert!(expanded.keyword_groups[0]
            .synonyms
            .contains(&"述职".to_string()));
        assert!(
            !expanded.keyword_groups[0].all().contains(&"张三"),
            "修饰语张三不应进组"
        );
    }

    #[test]
    fn expand_gazetteer_rescues_dirty_keyword() {
        // 崩塌模式 2（简历在哪）：parser 分词不净。gazetteer 从 query 原文扫到干净
        // 「工作汇报」→ 替代脏 keyword「工作汇报在哪」。
        let e = expander();
        let intent = intent_with(vec!["工作汇报在哪"]);
        let expanded = e.expand(intent, "我的工作汇报在哪");
        assert_eq!(expanded.keyword_groups.len(), 1);
        assert_eq!(expanded.keyword_groups[0].head, "工作汇报");
    }

    #[test]
    fn expand_keeps_parser_keyword_when_gazetteer_misses() {
        // gazetteer 零命中（query 无词典内容词）→ 保留 parser keyword（不恶化）。
        let e = expander();
        let intent = intent_with(vec!["项目计划"]);
        let expanded = e.expand(intent, "项目计划相关");
        assert_eq!(expanded.keyword_groups.len(), 1);
        assert_eq!(expanded.keyword_groups[0].head, "项目计划");
    }

    #[test]
    fn expand_fallback_no_dict_hit_yields_identity() {
        // parser 无 keyword 且 query 不含任何词典内容词 → gazetteer 无命中 → 空 group（identity）。
        // （类型/媒体词被守护跳过的逻辑已由 gazetteer_lookup_multi 单测覆盖。）
        let e = expander();
        let intent = scout_intent_parser::parse("找一份ppt");
        assert!(intent.search_keywords().map_or(true, <[String]>::is_empty));
        let expanded = e.expand(intent, "找一份ppt");
        assert!(expanded.keyword_groups.is_empty());
    }

    #[test]
    fn expand_bare_content_word_injects_singleton_keyword() {
        // 裸词「英语」：非词典词、parser 无 keyword → 兜底注入为内容关键词（而非 match-all）。
        let e = expander();
        let intent = scout_intent_parser::parse("英语");
        let expanded = e.expand(intent, "英语");
        assert_eq!(expanded.keyword_groups.len(), 1);
        assert_eq!(expanded.keyword_groups[0].head, "英语");
    }

    #[test]
    fn expand_bare_content_word_strips_leading_verb() {
        let e = expander();
        let intent = scout_intent_parser::parse("找英语");
        let expanded = e.expand(intent, "找英语");
        assert_eq!(expanded.keyword_groups.len(), 1);
        assert_eq!(expanded.keyword_groups[0].head, "英语");
    }

    #[test]
    fn bare_content_keyword_skips_type_and_empty() {
        // 含 file_type 的查询不走裸词兜底（保持类型查询语义，交回原路径）。
        assert_eq!(bare_content_keyword("找一份ppt"), None);
        assert_eq!(bare_content_keyword(""), None);
        assert_eq!(bare_content_keyword("   "), None);
    }

    #[test]
    fn strip_leading_search_verbs_removes_zh_and_en_verbs() {
        assert_eq!(strip_leading_search_verbs("找英语"), "英语");
        assert_eq!(strip_leading_search_verbs("搜索报告"), "报告");
        assert_eq!(strip_leading_search_verbs("search report"), "report");
        assert_eq!(strip_leading_search_verbs("英语"), "英语");
        // 英文动词需后接空白：findings 不被误剥成 ings。
        assert_eq!(strip_leading_search_verbs("findings"), "findings");
    }

    #[test]
    fn cap_keyword_groups_truncates_synonyms_tail_by_total_budget() {
        let groups = vec![
            KeywordGroup {
                head: "h1".into(),
                synonyms: (0..20).map(|i| format!("s1-{i}")).collect(),
            },
            KeywordGroup {
                head: "h2".into(),
                synonyms: (0..20).map(|i| format!("s2-{i}")).collect(),
            },
        ];
        // 总词数 = 2 + 20 + 20 = 42,cap 32 → 截掉尾部 10 词
        let (capped, warn) = cap_keyword_groups(groups, 32);
        let total: usize = capped.iter().map(|g| 1 + g.synonyms.len()).sum();
        assert_eq!(total, 32);
        assert!(warn);
    }

    #[test]
    fn cap_keyword_groups_under_budget_is_passthrough() {
        let groups = vec![KeywordGroup {
            head: "h1".into(),
            synonyms: vec!["s1".into(), "s2".into()],
        }];
        let (capped, warn) = cap_keyword_groups(groups, 32);
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].synonyms.len(), 2);
        assert!(!warn);
    }

    #[test]
    fn multiword_key_overrides_single_token_keyword() {
        // parser 抽单 token "cover"，query 含多词键 "cover letter" → 用多词键组覆盖。
        let e = expander();
        let out = e.expand(
            intent_with(vec!["cover"]),
            "find my cover letter for the Google position",
        );
        assert_eq!(out.keyword_groups.len(), 1);
        assert_eq!(out.keyword_groups[0].head, "cover letter");
        assert!(out.keyword_groups[0]
            .synonyms
            .contains(&"application".to_string()));
    }

    #[test]
    fn multiword_key_overrides_style_guide() {
        let e = expander();
        let out = e.expand(
            intent_with(vec!["style"]),
            "find the style guide for our brand assets",
        );
        assert_eq!(out.keyword_groups[0].head, "style guide");
        assert!(out.keyword_groups[0]
            .synonyms
            .contains(&"branding".to_string()));
    }

    #[test]
    fn multiword_override_noop_when_key_absent_in_query() {
        // query 不含多词键 → 单 token keyword 组不变。
        let e = expander();
        let out = e.expand(intent_with(vec!["slides"]), "find slides about budgets");
        assert_eq!(out.keyword_groups[0].head, "slides");
        assert_eq!(
            out.keyword_groups[0].synonyms,
            vec!["slideshow", "presentation"]
        );
    }

    #[test]
    fn multiword_override_dedups_when_two_tokens_map_to_same_key() {
        // 两个 keyword 都被同一多词键包含 → 覆盖后去重为单组。
        let e = expander();
        let out = e.expand(intent_with(vec!["cover", "letter"]), "find my cover letter");
        assert_eq!(out.keyword_groups.len(), 1);
        assert_eq!(out.keyword_groups[0].head, "cover letter");
    }
}
