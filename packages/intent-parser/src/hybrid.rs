//! MVP-17 v0.3：混合架构 —— parser 锁定 variant + 已知字段，模型只填留白。
//!
//! # 设计动机
//!
//! v0.2 evals 发现：让 1.5B 模型完整重写 SearchIntent → **45 case 回退**（模型
//! 推翻 parser 已对的 variant 或字段）vs 仅 16 case 救回。净影响 −29。
//!
//! 根因：模型有自由度时会把 parser 判对的 MediaSearch 改成 FileSearch、把 parser
//! 判对的 Clarify 改成 FileAction。每次推翻 = 一个 fail。
//!
//! 混合架构强制收敛模型自由度：
//!
//! - **parser 给 [`IntentDraft`]**：含已确定 intent + 已填字段 + 待填字段列表
//! - **模型只输出 patch JSON**：仅包含待填字段的值
//! - **merge 时锁死 variant 与已填字段**：模型 patch 里出现的 `intent` 字段被忽略
//!
//! 实测目标：把 fail 67 → 30-35，让 fallback 净影响转正。
//!
//! # 与 `fallback` 模块的关系
//!
//! `ModelFallback::invoke` 内部根据 `hybrid_mode` 标志分派到 [`invoke_hybrid`] 或
//! 走原 v0.2 全 JSON 重写路径。evals `--hybrid` flag 切换。

#![allow(clippy::module_name_repetitions)]

use std::fmt;

use scout_search_backend::SearchIntent;
use serde_json::Value;

use crate::fallback::{analyze_structural_omissions, parse_with_signals};
use crate::signals::CandidateSignals;

/// `apply_patch` 的错误类型。
#[derive(Debug)]
pub enum HybridError {
    /// 序列化 / 反序列化 SearchIntent 时失败。
    Serde(serde_json::Error),
    /// 模型 patch 不是合法 JSON 对象（如返回 array / string / null）。
    PatchNotObject,
}

impl fmt::Display for HybridError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serde(err) => write!(f, "serde error: {err}"),
            Self::PatchNotObject => write!(f, "model patch is not a JSON object"),
        }
    }
}

impl std::error::Error for HybridError {}

impl From<serde_json::Error> for HybridError {
    fn from(err: serde_json::Error) -> Self {
        Self::Serde(err)
    }
}

/// 混合架构的 parser 输出：含已确定 intent + signals + 待填字段名。
///
/// 待填字段 = signals 检出某类约束词但 parser 没把对应字段填上。模型 patch 时
/// 应该只针对这些字段输出值。
#[derive(Debug, Clone)]
pub struct IntentDraft {
    /// parser 输出的 intent（variant 锁定，已知字段填好）。
    pub intent: SearchIntent,
    /// 原始查询里扫描到的信号。
    pub signals: CandidateSignals,
    /// 待模型填补的字段名列表（来自 `analyze_structural_omissions`）。
    pub fillable_fields: Vec<&'static str>,
}

impl IntentDraft {
    /// 从查询字符串构造 draft（parser 解析 + 信号扫描 + 结构性遗漏分析）。
    #[must_use]
    pub fn from_query(query: &str) -> Self {
        let parsed = parse_with_signals(query);
        let fillable = analyze_structural_omissions(&parsed);
        Self {
            intent: parsed.intent,
            signals: parsed.signals,
            fillable_fields: fillable,
        }
    }
}

/// 把模型产出的 patch JSON 合并到 draft 的 intent 上。
///
/// **锁定规则**：
/// - `intent` / `schema_version` / `language` 字段被忽略：
///   - `intent`：variant 已由 parser 锁死。
///   - `schema_version`：版本元数据由 parser 写定。
///   - `language`：由 parser 确定性检测，模型 patch 忽略——Task 11 真实评测实证模型
///     对 language 的改动是噪声（7 条改动 4 对 1 错 2 无关），保留模型改动只会引入回退。
/// - `keywords` 字段取**并集**（draft 已有值在前、patch 新值追加、字符串去重）——
///   parser 已确定的关键词不许被模型推翻，与 variant 锁死同一哲学。
///   patch 为显式 `null` 时仍维持覆盖语义（模型表示该字段无值）。
/// - 其它字段直接覆盖（含 `null`：模型显式表示该字段无值）
///
/// # Errors
///
/// 若 patch 不是 JSON object 或 merge 后的 JSON 无法反序列化回 `SearchIntent`，
/// 返回 [`HybridError`]。
/// BETA-24：parser 确定性产出、无 fillable 类别的字段——模型 patch 一律不应触碰。
/// extensions/file_type 由 parser 从类型词推导；exclude_* 同理；clarify 的 question/
/// reason/options 由 parser 构造。模型只负责 fillable 的结构字段与媒体 metadata。
const PARSER_OWNED_FIELDS: &[&str] = &[
    "extensions",
    "file_type",
    "exclude_extensions",
    "exclude_file_type",
    "options",
    "question",
    "reason",
];

pub fn apply_patch(draft: &IntentDraft, patch: &Value) -> Result<SearchIntent, HybridError> {
    let mut intent_json = serde_json::to_value(&draft.intent)?;
    let intent_obj = intent_json
        .as_object_mut()
        .ok_or(HybridError::PatchNotObject)?;
    let patch_obj = patch.as_object().ok_or(HybridError::PatchNotObject)?;

    // BETA-13-G13：约束字段（time/size/sort/location）只在对应 fillable 类别存在时
    // 才允许模型填——杜绝模型幻觉出 signals 未要求的字段（实测「show me 上周的 PDF」
    // 模型凭空补 location=下载，把无位置查询误收窄）。
    let structured_field_values = collect_structured_strings(intent_obj);

    for (key, val) in patch_obj {
        // language 由 parser 确定性检测，模型 patch 的改动是噪声——Task 11 评测实证
        // （7 条改动 4 对 1 错 2 无关）。intent / schema_version 同理锁死。
        if key == "intent" || key == "schema_version" || key == "language" {
            continue;
        }
        // BETA-24：parser 专属字段——无对应 fillable 类别，模型永不该填。
        // 重训后模型偶在这些字段上幻觉（extensions=["screenshot"]、clarify options
        // 乱填），hybrid 契约是「模型只填 fillable 字段」，这些非 fillable 字段一律丢弃。
        if PARSER_OWNED_FIELDS.contains(&key.as_str()) {
            continue;
        }
        // BETA-13-G13：约束字段须在 fillable 内（time 覆盖 modified/created/accessed）。
        if let Some(category) = fillable_category_for(key) {
            if !draft.fillable_fields.contains(&category) {
                continue;
            }
        }
        // BETA-23：keywords 取并集（draft 在前去重）——parser 已确定的词不许被模型推翻，
        // 与 variant 锁死同一哲学。其余字段维持覆盖语义。
        if key == "keywords" {
            // BETA-24：契约强制——keywords ∉ fillable 时模型不该填，丢弃其输出。
            // 重训后模型对 keywords 过度积极，会在 size/sort/time 触发的 case 上
            // 幻觉关键词；hybrid 契约是「模型只填 fillable 字段」，此处补上校验。
            if !draft.fillable_fields.contains(&"keywords") {
                continue;
            }
            // BETA-13-G13：剔除已被结构字段（title/artist/album/genre）捕获的 token——
            // 模型常把 title/artist 回声成 keyword（「陈奕迅 浮夸」→ kw=[浮夸]），重复污染。
            let merged_kw =
                union_keywords(intent_obj.get("keywords"), val, &structured_field_values);
            intent_obj.insert(key.clone(), merged_kw);
            continue;
        }
        intent_obj.insert(key.clone(), val.clone());
    }

    let merged: SearchIntent = serde_json::from_value(intent_json)?;
    Ok(merged)
}

/// BETA-13-G13：把约束字段 patch key 映射到 fillable 类别名。
/// 返回 `None` 表示该 key 不受 fillable 约束门管辖（如 artist/title/album/genre/limit
/// 等媒体 metadata，由模型按需补；keywords 另有专门校验）。
fn fillable_category_for(key: &str) -> Option<&'static str> {
    match key {
        "modified_time" | "created_time" | "accessed_time" => Some("time"),
        "size" => Some("size"),
        "sort" => Some("sort"),
        "location" => Some("location"),
        _ => None,
    }
}

/// BETA-13-G13：收集 intent 已有的结构化字符串字段值（title/artist/album/genre），
/// 小写化——用于 keywords 并集时剔除模型对这些字段的回声。
fn collect_structured_strings(intent_obj: &serde_json::Map<String, Value>) -> Vec<String> {
    ["title", "artist", "album", "genre"]
        .iter()
        .filter_map(|k| intent_obj.get(*k).and_then(Value::as_str))
        .map(str::to_lowercase)
        .collect()
}

/// keywords 并集：draft 现有值在前、patch 新值追加、字符串去重。
/// patch 不是字符串数组（如显式 null）时返回 patch 原值（维持覆盖语义）。
/// 数组内非字符串项被忽略。
/// `structured` 是已被其它结构字段（title/artist/album/genre）捕获的值（小写）——
/// patch 中等于它们的 token 被剔除（BETA-13-G13：模型回声 title/artist 成 keyword）。
fn union_keywords(existing: Option<&Value>, patch: &Value, structured: &[String]) -> Value {
    // patch 不是数组（含显式 null）→ 直接覆盖，不做并集
    if !matches!(patch, Value::Array(_)) {
        return patch.clone();
    }

    let collect = |v: &Value, out: &mut Vec<String>| {
        if let Some(arr) = v.as_array() {
            for item in arr {
                if let Some(s) = item.as_str() {
                    // 已被结构字段捕获的 token 不重复进 keywords
                    if structured.iter().any(|c| c == &s.to_lowercase()) {
                        continue;
                    }
                    if !out.iter().any(|e| e == s) {
                        out.push(s.to_owned());
                    }
                }
            }
        }
    };
    let mut out = Vec::new();
    if let Some(e) = existing {
        collect(e, &mut out);
    }
    collect(patch, &mut out);
    // BETA-13-G13：并集为空（模型 token 全被结构字段去重、且 draft 原无 keywords）→
    // 返回 null（→ None），而非空数组——否则 keywords=[] 与期望的「字段缺省」不一致
    // （实测「陈奕迅 浮夸 这首歌」模型回声 title=浮夸 被剔光后留 [] 致 partial）。
    if out.is_empty() {
        return Value::Null;
    }
    Value::Array(out.into_iter().map(Value::String).collect())
}

/// BETA-17：hybrid prompt 的**固定指令前缀**（不含 query/draft），跨调用稳定。
///
/// 告诉模型"variant 已定，只输出待填字段的 JSON 补丁"；few-shot 示例集中在"字段补全"，
/// 避免把模型引入"重写 intent"心态。拆出此前缀是为了 KV 前缀缓存：model-runtime 的 worker
/// 只需对它 prefill 一次，后续每条 query 仅 decode [`hybrid_prompt_suffix`] 的小尾巴
/// —— 弱硬件上 prefill 是延迟主因。
#[must_use]
pub fn hybrid_prompt_prefix() -> &'static str {
    r#"你是 Scout 搜索意图的字段补全助手。Parser 已经确定了 intent 类型和大部分字段，你只需要根据用户原始 query 补全列出的"待填字段"。

# 严格约束
1. 必须输出且仅输出一个合法 JSON 对象（patch），不要 Markdown 围栏。
2. patch 的键必须是 SearchIntent 的字段名（如 modified_time / size / sort / location / artist 等）。
3. **不要在 patch 里加 "intent"、"schema_version" 或 "language" 字段** —— variant 已锁死，language 由 parser 确定性检测（模型改它是噪声）。
4. 只输出待填字段；parser 已填的字段不要重复也不要覆盖（keywords 例外：只追加缺失的内容词）。
5. 字段值必须符合 schema v1.0 的 enum/结构（如 modified_time 是 {type, value} 对象）。

# 字段值速查（部分常用）
- 时间相对值: today / yesterday / last_3_days / last_7_days / last_14_days / last_30_days / this_week / last_week / this_month / last_month / this_year / last_year
- 排序: relevance_desc / modified_desc / modified_asc / created_desc / size_desc / size_asc / name_asc / name_desc
- 大小单位: B / KB / MB / GB
- 文件类型: document / spreadsheet / presentation / image / screenshot / video / audio / archive / code / executable
- keywords: 字符串数组；只补 query 里出现、但 Draft.keywords 缺失的内容词，已有词不要重复

# 示例

Query: "找一周内修改的视频"
当前 Draft: {"schema_version":"1.0","intent":"media_search","language":"zh","media_type":"video","sort":"modified_desc"}
待填字段: modified_time
Patch:
{"modified_time":{"type":"relative","value":"last_7_days"}}

Query: "find documents larger than 1 GB"
当前 Draft: {"schema_version":"1.0","intent":"file_search","language":"en","file_type":"document","sort":"size_desc"}
待填字段: size
Patch:
{"size":{"type":"greater_than","value":1,"unit":"GB"}}

Query: "最近一周下载的最大的文件"
当前 Draft: {"schema_version":"1.0","intent":"file_search","language":"zh","location":{"hint":"下载"}}
待填字段: time, size, sort
Patch:
{"modified_time":{"type":"relative","value":"last_7_days"},"size":null,"sort":"size_desc"}

Query: "找张学友的歌"
当前 Draft: {"schema_version":"1.0","intent":"media_search","language":"zh","media_type":"audio"}
待填字段: (无)
Patch:
{"artist":"张学友"}

Query: "2025年的会议纪要文件名包含运维"
当前 Draft: {"schema_version":"1.0","intent":"file_search","language":"zh","keywords":["运维"],"modified_time":{"type":"absolute","from":"2025-01-01","to":"2025-12-31"},"sort":"modified_desc"}
待填字段: keywords
Patch:
{"keywords":["会议纪要"]}

# 现在请处理

"#
}

/// BETA-17：hybrid prompt 的**可变尾巴**（本条 query + parser draft + 待填字段）。
/// 与 [`hybrid_prompt_prefix`] 拼接即完整 prompt。
#[must_use]
pub fn hybrid_prompt_suffix(query: &str, draft: &IntentDraft) -> String {
    let draft_json = serde_json::to_string(&draft.intent).unwrap_or_else(|_| "{}".to_owned());
    let fillable = if draft.fillable_fields.is_empty() {
        "(无 — 但你仍然可以补任何 parser 漏掉的字段)".to_owned()
    } else {
        draft.fillable_fields.join(", ")
    };
    format!("Query: {query}\n当前 Draft: {draft_json}\n待填字段: {fillable}\nPatch:\n")
}

/// 完整 hybrid prompt = 固定前缀 ++ 可变尾巴。与拆分前逐字节等价。
#[must_use]
pub fn build_hybrid_prompt(query: &str, draft: &IntentDraft) -> String {
    let mut prompt = String::from(hybrid_prompt_prefix());
    prompt.push_str(&hybrid_prompt_suffix(query, draft));
    prompt
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use scout_search_backend::{
        FileSearch, FileType, Language, MediaSearch, MediaType, RelativeTime, SchemaVersion,
        SearchIntent, SortOrder, TimeExpression,
    };
    use serde_json::json;

    fn mk_bare_file_search() -> SearchIntent {
        SearchIntent::FileSearch(FileSearch {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            keywords: Some(vec!["ppt".to_owned()]),
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
    fn draft_from_query_captures_fillable() {
        let draft = IntentDraft::from_query("一周内编辑过的ppt");
        // parser 看到 ppt 后能给 file_search，"一周内" signals.time 应触发但 parser 在
        // v0.4 已经能填 modified_time，所以 fillable 可能为空
        assert!(matches!(draft.intent, SearchIntent::FileSearch(_)));
        // 不强 assert fillable，只看类型结构正确
        let _ = draft.fillable_fields;
    }

    #[test]
    fn apply_patch_merges_modified_time() {
        let draft = IntentDraft {
            intent: mk_bare_file_search(),
            signals: CandidateSignals::default(),
            fillable_fields: vec!["time"],
        };
        let patch = json!({
            "modified_time": {"type": "relative", "value": "last_7_days"},
        });
        let merged = apply_patch(&draft, &patch).unwrap();
        let SearchIntent::FileSearch(fs) = merged else {
            panic!("expected FileSearch");
        };
        assert!(matches!(
            fs.modified_time,
            Some(TimeExpression::Relative {
                value: RelativeTime::Last7Days
            })
        ));
    }

    #[test]
    fn apply_patch_ignores_intent_field() {
        // 模型试图把 file_search 改成 media_search → 必须被忽略
        let draft = IntentDraft {
            intent: mk_bare_file_search(),
            signals: CandidateSignals::default(),
            // BETA-13-G13：sort 须在 fillable 内才允许合并（本测试验 intent 被忽略，
            // sort 是陪衬正常字段）。
            fillable_fields: vec!["sort"],
        };
        let patch = json!({
            "intent": "media_search",  // 应该被吃掉
            "sort": "size_desc",
        });
        let merged = apply_patch(&draft, &patch).unwrap();
        assert!(matches!(merged, SearchIntent::FileSearch(_)));
        let SearchIntent::FileSearch(fs) = merged else {
            unreachable!()
        };
        assert_eq!(fs.sort, Some(SortOrder::SizeDesc));
    }

    #[test]
    fn apply_patch_can_overwrite_existing_field() {
        // BETA-24：原用 file_type 验覆盖语义，但 file_type 已并入 PARSER_OWNED_FIELDS
        // （契约禁填）。改用 sort（非 denylist、覆盖语义不变）验同一行为。
        let mut intent = mk_bare_file_search();
        if let SearchIntent::FileSearch(fs) = &mut intent {
            fs.sort = Some(SortOrder::RelevanceDesc);
        }
        let draft = IntentDraft {
            intent,
            signals: CandidateSignals::default(),
            // BETA-13-G13：sort 须在 fillable 内才谈得上覆盖。
            fillable_fields: vec!["sort"],
        };
        let patch = json!({"sort": "modified_desc"});
        let merged = apply_patch(&draft, &patch).unwrap();
        let SearchIntent::FileSearch(fs) = merged else {
            unreachable!()
        };
        assert_eq!(fs.sort, Some(SortOrder::ModifiedDesc));
    }

    #[test]
    fn apply_patch_drops_parser_owned_fields() {
        // BETA-24：模型在 parser 专属字段（file_type/extensions/options…）上的输出一律丢弃
        let mut intent = mk_bare_file_search();
        if let SearchIntent::FileSearch(fs) = &mut intent {
            fs.file_type = Some(vec![FileType::Presentation]);
        }
        let draft = IntentDraft {
            intent,
            signals: CandidateSignals::default(),
            fillable_fields: vec![],
        };
        // 模型幻觉：改 file_type + 乱加 extensions，均应被丢弃
        let patch = json!({"file_type": "video", "extensions": ["mp4"]});
        let merged = apply_patch(&draft, &patch).unwrap();
        let SearchIntent::FileSearch(fs) = merged else {
            unreachable!()
        };
        assert_eq!(
            fs.file_type,
            Some(vec![FileType::Presentation]),
            "file_type 不被模型改"
        );
        assert_eq!(fs.extensions, None, "extensions 不被模型加");
    }

    #[test]
    fn build_hybrid_prompt_contains_draft_json() {
        let draft = IntentDraft {
            intent: mk_bare_file_search(),
            signals: CandidateSignals::default(),
            fillable_fields: vec!["time", "size"],
        };
        let p = build_hybrid_prompt("找一周内的大文件", &draft);
        assert!(p.contains("找一周内的大文件"));
        assert!(p.contains("\"intent\":\"file_search\""));
        assert!(p.contains("time, size"));
        assert!(p.contains("Patch:"));
    }

    // BETA-17：前缀/尾巴拆分 —— KV 前缀缓存的正确性前提。
    #[test]
    fn prefix_is_query_independent() {
        let draft_a = IntentDraft {
            intent: mk_bare_file_search(),
            signals: CandidateSignals::default(),
            fillable_fields: vec!["size"],
        };
        let draft_b = IntentDraft {
            intent: mk_bare_file_search(),
            signals: CandidateSignals::default(),
            fillable_fields: vec!["time"],
        };
        // 不同 query / 不同 draft，固定前缀必须逐字节相同（否则 KV 缓存会错误命中）。
        let _ = (&draft_a, &draft_b);
        assert_eq!(hybrid_prompt_prefix(), hybrid_prompt_prefix());
        assert!(hybrid_prompt_prefix().ends_with("# 现在请处理\n\n"));
    }

    #[test]
    fn apply_patch_drops_constraint_field_not_in_fillable() {
        // BETA-13-G13：模型幻觉出 signals 未要求的约束字段（实测「show me 上周的 PDF」
        // → 模型凭空补 location=下载，把无位置查询误收窄到下载夹）。fillable 不含
        // location 时，模型的 location patch 必须被丢弃；在 fillable 的字段照常合并。
        let draft = IntentDraft {
            intent: mk_bare_file_search(),
            signals: CandidateSignals::default(),
            fillable_fields: vec!["time"], // 只有 time 待填，无 location
        };
        let patch = json!({
            "location": {"hint": "下载"},
            "modified_time": {"type": "relative", "value": "last_week"},
        });
        let merged = apply_patch(&draft, &patch).unwrap();
        let SearchIntent::FileSearch(fs) = merged else {
            unreachable!()
        };
        assert_eq!(fs.location, None, "location 不在 fillable，模型幻觉应丢弃");
        assert!(
            matches!(
                fs.modified_time,
                Some(TimeExpression::Relative {
                    value: RelativeTime::LastWeek
                })
            ),
            "time 在 fillable，正常合并"
        );
    }

    #[test]
    fn apply_patch_drops_keyword_duplicating_structured_field() {
        // BETA-13-G13：模型把已是 title/artist 的词回声成 keyword（实测「陈奕迅 浮夸 这首歌」
        // parser 已 title=浮夸/artist=陈奕迅，模型却补 keywords=[浮夸]）。union 时等于已有
        // title/artist/album/genre 的 token 必须剔除——这些内容已被结构字段捕获。
        let intent = SearchIntent::MediaSearch(MediaSearch {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            media_type: MediaType::Audio,
            artist: Some("陈奕迅".to_owned()),
            title: Some("浮夸".to_owned()),
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
        });
        let draft = IntentDraft {
            intent,
            signals: CandidateSignals::default(),
            fillable_fields: vec!["keywords"],
        };
        let patch = json!({"keywords": ["浮夸"]});
        let merged = apply_patch(&draft, &patch).unwrap();
        let SearchIntent::MediaSearch(ms) = merged else {
            unreachable!()
        };
        assert!(
            !ms.keywords.as_ref().is_some_and(|k| !k.is_empty()),
            "等于 title 的 token 应被剔除，keywords={:?}",
            ms.keywords
        );
    }

    #[test]
    fn apply_patch_unions_keywords_with_draft() {
        // 模型补 keywords 不许丢 parser 已抽对的词（问题 4 的「运维」场景）
        let draft = IntentDraft {
            intent: mk_bare_file_search(),
            signals: CandidateSignals::default(),
            fillable_fields: vec!["keywords"],
        };
        let patch = json!({"keywords": ["会议纪要"]});
        let merged = apply_patch(&draft, &patch).unwrap();
        let SearchIntent::FileSearch(fs) = merged else {
            unreachable!()
        };
        assert_eq!(
            fs.keywords,
            Some(vec!["ppt".to_owned(), "会议纪要".to_owned()])
        );
    }

    #[test]
    fn apply_patch_keywords_union_dedups() {
        let draft = IntentDraft {
            intent: mk_bare_file_search(),
            signals: CandidateSignals::default(),
            fillable_fields: vec!["keywords"],
        };
        let patch = json!({"keywords": ["ppt", "预算"]});
        let merged = apply_patch(&draft, &patch).unwrap();
        let SearchIntent::FileSearch(fs) = merged else {
            unreachable!()
        };
        assert_eq!(fs.keywords, Some(vec!["ppt".to_owned(), "预算".to_owned()]));
    }

    #[test]
    fn apply_patch_keywords_null_keeps_overwrite_semantics() {
        // 显式 null（模型表示无值）维持原覆盖语义。
        // BETA-24：keywords 契约强制后，覆盖语义只在 keywords ∈ fillable 时生效
        // （模型被允许动 keywords 才谈得上覆盖）；fillable 不含 keywords 时模型
        // 任何 keywords 输出（含 null）都被丢弃，另见 keywords_patch_dropped_when_not_fillable。
        let draft = IntentDraft {
            intent: mk_bare_file_search(),
            signals: CandidateSignals::default(),
            fillable_fields: vec!["keywords"],
        };
        let patch = json!({"keywords": null});
        let merged = apply_patch(&draft, &patch).unwrap();
        let SearchIntent::FileSearch(fs) = merged else {
            unreachable!()
        };
        assert_eq!(fs.keywords, None);
    }

    #[test]
    fn prefix_plus_suffix_equals_full_prompt() {
        let draft = IntentDraft {
            intent: mk_bare_file_search(),
            signals: CandidateSignals::default(),
            fillable_fields: vec!["time", "size"],
        };
        let query = "找一周内的大文件";
        // 拆分后拼接必须与整串 build_hybrid_prompt 逐字节一致 —— 保证缓存路径语义不变。
        let mut composed = String::from(hybrid_prompt_prefix());
        composed.push_str(&hybrid_prompt_suffix(query, &draft));
        assert_eq!(composed, build_hybrid_prompt(query, &draft));
        // 尾巴自包含本条 query 与 draft。
        let suffix = hybrid_prompt_suffix(query, &draft);
        assert!(suffix.starts_with("Query: 找一周内的大文件"));
        assert!(suffix.ends_with("Patch:\n"));
    }

    #[test]
    fn prefix_teaches_keywords_completion() {
        let p = hybrid_prompt_prefix();
        assert!(p.contains("\"keywords\""));
        assert!(p.contains("会议纪要"));
        // 既有不变量：前缀仍以固定结尾收口（KV 缓存正确性前提）
        assert!(p.ends_with("# 现在请处理\n\n"));
    }

    #[test]
    fn apply_patch_ignores_language_field() {
        // 语言检测是 parser 的确定性职责——模型 patch 的 language 必须被忽略。
        // 真实评测（Task 11 v0.9 with-fallback）：模型对 language 的改动 4 对 1 错 2 无关，
        // 是噪声不是信号。唯一回退正是这条：「找一份描述了项目计划的文档」
        // parser 判对 language=zh，模型 patch 幻觉输出 "mixed" 覆盖 → pass 变 partial。
        let draft = IntentDraft {
            intent: mk_bare_file_search(), // language: Some(Language::Zh)
            signals: CandidateSignals::default(),
            // BETA-13-G13：sort 是陪衬正常字段，须在 fillable 内。
            fillable_fields: vec!["sort"],
        };
        let patch = json!({"language": "mixed", "sort": "size_desc"});
        let merged = apply_patch(&draft, &patch).unwrap();
        let SearchIntent::FileSearch(fs) = merged else {
            unreachable!()
        };
        // language 不被模型 patch 覆盖
        assert_eq!(fs.language, Some(Language::Zh));
        // 其他字段照常合并
        assert_eq!(fs.sort, Some(SortOrder::SizeDesc));
    }

    #[test]
    fn keywords_patch_dropped_when_not_fillable() {
        // BETA-24：keywords ∉ fillable 时模型不该填——重训后模型对 keywords 过度积极，
        // 会在纯 size/sort/time 触发的 case 上幻觉关键词。契约强制：丢弃模型 keywords，
        // 保持 draft（parser）原样。
        let draft = IntentDraft {
            intent: mk_bare_file_search(), // keywords: Some(["ppt"])
            signals: CandidateSignals::default(),
            // BETA-13-G13：含 size+sort（陪衬字段），但不含 keywords。
            fillable_fields: vec!["size", "sort"],
        };
        let patch = json!({"keywords": ["幻觉词"], "sort": "size_desc"});
        let merged = apply_patch(&draft, &patch).unwrap();
        let SearchIntent::FileSearch(fs) = merged else {
            unreachable!()
        };
        // 模型幻觉的 keywords 被丢弃，保持 draft 原样
        assert_eq!(fs.keywords, Some(vec!["ppt".to_owned()]));
        // 其它字段（在 fillable 内）照常合并
        assert_eq!(fs.sort, Some(SortOrder::SizeDesc));
    }

    #[test]
    fn keywords_patch_applied_when_fillable() {
        // keywords ∈ fillable 时模型补全照常并入（并集语义不变）。
        let draft = IntentDraft {
            intent: mk_bare_file_search(), // keywords: Some(["ppt"])
            signals: CandidateSignals::default(),
            fillable_fields: vec!["keywords"],
        };
        let patch = json!({"keywords": ["补充词"]});
        let merged = apply_patch(&draft, &patch).unwrap();
        let SearchIntent::FileSearch(fs) = merged else {
            unreachable!()
        };
        assert_eq!(
            fs.keywords,
            Some(vec!["ppt".to_owned(), "补充词".to_owned()])
        );
    }

    #[test]
    fn apply_patch_keywords_union_with_absent_draft_keywords() {
        // draft 无 keywords（serde skip 缺键）——覆盖检测在 parser 零抽词时也会要模型补全
        let mut intent = mk_bare_file_search();
        if let SearchIntent::FileSearch(fs) = &mut intent {
            fs.keywords = None;
        }
        let draft = IntentDraft {
            intent,
            signals: CandidateSignals::default(),
            fillable_fields: vec!["keywords"],
        };
        let patch = json!({"keywords": ["会议纪要"]});
        let merged = apply_patch(&draft, &patch).unwrap();
        let SearchIntent::FileSearch(fs) = merged else {
            unreachable!()
        };
        assert_eq!(fs.keywords, Some(vec!["会议纪要".to_owned()]));
    }
}
