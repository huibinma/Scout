//! `scout-intent-parser` — 自然语言 → `SearchIntent` 规则解析器 v0.1。
//!
//! **不接 LLM**；模型 fallback 留到 MVP-17。
//! 设计参考：[docs/search-intent-schema.md](../../../../docs/search-intent-schema.md)。
//!
//! # 用法
//!
//! ```
//! use scout_intent_parser::parse;
//! let intent = parse("查找昨天编辑过的 ppt");
//! ```
//!
//! # 实现策略（v0.1）
//!
//! 1. [`language::detect`] 识别语言
//! 2. 高优先级 clarify 触发检测（按 schema §3.5 触发规则表）
//! 3. file_action / refine 路径占位（v0.1 待完善 — fallback 到 clarify 直至实现）
//! 4. media_search 解析（artist / media_type / 时长 等）
//! 5. 默认 file_search 解析（扩展名 / 文件类型 / 时间 / 位置 / 大小 / 排序 / 关键词）

#![forbid(unsafe_code)]
// v0.1 阶段优先功能正确性；clippy::pedantic 的风格细节留到 v0.2 重构时再清理。
#![allow(
    clippy::pedantic,
    clippy::expect_used, // regex / OnceLock 初始化用 expect 是合理的（regex 是常量）
    clippy::while_let_on_iterator,
    clippy::naive_bytecount,
    unused_imports
)]

pub mod fallback;
pub mod hybrid;
pub mod language;
pub mod lexicon;
pub mod parsers;
pub mod prompt;
pub mod signals;

/// MVP-17 v0.2：SearchIntent schema v1.0 的 GBNF 表示。
///
/// 由 [`crate::fallback::ModelFallback::with_grammar`] 使用，启用 llama.cpp 受
/// 限解码，强制模型输出符合 schema 的 JSON。手写而非自动转换，schema 演进时
/// 需手动同步本文件。
pub const SEARCH_INTENT_GBNF: &str = include_str!("grammar/search-intent.gbnf");

use scout_search_backend::{
    BaseRef, Clarify, ClarifyReason, FileAction, FileActionKind, FileSearch, FileType, Language,
    Location, MediaSearch, MediaType, Refine, RefineDelta, RelativeTime, SchemaVersion,
    SearchIntent, SizeExpression, SizeUnit, SortOrder, TargetRef, TargetSelector, TimeExpression,
};

use parsers::clarify::{clarify_unknown, clarify_with, pick};
use parsers::common::{
    is_cjk, is_word_char, parse_date_with_before, parse_location_with_language,
    parse_time_expression, parse_time_fields, parse_year, word_present,
};
use parsers::file_action::try_parse_file_action;
use parsers::file_search::{
    has_any_extension_signal, has_any_location_signal, has_keyword_like_signal, is_size_shaped,
    match_extensions, parse_duration, parse_file_search, parse_location,
};
use parsers::media_search::{
    contains_known_artist, has_any_media_signal, has_strong_media_signal, is_media_query,
    parse_media_search,
};
use parsers::refine::try_parse_refine;

/// 顶层解析入口。
///
/// **永远不会 panic**；失败时返回 `clarify(reason: unknown)`。
#[must_use]
pub fn parse(input: &str) -> SearchIntent {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return clarify_unknown(Language::Unknown, "请输入要搜索的内容。");
    }

    let language = language::detect(trimmed);
    let lower = trimmed.to_lowercase();

    // 1. 高优先级 clarify 触发（高风险 / 模糊）— 按 schema §3.5。
    //    文案与 options 按 language 双语呈现（2026-07-06 i18n）：这 4 类 options 与
    //    reason 的通用 standard_options 语义不同（如 bulk-action 给「确认全部/只选择部分」
    //    而非动作枚举），故就地双语构造、不走 standard_options。
    if has_unsafe_delete_signal(&lower) {
        return SearchIntent::Clarify(Clarify {
            schema_version: SchemaVersion::V1,
            language: Some(language),
            reason: ClarifyReason::UnsafeAction,
            question: pick(
                language,
                "删除操作会移到回收站，且 MVP 暂不支持。是否改为在访达 / 资源管理器中显示，由你手动操作？",
                "Deletion moves items to the trash and isn't supported in the MVP. Show them in Finder / Explorer so you can act manually?",
            )
            .to_owned(),
            options: Some(bilingual_options(
                language,
                &["在访达/资源管理器中显示", "取消"],
                &["Show in Finder / Explorer", "Cancel"],
            )),
        });
    }
    if is_recent_only_query(&lower) {
        return SearchIntent::Clarify(Clarify {
            schema_version: SchemaVersion::V1,
            language: Some(language),
            reason: ClarifyReason::AmbiguousTime,
            question: pick(
                language,
                "你说的「最近」是指最近几天？",
                "How recent do you mean — which time range?",
            )
            .to_owned(),
            options: Some(bilingual_options(
                language,
                &["今天", "过去 3 天", "过去一周", "过去一个月"],
                &["Today", "Past 3 days", "Past week", "Past month"],
            )),
        });
    }
    // 高风险批量操作 target 不明 → ambiguous_action
    if is_ambiguous_bulk_action(&lower) {
        return SearchIntent::Clarify(Clarify {
            schema_version: SchemaVersion::V1,
            language: Some(language),
            reason: ClarifyReason::AmbiguousAction,
            question: pick(
                language,
                "要对上一轮的全部结果执行此操作吗？请先确认目标文件列表。",
                "Apply this to all of the previous results? Please confirm the target file list first.",
            )
            .to_owned(),
            options: Some(bilingual_options(
                language,
                &["确认全部", "只选择部分", "取消"],
                &["Confirm all", "Select some", "Cancel"],
            )),
        });
    }
    // 位置 hint 未识别且无强约束 → ambiguous_location
    if is_unknown_location_only(trimmed, &lower) {
        return SearchIntent::Clarify(Clarify {
            schema_version: SchemaVersion::V1,
            language: Some(language),
            reason: ClarifyReason::AmbiguousLocation,
            question: pick(
                language,
                "没找到对应的目录。要不要在哪个范围内搜索？",
                "I couldn't find that folder. Which scope should I search in?",
            )
            .to_owned(),
            options: Some(bilingual_options(
                language,
                &["全盘搜索", "下载", "文稿", "桌面", "取消"],
                &["Whole disk", "Downloads", "Documents", "Desktop", "Cancel"],
            )),
        });
    }

    // BETA-13-G7（中度阈值）：模糊查询 → clarify（精确区分 reason）。仅在**无任何具体约束**
    // （扩展名 / 媒体词 / artist / 引号关键词）时触发，避免误伤「昨天的 pdf」类真搜索。
    if let Some(intent) = detect_vague_clarify(trimmed, &lower, language) {
        return intent;
    }

    // 2. 文件操作（"打开第 N 个" / "open the N-th" / "在访达..." / "改名" / "复制" / "移动"）
    if let Some(intent) = try_parse_file_action(trimmed, &lower, language) {
        return intent;
    }

    // 3. Refine（"只看 X" / "排除 X" / "show only" / "按 X 排序" / "不限制 X"）
    if let Some(intent) = try_parse_refine(trimmed, &lower, language) {
        return intent;
    }

    // 4. media_search（含强媒体词 / 艺人）
    if is_media_query(&lower) {
        return parse_media_search(trimmed, &lower, language);
    }

    // 5. 默认 file_search
    parse_file_search(trimmed, &lower, language)
}

// ============================================================
// Clarify 触发检测
// ============================================================

/// BETA-13-G7（中度阈值，精确 reason）：模糊查询 → clarify。
///
/// 仅在**无任何具体约束**（扩展名 / 媒体词 / 已知 artist / 引号关键词）时触发，避免误伤
/// 「昨天的 pdf」类真搜索。各 reason 用具体措辞触发，检测顺序：action → location → time →
/// type → unknown。纯时间约束（昨天的 / 最近的）按"中度"不在此处拦（最近的另由
/// [`is_recent_only_query`] 处理）。
fn detect_vague_clarify(input: &str, lower: &str, language: Language) -> Option<SearchIntent> {
    // 门控：有具体约束就不算模糊
    let has_concrete = has_any_extension_signal(lower)
        || has_any_media_signal(lower)
        || contains_known_artist(lower)
        || input.contains('「')
        || input.contains('《')
        || input.contains('"');
    if has_concrete {
        return None;
    }

    // BETA-13-G12 决策 F（§3.4）：孤立时间词（昨天的/今天的…，剥后无残留）→ ambiguous_type。
    // 时间已明、缺类型，做 keyword 搜索无意义。v0.5 全部「昨天+宾语」锚点带类型/媒体词/关键词
    // （被 has_concrete 或残留非空挡住），故不受影响。
    if let Some(label) = bare_relative_time_only(input) {
        return Some(clarify_with(
            language,
            ClarifyReason::AmbiguousType,
            &format!("你想找{label}的什么类型的文件？"),
        ));
    }

    const ACTION: &[&str] = &[
        "处理一下",
        "处理下",
        "处理这些",
        "弄好它",
        "弄一下",
        "弄好",
        "搞定",
        "收拾一下",
        "do something with",
        "sort this out",
    ];
    if ACTION.iter().any(|s| lower.contains(s)) {
        return Some(clarify_with(
            language,
            ClarifyReason::AmbiguousAction,
            pick(
                language,
                "想对这些文件做什么操作？请说得具体一些。",
                "What would you like to do with these files? Please be more specific.",
            ),
        ));
    }

    const LOC: &[&str] = &[
        "那个文件夹",
        "这个文件夹",
        "某个文件夹",
        "somewhere on",
        "that folder",
        "in that folder",
    ];
    if LOC.iter().any(|s| lower.contains(s)) {
        return Some(clarify_with(
            language,
            ClarifyReason::AmbiguousLocation,
            pick(
                language,
                "在哪个目录里找？",
                "Which folder should I look in?",
            ),
        ));
    }

    const TIME: &[&str] = &["前几天", "之前存的", "之前的", "old ones", "earlier ones"];
    if TIME.iter().any(|s| lower.contains(s)) {
        return Some(clarify_with(
            language,
            ClarifyReason::AmbiguousTime,
            pick(language, "具体是哪段时间？", "Which time range exactly?"),
        ));
    }

    const TYPE_: &[&str] = &[
        "那个东西",
        "这个东西",
        "那个文件",
        "那个 file",
        "that thing",
        "the recent stuff",
        "recent stuff",
    ];
    if TYPE_.iter().any(|s| lower.contains(s)) {
        return Some(clarify_with(
            language,
            ClarifyReason::AmbiguousType,
            pick(
                language,
                "你要找什么类型的文件？",
                "What type of file are you looking for?",
            ),
        ));
    }

    const UNKNOWN: &[&str] = &[
        "帮我看看",
        "帮帮我",
        "随便找点",
        "随便看看",
        "随便找",
        "find it",
        "help me out",
    ];
    if UNKNOWN.iter().any(|s| lower.contains(s)) {
        return Some(clarify_with(
            language,
            ClarifyReason::Unknown,
            pick(
                language,
                "想搜索什么？请描述一下。",
                "What would you like to search for? Please describe it.",
            ),
        ));
    }

    None
}

/// 按 `language` 选中/英文 options 列表并转 `Vec<String>`（lib.rs 内 4 类高优先级
/// clarify 就地双语构造用；detect_vague_clarify 一族经 `standard_options` 自动挂）。
fn bilingual_options(language: Language, zh: &[&str], en: &[&str]) -> Vec<String> {
    let src = if matches!(language, Language::En) {
        en
    } else {
        zh
    };
    src.iter().map(|s| (*s).to_owned()).collect()
}

/// BETA-13-G12 决策 F（§3.4）：识别「孤立时间词」——剥除前导搜索动词与尾「的」后，
/// 剩余恰为单个相对时间词。返回用于 clarify 提问的中文标签；否则 None。
///
/// 收紧到精确等于（非 substring）：`昨天的会议纪要` 剥后是 `昨天的会议纪要`（非时间词）→ None，
/// 不误触发；只有 `昨天的` / `找昨天的` 这类纯时间词才命中。
fn bare_relative_time_only(input: &str) -> Option<&'static str> {
    let mut s = input.trim();
    // 剥前导搜索动词（最长优先）。
    for verb in [
        "帮我找一下",
        "帮我找",
        "帮我",
        "找一下",
        "找找",
        "搜一下",
        "查一下",
        "找",
        "搜",
        "查",
        "find ",
        "search ",
        "show me ",
    ] {
        if let Some(rest) = s.strip_prefix(verb) {
            s = rest.trim();
            break;
        }
    }
    // 剥尾「的」。
    let core = s.trim_end_matches('的').trim();
    const TIMES: &[(&str, &str)] = &[
        ("昨天", "昨天"),
        ("今天", "今天"),
        ("前天", "前天"),
        ("明天", "明天"),
        ("yesterday", "昨天"),
        ("today", "今天"),
    ];
    for (word, label) in TIMES {
        if core.eq_ignore_ascii_case(word) {
            return Some(label);
        }
    }
    None
}

fn has_unsafe_delete_signal(lower: &str) -> bool {
    const SIGNALS: &[&str] = &[
        "全部删掉",
        "全部删除",
        "都删掉",
        "delete all",
        "delete everything",
        "remove all",
        // v0.5：mixed "delete 全部" / 单 "删 全部"
        "delete 全部",
        "remove 全部",
        "删 全部",
        "删除 全部",
        // BETA-13-G7：更多破坏性措辞（删掉所有东西 / 全部清空 / 都删了 / erase everything）
        "删掉所有",
        "删除所有",
        "所有东西",
        "全部清空",
        "清空所有",
        "都删了",
        "都清空",
        "erase everything",
        "wipe everything",
        "erase all",
    ];
    SIGNALS.iter().any(|s| lower.contains(s))
}

/// "最近的"作为唯一约束触发 clarify（schema §3.5 触发规则）。
/// 简化判定：含"最近"且没有强约束词（扩展名 / 文件类型 / 关键词 / 位置 / 媒体词 / 时间窗）。
fn is_recent_only_query(lower: &str) -> bool {
    let has_recent_word =
        lower.contains("最近的") || lower.contains("最近") || lower.contains("recent");
    if !has_recent_word {
        return false;
    }
    // 如果"最近"后面接了具体时间窗，不算"模糊"
    let has_time_window = [
        "几天",
        "一段时间",
        "天",
        "周",
        "月",
        "year",
        "day",
        "week",
        "month",
    ]
    .iter()
    .any(|w| lower.contains(w));
    if has_time_window && !is_recent_alone_phrase(lower) {
        return false;
    }
    // 强约束检测
    let has_strong_constraint = has_any_extension_signal(lower)
        || has_any_location_signal(lower)
        || has_any_media_signal(lower)
        || lower.chars().any(|c| c.is_ascii_digit());
    !has_strong_constraint && !has_keyword_like_signal(lower)
}

/// "找最近的" 这种孤立短语 — 用户没说"最近 X 天"。
fn is_recent_alone_phrase(lower: &str) -> bool {
    let stripped = lower.trim_matches(|c: char| !c.is_alphanumeric() && !is_cjk(c));
    matches!(stripped, "最近的" | "最近" | "recent")
        || stripped.ends_with("最近的")
        || stripped.ends_with("最近")
        || stripped == "找最近的"
        || stripped == "找 最近的"
}

fn is_ambiguous_bulk_action(lower: &str) -> bool {
    let bulk = ["这些都", "全部", "everything", "all of them"]
        .iter()
        .any(|s| lower.contains(s));
    let write_action = ["移动", "复制", "重命名", "改名", "move", "copy", "rename"]
        .iter()
        .any(|s| lower.contains(s));
    bulk && write_action
}

/// 检测"找 X 里的文件"且 X 不是已知 location 时触发。
fn is_unknown_location_only(input: &str, lower: &str) -> bool {
    // 含"里的文件"且不含具体扩展名 / 文件类型 / 已知位置
    let trigger = input.contains("里的文件") || lower.contains("inside") && lower.contains("files");
    if !trigger {
        return false;
    }
    !has_any_extension_signal(lower) && !has_any_location_signal(lower)
}

#[cfg(test)]
mod tests_clarify_i18n {
    //! 2026-07-06 i18n：clarify options/question 按 language 双语呈现。
    //! eval 不校验 clarify 文案/options 内容，故此改动 eval-neutral，只由本组单测守护。
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    fn clarify_of(q: &str) -> Clarify {
        match parse(q) {
            SearchIntent::Clarify(c) => c,
            other => panic!("{q} 应 clarify，实得 {other:?}"),
        }
    }

    #[test]
    fn en_query_gets_english_options_and_question() {
        // 顶层触发（unsafe delete）：英文 query → 英文文案 + 英文 options。
        let c = clarify_of("just erase everything on my computer");
        assert_eq!(c.reason, ClarifyReason::UnsafeAction);
        assert!(
            c.question.is_ascii(),
            "en clarify question 应为英文，实得 {}",
            c.question
        );
        assert_eq!(
            c.options.as_deref().unwrap(),
            ["Show in Finder / Explorer", "Cancel"]
        );
        // detect_vague_clarify 路径（ambiguous_type）：英文 options 来自 standard_options。
        let c = clarify_of("that thing from before");
        assert_eq!(c.reason, ClarifyReason::AmbiguousType);
        assert_eq!(
            c.options.as_deref().unwrap(),
            ["Documents", "Images", "Videos", "Music"]
        );
        assert!(c.question.is_ascii());
    }

    #[test]
    fn zh_query_keeps_chinese_options_and_question() {
        // 中文 query → 中文文案 + 中文 options（既有行为不变）。
        let c = clarify_of("那个东西在哪");
        assert_eq!(c.reason, ClarifyReason::AmbiguousType);
        assert_eq!(
            c.options.as_deref().unwrap(),
            ["文档", "图片", "视频", "音乐"]
        );
        assert!(!c.question.is_ascii(), "zh clarify question 应为中文");
    }

    #[test]
    fn unknown_reason_never_carries_options_either_language() {
        // Unknown reason 无枚举收窄项 → 双语均 options=None。
        assert_eq!(clarify_of("帮我看看").options, None);
        assert_eq!(clarify_of("help me out here").options, None);
    }
}

#[cfg(test)]
mod tests_file_action_v03 {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    fn target_index_of(intent: &SearchIntent) -> Option<u32> {
        match intent {
            SearchIntent::FileAction(fa) => match &fa.target_ref {
                TargetRef::LastResults {
                    selector: TargetSelector::Index { value },
                } => Some(*value),
                _ => None,
            },
            _ => None,
        }
    }

    #[test]
    fn zh_digit_glued_to_di() {
        let intent = parse("打开第1个");
        assert!(
            matches!(intent, SearchIntent::FileAction(_)),
            "expected FileAction, got {intent:?}"
        );
        assert_eq!(target_index_of(&intent), Some(1));
    }

    #[test]
    fn zh_digit_glued_other_actions() {
        for (q, want_action, want_idx) in [
            ("在访达里显示第2个", FileActionKind::Locate, 2),
            ("把第3个复制到桌面", FileActionKind::Copy, 3),
            ("把第4个移动到文稿", FileActionKind::Move, 4),
            ("把第5个改名为 synthetic-final", FileActionKind::Rename, 5),
        ] {
            let intent = parse(q);
            match &intent {
                SearchIntent::FileAction(fa) => {
                    assert_eq!(fa.action, want_action, "query={q}");
                    assert_eq!(target_index_of(&intent), Some(want_idx), "query={q}");
                }
                _ => panic!("query={q} expected FileAction, got {intent:?}"),
            }
        }
    }

    #[test]
    fn en_the_digit_result() {
        for (q, want_action, want_idx) in [
            ("open the 1 result", FileActionKind::Open, 1u32),
            ("show the 2 result in Finder", FileActionKind::Locate, 2),
            ("copy the 3 result to desktop", FileActionKind::Copy, 3),
            ("move the 4 result to documents", FileActionKind::Move, 4),
            (
                "rename the 5 result to synthetic-final",
                FileActionKind::Rename,
                5,
            ),
        ] {
            let intent = parse(q);
            match &intent {
                SearchIntent::FileAction(fa) => {
                    assert_eq!(fa.action, want_action, "query={q}");
                    assert_eq!(target_index_of(&intent), Some(want_idx), "query={q}");
                }
                _ => panic!("query={q} expected FileAction, got {intent:?}"),
            }
        }
    }
}

#[cfg(test)]
mod tests_refine_v03 {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use scout_search_backend::{ClearableField, RelativeTime, TimeExpression};

    fn refine_of(intent: &SearchIntent) -> &Refine {
        match intent {
            SearchIntent::Refine(r) => r,
            other => panic!("expected Refine, got {other:?}"),
        }
    }

    #[test]
    fn zh_only_with_time() {
        let intent = parse("只看上周修改的");
        let r = refine_of(&intent);
        assert_eq!(
            r.delta.modified_time,
            Some(TimeExpression::Relative {
                value: RelativeTime::LastWeek
            })
        );
    }

    #[test]
    fn en_only_in_location() {
        // Task 4 让 parse_location_with_language 按 language 输出英文 hint
        let intent = parse("only in downloads");
        let r = refine_of(&intent);
        assert_eq!(
            r.delta.location.as_ref().and_then(|l| l.hint.clone()),
            Some("downloads".to_owned())
        );
    }

    #[test]
    fn en_clear_location_limit() {
        let intent = parse("clear the location limit");
        let r = refine_of(&intent);
        assert_eq!(r.clear, Some(vec![ClearableField::Location]));
    }

    #[test]
    fn en_limit_to_time() {
        let intent = parse("limit to last week");
        let r = refine_of(&intent);
        assert_eq!(
            r.delta.modified_time,
            Some(TimeExpression::Relative {
                value: RelativeTime::LastWeek
            })
        );
    }

    #[test]
    fn en_exclude_videos() {
        let intent = parse("exclude videos");
        let r = refine_of(&intent);
        assert_eq!(
            r.delta.exclude_file_type,
            Some(vec![scout_search_backend::FileType::Video])
        );
    }
}

#[cfg(test)]
mod tests_location_hint_v03 {
    use super::*;

    fn hint(intent: &SearchIntent) -> Option<String> {
        match intent {
            SearchIntent::FileSearch(fs) => fs.location.as_ref().and_then(|l| l.hint.clone()),
            SearchIntent::MediaSearch(ms) => ms.location.as_ref().and_then(|l| l.hint.clone()),
            SearchIntent::Refine(r) => r.delta.location.as_ref().and_then(|l| l.hint.clone()),
            _ => None,
        }
    }

    #[test]
    fn zh_query_outputs_zh_hint() {
        assert_eq!(
            hint(&parse("找桌面上的 word 文档")),
            Some("桌面".to_owned())
        );
        assert_eq!(
            hint(&parse("找下载目录中大于 100MB 的视频")),
            Some("下载".to_owned())
        );
    }

    #[test]
    fn presentation_does_not_falsely_trigger_documents_location() {
        // BETA-13 回归：「演示文稿」(presentation) 含子串「文稿」(documents location)，
        // 旧版中文 location 走纯 substring 匹配会误报 location.hint="文稿"。
        assert_eq!(hint(&parse("找一份介绍区块链的演示文稿")), None);
        assert_eq!(hint(&parse("找找有没有演示文稿")), None);
        // 真实文稿目录引用不被误伤。
        assert_eq!(hint(&parse("找文稿目录里的 ppt")), Some("文稿".to_owned()));
    }

    #[test]
    fn en_query_outputs_en_hint() {
        assert_eq!(
            hint(&parse("show pdf in downloads")),
            Some("downloads".to_owned())
        );
        assert_eq!(
            hint(&parse("only in downloads")),
            Some("downloads".to_owned())
        );
        assert_eq!(
            hint(&parse("find files on desktop")),
            Some("desktop".to_owned())
        );
    }

    #[test]
    fn mixed_query_preserves_keyword_form_v05() {
        // v0.5：mixed query 的 location hint 按**实际命中的 keyword 形式**选择
        // （撤销 v0.3 的"mixed → zh canonical"政策）。fixture 显示用户用 "desktop"
        // 期望输出 "desktop"，用 "桌面" 期望输出 "桌面"。
        assert_eq!(
            hint(&parse("找 pdf 在 desktop")),
            Some("desktop".to_owned())
        );
        assert_eq!(
            hint(&parse("找 downloads 里的 mp4")),
            Some("downloads".to_owned())
        );
        // 中文 keyword 在 mixed query 中仍输出 zh hint
        assert_eq!(hint(&parse("find pdf 在 桌面")), Some("桌面".to_owned()));
    }
}

#[cfg(test)]
mod tests_documents_disambiguation {
    #![allow(clippy::unwrap_used, clippy::panic)]
    // BETA-09 后续 fix：英文 "documents" 仅作位置词，不应触发 file_type；
    // 中文 "文档" 仍作 file_type trigger（fixture v05-schema-7-007 期望）。
    use super::*;
    use scout_search_backend::FileType;

    fn file_search_of(intent: &SearchIntent) -> &scout_search_backend::FileSearch {
        match intent {
            SearchIntent::FileSearch(fs) => fs,
            other => panic!("expected FileSearch, got {other:?}"),
        }
    }

    #[test]
    fn en_in_documents_is_location_not_file_type() {
        // 5 个 v1 evals partial case 模板："find files containing X in documents"
        // 期望 location.hint="documents"，file_type=None
        let intent = parse("find files containing synthetic-report in documents");
        let fs = file_search_of(&intent);
        assert_eq!(fs.file_type, None, "documents 作位置词不应触发 file_type");
        assert_eq!(
            fs.location.as_ref().and_then(|l| l.hint.clone()),
            Some("documents".to_owned())
        );
    }

    #[test]
    fn zh_wendang_still_triggers_file_type() {
        // fixture v05-schema-7-007 "找名字以「会议纪要」开头的文档" 期望 file_type=document
        let intent = parse("找名字以「会议纪要」开头的文档");
        let fs = file_search_of(&intent);
        assert_eq!(fs.file_type, Some(vec![FileType::Document]));
    }

    #[test]
    fn en_pdf_still_triggers_file_type() {
        // 保留 .pdf / .docx / Excel 等真 file_type trigger 不受影响
        let intent = parse("find pdf in downloads");
        let fs = file_search_of(&intent);
        assert_eq!(fs.file_type, Some(vec![FileType::Document]));
    }
}

#[cfg(test)]
mod tests_en_natural_query_keywords {
    #![allow(clippy::unwrap_used, clippy::panic)]
    // B3.5/en-recall：自然英文 query 不应把疑问词(where)/动词(need/did/save)抽成 keyword，
    // 应跳到真正的内容名词。否则错误 keyword 抑制 gazetteer 兜底，召回漏命中（BETA-15A en gap）。
    use super::*;

    fn keyword_of(intent: &SearchIntent) -> Option<Vec<String>> {
        match intent {
            SearchIntent::FileSearch(fs) => fs.keywords.clone(),
            _ => None,
        }
    }

    fn assert_keyword_contains(query: &str, expected: &str) {
        let intent = parse(query);
        let kw = keyword_of(&intent);
        assert!(
            kw.as_ref()
                .is_some_and(|ks| ks.iter().any(|k| k.eq_ignore_ascii_case(expected))),
            "query={query:?} 应抽到内容词 {expected:?}，实际 keywords={kw:?}"
        );
    }

    #[test]
    fn question_word_where_is_not_extracted() {
        assert_keyword_contains(
            "where is the agreement for the partnership deal",
            "agreement",
        );
        assert_keyword_contains(
            "where did I save the proposal for the new product",
            "proposal",
        );
        assert_keyword_contains(
            "where is the onboarding document for new hires",
            "onboarding",
        );
        // BETA-13-G1：residual 抽内容短语而非单 token，「recipe collection」整体为关键词。
        assert_keyword_contains(
            "where did I save the recipe collection",
            "recipe collection",
        );
    }

    #[test]
    fn verb_need_is_not_extracted() {
        assert_keyword_contains("I need my invoice from last month's trip", "invoice");
    }
}

#[cfg(test)]
mod tests_zh_leading_verb_keywords {
    #![allow(clippy::unwrap_used, clippy::panic)]
    // BETA-13 回归修复：裸单字搜索动词「找/搜/查」直接粘内容词（无「一下/一份」等量词框架）时，
    // 应作为前导动词剥离，不混入 keyword。修复前「找英语」→["找英语"]、「找合同和报告」→["找合同"]。
    use super::*;

    fn keywords_of(query: &str) -> Vec<String> {
        match parse(query) {
            SearchIntent::FileSearch(fs) => fs.keywords.unwrap_or_default(),
            other => panic!("query={query:?} 期望 FileSearch，实际 {other:?}"),
        }
    }

    #[test]
    fn leading_bare_search_verb_stripped() {
        for (q, must_have, must_not) in [
            ("找英语", "英语", "找英语"),
            ("找合同和报告", "合同", "找合同"),
            ("找简历和会议纪要", "简历", "找简历"),
        ] {
            let kws = keywords_of(q);
            assert!(
                kws.iter().any(|k| k == must_have),
                "query={q:?} 应含内容词 {must_have:?}，实际 {kws:?}"
            );
            assert!(
                !kws.iter().any(|k| k == must_not),
                "query={q:?} 不应含带前导动词的 {must_not:?}，实际 {kws:?}"
            );
        }
    }
}

#[cfg(test)]
mod tests_multi_extension {
    #![allow(clippy::unwrap_used, clippy::panic)]
    // B3.5：同范畴多类型查询（如「pdf和doc」）应保留全部扩展名，而非只取首个命中
    // alias（旧 `match_extensions` 用 `.find()` → 丢 pdf）。
    use super::*;
    use scout_search_backend::FileType;

    fn file_search_of(intent: &SearchIntent) -> &scout_search_backend::FileSearch {
        match intent {
            SearchIntent::FileSearch(fs) => fs,
            other => panic!("expected FileSearch, got {other:?}"),
        }
    }

    fn exts_of(intent: &SearchIntent) -> Vec<String> {
        file_search_of(intent)
            .extensions
            .clone()
            .unwrap_or_default()
    }

    #[test]
    fn multi_same_category_keeps_all_extensions() {
        // 用户原 case：「找pdf和doc文件」旧实现只回 docx（doc alias 排 pdf 前 → pdf 丢失）
        let intent = parse("找pdf和doc文件");
        let exts = exts_of(&intent);
        assert!(exts.contains(&"pdf".to_owned()), "应含 pdf，实际 {exts:?}");
        assert!(exts.contains(&"doc".to_owned()), "应含 doc，实际 {exts:?}");
        assert!(
            exts.contains(&"docx".to_owned()),
            "应含 docx，实际 {exts:?}"
        );
        assert_eq!(
            file_search_of(&intent).file_type,
            Some(vec![FileType::Document]),
            "pdf+doc 同为 Document"
        );
    }

    #[test]
    fn single_extension_unchanged() {
        // 回归守护：单类型行为完全不变
        let intent = parse("find pdf in downloads");
        assert_eq!(exts_of(&intent), vec!["pdf".to_owned()]);
        assert_eq!(
            file_search_of(&intent).file_type,
            Some(vec![FileType::Document])
        );
    }

    #[test]
    fn single_multi_extension_alias_unchanged() {
        // 回归守护：单个含多扩展名的 alias（ppt → ppt+pptx）不变
        let intent = parse("找ppt");
        assert_eq!(exts_of(&intent), vec!["ppt".to_owned(), "pptx".to_owned()]);
        assert_eq!(
            file_search_of(&intent).file_type,
            Some(vec![FileType::Presentation])
        );
    }

    // ===== BETA-18：跨范畴多类型（不同 file_type，旧版退回首范畴丢类型）=====

    #[test]
    fn cross_category_ppt_and_pdf_keeps_both_types() {
        // 「ppt和pdf」分属 Presentation / Document：旧实现退回首范畴（Presentation）丢 pdf；
        // 现保留两类型。BETA-13 决策 A/B：跨范畴多类型（≥2 个不同 file_type）只靠 file_type
        // 数组表达类别，extensions 统一为 None（不列部分/不对称扩展名）。
        let intent = parse("找ppt和pdf");
        assert!(
            file_search_of(&intent).extensions.is_none(),
            "跨范畴多类型 extensions 应为 None，实际 {:?}",
            exts_of(&intent)
        );
        assert_eq!(
            file_search_of(&intent).file_type,
            Some(vec![FileType::Presentation, FileType::Document]),
            "ppt(Presentation) 在前、pdf(Document) 在后，按命中序"
        );
    }

    #[test]
    fn cross_category_image_and_video_keeps_both_types() {
        // 「图片和视频」分属 Image / Video，且两者靠空扩展名 + file_type 表达：
        // 旧实现退回首范畴（Image）丢 Video；现应保留两类型，extensions 为空（None）。
        let intent = parse("找图片和视频");
        // BETA-13-G3/BETA-18：顺序按 query 中首次出现位置（图片在前、视频在后），
        // 与覆盖标注的用户语序一致。
        assert_eq!(
            file_search_of(&intent).file_type,
            Some(vec![FileType::Image, FileType::Video]),
        );
        assert!(
            file_search_of(&intent).extensions.is_none(),
            "图片/视频均无具体扩展名，extensions 应为 None"
        );
    }

    #[test]
    fn cross_category_single_value_serializes_as_scalar() {
        // wire 兼容守护：单 file_type 序列化回标量（与单值 schema byte-equal）；
        // 多 file_type 序列化为数组。
        let single = parse("find pdf in downloads");
        let json = serde_json::to_value(&single).unwrap();
        assert_eq!(
            json["file_type"],
            serde_json::json!("document"),
            "单值应为标量"
        );

        let multi = parse("找ppt和pdf");
        let json = serde_json::to_value(&multi).unwrap();
        assert_eq!(
            json["file_type"],
            serde_json::json!(["presentation", "document"]),
            "多值应为数组"
        );
    }
}

#[cfg(test)]
mod tests_beta13_g3_type_words {
    //! BETA-13-G3：中文/范畴类型词 → file_type（不带具体扩展名）。
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use scout_search_backend::FileType;

    fn fs(intent: SearchIntent) -> scout_search_backend::FileSearch {
        match intent {
            SearchIntent::FileSearch(fs) => fs,
            other => panic!("expected FileSearch, got {other:?}"),
        }
    }

    #[test]
    fn category_words_map_to_file_type_without_extensions() {
        for (q, ft) in [
            ("这周改的表格", FileType::Spreadsheet),
            ("今年做的演示文稿", FileType::Presentation),
            ("最近的幻灯片", FileType::Presentation),
            ("找一些照片", FileType::Image),
            ("音频和图片", FileType::Audio),
            ("代码文件在哪", FileType::Code),
            ("找可执行文件", FileType::Executable),
        ] {
            let f = fs(parse(q));
            assert!(
                f.file_type.as_deref().is_some_and(|t| t.contains(&ft)),
                "{q:?} 应含 file_type {ft:?}，实得 {:?}",
                f.file_type
            );
        }
    }

    #[test]
    fn chinese_spreadsheet_word_carries_no_extensions() {
        // 「表格」是范畴词，只给 file_type 不带具体扩展名（区别于 ASCII「excel」→ xls/xlsx）。
        let f = fs(parse("这周改的表格"));
        assert_eq!(f.file_type, Some(vec![FileType::Spreadsheet]));
        assert!(f.extensions.is_none(), "中文类型词不应带扩展名");
    }

    #[test]
    fn ascii_format_word_still_carries_extensions() {
        // v0.5 锚点不回归：ASCII 格式词 excel/ppt 仍带扩展名。
        let f = fs(parse("find Excel modified this week"));
        assert_eq!(f.file_type, Some(vec![FileType::Spreadsheet]));
        assert_eq!(
            f.extensions,
            Some(vec!["xls".to_owned(), "xlsx".to_owned()])
        );
    }

    #[test]
    fn english_code_phrase_does_not_match_verification_code() {
        // 「code」单词易在「verification code / QR code」误命中 → 英文只认短语形式。
        let f = fs(parse("find files with name containing verification code"));
        assert!(
            !f.file_type
                .as_deref()
                .is_some_and(|t| t.contains(&FileType::Code)),
            "「verification code」不应触发 Code，实得 {:?}",
            f.file_type
        );
    }

    #[test]
    fn cross_category_file_types_follow_query_order() {
        // BETA-18/19：多 file_type 按 query 语序（非词典序）。
        assert_eq!(
            fs(parse("找音频和图片")).file_type,
            Some(vec![FileType::Audio, FileType::Image])
        );
        assert_eq!(
            fs(parse("word 和 ppt 文档")).file_type,
            Some(vec![FileType::Document, FileType::Presentation])
        );
    }
}

#[cfg(test)]
mod tests_beta13_g6_sort {
    //! BETA-13-G6：上下文感知的显式排序词。
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use scout_search_backend::SortOrder;

    fn sort_of(intent: SearchIntent) -> Option<SortOrder> {
        match intent {
            SearchIntent::FileSearch(fs) => fs.sort,
            other => panic!("expected FileSearch, got {other:?}"),
        }
    }

    #[test]
    fn name_sort_asc_and_desc() {
        // 「按名字」不是 refine 信号（refine 只认「按名称」），留在 file_search。
        assert_eq!(
            sort_of(parse("按名字排序所有的 PDF")),
            Some(SortOrder::NameAsc)
        );
        assert_eq!(sort_of(parse("找 PDF 按名字排")), Some(SortOrder::NameAsc));
        assert_eq!(
            sort_of(parse("把文档按名字倒序排")),
            Some(SortOrder::NameDesc)
        );
    }

    #[test]
    fn direction_with_dimension() {
        assert_eq!(
            sort_of(parse("最早创建的那批照片")),
            Some(SortOrder::CreatedAsc)
        );
        assert_eq!(
            sort_of(parse("最新创建的报告")),
            Some(SortOrder::CreatedDesc)
        );
        assert_eq!(
            sort_of(parse("最近访问的文件")),
            Some(SortOrder::AccessedDesc)
        );
        assert_eq!(
            sort_of(parse("最早改动的那几个表格")),
            Some(SortOrder::ModifiedAsc)
        );
    }

    #[test]
    fn superlative_without_dimension_keeps_default() {
        // 无维度词时不触发上下文解析，保留既有 SORT_ALIASES 行为（最新 → modified_desc）。
        assert_eq!(
            sort_of(parse("找最新的报告")),
            Some(SortOrder::ModifiedDesc)
        );
    }
}

#[cfg(test)]
mod tests_beta13_g1_keywords {
    //! BETA-13-G1：纯中文自然语言查询的跨度剥离关键词抽取。
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    fn kw(intent: SearchIntent) -> Option<Vec<String>> {
        match intent {
            SearchIntent::FileSearch(fs) => fs.keywords,
            other => panic!("expected FileSearch, got {other:?}"),
        }
    }

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn extracts_content_phrase_from_natural_query() {
        for (q, expected) in [
            ("帮我找找关于年度预算的表格", &["年度预算"][..]),
            ("找一份工作汇报相关的文档", &["工作汇报"][..]),
            ("找一下租房合同", &["租房合同"][..]),
            ("我想找一下产品需求文档", &["产品需求"][..]),
            ("帮我找份履历", &["履历"][..]),
            ("找下跟客户报价有关的文件", &["客户报价"][..]),
            ("上个月改过的发票", &["发票"][..]),
            ("照片里有车牌号的图片", &["车牌号"][..]),
        ] {
            assert_eq!(kw(parse(q)), Some(v(expected)), "{q:?}");
        }
    }

    #[test]
    fn drops_container_nouns_keeps_content() {
        // 「X的报告/笔记」→ 报告/笔记 作整段容器名词丢弃，但「体检报告」整词保留。
        assert_eq!(
            kw(parse("找一份关于股票投资的笔记")),
            Some(v(&["股票投资"]))
        );
        assert_eq!(kw(parse("我要找体检报告")), Some(v(&["体检报告"])));
    }

    #[test]
    fn pure_signal_query_yields_no_keyword() {
        // 全是信号词（时间/类型/排序）→ 残留为空，不臆造关键词。
        assert_eq!(kw(parse("最早改动的那几个表格")), None);
        assert_eq!(kw(parse("把文档按名字倒序排")), None);
        assert_eq!(kw(parse("这周改的表格")), None);
    }

    #[test]
    fn english_extracts_content_phrase_not_noise() {
        // 英文残留短语抽取：取内容短语，而非 about/the 等噪声词或单 token。
        assert_eq!(
            kw(parse("find a spreadsheet about the annual budget")),
            Some(v(&["annual budget"]))
        );
        assert_eq!(
            kw(parse("I'm looking for the meeting notes")),
            Some(v(&["meeting notes"]))
        );
        assert_eq!(
            kw(parse("find the research paper on climate change")),
            Some(v(&["research paper", "climate change"]))
        );
        // 容器名词整段丢弃，X report 保留。
        assert_eq!(
            kw(parse("report whose body mentions market share")),
            Some(v(&["market share"]))
        );
        // 纯信号 → None。
        assert_eq!(kw(parse("spreadsheets changed this week")), None);
        assert_eq!(kw(parse("find pdf larger than 1GB in downloads")), None);
    }

    #[test]
    fn mixed_merges_en_phrase_and_cjk_segment_in_order() {
        // mixed：英文短语 + 中文残留段按 query 语序合并。
        assert_eq!(
            kw(parse("找一份关于 marketing plan 的ppt")),
            Some(v(&["marketing plan"]))
        );
        assert_eq!(
            kw(parse("找一下关于 SEO 优化的资料")),
            Some(v(&["SEO", "优化"]))
        );
        assert_eq!(kw(parse("find 一下年度预算的表格")), Some(v(&["年度预算"])));
    }
}

#[cfg(test)]
mod tests_keyword_cleanup_v03 {
    use super::*;

    fn keywords_of(intent: &SearchIntent) -> Option<&[String]> {
        match intent {
            SearchIntent::FileSearch(fs) => fs.keywords.as_deref(),
            _ => None,
        }
    }

    #[test]
    fn size_token_does_not_leak_into_keywords() {
        for q in [
            "找下载目录中大于 100MB 的视频",
            "找过去一个月里大于 1GB 的视频",
            "find files larger than 500mb",
        ] {
            let intent = parse(q);
            assert!(
                keywords_of(&intent).map_or(true, <[String]>::is_empty),
                "query={q} expected no keywords, got {:?}",
                keywords_of(&intent)
            );
        }
    }
}

#[cfg(test)]
mod tests_refine_v05 {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use scout_search_backend::ClearableField;

    fn refine_of(intent: &SearchIntent) -> &Refine {
        match intent {
            SearchIntent::Refine(r) => r,
            other => panic!("expected Refine, got {other:?}"),
        }
    }

    #[test]
    fn v05_clear_previous_location_routes_to_refine() {
        // v05-schema-43-044
        let intent = parse("清空上一轮位置约束");
        let r = refine_of(&intent);
        assert_eq!(r.clear, Some(vec![ClearableField::Location]));
    }

    #[test]
    fn v05_only_downloads_lide_mixed_routes_to_refine_location() {
        // v05-refine-template-446：路由正确 + location 存在；hint 值（zh "下载" vs en "downloads"）
        // 沿用 v0.3 mixed → zh canonical 政策，与 fixture 期望英文 hint 偏差作为 partial 接受。
        let intent = parse("only downloads 里的");
        let r = refine_of(&intent);
        assert!(
            r.delta
                .location
                .as_ref()
                .and_then(|l| l.hint.as_ref())
                .is_some(),
            "expected delta.location present, got {:?}",
            r.delta.location
        );
    }
}

#[cfg(test)]
mod tests_refine_v09_delta {
    //! BETA-14 缺口盘点第 2 刀：设值 scope / 排除后设值 / refine artist 兜底 / sort 附加 / clear 语序。
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use scout_search_backend::{ClearableField, FileType, SortOrder};

    fn refine_of(intent: &SearchIntent) -> Refine {
        match intent {
            SearchIntent::Refine(r) => r.clone(),
            other => panic!("expected Refine, got {other:?}"),
        }
    }

    #[test]
    fn exclude_then_keep_sets_kept_type_not_excluded() {
        // 「把 ppt 也排除掉，只看视频」：设值对象是 视频，ppt 归排除
        let r = refine_of(&parse("把 ppt 也排除掉，只看视频"));
        assert_eq!(r.delta.file_type, Some(vec![FileType::Video]));
        assert_eq!(r.delta.extensions, None, "被排除的 ppt 不应进 extensions");
        assert_eq!(
            r.delta.exclude_file_type,
            Some(vec![FileType::Presentation])
        );
    }

    #[test]
    fn refine_artist_fallback_zh_en() {
        let r = refine_of(&parse("只要周杰伦的"));
        assert_eq!(r.delta.artist.as_deref(), Some("周杰伦"));
        let r = refine_of(&parse("only the ones by Adele"));
        assert_eq!(r.delta.artist.as_deref(), Some("Adele"));
    }

    #[test]
    fn artist_fallback_not_triggered_when_other_fields_set() {
        // 位置/类型/时间已设值时不做 artist 兜底
        for q in ["只看下载目录里的", "只看 pdf 的", "只看上周修改的"] {
            let r = refine_of(&parse(q));
            assert_eq!(r.delta.artist, None, "{q} 不应抽 artist");
        }
    }

    #[test]
    fn sort_attached_when_refine_triggered_by_keep() {
        // keep 触发 refine 后，排序词也要附加（"just the videos, sorted by size"）
        let r = refine_of(&parse("just the videos, sorted by size"));
        assert_eq!(r.delta.file_type, Some(vec![FileType::Video]));
        assert_eq!(r.delta.sort, Some(SortOrder::SizeDesc));
    }

    #[test]
    fn multi_clear_follows_query_order() {
        // 「位置不限，类型也不限」→ [location, file_type]（按语序）
        let r = refine_of(&parse("位置不限，类型也不限"));
        assert_eq!(
            r.clear,
            Some(vec![ClearableField::Location, ClearableField::FileType])
        );
        // 反向语序仍按语序
        let r = refine_of(&parse("类型不限，位置也不限"));
        assert_eq!(
            r.clear,
            Some(vec![ClearableField::FileType, ClearableField::Location])
        );
    }

    #[test]
    fn refine_from_last_month_is_modified_time() {
        // "now only show me the ones from last month" → modified_time（非 created）
        let r = refine_of(&parse("now only show me the ones from last month"));
        assert!(r.delta.modified_time.is_some(), "{:?}", r.delta);
        assert_eq!(r.delta.created_time, None);
    }
}

#[cfg(test)]
mod tests_clarify_v05 {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    fn clarify_of(intent: &SearchIntent) -> &Clarify {
        match intent {
            SearchIntent::Clarify(c) => c,
            other => panic!("expected Clarify, got {other:?}"),
        }
    }

    #[test]
    fn v05_find_recent_alone_triggers_clarify_ambiguous_time() {
        // v05-clarify-template-481
        let intent = parse("find recent");
        let c = clarify_of(&intent);
        assert_eq!(c.reason, ClarifyReason::AmbiguousTime);
    }

    #[test]
    fn v05_zhao_recent_de_triggers_clarify_ambiguous_time() {
        // v05-clarify-template-496
        let intent = parse("找 recent 的");
        let c = clarify_of(&intent);
        assert_eq!(c.reason, ClarifyReason::AmbiguousTime);
    }

    #[test]
    fn v05_delete_quanbu_triggers_clarify_unsafe() {
        // v05-clarify-template-497
        let intent = parse("delete 全部");
        let c = clarify_of(&intent);
        assert_eq!(c.reason, ClarifyReason::UnsafeAction);
    }

    #[test]
    fn v05_find_recent_files_does_not_trigger_clarify() {
        // 反例：含具体 file_type 不应触发 clarify
        let intent = parse("find recent pdf");
        assert!(
            !matches!(intent, SearchIntent::Clarify(_)),
            "含具体 file_type 不应 clarify，got {intent:?}"
        );
    }
}

#[cfg(test)]
mod tests_copula_stopwords {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    #[test]
    fn copula_are_not_extracted_as_keyword_en_recall() {
        // "are"（3 字符 copula）不应作内容 keyword，应跳到真正的内容名词 "minutes"。
        let intent = parse("where are the minutes from the October all-hands");
        let SearchIntent::FileSearch(fs) = intent else {
            panic!("应为 FileSearch，实际 {intent:?}");
        };
        let kws = fs.keywords.unwrap_or_default();
        assert!(
            kws.iter().any(|k| k == "minutes"),
            "keywords 应含 minutes，实际 {kws:?}"
        );
        assert!(
            !kws.iter().any(|k| k == "are"),
            "keywords 不应含 are，实际 {kws:?}"
        );
    }
}

#[cfg(test)]
mod tests_beta13_g4_g5_routing {
    //! BETA-13-G4（refine 自然标记词）+ G5（file_action 自然识别）。
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    #[test]
    fn refine_natural_markers() {
        // 换成/再大一点/去掉…限制/不限…了/just keep/switch to 等自然 refine 措辞。
        assert!(matches!(parse("换成上周改的"), SearchIntent::Refine(_)));
        assert!(matches!(
            parse("再大一点的，超过 50MB 的"),
            SearchIntent::Refine(_)
        ));
        assert!(matches!(parse("去掉位置限制"), SearchIntent::Refine(_)));
        assert!(matches!(parse("不限类型了"), SearchIntent::Refine(_)));
        assert!(matches!(
            parse("change it to last week instead"),
            SearchIntent::Refine(_)
        ));
        assert!(matches!(
            parse("drop the type filter"),
            SearchIntent::Refine(_)
        ));
    }

    #[test]
    fn refine_clear_targets_correct_field() {
        let SearchIntent::Refine(r) = parse("时间不限了") else {
            panic!("应为 refine");
        };
        assert_eq!(
            r.clear,
            Some(vec![scout_search_backend::ClearableField::ModifiedTime])
        );
    }

    #[test]
    fn file_action_natural_targets() {
        use scout_search_backend::{FileActionKind, TargetRef, TargetSelector};
        // 指示代词 → 首个
        let SearchIntent::FileAction(a) = parse("在访达里定位这个文件") else {
            panic!("应为 file_action");
        };
        assert_eq!(a.action, FileActionKind::Locate);
        assert!(matches!(
            a.target_ref,
            TargetRef::LastResults {
                selector: TargetSelector::Index { value: 1 }
            }
        ));
        // 删除 + 全部
        let SearchIntent::FileAction(d) = parse("删除这些") else {
            panic!("应为 file_action");
        };
        assert_eq!(d.action, FileActionKind::Delete);
        assert!(matches!(
            d.target_ref,
            TargetRef::LastResults {
                selector: TargetSelector::All
            }
        ));
        // 绝对路径目标
        let SearchIntent::FileAction(p) = parse("打开 /Users/me/Documents/report.pdf") else {
            panic!("应为 file_action");
        };
        assert!(matches!(p.target_ref, TargetRef::Path { .. }));
        // 多序数 → Indices
        let SearchIntent::FileAction(m) = parse("删掉第2和第4个") else {
            panic!("应为 file_action");
        };
        assert!(matches!(
            m.target_ref,
            TargetRef::LastResults {
                selector: TargetSelector::Indices { .. }
            }
        ));
    }
}

#[cfg(test)]
mod tests_beta13_g2_music {
    //! BETA-13-G2：音乐 metadata 措辞鲁棒性（artist/genre/album/title/quality/duration）。
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use scout_search_backend::{MediaType, Quality, SizeExpression, SizeUnit};

    fn ms(intent: SearchIntent) -> scout_search_backend::MediaSearch {
        match intent {
            SearchIntent::MediaSearch(m) => m,
            other => panic!("expected MediaSearch, got {other:?}"),
        }
    }

    #[test]
    fn free_artist_extraction() {
        assert_eq!(ms(parse("周杰伦的歌")).artist.as_deref(), Some("周杰伦"));
        assert_eq!(
            ms(parse("放首李荣浩唱的")).artist.as_deref(),
            Some("李荣浩")
        );
        assert_eq!(
            ms(parse("find tracks by Coldplay")).artist.as_deref(),
            Some("Coldplay")
        );
        assert_eq!(ms(parse("songs by Adele")).artist.as_deref(), Some("Adele"));
    }

    #[test]
    fn genre_quality_duration() {
        assert_eq!(ms(parse("找一些爵士乐")).genre.as_deref(), Some("爵士"));
        assert_eq!(
            ms(parse("find some jazz music")).genre.as_deref(),
            Some("jazz")
        );
        assert_eq!(ms(parse("无损音乐")).quality, Some(Quality::Lossless));
        assert_eq!(ms(parse("找一些高品质的歌")).quality, Some(Quality::High));
        assert_eq!(
            ms(parse("时长不到3分钟的歌曲")).duration,
            Some(SizeExpression::LessThan {
                value: 3.0,
                unit: SizeUnit::Min
            })
        );
        assert_eq!(
            ms(parse("找超过一小时的音频")).duration,
            Some(SizeExpression::GreaterThan {
                value: 1.0,
                unit: SizeUnit::Hour
            })
        );
    }

    #[test]
    fn album_and_title() {
        assert_eq!(
            ms(parse("薛之谦《绅士》专辑")).album.as_deref(),
            Some("绅士")
        );
        assert_eq!(ms(parse("the album 1989")).album.as_deref(), Some("1989"));
        assert_eq!(ms(parse("播放 晴天")).title.as_deref(), Some("晴天"));
        assert_eq!(
            ms(parse("play the song Hello")).title.as_deref(),
            Some("Hello")
        );
    }

    #[test]
    fn music_metadata_routes_to_media() {
        for q in [
            "周杰伦的歌",
            "找一些爵士乐",
            "the album 1989",
            "lossless music",
            "find tracks by Coldplay",
        ] {
            assert!(
                matches!(parse(q), SearchIntent::MediaSearch(_)),
                "{q:?} 应路由 media"
            );
        }
        // 无损格式不应被当 artist
        assert!(ms(parse("无损格式的歌曲")).artist.is_none());
        // 媒体类型：音乐视频 → Video
        assert_eq!(ms(parse("音乐视频")).media_type, MediaType::Video);
    }
}

#[cfg(test)]
mod tests_beta13_g12_decision_e_quantity_media {
    //! BETA-13-G12 决策 E（§3.2）：数量/程度修饰 + 视觉媒体 → media_search。
    //! 「几个/some/短」等修饰词不应打断媒体路由（coverage v09-d2-zh-029/zh-032/en-024）。
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use scout_search_backend::MediaType;

    fn ms(intent: SearchIntent) -> scout_search_backend::MediaSearch {
        match intent {
            SearchIntent::MediaSearch(m) => m,
            other => panic!("expected MediaSearch, got {other:?}"),
        }
    }

    #[test]
    fn quantity_degree_modifier_routes_video_to_media() {
        for q in ["找几个视频", "短视频", "some videos"] {
            let m = ms(parse(q));
            assert_eq!(m.media_type, MediaType::Video, "query={q:?} 应为 video");
            // 修饰词不应泄漏成 artist/title/genre/keyword。
            assert!(
                m.artist.is_none(),
                "query={q:?} artist 应空，实得 {:?}",
                m.artist
            );
            assert!(
                m.title.is_none(),
                "query={q:?} title 应空，实得 {:?}",
                m.title
            );
            assert!(
                m.genre.is_none(),
                "query={q:?} genre 应空，实得 {:?}",
                m.genre
            );
        }
    }
}

#[cfg(test)]
mod tests_beta13_g12_decision_f_bare_time_clarify {
    //! BETA-13-G12 决策 F（§3.4）：孤立时间词（昨天的，剥后无残留）→ clarify(ambiguous_type)。
    //! 时间已明、缺类型（coverage v09-d8-zh-003）。带类型/媒体/宾语的「昨天 X」不受影响。
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use scout_search_backend::ClarifyReason;

    #[test]
    fn bare_relative_time_triggers_ambiguous_type() {
        for q in ["昨天的", "找昨天的"] {
            match parse(q) {
                SearchIntent::Clarify(c) => {
                    assert_eq!(c.reason, ClarifyReason::AmbiguousType, "query={q:?}");
                }
                other => panic!("query={q:?} 应为 Clarify(ambiguous_type)，实得 {other:?}"),
            }
        }
    }

    #[test]
    fn time_with_object_does_not_clarify() {
        // v0.5 全部「昨天+宾语」锚点形态：带类型/媒体词/关键词时不应误触发 clarify。
        for q in [
            "昨天的 pdf",
            "昨天的视频",
            "昨天的会议纪要",
            "找昨天下载目录的ppt",
        ] {
            assert!(
                !matches!(parse(q), SearchIntent::Clarify(_)),
                "query={q:?} 不应 clarify"
            );
        }
    }
}

#[cfg(test)]
mod tests_beta13_g7_clarify {
    //! BETA-13-G7：中度阈值模糊查询 → clarify（精确 reason）。
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use scout_search_backend::ClarifyReason;

    fn reason(intent: SearchIntent) -> ClarifyReason {
        match intent {
            SearchIntent::Clarify(c) => c.reason,
            other => panic!("expected Clarify, got {other:?}"),
        }
    }

    #[test]
    fn precise_reasons() {
        assert_eq!(reason(parse("删掉所有东西")), ClarifyReason::UnsafeAction);
        assert_eq!(reason(parse("全部清空")), ClarifyReason::UnsafeAction);
        assert_eq!(
            reason(parse("处理一下这个")),
            ClarifyReason::AmbiguousAction
        );
        assert_eq!(
            reason(parse("放在那个文件夹里的")),
            ClarifyReason::AmbiguousLocation
        );
        assert_eq!(reason(parse("前几天那个")), ClarifyReason::AmbiguousTime);
        assert_eq!(reason(parse("那个东西在哪")), ClarifyReason::AmbiguousType);
        assert_eq!(reason(parse("帮我看看")), ClarifyReason::Unknown);
        assert_eq!(reason(parse("find it")), ClarifyReason::Unknown);
    }

    #[test]
    fn concrete_query_not_clarified() {
        // 有具体约束（扩展名/关键词）→ 不误判为模糊。
        assert!(matches!(parse("昨天的 pdf"), SearchIntent::FileSearch(_)));
        assert!(matches!(
            parse("处理一下报告.pdf"),
            SearchIntent::FileSearch(_)
        ));
    }
}
