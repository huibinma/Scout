//! `SearchIntent::Refine` 规则解析器。
#![allow(clippy::pedantic, clippy::expect_used, clippy::while_let_on_iterator)]

use scout_search_backend::{
    BaseRef, ClearableField, Language, Refine, RefineDelta, SchemaVersion, SearchIntent, SortOrder,
};

use super::common::{parse_location_with_language, parse_time_fields};
use super::file_search::{match_extensions, parse_size};
use super::media_search::extract_artist;

/// BETA-13-G12 决策 C 同构：判定查询是否为「正向类型 + 排除某类型」的**全新搜索**
/// （应走 file_search 带 exclude_file_type，而非 refine）。
///
/// 三条判据须同时满足（穿过 v0.5 全部裸排除锚点 + v0.9 反例「把 ppt 也排除掉」）：
/// 1. 含前向 exclude 标记「排除」/「exclude」；
/// 2. 标记**紧跟**类型/扩展名词（前向形 `排除压缩包`/`exclude archives`）——藉此排除
///    尾置形「把 ppt 也排除**掉**」（排除后是「掉」非类型）；
/// 3. 标记**之前**存在正向类型/扩展名——藉此排除裸「排除视频」「exclude videos」
///    （排除前无正向前缀，v0.5 全部如此 → 仍 refine，byte-equal 安全）。
fn is_fresh_positive_then_exclude(lower: &str) -> bool {
    for marker in ["排除", "exclude"] {
        let Some(pos) = lower.find(marker) else {
            continue;
        };
        let before = &lower[..pos];
        let after = lower[pos + marker.len()..].trim_start();
        let after_starts_type = lexicon_extension_keywords().any(|k| after.starts_with(k));
        let before_has_type = match_extensions(before).is_some();
        if after_starts_type && before_has_type {
            return true;
        }
    }
    false
}

/// 所有扩展名 alias 的 keyword（供前向 exclude 标记的「紧跟类型词」前缀判定）。
fn lexicon_extension_keywords() -> impl Iterator<Item = &'static str> {
    crate::lexicon::EXTENSION_ALIASES
        .iter()
        .flat_map(|a| a.keywords.iter().copied())
}

/// 最早出现的设值标记（keep/replace/add/limit_to 词集）在 lower 中的**结束**字节偏移。
/// 用于把类型匹配限定在设值对象上（排除子句里的类型词不参与设值）。
fn earliest_set_marker_end(lower: &str) -> Option<usize> {
    const SET_MARKERS: &[&str] = &[
        "只看",
        "只要",
        "只保留",
        "show only",
        "only in",
        "only ",
        "just keep",
        "just the",
        "换成",
        "改成",
        "换为",
        "改为",
        "change it to",
        "change to",
        "switch to",
        "再加上",
        "再加",
        "limit to",
        "限定到",
        "限定为",
    ];
    SET_MARKERS
        .iter()
        .filter_map(|m| lower.find(m).map(|pos| (pos, pos + m.len())))
        .min()
        .map(|(_, end)| end)
}

/// refine 语境 artist 兜底：「只要/只看 X 的」（X = 2-4 汉字）/ "the ones by X"（首字母大写词串）。
fn refine_artist_fallback(input: &str) -> Option<String> {
    use regex::Regex;
    use std::sync::OnceLock;
    static RE_ZH: OnceLock<Regex> = OnceLock::new();
    let re_zh =
        RE_ZH.get_or_init(|| Regex::new(r"(?:只要|只看)\s*([\p{Han}]{2,4})\s*的").expect("regex"));
    if let Some(cap) = re_zh.captures(input) {
        let candidate = cap[1].trim();
        if !super::artist::is_stopword_artist(candidate) {
            return Some(candidate.to_owned());
        }
    }
    static RE_EN: OnceLock<Regex> = OnceLock::new();
    let re_en = RE_EN.get_or_init(|| {
        Regex::new(r"(?:ones|those|these)\s+by\s+([A-Z][A-Za-z]*(?:\s+[A-Z][A-Za-z]*)*)")
            .expect("regex")
    });
    if let Some(cap) = re_en.captures(input) {
        let candidate = cap[1].trim();
        if !super::artist::is_stopword_artist(candidate) {
            return Some(candidate.to_owned());
        }
    }
    None
}

pub(crate) fn try_parse_refine(
    input: &str,
    lower: &str,
    language: Language,
) -> Option<SearchIntent> {
    // ---- 触发词 ----
    // BETA-13-G4：扩展自然 refine 措辞。仅加入**隐含"修改上一轮检索"语义**的标记词
    // （换成/改成/再加上/去掉/不限…了/just keep/change it to/switch to/drop/forget/remove），
    // 不动 sort 触发词——「按大小/by name」已会过度捕获带排序的 file_search 查询，
    // 再加裸排序词会把「按名字排序所有的 PDF」误判为 refine。
    let keep_signal = [
        "只看",
        "只要",
        "只保留",
        "show only",
        "only in",
        "only ",
        "just keep",
        "just the",
    ]
    .iter()
    .any(|s| lower.contains(s));
    let replace_signal = [
        "换成",
        "改成",
        "换为",
        "改为",
        "change it to",
        "change to",
        "switch to",
        "instead",
    ]
    .iter()
    .any(|s| lower.contains(s));
    let add_signal = [
        "再加上",
        "再加",
        "再大一点",
        "再小一点",
        "the bigger ones",
        "the smaller ones",
    ]
    .iter()
    .any(|s| lower.contains(s));
    // BETA-13-G12 决策 C 同构：区分「纯排除」(对上一轮结果细化 → refine) 与
    // 「正向类型 + 排除某类型」的全新搜索 (如「文档和图片，排除压缩包」→ file_search 带
    // exclude_file_type)。判据见 [`is_fresh_positive_then_exclude`]——满足则不作 refine 触发，
    // 交 file_search。裸「排除视频」(排除前无正向类型) 与「把 ppt 也排除掉」(尾置形) 仍 refine。
    let exclude_signal = ["排除", "exclude"].iter().any(|s| lower.contains(s))
        && !is_fresh_positive_then_exclude(lower);
    // BETA-13 决策 C：区分「纯排序」(对上一轮结果细化 → refine) 与「带搜索约束的排序句」
    // (如「sort all my PDFs by name」「下载里压缩包按大小排」→ 新搜索 file_search)。
    // 裸排序词(无文件类型/位置约束，如「按大小倒序」「sort by size」「按名字排序」)仍是 refine；
    // 一旦句中含文件类型或位置约束，排序词不再作 refine 触发，交 file_search 处理。
    let raw_sort = [
        "按大小",
        "按 size",
        "按名称",
        "按名字",
        "by size",
        "by name",
    ]
    .iter()
    .any(|s| lower.contains(s));
    let has_search_scope =
        match_extensions(lower).is_some() || super::file_search::has_any_location_signal(lower);
    let sort_signal = raw_sort && !has_search_scope;
    let clear_signal = [
        "不限制",
        "不限",
        "去掉",
        "lift the limit",
        "clear the",
        "drop the",
        "forget the",
        "remove the",
        "清空上一轮",
        "清空 ",
        "清除",
    ]
    .iter()
    .any(|s| lower.contains(s));
    let limit_to_signal = ["limit to", "限定到", "限定为"]
        .iter()
        .any(|s| lower.contains(s));

    if !(keep_signal
        || replace_signal
        || add_signal
        || exclude_signal
        || sort_signal
        || clear_signal
        || limit_to_signal)
    {
        return None;
    }

    let mut delta = RefineDelta::default();
    let mut clears: Vec<ClearableField> = Vec::new();

    // ---- clear：判断要清除哪个字段（可多个，按 query 语序输出）----
    if clear_signal {
        // 每类字段取其触发词在 query 中的最早出现位置，多字段 clear 按出现顺序排列
        // （「位置不限，类型也不限」→ [location, file_type]）。
        let mut positioned: Vec<(usize, ClearableField)> = Vec::new();
        let field_markers: &[(&[&str], ClearableField)] = &[
            (
                &["类型", "扩展名", "type", "format"],
                ClearableField::FileType,
            ),
            (
                &["时间", "日期", "time", "date"],
                ClearableField::ModifiedTime,
            ),
            (
                &["位置", "文件夹", "目录", "location", "folder"],
                ClearableField::Location,
            ),
        ];
        for (markers, field) in field_markers {
            if let Some(pos) = markers.iter().filter_map(|m| lower.find(m)).min() {
                positioned.push((pos, *field));
            }
        }
        positioned.sort_by_key(|(pos, _)| *pos);
        clears.extend(positioned.into_iter().map(|(_, f)| f));
        // 兜底（v0.5 "不限制 X" / "清空上一轮位置约束" 默认 location；"clear the time" → time）
        if clears.is_empty() {
            clears.push(if lower.contains("time") || lower.contains("时间") {
                ClearableField::ModifiedTime
            } else {
                ClearableField::Location
            });
        }
    }

    // ---- 设新值：keep / replace / add / limit_to 都可能带入新约束 ----
    let set_value = keep_signal || replace_signal || add_signal || limit_to_signal;
    if set_value {
        // 类型匹配只看**设值标记之后**的文本：「把 ppt 也排除掉，只看视频」的设值对象是
        // 「视频」而非被排除的 ppt。标记前无类型词的 v0.5 形态（只看 pdf）不受影响。
        let set_scope = earliest_set_marker_end(lower).map_or(lower, |end| &lower[end..]);
        // 扩展名 / 文件类型（行为同 v0.5：format 词带 ext+ft，category 词仅 ft）
        if let Some(m) = match_extensions(set_scope) {
            if !m.extensions.is_empty() {
                delta.extensions = Some(m.extensions.iter().map(|s| (*s).to_string()).collect());
            }
            delta.file_type = Some(vec![m.file_type]);
        }
        if let Some(loc) = parse_location_with_language(lower, language) {
            delta.location = Some(loc);
        }
        // 时间（被 clear 标记的时间字段不再设新值）
        if !clears.contains(&ClearableField::ModifiedTime) {
            let (created, modified, accessed) = parse_time_fields(lower, input);
            if created.is_some() {
                delta.created_time = created;
            }
            if modified.is_some() {
                delta.modified_time = modified;
            }
            if accessed.is_some() {
                delta.accessed_time = accessed;
            }
        }
        // 大小（"再大一点的，超过 50MB" / "the bigger ones, over 50MB"）
        if let Some(size) = parse_size(lower) {
            delta.size = Some(size);
        }
        // 艺人（"只要周杰伦的" / "only the ones by Adele"）
        if let Some(artist) = extract_artist(input, lower) {
            delta.artist = Some(artist);
        }
        // refine 语境 artist 兜底：媒体结构词（的歌/songs by）在 refine 短句里通常省略，
        // 「只要周杰伦的」/「only the ones by Adele」。仅当其它字段全空时启用，
        // 避免把「只看下载目录里的」的位置词误作人名。
        if delta == RefineDelta::default() && clears.is_empty() {
            if let Some(artist) = refine_artist_fallback(input) {
                delta.artist = Some(artist);
            }
        }
    }

    if exclude_signal {
        if let Some(m) = match_extensions(lower) {
            delta.exclude_file_type = Some(vec![m.file_type]);
        }
    }

    // 排序词一旦出现在**已判定为 refine** 的查询里就附加 sort（「just the videos, sorted by
    // size」经 keep 触发后 sort 也要带上）；has_search_scope 门只挡"排序词作为唯一触发"，
    // 不挡已触发 refine 后的 sort 附加。
    if raw_sort {
        if lower.contains("按大小") || lower.contains("按 size") || lower.contains("by size") {
            delta.sort = Some(SortOrder::SizeDesc);
        } else if lower.contains("按名称") || lower.contains("按名字") || lower.contains("by name")
        {
            delta.sort = Some(SortOrder::NameAsc);
        }
    }
    // 「按修改时间倒序」等显式排序（仅在已是 refine 时附加，不作触发词）
    if lower.contains("按修改时间") {
        delta.sort = Some(SortOrder::ModifiedDesc);
    }

    let clear = if clears.is_empty() {
        None
    } else {
        Some(clears)
    };

    Some(SearchIntent::Refine(Refine {
        schema_version: SchemaVersion::V1,
        language: Some(language),
        base_ref: BaseRef::LastIntent,
        delta,
        clear,
    }))
}
