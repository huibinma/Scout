//! `SearchIntent::Clarify` 构造 helper。
//!
//! 顶层 `parse()` 内的 clarify 触发判断仍在 lib.rs，作为 dispatcher 前置。
#![allow(clippy::pedantic)]

use scout_search_backend::{Clarify, ClarifyReason, Language, SchemaVersion, SearchIntent};

/// 按 `language` 二选一呈现文案：`En` → 英文，其余（zh / mixed / unknown）→ 中文。
/// mixed 仍归中文（mixed query 含中文、CJK 主导，中文回应更自然）。
pub(crate) fn pick<'a>(language: Language, zh: &'a str, en: &'a str) -> &'a str {
    match language {
        Language::En => en,
        _ => zh,
    }
}

/// 方案 A（2026-07-06 拍板）：
/// clarify 是否带 options **由 reason 决定**——凡有"可枚举的收窄维度"的 reason 一律带一组标准
/// options（一键收窄 UX），唯 `Unknown` 无枚举项、只能自由文本追问、不带 options。
///
/// eval 仅校验 options 的**结构存在性**（都是 Array 或都是 null），内容/长度/顺序不作判定。
/// 内容按 `language` 双语呈现（2026-07-06 i18n：en query 返回英文 options，闭合旧缺口）。
fn standard_options(reason: ClarifyReason, language: Language) -> Option<Vec<String>> {
    let opts: &[&str] = match reason {
        ClarifyReason::UnsafeAction => pick_opts(
            language,
            &["在访达/资源管理器中显示", "取消"],
            &["Show in Finder / Explorer", "Cancel"],
        ),
        ClarifyReason::AmbiguousAction => pick_opts(
            language,
            &["打开", "移动", "删除", "取消"],
            &["Open", "Move", "Delete", "Cancel"],
        ),
        ClarifyReason::AmbiguousType => pick_opts(
            language,
            &["文档", "图片", "视频", "音乐"],
            &["Documents", "Images", "Videos", "Music"],
        ),
        ClarifyReason::AmbiguousTime => pick_opts(
            language,
            &["今天", "过去 3 天", "过去一周", "过去一个月"],
            &["Today", "Past 3 days", "Past week", "Past month"],
        ),
        ClarifyReason::AmbiguousLocation => pick_opts(
            language,
            &["全盘搜索", "下载", "文稿", "桌面", "取消"],
            &["Whole disk", "Downloads", "Documents", "Desktop", "Cancel"],
        ),
        ClarifyReason::Unknown => return None,
    };
    Some(opts.iter().map(|s| (*s).to_owned()).collect())
}

fn pick_opts(
    language: Language,
    zh: &'static [&'static str],
    en: &'static [&'static str],
) -> &'static [&'static str] {
    match language {
        Language::En => en,
        _ => zh,
    }
}

pub(crate) fn clarify_unknown(language: Language, question: &str) -> SearchIntent {
    clarify_with(language, ClarifyReason::Unknown, question)
}

/// BETA-13-G7：以指定 `reason` 构造 clarify（question 仅作呈现，eval 不比对文案）。
/// 方案 A 起 options 按 `reason` + `language` 自动挂载（见 [`standard_options`]），Unknown 除外。
pub(crate) fn clarify_with(
    language: Language,
    reason: ClarifyReason,
    question: &str,
) -> SearchIntent {
    SearchIntent::Clarify(Clarify {
        schema_version: SchemaVersion::V1,
        language: Some(language),
        reason,
        question: question.to_owned(),
        options: standard_options(reason, language),
    })
}
