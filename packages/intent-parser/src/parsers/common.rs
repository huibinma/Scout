//! 多个 parser 共享的纯函数 helper。
//!
//! 行为不变（v0.3 收尾时的实现），从原 `lib.rs` 整体迁移而来。
#![allow(clippy::pedantic, clippy::while_let_on_iterator, clippy::expect_used)]

use scout_search_backend::{Language, Location, RelativeTime, TimeExpression};

use crate::lexicon;

// ============================================================
// Location（按语言保留 hint）
// ============================================================

/// 按 language 输出 hint。zh → zh_hint；en → en_hint；
/// **mixed → 按实际命中的 keyword 形式**（输入用 "downloads" 输 en_hint，用 "下载" 输 zh_hint）；
/// unknown → zh_hint（向后兼容）。
pub(crate) fn parse_location_with_language(lower: &str, language: Language) -> Option<Location> {
    for a in lexicon::LOCATION_ALIASES {
        for k in a.keywords {
            if word_present(lower, k) && !cjk_location_shadowed(lower, k) {
                // BETA-13-G15：英文歧义名词 documents/pictures 仅在带位置标记（in/里）时才作
                // location；否则是类型/复数名词（→ file_type，见 file_search 注入），跳过此命中。
                // 中文 alias（文稿/文档目录/图片目录…）无歧义，不受此门控影响。
                if (*k == "documents" || *k == "pictures")
                    && !en_ambiguous_noun_is_location(lower, k)
                {
                    continue;
                }
                let hint = match language {
                    Language::En => a.en_hint,
                    Language::Mixed => {
                        // v0.5：mixed 按实际命中 keyword 形式选 hint。
                        // 「music 目录」类 en 名 + 中文容器词的混排 keyword，名字部分是
                        // ascii → en_hint（d5-mixed-010 锚）；纯 ascii / 纯中文行为不变。
                        if alias_name_part_is_ascii(k) {
                            a.en_hint
                        } else {
                            a.zh_hint
                        }
                    }
                    _ => a.zh_hint,
                };
                return Some(Location {
                    hint: Some(hint.to_owned()),
                    include: None,
                    exclude: None,
                });
            }
        }
    }
    None
}

/// mixed 查询的 hint 形态判定：命中的 alias keyword 剥去中文容器尾词（目录/文件夹/夹）
/// 与空白后，名字部分为**非空纯 ASCII** → 用户用的是 en 目录名（「music 目录」→ music），
/// 输 en_hint。纯 ASCII keyword（downloads）与纯中文 keyword（下载）行为与旧
/// `k.is_ascii()` 判定完全一致，仅混排 keyword 走新分支。
fn alias_name_part_is_ascii(k: &str) -> bool {
    let name = k
        .trim_end_matches("目录")
        .trim_end_matches("文件夹")
        .trim_end_matches('夹')
        .trim();
    !name.is_empty() && name.is_ascii()
}

/// 中文 location 关键词若是查询中某个**更长且实际存在**的类型关键词的子串
/// （如 "文稿" ⊂ "演示文稿"=presentation），则该 location 命中是子串误匹配，应抑制。
///
/// 仅对非 ASCII（中文）location 关键词生效：ASCII 关键词由 [`word_present`] 的单词边界
/// 兜底，不会子串误匹配。`找文稿里的ppt` 中无更长含 "文稿" 的类型词存在 → 不抑制。
fn cjk_location_shadowed(lower: &str, k: &str) -> bool {
    if k.is_ascii() {
        return false;
    }
    lexicon::EXTENSION_ALIASES.iter().any(|a| {
        a.keywords
            .iter()
            .any(|t| !t.is_ascii() && t.len() > k.len() && t.contains(k) && lower.contains(t))
    })
}

// ============================================================
// Word boundary helpers
// ============================================================

/// 整词命中（避免 "doc" 匹配到 "docs"）；对中文直接 substring。
pub(crate) fn word_present(haystack: &str, needle: &str) -> bool {
    if needle.is_ascii() {
        // ASCII：用单词边界
        let bytes = haystack.as_bytes();
        let nb = needle.as_bytes();
        let mut start = 0;
        while let Some(pos) = haystack[start..].find(needle) {
            let abs = start + pos;
            let before_ok = abs == 0 || !is_word_char(bytes[abs - 1]);
            let after_ok = abs + nb.len() == bytes.len() || !is_word_char(bytes[abs + nb.len()]);
            if before_ok && after_ok {
                return true;
            }
            start = abs + nb.len();
        }
        false
    } else {
        haystack.contains(needle)
    }
}

pub(crate) fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// CJK 字符范围（含日文假名）。
pub(crate) fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x4E00..=0x9FFF
        | 0x3400..=0x4DBF
        | 0xF900..=0xFAFF
        | 0x3040..=0x309F
        | 0x30A0..=0x30FF
    )
}

// ============================================================
// 时间词解析
// ============================================================

/// BETA-13-G12 ②′：截图/截屏作**目录名**（截图目录 / 截图文件夹 / 截图夹）而非搜索目标。
/// 此时 截图 是 location hint，既不应触发 media 路由，也不应作 file_type=Screenshot。
/// media_search（路由）与 file_search（file_type 抑制）共用。v0.5 无此形态（0 条）。
pub(crate) fn screenshot_dir_is_location(lower: &str) -> bool {
    const DIR_FORMS: &[&str] = &[
        "截图目录",
        "截图文件夹",
        "截图夹",
        "截屏目录",
        "截屏文件夹",
        "截屏夹",
    ];
    DIR_FORMS.iter().any(|s| lower.contains(s))
}

/// BETA-13-G14 B2：「图片文件夹」= 图片所在文件夹（location，已抽为 hint=图片），
/// 故 图片 是文件夹名而非搜索的类型 → file_search 应抑制 file_type=Image（mirror
/// [`screenshot_dir_is_location`]）。仅匹配新 folder 形「图片文件夹」，不动既有「图片目录」
/// （v0.5 可能含、避免破 byte-equal）。
pub(crate) fn picture_dir_is_location(lower: &str) -> bool {
    lower.contains("图片文件夹")
}

/// BETA-13-G15：英文歧义名词 `documents`/`pictures` 是否为「位置义」（带显式位置标记）。
///
/// 严格——仅认实证形态：
/// - 前置介词 `in`：句首 `in <kw>` 或 ` in <kw>`（要求 in 前有空格/句首，排除
///   `within documents` 这类子串误命中）；允许限定词 `in the/my <kw>`
///   （「wallpapers in the pictures folder」，v09-d5-en-020；`within … folder` 仍不命中）；
/// - 后置「里」：`<kw>` 后（允许中间空格）紧跟 `里`（如 `Documents 里` / `documents里`）。
///
/// 其余位置（裸 / 句首 / 并列枚举 / 内容子句 / 尾置名词）= 类型义 → false。
/// 仅对 ascii 关键词 `documents` / `pictures` 调用；中文 alias 不经此门控。
pub(crate) fn en_ambiguous_noun_is_location(lower: &str, kw: &str) -> bool {
    for det in ["", "the ", "my "] {
        let in_kw = format!("in {det}{kw}");
        if lower.starts_with(&in_kw) || lower.contains(&format!(" {in_kw}")) {
            return true;
        }
    }
    let mut start = 0;
    while let Some(pos) = lower[start..].find(kw) {
        let abs = start + pos;
        if lower[abs + kw.len()..].trim_start().starts_with('里') {
            return true;
        }
        start = abs + kw.len();
    }
    false
}

/// 把识别到的时间表达式分到 modified / created / accessed 字段。
pub(crate) fn parse_time_fields(
    lower: &str,
    input: &str,
) -> (
    Option<TimeExpression>,
    Option<TimeExpression>,
    Option<TimeExpression>,
) {
    let expr = parse_time_expression(lower, input);
    let Some(expr) = expr else {
        return (None, None, None);
    };

    // BETA-13-G16：「opened」补入访问维度（对齐 decide_sort 的 accessed_dim）。
    // v0.5 仅有裸「open ...」file_action（无 "opened" 子串，且路由到 FileAction 不经此处）→ byte-equal 安全。
    if lower.contains("访问")
        || lower.contains("打开过")
        || lower.contains("opened")
        || lower.contains("accessed")
    {
        return (None, None, Some(expr));
    }
    // 注：「from last/from yesterday」曾在此映射 created——v0.5 该形态锚点全是 screenshot，
    // 而 screenshot 路径自身会把时间归 created（media_search.rs），非 screenshot 的
    // 「reports from last year」语义是修改时间 → 改走默认 modified（v0.9 d5/d7 标注约定）。
    if lower.contains("收到")
        || lower.contains("下载的")
        || lower.contains("创建")
        || lower.contains("created")
        || lower.contains("截的")
        || lower.contains("截了")
        || lower.contains("downloaded")
        // 「新增/新建/做的/做了」也是创建维度（v0.9 d5 分片；v0.5 零出现）。
        // 注意：这组词不入 decide_sort 的 created 翻转触发词（d5 锚点期望保持 modified_desc）。
        || lower.contains("新增")
        || lower.contains("新建")
        || lower.contains("做的")
        || lower.contains("做了")
    {
        return (Some(expr), None, None);
    }
    // 默认 modified
    (None, Some(expr), None)
}

pub(crate) fn parse_time_expression(lower: &str, input: &str) -> Option<TimeExpression> {
    // 绝对边界（之前/之后/区间）必须先于裸年份：「2026 年 5 月 1 日之前」含「2026 年」，
    // 先查 parse_year 会误吞成整年区间（v05-schema-11 实锚）。
    if let Some(expr) = parse_absolute_bounds(lower) {
        return Some(expr);
    }

    // 绝对范围："2025 年"
    if let Some(year) = parse_year(lower) {
        if let (Some(from), Some(to)) = (
            chrono::NaiveDate::from_ymd_opt(year, 1, 1),
            chrono::NaiveDate::from_ymd_opt(year, 12, 31),
        ) {
            return Some(TimeExpression::Absolute { from, to });
        }
    }

    // "X 年 Y 月 Z 日之前"
    if let Some(date) = parse_date_with_before(lower, input) {
        return Some(date);
    }

    // 相对时间词
    let relative = match () {
        // 「最近拍的」= 近期拍摄（v09-d4-zh-030 锚 last_7_days）。裸「最近」不映射
        // （v0.5「找最近的 X Y」锚点期望无时间过滤，仅作排序语义）。
        () if lower.contains("最近拍") => Some(RelativeTime::Last7Days),
        () if lower.contains("昨天") || word_present(lower, "yesterday") => {
            Some(RelativeTime::Yesterday)
        }
        () if lower.contains("今天") || word_present(lower, "today") => Some(RelativeTime::Today),
        () if lower.contains("最近三天")
            || lower.contains("过去三天")
            || lower.contains("过去 3 天")
            || lower.contains("最近 3 天")
            || lower.contains("past 3 days")
            || lower.contains("last 3 days")
            || lower.contains("last three days")
            || lower.contains("past three days") =>
        {
            Some(RelativeTime::Last3Days)
        }
        () if lower.contains("最近一周")
            || lower.contains("过去一周")
            || lower.contains("近一周")
            || lower.contains("一周内")
            || lower.contains("最近 7 天")
            || lower.contains("过去 7 天")
            || lower.contains("past 7 days")
            || lower.contains("last 7 days")
            || lower.contains("last seven days")
            || lower.contains("past seven days") =>
        {
            Some(RelativeTime::Last7Days)
        }
        () if lower.contains("最近两周")
            || lower.contains("过去两周")
            || lower.contains("past 14 days")
            || lower.contains("last 14 days")
            || lower.contains("last two weeks")
            || lower.contains("past two weeks")
            || lower.contains("last fourteen days") =>
        {
            Some(RelativeTime::Last14Days)
        }
        () if lower.contains("最近一个月")
            || lower.contains("过去一个月")
            || lower.contains("最近 30 天")
            || lower.contains("过去 30 天")
            || lower.contains("past 30 days")
            || lower.contains("last 30 days")
            || lower.contains("last thirty days")
            || lower.contains("past thirty days") =>
        {
            Some(RelativeTime::Last30Days)
        }
        // 「这周/这个月」与「本周/本月」同义（v0.9 d5 分片；v0.5 零出现）。
        () if lower.contains("本周") || lower.contains("这周") || lower.contains("this week") => {
            Some(RelativeTime::ThisWeek)
        }
        () if lower.contains("上周") || lower.contains("last week") => {
            Some(RelativeTime::LastWeek)
        }
        () if lower.contains("本月")
            || lower.contains("这个月")
            || lower.contains("this month") =>
        {
            Some(RelativeTime::ThisMonth)
        }
        () if lower.contains("上个月") || lower.contains("last month") => {
            Some(RelativeTime::LastMonth)
        }
        () if lower.contains("今年") || lower.contains("this year") => {
            Some(RelativeTime::ThisYear)
        }
        () if lower.contains("去年") || lower.contains("last year") => {
            Some(RelativeTime::LastYear)
        }
        () => None,
    };
    if let Some(value) = relative {
        return Some(TimeExpression::Relative { value });
    }

    // 裸月份兜底："3 月"/"3 月份"（无年份、无具体日/之前）→ 该月绝对区间。
    parse_month_only(lower)
}

/// 解析裸月份「3月」「3 月份」为该月的绝对日期区间 `[当月1日, 当月末日]`。
///
/// 仅处理**不含年份**的裸月份；含「年」的表达交由 [`parse_year`] /
/// [`parse_date_with_before`] 处理。年份用「最近一次出现」启发：若该月晚于当前月
/// （今年尚未到），取去年；否则取今年。
fn parse_month_only(lower: &str) -> Option<TimeExpression> {
    use regex::Regex;
    use std::sync::OnceLock;

    // 含年份的表达不在此处理，避免与年份/绝对日期分支冲突。
    if lower.contains('年') {
        return None;
    }

    static RE: OnceLock<Regex> = OnceLock::new();
    // (?:^|[^0-9]) 确保月份数字不是更大数字的一部分（如 "13月" 不取 "3月"）。
    let re = RE.get_or_init(|| Regex::new(r"(?:^|[^0-9])([1-9]|1[0-2])\s*月份?").expect("regex"));
    let month: u32 = re.captures(lower)?[1].parse().ok()?;

    let year = guess_year_for_month(month);
    let from = chrono::NaiveDate::from_ymd_opt(year, month, 1)?;
    let to = last_day_of_month(year, month)?;
    Some(TimeExpression::Absolute { from, to })
}

/// 无年份月份的年份启发：若该月晚于当前月（今年尚未到），取去年；否则取今年。
fn guess_year_for_month(month: u32) -> i32 {
    use chrono::Datelike;
    let today = chrono::Local::now().date_naive();
    if month > today.month() {
        today.year() - 1
    } else {
        today.year()
    }
}

/// 英文月份名 → 月序。
fn en_month_number(s: &str) -> Option<u32> {
    match s {
        "january" => Some(1),
        "february" => Some(2),
        "march" => Some(3),
        "april" => Some(4),
        "may" => Some(5),
        "june" => Some(6),
        "july" => Some(7),
        "august" => Some(8),
        "september" => Some(9),
        "october" => Some(10),
        "november" => Some(11),
        "december" => Some(12),
        _ => None,
    }
}

/// 月份 token（数字 / 汉字数词 / 英文数词，如 "5" / "五" / "five"）→ 月序。
fn month_token_number(s: &str) -> Option<u32> {
    if let Ok(n) = s.parse::<u32>() {
        return (1..=12).contains(&n).then_some(n);
    }
    match s {
        "一" | "one" => Some(1),
        "二" | "two" => Some(2),
        "三" | "three" => Some(3),
        "四" | "four" => Some(4),
        "五" | "five" => Some(5),
        "六" | "six" => Some(6),
        "七" | "seven" => Some(7),
        "八" | "eight" => Some(8),
        "九" | "nine" => Some(9),
        "十" | "ten" => Some(10),
        "十一" | "eleven" => Some(11),
        "十二" | "twelve" => Some(12),
        _ => None,
    }
}

/// 绝对时间边界解析：「之前/以前」→ [`TimeExpression::Before`]、「之后/以后」→
/// [`TimeExpression::After`]、「X 到 Y（之间）」→ [`TimeExpression::Absolute`] 区间。
///
/// 覆盖形态（v0.9 d5 分片锚点；v0.5 无 之后/区间/英文月名 表达，仅 v05-schema-11
/// 一条完整年月日之前）：
/// - 中文：`2026年5月1日之前` / `2026年1月之前`（无日 → 当月 1 日）/ `五月之后` /
///   `5月20号之后` / `5月20到24号之间`；
/// - 英文：`before January 2026` / `after May 1st` / `between May 20 and May 24`；
/// - 混排：`before 2026年1月` / `five月之后`。
///
/// 无年份时沿用 [`guess_year_for_month`] 的「最近一次出现」启发；「X月之前/之后」的
/// 边界值取当月 1 日（对齐 d5 标注约定，如 五月之后 → after 2026-05-01）。
fn parse_absolute_bounds(lower: &str) -> Option<TimeExpression> {
    use regex::Regex;
    use std::sync::OnceLock;

    const MONTHS: &str =
        "january|february|march|april|may|june|july|august|september|october|november|december";

    let bound = |is_before: bool, y: i32, m: u32, d: u32| -> Option<TimeExpression> {
        let value = chrono::NaiveDate::from_ymd_opt(y, m, d)?;
        Some(if is_before {
            TimeExpression::Before { value }
        } else {
            TimeExpression::After { value }
        })
    };

    // ① 完整中文日期 + 之前/之后：「2026年5月1日之前」「2026年5月20号之后」。
    static RE_FULL: OnceLock<Regex> = OnceLock::new();
    let re_full = RE_FULL.get_or_init(|| {
        Regex::new(r"(\d{4})\s*年\s*(\d{1,2})\s*月\s*(\d{1,2})\s*[日号]?\s*(之前|以前|之后|以后)")
            .expect("regex")
    });
    if let Some(cap) = re_full.captures(lower) {
        let is_before = matches!(&cap[4], "之前" | "以前");
        return bound(
            is_before,
            cap[1].parse().ok()?,
            cap[2].parse().ok()?,
            cap[3].parse().ok()?,
        );
    }

    // ② 英文/混排 before|after 前缀 + 中文年月（日可选）：「before 2026年1月」。
    static RE_EN_ZH: OnceLock<Regex> = OnceLock::new();
    let re_en_zh = RE_EN_ZH.get_or_init(|| {
        Regex::new(r"(before|after)\s+(\d{4})\s*年\s*(\d{1,2})\s*月(?:\s*(\d{1,2})\s*[日号]?)?")
            .expect("regex")
    });
    if let Some(cap) = re_en_zh.captures(lower) {
        let is_before = &cap[1] == "before";
        let d: u32 = cap.get(4).map_or(Some(1), |m| m.as_str().parse().ok())?;
        return bound(is_before, cap[2].parse().ok()?, cap[3].parse().ok()?, d);
    }

    // ③ 中文年月（无日）+ 之前/之后：「2026年1月之前」→ 当月 1 日为界。
    static RE_YM: OnceLock<Regex> = OnceLock::new();
    let re_ym = RE_YM.get_or_init(|| {
        Regex::new(r"(\d{4})\s*年\s*(\d{1,2})\s*月(?:份)?\s*(之前|以前|之后|以后)").expect("regex")
    });
    if let Some(cap) = re_ym.captures(lower) {
        let is_before = matches!(&cap[3], "之前" | "以前");
        return bound(is_before, cap[1].parse().ok()?, cap[2].parse().ok()?, 1);
    }

    // ④ 英文月名 + 年：「before January 2026」。
    static RE_EN_MY: OnceLock<Regex> = OnceLock::new();
    let re_en_my = RE_EN_MY.get_or_init(|| {
        Regex::new(&format!(r"(before|after)\s+({MONTHS})\s+(\d{{4}})")).expect("regex")
    });
    if let Some(cap) = re_en_my.captures(lower) {
        let is_before = &cap[1] == "before";
        return bound(
            is_before,
            cap[3].parse().ok()?,
            en_month_number(&cap[2])?,
            1,
        );
    }

    // ⑤ 英文月名 + 日（年可选）：「after May 1st」「before May 20, 2026」。
    static RE_EN_MD: OnceLock<Regex> = OnceLock::new();
    let re_en_md = RE_EN_MD.get_or_init(|| {
        Regex::new(&format!(
            r"(before|after)\s+({MONTHS})\s+(\d{{1,2}})(?:st|nd|rd|th)?(?:\s*,?\s+(\d{{4}}))?"
        ))
        .expect("regex")
    });
    if let Some(cap) = re_en_md.captures(lower) {
        let is_before = &cap[1] == "before";
        let m = en_month_number(&cap[2])?;
        let y: i32 = cap.get(4).map_or_else(
            || Some(guess_year_for_month(m)),
            |v| v.as_str().parse().ok(),
        )?;
        return bound(is_before, y, m, cap[3].parse().ok()?);
    }

    // ⑥ 英文区间：「between May 20 and May 24」。
    static RE_EN_RANGE: OnceLock<Regex> = OnceLock::new();
    let re_en_range = RE_EN_RANGE.get_or_init(|| {
        Regex::new(&format!(
            r"between\s+({MONTHS})\s+(\d{{1,2}})(?:st|nd|rd|th)?\s+and\s+(?:({MONTHS})\s+)?(\d{{1,2}})(?:st|nd|rd|th)?"
        ))
        .expect("regex")
    });
    if let Some(cap) = re_en_range.captures(lower) {
        let m1 = en_month_number(&cap[1])?;
        let m2 = cap
            .get(3)
            .map_or(Some(m1), |v| en_month_number(v.as_str()))?;
        let y = guess_year_for_month(m1);
        let from = chrono::NaiveDate::from_ymd_opt(y, m1, cap[2].parse().ok()?)?;
        let to = chrono::NaiveDate::from_ymd_opt(y, m2, cap[4].parse().ok()?)?;
        return Some(TimeExpression::Absolute { from, to });
    }

    // ⑦ 中文区间：「5月20到24号之间」「5月20日至6月3号」。
    static RE_ZH_RANGE: OnceLock<Regex> = OnceLock::new();
    let re_zh_range = RE_ZH_RANGE.get_or_init(|| {
        Regex::new(
            r"(\d{1,2})\s*月\s*(\d{1,2})\s*[日号]?\s*(?:到|至)\s*(?:(\d{1,2})\s*月\s*)?(\d{1,2})\s*[日号]?",
        )
        .expect("regex")
    });
    if let Some(cap) = re_zh_range.captures(lower) {
        let m1: u32 = cap[1].parse().ok()?;
        let m2: u32 = cap.get(3).map_or(Some(m1), |v| v.as_str().parse().ok())?;
        let y = guess_year_for_month(m1);
        let from = chrono::NaiveDate::from_ymd_opt(y, m1, cap[2].parse().ok()?)?;
        let to = chrono::NaiveDate::from_ymd_opt(y, m2, cap[4].parse().ok()?)?;
        return Some(TimeExpression::Absolute { from, to });
    }

    // ⑧ 月份 + 日（无年）+ 之前/之后：「5月20号之后」。
    static RE_MD: OnceLock<Regex> = OnceLock::new();
    let re_md = RE_MD.get_or_init(|| {
        Regex::new(r"(\d{1,2})\s*月\s*(\d{1,2})\s*[日号]\s*(之前|以前|之后|以后)").expect("regex")
    });
    if let Some(cap) = re_md.captures(lower) {
        let is_before = matches!(&cap[3], "之前" | "以前");
        let m: u32 = cap[1].parse().ok()?;
        return bound(is_before, guess_year_for_month(m), m, cap[2].parse().ok()?);
    }

    // ⑨ 裸月份 + 之前/之后（数字 / 汉字数词 / 英文数词）：「五月之后」「five月之后」。
    //    汉字数词紧贴「月」（「上个月/一个月」的「个」不在数词类内，不会误命中）。
    static RE_M: OnceLock<Regex> = OnceLock::new();
    let re_m = RE_M.get_or_init(|| {
        Regex::new(
            r"(\d{1,2}|十一|十二|十|一|二|三|四|五|六|七|八|九|one|two|three|four|five|six|seven|eight|nine|ten|eleven|twelve)\s*月\s*(之前|以前|之后|以后)",
        )
        .expect("regex")
    });
    if let Some(cap) = re_m.captures(lower) {
        let is_before = matches!(&cap[2], "之前" | "以前");
        let m = month_token_number(&cap[1])?;
        return bound(is_before, guess_year_for_month(m), m, 1);
    }

    None
}

/// 返回 `year` 年 `month` 月的最后一天。
fn last_day_of_month(year: i32, month: u32) -> Option<chrono::NaiveDate> {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    chrono::NaiveDate::from_ymd_opt(next_year, next_month, 1)?.pred_opt()
}

pub(crate) fn parse_year(lower: &str) -> Option<i32> {
    use regex::Regex;
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(20\d{2})\s*年").expect("regex"));
    let cap = re.captures(lower)?;
    cap[1].parse().ok()
}

pub(crate) fn parse_date_with_before(lower: &str, _input: &str) -> Option<TimeExpression> {
    use regex::Regex;
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(\d{4})\s*年\s*(\d{1,2})\s*月\s*(\d{1,2})\s*日?\s*之前").expect("regex")
    });
    let cap = re.captures(lower)?;
    let y: i32 = cap[1].parse().ok()?;
    let m: u32 = cap[2].parse().ok()?;
    let d: u32 = cap[3].parse().ok()?;
    let value = chrono::NaiveDate::from_ymd_opt(y, m, d)?;
    Some(TimeExpression::Before { value })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use chrono::Datelike;

    #[test]
    fn bare_month_parses_to_absolute_month_range() {
        let expr = parse_time_expression("3月份编辑的pdf", "3月份编辑的PDF").unwrap();
        match expr {
            TimeExpression::Absolute { from, to } => {
                assert_eq!((from.month(), from.day()), (3, 1));
                assert_eq!((to.month(), to.day()), (3, 31));
                assert_eq!(from.year(), to.year());
            }
            other => panic!("应为 3 月绝对区间, 实得 {other:?}"),
        }
    }

    #[test]
    fn bare_month_february_last_day_valid() {
        let expr = parse_time_expression("2月的文件", "2月的文件").unwrap();
        match expr {
            TimeExpression::Absolute { from, to } => {
                assert_eq!(from.month(), 2);
                assert_eq!(to.month(), 2);
                assert!(
                    matches!(to.day(), 28 | 29),
                    "2 月末日应为 28/29, 实得 {}",
                    to.day()
                );
            }
            other => panic!("应为 2 月绝对区间, 实得 {other:?}"),
        }
    }

    #[test]
    fn year_only_still_whole_year_not_bare_month() {
        let expr = parse_time_expression("2025年的报告", "2025年的报告").unwrap();
        match expr {
            TimeExpression::Absolute { from, to } => {
                assert_eq!((from.year(), from.month(), from.day()), (2025, 1, 1));
                assert_eq!((to.year(), to.month(), to.day()), (2025, 12, 31));
            }
            other => panic!("含年份应为整年区间, 实得 {other:?}"),
        }
    }

    #[test]
    fn invalid_month_thirteen_not_parsed() {
        assert!(parse_time_expression("13月的东西", "13月的东西").is_none());
    }

    #[test]
    fn relative_last_month_takes_precedence_over_bare_month() {
        let expr = parse_time_expression("上个月的pdf", "上个月的PDF").unwrap();
        assert!(matches!(
            expr,
            TimeExpression::Relative {
                value: RelativeTime::LastMonth
            }
        ));
    }

    #[test]
    fn g15_en_ambiguous_noun_location_marker() {
        use super::en_ambiguous_noun_is_location as is_loc;
        // 位置义：前置 in / 后置 里
        assert!(is_loc("find ppt over 100mb in documents", "documents"));
        assert!(is_loc("in documents", "documents"));
        assert!(is_loc("find documents 里的 ppt", "documents"));
        assert!(is_loc("find documents里的 ppt", "documents"));
        assert!(is_loc("find photos in pictures", "pictures"));
        // 类型义：裸 / 句首 / 并列 / 内容子句 / 尾置
        assert!(!is_loc("documents and images", "documents"));
        assert!(!is_loc(
            "documents that mention quarterly revenue",
            "documents"
        ));
        assert!(!is_loc("code files and documents", "documents"));
        assert!(!is_loc("我昨天 opened 的 documents", "documents"));
        assert!(!is_loc("png and jpg pictures", "pictures"));
        // 不被 "within" 误命中（"within documents" 不含独立 " in documents"）
        assert!(!is_loc("files within documents folder", "documents"));
    }

    #[test]
    fn absolute_bounds_before_after_range() {
        // 时间表达簇（2026-07-04）：之前/之后/区间的绝对边界解析。
        let y5 = super::guess_year_for_month(5);
        let d = |m: u32, day: u32| chrono::NaiveDate::from_ymd_opt(y5, m, day).unwrap();
        let before = |v| TimeExpression::Before { value: v };
        let after = |v| TimeExpression::After { value: v };
        let cases: &[(&str, TimeExpression)] = &[
            // 完整年月日之前（v05-schema-11：先于 parse_year，不得吞成整年区间）
            (
                "找 2026 年 5 月 1 日之前修改的 zip",
                before(chrono::NaiveDate::from_ymd_opt(2026, 5, 1).unwrap()),
            ),
            // 年月（无日）之前 → 当月 1 日为界
            (
                "2026年1月之前创建的合同",
                before(chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()),
            ),
            // 混排 before + 中文年月
            (
                "before 2026年1月 创建的合同",
                before(chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()),
            ),
            // 英文月名 + 年
            (
                "contracts created before january 2026",
                before(chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()),
            ),
            // 英文月名 + 序数日（无年 → 最近一次出现启发）
            ("files edited after may 1st", after(d(5, 1))),
            // 汉字数词月 + 之后
            ("五月之后改的设计稿", after(d(5, 1))),
            // 英文数词月 + 之后（混排）
            ("five月之后 modified 的设计稿", after(d(5, 1))),
        ];
        for (q, expected) in cases {
            let got = parse_time_expression(&q.to_lowercase(), q);
            assert_eq!(got.as_ref(), Some(expected), "query: {q}");
        }
        // 区间：中文「5月20到24号之间」/ 英文 between
        for q in [
            "5月20到24号之间改过的文件",
            "files modified between may 20 and may 24",
        ] {
            let got = parse_time_expression(&q.to_lowercase(), q);
            assert_eq!(
                got,
                Some(TimeExpression::Absolute {
                    from: d(5, 20),
                    to: d(5, 24)
                }),
                "query: {q}"
            );
        }
    }

    #[test]
    fn zhezhou_zhegeyue_zuijinpai_relative_forms() {
        // 「这周/这个月」同义于「本周/本月」；「最近拍」→ last_7_days；裸「最近的」不映射。
        let rel = |v| Some(TimeExpression::Relative { value: v });
        assert_eq!(
            parse_time_expression("这周改的表格", "这周改的表格"),
            rel(RelativeTime::ThisWeek)
        );
        assert_eq!(
            parse_time_expression("这个月新增的 pdf", "这个月新增的 PDF"),
            rel(RelativeTime::ThisMonth)
        );
        assert_eq!(
            parse_time_expression("找最近拍的视频", "找最近拍的视频"),
            rel(RelativeTime::Last7Days)
        );
        // v0.5 保护：「找最近的 X Y」无时间过滤（最近仅是排序语义）
        assert_eq!(
            parse_time_expression("找最近的 synthetic-plan ppt", "找最近的 synthetic-plan ppt"),
            None
        );
    }

    #[test]
    fn xinzeng_zuode_map_created_dimension() {
        // 「新增/做的」映射 created 维度（d5-zh-006/008）。
        for q in ["这个月新增的 pdf", "今年做的演示文稿"] {
            let (created, modified, accessed) = super::parse_time_fields(q, q);
            assert!(created.is_some(), "{q} 应有 created_time");
            assert_eq!(modified, None, "{q}");
            assert_eq!(accessed, None, "{q}");
        }
    }

    #[test]
    fn g16_opened_maps_accessed_time() {
        // BETA-13-G16 刀3：「opened」时间维度 → accessed_time（对齐 decide_sort 的 accessed_dim）。
        // v0.5 仅有裸「open ...」file_action（无 "opened" 子串、且不经此函数）→ byte-equal 安全。
        let q = "我昨天 opened 的 documents";
        let (created, modified, accessed) = super::parse_time_fields(q, q);
        assert_eq!(created, None);
        assert_eq!(modified, None);
        assert_eq!(
            accessed,
            Some(TimeExpression::Relative {
                value: RelativeTime::Yesterday
            })
        );
    }
}
