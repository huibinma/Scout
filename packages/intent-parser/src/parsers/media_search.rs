//! `SearchIntent::MediaSearch` 规则解析器（v0.4 主战场）。
#![allow(clippy::pedantic, clippy::expect_used, clippy::while_let_on_iterator)]

use scout_search_backend::{
    Language, MediaSearch, MediaType, Quality, SchemaVersion, SearchIntent, SortOrder,
};

use crate::lexicon;

pub(crate) use super::artist::{contains_known_artist, extract_artist};
use super::artist::{has_free_artist_structure, is_stopword_artist};
use super::common::{parse_location_with_language, parse_time_fields, screenshot_dir_is_location};
use super::file_search::{parse_duration, parse_size};

pub(crate) fn is_media_query(lower: &str) -> bool {
    // BETA-13-G：截图 + 内容子句 → 按内容搜，交 file_search。仅截图分支；
    // 纯「截图+时间/size」无内容子句不受影响（保持 v0.5）。
    if has_screenshot_word(lower) && detect_content_clause(lower).is_some() {
        return false;
    }
    // BETA-13-G12 ②′：「截图目录/截图文件夹/截图夹」= 位置名（截图夹），不是搜截图本身 →
    // 交 file_search（location=截图 + 其它类型词，如 file_type=image）。v0.5 无此形态（0 条）。
    if screenshot_dir_is_location(lower) {
        return false;
    }
    // 跨范畴多类型（≥2 个不同 file_type 类别 + **显式连词** + 无 artist）→ 交 file_search
    // （file_type 多值 + BETA-19 均衡展示），避免 media 单值 media_type 丢一类。
    // 推广「最大的图片和视频」到强媒体词跨范畴（「音乐和视频」「截图和视频」）。
    // **显式连词门**避开「音乐视频」(music video / MV，单概念) 误判。带 artist 的仍走 media。
    if has_cross_category_media_conjunction(lower) && !contains_known_artist(lower) {
        return false;
    }
    if has_strong_media_signal(lower) || contains_known_artist(lower) {
        return true;
    }
    // BETA-13-G2：音频 metadata 信号（play 动词 / 专辑 / 流派 / 自由 artist / 标题结构）→ media。
    if has_audio_metadata_signal(lower) {
        return true;
    }
    // Bucket C: 视频/图片 类型词 + 抽象 sort/time 修饰 → media_search
    // （但具体 size 阈值如 "100MB" 仍走 file_search file_type=video）
    has_visual_media_with_abstract_modifier(lower)
}

/// BETA-13-G：检测「内容子句」——用户按内容/正文文字搜（非按文件名/类型）。
/// 命中返回干净内容短语（剥除子句引导词、容器尾巴）。
///
/// 另有一个更广的**存在性谓词**
/// [`file_search::has_content_clause_signal`](super::file_search)（词集更宽、仅判 true/false、
/// 不抽取，用于 gating 文档类尾名词 → Document）；二者有意分立，详见该函数文档。
pub(crate) fn detect_content_clause(input: &str) -> Option<String> {
    use regex::Regex;
    use std::sync::OnceLock;
    static RE_ZH: OnceLock<Regex> = OnceLock::new();
    let re_zh = RE_ZH.get_or_init(|| {
        Regex::new(r"(?:里写着|里写了|写着|写了|里提到|提到|里面有|里面提到|同时出现|同时包含)\s*([^，。,]+?)(?:的那张|的那个|的报错|的提示|的错误提示|的)?$")
            .expect("content clause zh regex")
    });
    if let Some(cap) = re_zh.captures(input.trim()) {
        let s = cap[1].trim();
        // 退化保护：纯停用词（如「截图里写着的」抽出的「的/了」）不算内容子句。
        if !s.is_empty() && !is_degenerate_zh_phrase(s) {
            return Some(s.to_owned());
        }
    }
    static RE_EN: OnceLock<Regex> = OnceLock::new();
    let re_en = RE_EN.get_or_init(|| {
        // 锚定 `that` 前缀：避免裸 says/mentions/shows 把祈使句（如
        // "show me all screenshots" / "show screenshots sorted by size"）误判为内容子句。
        Regex::new(r"(?i)(?:that\s+(?:says|mentions?|shows?)|with both)\s+(.+?)\s*$")
            .expect("content clause en regex")
    });
    if let Some(cap) = re_en.captures(input.trim()) {
        let s = cap[1].trim();
        if !s.is_empty() {
            return Some(s.to_owned());
        }
    }
    None
}

/// BETA-13-G follow-up：内容子句是否含「both/同时」语义（多对象并列）。
/// 仅此类才把内容按 和/and 拆多关键词；常规内容子句保持单关键词。
pub(crate) fn content_clause_is_multi(input: &str) -> bool {
    let lower = input.to_lowercase();
    input.contains("同时出现") || input.contains("同时包含") || lower.contains("with both")
}

/// BETA-13-G follow-up：把 both/同时 内容子句的短语按并列连词拆成多关键词。
/// 分隔符：中文 和 / 顿号、，英文 " and "（带空格避免切断 brand/order id 内部）。
/// trim + 过滤空段；全空（病态输入如「同时出现和」）则回退该短语本身的 trim 单元素。
pub(crate) fn split_content_clause(phrase: &str) -> Vec<String> {
    let parts: Vec<String> = phrase
        .split(['和', '、'])
        .flat_map(|p| p.split(" and "))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        let t = phrase.trim();
        if t.is_empty() {
            Vec::new()
        } else {
            vec![t.to_string()]
        }
    } else {
        parts
    }
}

/// 中文内容短语退化判定：trim 后为空，或仅由纯停用词（的/了/吗等）组成 → 视为无内容。
fn is_degenerate_zh_phrase(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return true;
    }
    t.chars()
        .all(|c| matches!(c, '的' | '了' | '吗' | '呢' | '啊'))
}

/// 截图词快速路由表（routing fast-path）。这是与 lexicon.rs:150 截图 alias 对齐的有意拷贝，
/// 用于 `is_media_query` 早退判定，避免为单点路由判断走完整 lexicon 匹配。
pub(crate) fn has_screenshot_word(lower: &str) -> bool {
    ["截图", "截屏", "screenshot", "screenshots"]
        .iter()
        .any(|w| lower.contains(w))
}

/// BETA-13-G2：音频 metadata 路由信号（播放动词 / 专辑 / 流派 / 自由 artist 结构 / 标题结构）。
/// 保守门控：英文短流派词需 music/song(s)/some 上下文；artist 仅认 "songs/tracks by X" 与
/// 中文 "X的歌/X唱的"，避开 "sorted by name" 等误判。
fn has_audio_metadata_signal(lower: &str) -> bool {
    const PLAY: &[&str] = &[
        "放首",
        "播放",
        "放一首",
        "来一首",
        "来点",
        "放点",
        "play the song",
        "play songs",
        "play some",
    ];
    if PLAY.iter().any(|p| lower.contains(p)) {
        return true;
    }
    if lower.contains("专辑") || lower.contains("the album") || lower.contains("album ") {
        return true;
    }
    // 流派
    let zh_genre = [
        "爵士",
        "摇滚",
        "古典",
        "民谣",
        "轻音乐",
        "说唱",
        "嘻哈",
        "电子乐",
        "蓝调",
        "乡村音乐",
        "金属乐",
    ]
    .iter()
    .any(|g| lower.contains(g));
    if zh_genre {
        return true;
    }
    if detect_genre(lower).is_some()
        && ["music", "songs", "song", "some ", "tracks", "track"]
            .iter()
            .any(|c| lower.contains(c))
    {
        return true;
    }
    // 自由 artist 结构（中文 X的歌/X唱的；英文 songs/tracks by X）
    if has_free_artist_structure(lower) {
        return true;
    }
    // 标题结构 《X》 / a song called / the song
    if lower.contains('《')
        || lower.contains("the song")
        || lower.contains("a song called")
        || (lower.contains('叫') && lower.contains("的歌"))
    {
        return true;
    }
    false
}

/// 查询是否含**显式连词** + 跨越 ≥2 个媒体类别（audio / image(含 screenshot) / video）。
/// 用于把「音乐和视频」「截图和视频」「图片或视频」这类跨范畴查询交给 file_search
/// （file_type 多值 + BETA-19 均衡）。连词门避开「音乐视频」(MV) 等单概念误判。
fn has_cross_category_media_conjunction(lower: &str) -> bool {
    // BETA-13 决策 A：连词补「、」(中文枚举逗号) 与「跟」(口语「和」)，
    // 覆盖「图片、视频跟音乐」这类多范畴枚举。
    const CONJ: &[&str] = &["和", "跟", "与", "及", "或", "、", " and ", " or "];
    if !CONJ.iter().any(|c| lower.contains(c)) {
        return false;
    }
    const AUDIO: &[&str] = &[
        "音乐", "音频", "歌曲", "歌", "music", "audio", "song", "录音",
    ];
    const VIDEO: &[&str] = &["视频", "videos", "video", "影片", "movies", "录像"];
    // 决策 A：补单数「image」(原仅 images)；截图独立成类，使「图片和截图」算跨范畴
    // (image≠screenshot，file_search 产 file_type=[image,screenshot])。
    const IMAGE: &[&str] = &["图片", "image", "images", "pictures"];
    const SCREENSHOT: &[&str] = &["截图", "截屏", "screenshots", "screenshot"];
    let categories = [AUDIO, VIDEO, IMAGE, SCREENSHOT]
        .iter()
        .filter(|cat| cat.iter().any(|w| lower.contains(w)))
        .count();
    categories >= 2
}

/// 视频/图片 类型词的路由判定（v0.5 局部修订 v0.4）：
///
/// - **具体 size 阈值（"100MB" / "1GB"）→ media_search**（v0.5 fixture 统计契约：22 MS > 11 FS）
/// - **抽象 sort 词（最大/最重/largest/newest）→ media_search**（沿用 v0.4 + 加 "最重"）
/// - **时间词（修改/上周/modified/last week）→ media_search**（沿用 v0.4：fixture 25 MS > 10 FS）
///
/// v0.4 唯一选错的方向是"具体 size → file_search"；v0.5 全集 fixture 数据（500 case）显示
/// 该维度契约相反，反转之。其余维度保持 v0.4 决策。剩余 "video + (size|time)" 的少数
/// outlier（fixture 内部模板生成不一致）为已知遗留，不再规划专门修复。
fn has_visual_media_with_abstract_modifier(lower: &str) -> bool {
    const VISUAL_MEDIA: &[&str] = &[
        "视频", "video", "videos", "影片", "movies", "图片", "images", "pictures",
    ];
    if !VISUAL_MEDIA.iter().any(|w| lower.contains(w)) {
        return false;
    }
    // BETA-19 后续：跨范畴视觉媒体（同时含图片词 + 视频词，如「最大的图片和视频」）无任何
    // 音频专属语义（artist/album/duration），本质是 file_type=[Image,Video] + sort/size。
    // 交 file_search 处理（merge_extensions 产多值 file_type + decide_sort 接住 size/排序词），
    // 再复用 BETA-19 均衡分支 round-robin 展示两类型——避免 media 路径单值 media_type 丢一类。
    // 带 artist/强媒体词的查询在 is_media_query 上游已先行命中 media，不受影响。
    if has_cross_category_visual_media(lower) {
        return false;
    }
    // BETA-13-G12 ②′：image 单独（无 video 类词）+ 约束 → 交 file_search（file_type=image）。
    // §1.1 决策：video/screenshot+size/time 留 media；image 是 carve-out → file（v0.5 唯一
    // image 锚点 `find images on desktop` 即 file_search、coverage 0 条 image→media）。
    {
        const IMAGE_WORDS: &[&str] = &["图片", "images", "pictures"];
        const VIDEO_WORDS: &[&str] = &["视频", "video", "videos", "影片", "movies"];
        let image_only = IMAGE_WORDS.iter().any(|w| lower.contains(w))
            && !VIDEO_WORDS.iter().any(|w| lower.contains(w));
        if image_only {
            return false;
        }
    }
    // BETA-13-G12 决策 E（§3.2）：数量/程度修饰词（几个/some/短…）+ 视觉媒体 → media_search。
    // 这类修饰不携带 size/time 维度，但语义仍是「浏览这类媒体」（coverage v09-d2-zh-029/
    // zh-032/en-024）；不应被弱信号规则降级到 file_search。
    if has_quantity_degree_modifier(lower) {
        return true;
    }
    // v0.5 反转：具体 size 阈值 → media_search
    if has_explicit_size_threshold(lower) {
        return true;
    }
    // v0.4 + v0.5：抽象 sort 词（最大/最重/largest/newest 等）→ media_search
    if has_size_sort_signal(lower) {
        return true;
    }
    // v0.4：时间词（修改/上周/modified/last week）→ media_search
    const TIME_MODIFIERS: &[&str] = &[
        "修改",
        "modified",
        "edited",
        "created",
        "本周",
        "上周",
        "本月",
        "上个月",
        "一周内",
        "this week",
        "last week",
        "this month",
        "last month",
        "昨天",
        "今天",
        "前天",
        "yesterday",
        "today",
        "最近",
        "近一周",
        "过去一周",
        "past 7 days",
    ];
    TIME_MODIFIERS.iter().any(|w| lower.contains(w))
}

/// 是否同时含**图片类**词与**视频类**词（跨范畴视觉媒体）。两类都在 file_search 的
/// 扩展名词典里映射到 `file_type`（图片→Image、视频→Video），故交 file_search 能保留两类型。
fn has_cross_category_visual_media(lower: &str) -> bool {
    const IMAGE_WORDS: &[&str] = &["图片", "images", "pictures"];
    const VIDEO_WORDS: &[&str] = &["视频", "video", "videos", "影片", "movies"];
    IMAGE_WORDS.iter().any(|w| lower.contains(w)) && VIDEO_WORDS.iter().any(|w| lower.contains(w))
}

/// BETA-13-G12 决策 E（§3.2）：数量/程度修饰词。与视觉媒体词（已在调用处 gating）组合时，
/// 表「浏览这类媒体」而非「带 size/time 约束的文件搜索」，故路由到 media_search。
/// 仅在 `has_visual_media_with_abstract_modifier` 内（确认含视觉媒体词后）调用，避免裸触发。
fn has_quantity_degree_modifier(lower: &str) -> bool {
    const MODIFIERS: &[&str] = &[
        "几个", "几张", "几段", "一些", "若干", "某些", "短", "some", "a few", "short",
    ];
    MODIFIERS.iter().any(|w| lower.contains(w))
}

/// v0.5：检测含 size / recency 维度的 sort 词（"最重 / 最大 / largest / newest"）。
pub(crate) fn has_size_sort_signal(lower: &str) -> bool {
    const SIZE_SORT_WORDS: &[&str] = &[
        "最大",
        "最小",
        "最重",
        "最沉",
        "最新",
        "最旧",
        "biggest",
        "largest",
        "smallest",
        "newest",
        "oldest",
        "体积最大",
        "大文件",
        // v0.5：抽象 size mention 无具体数字
        "几个 g",
        "几个 m",
        "几 g",
        "几 m",
    ];
    SIZE_SORT_WORDS.iter().any(|w| lower.contains(w))
}

/// v0.5：检测 "最重 / 最大 / biggest / largest" 这类 **size** 排序词（不含 newest/oldest）。
fn has_size_desc_sort_word(lower: &str) -> bool {
    const SIZE_DESC_WORDS: &[&str] = &[
        "最大",
        "最重",
        "最沉",
        "biggest",
        "largest",
        "体积最大",
        "大文件",
        // 抽象 size 提及（「几个 G的视频」，v05-media-class1-size-074 锚）：与
        // [`has_size_sort_signal`] 的四个抽象形态镜像——v0.5 media+size 锚点 26/26 全
        // size_desc；「找几个视频」无单位词不命中（决策 E 路由不受影响）。
        "几个 g",
        "几个 m",
        "几 g",
        "几 m",
    ];
    SIZE_DESC_WORDS.iter().any(|w| lower.contains(w))
}

fn has_explicit_size_threshold(lower: &str) -> bool {
    use regex::Regex;
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\d+\s*(?:b|kb|mb|gb|tb)").expect("regex"));
    re.is_match(lower)
}

/// 时长词仅在前置数字时才算媒体信号（区分"5 minutes 的视频"与"会议 minutes 纪要"）。
/// 复用与 [`has_explicit_size_threshold`] 同款的数字+单位模式。
fn has_numeric_duration(lower: &str) -> bool {
    use regex::Regex;
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    // BETA-13-G2：数字含 ASCII 与中文（"一小时" / "半小时"）。
    let re = RE.get_or_init(|| {
        Regex::new(r"(?:\d+|[一两二三四五六七八九十半])\s*(?:分钟|小时|钟头|minutes?|hours?)")
            .expect("regex")
    });
    re.is_match(lower)
}

/// 强媒体信号：触发 `media_search` 路径。
///
/// 注意"视频/videos"、"图片/images" 是**弱信号** — 它们更常作为 `file_search.file_type`
/// 的指示符（如 "find videos larger than 1 GB" → file_search file_type=video）。
pub(crate) fn has_strong_media_signal(lower: &str) -> bool {
    const STRONG: &[&str] = &[
        "歌",
        "音乐",
        "音频",
        "audio",
        "song",
        "music",
        "tracks",
        "录音",
        "录像",
        "截图",
        "截屏",
        "screenshot",
        "screenshots",
        "截的",
        "截了",
    ];
    // 时长词（分钟/小时/minute(s)/hour(s)）仅在前置数字时算强信号，
    // 避免"minutes"（会议纪要）等内容词被误判为媒体（variant 漂移）。
    STRONG.iter().any(|s| lower.contains(s)) || has_numeric_duration(lower)
}

pub(crate) fn has_any_media_signal(lower: &str) -> bool {
    for (keywords, _) in lexicon::MEDIA_TYPE_KEYWORDS {
        if keywords.iter().any(|k| lower.contains(k)) {
            return true;
        }
    }
    false
}

/// BETA-13-G2：流派检测。
fn detect_genre(lower: &str) -> Option<String> {
    for (kws, canon) in lexicon::GENRE_KEYWORDS {
        if kws.iter().any(|k| lower.contains(k)) {
            return Some((*canon).to_owned());
        }
    }
    None
}

/// BETA-13-G2：专辑名。"《X》专辑" / "X》专辑" / "the album X" / "from the album X" / "album X"。
fn extract_album(input: &str, lower: &str) -> Option<String> {
    use regex::Regex;
    use std::sync::OnceLock;
    // 中文：含「专辑」时取《》内的名字
    if input.contains("专辑") {
        if let Some(name) = extract_cjk_bracket(input) {
            return Some(name);
        }
    }
    // 英文："the album X" / "from the album X" / "album X"（X 到行尾或介词前）
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"album\s+([A-Za-z0-9][A-Za-z0-9 ]*?)(?:\s+(?:by|from|over|under)\b|$)")
            .expect("regex")
    });
    if let Some(cap) = re.captures(lower) {
        // 用原串还原大小写
        let start = cap.get(1)?.start();
        let end = cap.get(1)?.end();
        let name = input.get(start..end)?.trim();
        if !name.is_empty() {
            return Some(name.to_owned());
        }
    }
    None
}

/// 取第一个《》/「」内的内容。
fn extract_cjk_bracket(input: &str) -> Option<String> {
    let start = input.find('《')? + '《'.len_utf8();
    let rest = &input[start..];
    let end = rest.find('》')?;
    let name = rest[..end].trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}

/// BETA-13-G2：歌曲标题。《X》（非专辑）/ 叫 X 的歌 / the song X / a song called X /
/// 播放·来一首 X / X 这首歌。
fn extract_title(input: &str, lower: &str, artist: &Option<String>) -> Option<String> {
    // 1. 《X》（不含「专辑」时整体作标题）
    if !input.contains("专辑") {
        if let Some(t) = extract_cjk_bracket(input) {
            return Some(t);
        }
    }
    // 2. 叫 X 的歌 / 叫 X 的
    if let Some(p) = input.find('叫') {
        let after = input[p + '叫'.len_utf8()..].trim_start();
        let t: String = after
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '的')
            .collect();
        if t.chars().count() >= 1 && !is_stopword_artist(&t) {
            return Some(t);
        }
    }
    // 3. 英文 the song X / song called X（到 " by " 或行尾）
    for marker in ["a song called ", "song called ", "the song "] {
        if let Some(p) = lower.find(marker) {
            let after = &input[p + marker.len()..];
            let end = after.to_lowercase().find(" by ").unwrap_or(after.len());
            let t = after[..end].trim();
            if !t.is_empty() {
                return Some(t.to_owned());
            }
        }
    }
    // 4. 播放 / 来一首 / 放首 + X（X 非 "…的歌/唱的"=artist）
    for verb in ["播放", "来一首", "放一首", "放首"] {
        if let Some(p) = input.find(verb) {
            let after = input[p + verb.len()..].trim_start();
            if after.contains("的歌") || after.contains("唱的") {
                break;
            }
            let t: String = after
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '的' && *c != '。' && *c != '，')
                .collect();
            if t.chars().count() >= 2 && !is_stopword_artist(&t) {
                return Some(t);
            }
        }
    }
    // 5. X 这首歌
    if let Some(p) = input.find("这首歌") {
        if let Some(tok) = input[..p].split_whitespace().last() {
            if tok.chars().count() >= 1 {
                return Some(tok.to_owned());
            }
        }
    }
    // 6. 兜底：artist 的 X
    extract_simple_title(input, artist)
}

/// BETA-13-G2：音乐时长（含 less_than 与中文数字 / 半小时）。
fn parse_media_duration(lower: &str) -> Option<scout_search_backend::SizeExpression> {
    use regex::Regex;
    use scout_search_backend::{SizeExpression, SizeUnit};
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"(超过|大于|多于|超出|不到|不足|短于|少于|over|longer than|more than|under|less than|shorter than)\s*(\d+|[一两二三四五六七八九十半])\s*(分钟|分|小时|钟头|minutes?|mins?|hours?|hrs?)",
        )
        .expect("regex")
    });
    let cap = re.captures(lower)?;
    let value = parse_cjk_or_digit(&cap[2])?;
    let unit = match &cap[3] {
        "小时" | "钟头" => SizeUnit::Hour,
        u if u.starts_with("hour") || u.starts_with("hr") => SizeUnit::Hour,
        _ => SizeUnit::Min,
    };
    let less = matches!(
        &cap[1],
        "不到" | "不足" | "短于" | "少于" | "under" | "less than" | "shorter than"
    );
    Some(if less {
        SizeExpression::LessThan { value, unit }
    } else {
        SizeExpression::GreaterThan { value, unit }
    })
}

fn parse_cjk_or_digit(s: &str) -> Option<f64> {
    if let Ok(v) = s.parse::<f64>() {
        return Some(v);
    }
    Some(match s {
        "一" => 1.0,
        "两" | "二" => 2.0,
        "三" => 3.0,
        "四" => 4.0,
        "五" => 5.0,
        "六" => 6.0,
        "七" => 7.0,
        "八" => 8.0,
        "九" => 9.0,
        "十" => 10.0,
        "半" => 0.5,
        _ => return None,
    })
}

fn detect_media_type(lower: &str) -> Option<MediaType> {
    // Bucket C: "截的" / "截了" 是 screenshot 触发词（lexicon 已含 "截图" / "截屏"，
    // 此处补"截的/截了"动词形态，且因检测顺序在 lexicon 前，保证优先于 audio 命中）。
    const SCREENSHOT_VERB_HINTS: &[&str] = &["截的", "截了"];
    if SCREENSHOT_VERB_HINTS.iter().any(|h| lower.contains(h)) {
        return Some(MediaType::Screenshot);
    }
    for (keywords, mt) in lexicon::MEDIA_TYPE_KEYWORDS {
        if keywords.iter().any(|k| lower.contains(k)) {
            return Some(*mt);
        }
    }
    // 有 artist 但没 media_type 词 → audio
    if contains_known_artist(lower) {
        return Some(MediaType::Audio);
    }
    None
}

fn detect_quality(lower: &str) -> Option<Quality> {
    for (keywords, quality) in lexicon::QUALITY_KEYWORDS {
        if keywords.iter().any(|k| lower.contains(k)) {
            return Some(*quality);
        }
    }
    None
}

pub(crate) fn parse_media_search(input: &str, lower: &str, language: Language) -> SearchIntent {
    let media_type = detect_media_type(lower).unwrap_or(MediaType::Audio);
    let artist = extract_artist(input, lower);

    // 音乐时长（"超过 10 分钟" / "不到 3 分钟" / "超过一小时" 等）
    let duration = parse_media_duration(lower).or_else(|| parse_duration(lower));

    // BETA-13-G2：标题 / 专辑 / 流派——仅对 audio 提取（video/screenshot 无这些语义）。
    // 先于时间解析：显式标题文本要从时间匹配面上剥掉（「a song called Yesterday」的
    // Yesterday 是标题不是时间词，v09-d4-en-005 锚）。
    let title = if media_type == MediaType::Audio {
        extract_title(input, lower, &artist)
    } else {
        extract_simple_title(input, &artist)
    };

    // 时间字段：是否说"下载的"（→ created_time）；匹配面 = 剥掉标题后的 lower。
    let title_stripped_lower = title
        .as_ref()
        .map(|t| lower.replace(t.to_lowercase().as_str(), " "));
    let time_lower: &str = title_stripped_lower.as_deref().unwrap_or(lower);
    let (mut created_time, modified_time, accessed_time) = parse_time_fields(time_lower, input);

    // v0.5：Screenshot + 时间词 → created_time（"昨天截的" / "from last month" 语义上是
    // 创建时间，不是修改时间）。v0.4 Bucket F 临时把 Screenshot 路由到 modified_time 对齐
    // 老 fixture template 已撤销 — schema seed PROTO-02 与 v0.5 fixture template 已统一为 created_time。
    if media_type == MediaType::Screenshot && created_time.is_none() && modified_time.is_some() {
        // 用户用"修改"动词的 Screenshot query（少见）按 created_time 处理（screenshot 语义优先）
        created_time = modified_time;
    }
    // 重新分配：Screenshot 永远把 time 放在 created_time
    let modified_time = if media_type == MediaType::Screenshot {
        None
    } else {
        modified_time
    };

    // v0.5：media_search 不再自动填充音频默认扩展名（fixture 期望 null；artist 自身已足够约束）。
    // BETA-09 后续 fix：screenshot path 在 query 显式含图片扩展名词时输出 extensions（如
    // "find JPG and PNG screenshots from yesterday"）。其他 media_type 仍保持 None。
    let extensions: Option<Vec<String>> = if media_type == MediaType::Screenshot {
        extract_screenshot_extensions(input)
    } else {
        None
    };

    let album = if media_type == MediaType::Audio {
        extract_album(input, lower)
    } else {
        None
    };
    let genre = if media_type == MediaType::Audio {
        detect_genre(lower)
    } else {
        None
    };

    // 截图 + 关键词（如"付款 二维码"）
    let keywords = if media_type == MediaType::Screenshot {
        extract_screenshot_keywords(input)
    } else {
        None
    };

    // BETA-13 follow-up：显式「按名字排序 / by name」优先（对齐 file_search BETA-13-G6 词集）。
    let sort = if lower.contains("按名字")
        || lower.contains("按名称")
        || lower.contains("名字排")
        || lower.contains("名称排")
        || lower.contains("by name")
    {
        if lower.contains("倒序") || lower.contains("降序") || lower.contains("name desc") {
            Some(SortOrder::NameDesc)
        } else {
            Some(SortOrder::NameAsc)
        }
    // v0.5：size 阈值或 size_desc sort 词 → SizeDesc 优先（覆盖时间排序）。
    } else if has_explicit_size_threshold(lower) || has_size_desc_sort_word(lower) {
        Some(SortOrder::SizeDesc)
    } else if lower.contains("最小") || lower.contains("smallest") {
        Some(SortOrder::SizeAsc)
    } else if lower.contains("最新") || lower.contains("newest") {
        Some(SortOrder::CreatedDesc)
    } else if lower.contains("最旧") || lower.contains("oldest") {
        Some(SortOrder::CreatedAsc)
    } else if created_time.is_some() {
        Some(SortOrder::CreatedDesc)
    } else if modified_time.is_some() {
        Some(SortOrder::ModifiedDesc)
    } else {
        Some(SortOrder::RelevanceDesc)
    };

    SearchIntent::MediaSearch(MediaSearch {
        schema_version: SchemaVersion::V1,
        language: Some(language),
        media_type,
        artist,
        title,
        album,
        genre,
        quality: detect_quality(lower),
        duration,
        keywords,
        extensions,
        file_type: None,
        location: parse_location_with_language(lower, language),
        modified_time,
        created_time,
        accessed_time,
        // v0.5：media_search 也提取 size（"大于 100MB的视频" 等）。
        // 仅对非 audio 媒体提取 size（audio 时长见 duration 字段）。
        size: if media_type == MediaType::Audio {
            None
        } else {
            parse_size(lower)
        },
        exclude_extensions: None,
        exclude_file_type: None,
        sort,
        limit: None,
    })
}

fn extract_simple_title(input: &str, artist: &Option<String>) -> Option<String> {
    let artist = artist.as_ref()?;
    let after = input.split(artist.as_str()).nth(1)?.trim();
    // 跳过紧随的"的"
    let title_part = after.strip_prefix('的').unwrap_or(after).trim();
    if title_part.is_empty() {
        return None;
    }
    // 取第一个 token（按空格 / 标点截断）
    let title: String = title_part
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != ',' && *c != '。' && *c != '，')
        .collect();
    let title = title.trim();
    if title.is_empty() || is_descriptor_segment(title) {
        None
    } else {
        Some(title.to_owned())
    }
}

/// title 兜底只收"点名"：artist 后残段若是修饰性描述（音质/流派/时长比较/专辑指代/
/// 泛指媒体名词的组合），不作标题。显式点名（叫 X / called X / 《X》/ 播放 X）由
/// `extract_title` 前置规则收取，不经此判定。
fn is_descriptor_segment(candidate: &str) -> bool {
    let lower = candidate.to_lowercase();
    // 专辑指代 / 书名号残段：专辑名归 album 字段，《X》标题由显式规则收取，兜底不收。
    if lower.contains("专辑") || lower.contains('《') {
        return true;
    }
    // 时长 / 大小比较短语开头的残段是约束描述，不是歌名。
    const COMPARATORS: &[&str] = &[
        "超过",
        "超出",
        "多于",
        "大于",
        "短于",
        "少于",
        "不到",
        "不足",
        "长于",
        "over",
        "under",
        "longer",
        "shorter",
        "more than",
        "less than",
    ];
    if COMPARATORS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    // 剥掉质量词 / 流派词（单一来源 lexicon）与泛指媒体名词、连接字后无实义残留 → 描述段。
    let mut rest = lower;
    for (kws, _) in lexicon::QUALITY_KEYWORDS {
        for k in *kws {
            rest = rest.replace(&k.to_lowercase(), "");
        }
    }
    for (kws, _) in lexicon::GENRE_KEYWORDS {
        for k in *kws {
            rest = rest.replace(&k.to_lowercase(), "");
        }
    }
    const GENERIC_MEDIA_WORDS: &[&str] = &[
        "音乐视频",
        "歌曲",
        "唱的",
        "风格",
        "音乐",
        "歌",
        "的",
        "里",
        "中",
        "music videos",
        "music video",
        "videos",
        "video",
        "songs",
        "song",
        "tracks",
        "track",
        "music",
        "mv",
    ];
    for g in GENERIC_MEDIA_WORDS {
        rest = rest.replace(g, "");
    }
    rest.chars()
        .all(|c| c.is_whitespace() || c.is_ascii_punctuation() || matches!(c, '，' | '。' | '、'))
}

/// 仅当 query 显式含图片扩展名词时输出 extensions。case-insensitive，去重，按出现顺序。
/// BETA-09 后续：v05-schema-44-045 "find JPG and PNG screenshots from yesterday"
/// fixture 期望 `extensions: ["jpg","png"]`；其他 19 个 screenshot fixture 期望 null。
fn extract_screenshot_extensions(input: &str) -> Option<Vec<String>> {
    let known: &[&str] = &["jpg", "jpeg", "png", "gif", "bmp"];
    let mut found: Vec<String> = Vec::new();
    for raw in input.split(|c: char| c.is_whitespace() || c == '，' || c == '。' || c == ',') {
        let t = raw.trim();
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        for ext in known {
            if lower == *ext && !found.iter().any(|f| f == ext) {
                found.push((*ext).to_string());
                break;
            }
        }
    }
    if found.is_empty() {
        None
    } else {
        Some(found)
    }
}

fn extract_screenshot_keywords(input: &str) -> Option<Vec<String>> {
    use regex::Regex;
    use std::sync::OnceLock;
    // BETA-13 收束：排序短语（"sorted by name" 等）已被 sort 字段消费，整短语剥除、
    // 不得漏进 keywords（v09-d5-en-024）。按短语剥而非把 "name" 加停用词——
    // "name" 单词在其它语境可以是内容词。v0.5 无 "sorted by" 锚点（零暴露）。
    static RE_SORT_PHRASE: OnceLock<Regex> = OnceLock::new();
    let re_sort = RE_SORT_PHRASE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:sorted|ordered)\s+by\s+(?:file\s*)?(?:name|size|date|time)\b")
            .expect("sort phrase regex")
    });
    let cleaned = re_sort.replace_all(input, " ");
    let input: &str = &cleaned;
    // 简单：去除已知动词 / 时间词 / 类型词后剩下的实词
    let stop_words: &[&str] = &[
        // 中文动词 / 助词 / 代词
        "找",
        "找我",
        "找上周截的",
        "找昨天截的",
        "截的",
        "截了",
        "的",
        "了",
        "我",
        "我的",
        // 中文时间词
        "昨天",
        "今天",
        "前天",
        "本周",
        "这周",
        "上周",
        "一周",
        "本月",
        "这个月",
        "上个月",
        "最近三天",
        "过去三天",
        "三天",
        "两周",
        "最近",
        "一周内",
        "近一周",
        // 中文排序词（「上个月的截图按名字排」——排序短语已被 sort 消费，不漏进 keywords）
        "按名字排",
        "按名称排",
        "按名字",
        "按名称",
        "按大小",
        "排序",
        "倒序",
        "升序",
        // 中文类型词
        "截图",
        "截屏",
        // 英文动词 / 类型 / 代词
        "find",
        "show",
        "me",
        "my",
        "screenshot",
        "screenshots",
        // Bucket D: 英文 stop words
        "from",
        "last",
        "next",
        "this",
        "past",
        // BETA-13 follow-up：英文数字词（"last three days" 等的 three/two…），不漏成截图内容词
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "in",
        "the",
        "a",
        "an",
        "of",
        "on",
        "at",
        "by",
        "and",
        "or",
        "for",
        "with",
        "modified",
        "edited",
        "created",
        "downloaded",
        // 英文时间单位
        "day",
        "days",
        "week",
        "weeks",
        "month",
        "months",
        "year",
        "years",
        "yesterday",
        "today",
        "tomorrow",
        // 常见图片后缀名作为 keyword 时也属噪声
        "jpg",
        "jpeg",
        "png",
        "gif",
        "bmp",
        // 位置词（fixture screenshot keywords 一致期望 null，不应把位置词当 keyword）
        "downloads",
        "desktop",
        "documents",
        "pictures",
        "movies",
        "music",
    ];
    let mut tokens: Vec<String> = Vec::new();
    for raw in input.split(|c: char| c.is_whitespace() || c == '，' || c == '。' || c == ',') {
        let t = raw.trim();
        if t.is_empty() {
            continue;
        }
        // 大小写不敏感比对：fixture 中 "JPG" / "PNG" 大写扩展名词须命中小写 stop list
        // （ASCII 部分 ignore case；中文 stop word 不受影响，eq_ignore_ascii_case 对非 ASCII byte 保持原值比对）
        if stop_words.iter().any(|s| s.eq_ignore_ascii_case(t)) {
            continue;
        }
        // 跳过纯数字
        if t.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        // v0.5：跳过 hyphenated ASCII identifier（"synthetic-receipt" 这类 fixture template
        // 占位符，不算 screenshot 内容关键词）
        if t.contains('-') && t.chars().all(|c| c.is_ascii_alphabetic() || c == '-') {
            continue;
        }
        // 截取出"付款二维码"类似的子串：识别非时间、非动词的连续中文段
        // 简化：直接保留
        tokens.push(t.to_owned());
    }
    // 进一步：把"付款二维码"切成 ["付款", "二维码"]：用常见类目词典
    let split_tokens = split_compound_zh_words(&tokens);
    // v0.5：每个 token 再剥离前缀 stop words（"找我昨天截的付款" → "付款"）
    let stripped: Vec<String> = split_tokens
        .into_iter()
        .filter_map(|t| {
            let stripped = strip_leading_stop_prefix(&t, stop_words);
            if stripped.is_empty() {
                None
            } else {
                Some(stripped)
            }
        })
        .collect();
    if stripped.is_empty() {
        None
    } else {
        Some(stripped)
    }
}

/// 剥离 token 开头连续的 stop word 前缀（按 stop list 中**长 token 优先**贪心匹配）。
/// 例："找我昨天截的付款" + stop=["找","我","昨天","截的"] → "付款"。
fn strip_leading_stop_prefix(token: &str, stop: &[&str]) -> String {
    let mut remaining = token;
    let mut stops_by_len: Vec<&&str> = stop.iter().collect();
    stops_by_len.sort_by_key(|s| std::cmp::Reverse(s.len()));
    loop {
        let mut stripped = false;
        for s in &stops_by_len {
            if remaining.starts_with(**s) {
                remaining = &remaining[s.len()..];
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }
    remaining.to_owned()
}

#[cfg(test)]
mod tests_screenshot_time_and_stopwords {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use scout_search_backend::{RelativeTime, SearchIntent, SortOrder, TimeExpression};

    #[test]
    fn screenshot_uses_created_time_not_modified_v05() {
        // v0.5：撤销 v0.4 Bucket F 决策。Screenshot + 时间词 → created_time
        // （"截的" / "from N" 语义上是创建时间），fixture schema seed 与 fill_media_search
        // 模板已统一对齐 created_time。
        let intent = crate::parse("找上周截的 synthetic-receipt 截图");
        let SearchIntent::MediaSearch(ms) = intent else {
            panic!();
        };
        assert!(
            ms.created_time.is_some(),
            "Screenshot 应输出 created_time，实际 created={:?} modified={:?}",
            ms.created_time,
            ms.modified_time
        );
        assert!(
            ms.modified_time.is_none(),
            "Screenshot 不应输出 modified_time，实际 {:?}",
            ms.modified_time
        );
        assert_eq!(
            ms.created_time,
            Some(TimeExpression::Relative {
                value: RelativeTime::LastWeek
            })
        );
        assert_eq!(ms.sort, Some(SortOrder::CreatedDesc));
    }

    #[test]
    fn title_text_not_parsed_as_time() {
        // 「a song called Yesterday」的 Yesterday 是标题不是时间词（v09-d4-en-005）。
        use scout_search_backend::{SearchIntent, SortOrder};
        let SearchIntent::MediaSearch(ms) = crate::parse("find a song called Yesterday") else {
            panic!()
        };
        assert_eq!(ms.title.as_deref(), Some("Yesterday"));
        assert_eq!(ms.modified_time, None, "标题词不应变成时间过滤");
        assert_eq!(ms.sort, Some(SortOrder::RelevanceDesc));
    }

    #[test]
    fn recently_shot_video_maps_last_7_days() {
        // 「最近拍的视频」→ modified last_7_days + modified_desc（v09-d4-zh-030，
        // 对齐 v0.5 video+modified 27 锚点的排序约定）。
        use scout_search_backend::{RelativeTime, SearchIntent, SortOrder, TimeExpression};
        let SearchIntent::MediaSearch(ms) = crate::parse("找最近拍的视频") else {
            panic!()
        };
        assert_eq!(
            ms.modified_time,
            Some(TimeExpression::Relative {
                value: RelativeTime::Last7Days
            })
        );
        assert_eq!(ms.sort, Some(SortOrder::ModifiedDesc));
    }

    #[test]
    fn screenshot_keywords_strip_time_sort_and_pronoun() {
        // screenshot keyword 泄漏三修：my / 最近三天残段 / 按名字排（d2-en-023、d5-zh-003/038）。
        use scout_search_backend::{SearchIntent, SortOrder};
        let SearchIntent::MediaSearch(ms) = crate::parse("my screenshots") else {
            panic!()
        };
        assert_eq!(ms.keywords, None, "my 不应泄漏");
        let SearchIntent::MediaSearch(ms) = crate::parse("最近三天的截图") else {
            panic!()
        };
        assert_eq!(ms.keywords, None, "三天的截图 不应泄漏");
        let SearchIntent::MediaSearch(ms) = crate::parse("上个月的截图按名字排") else {
            panic!()
        };
        assert_eq!(ms.keywords, None, "按名字排 不应泄漏");
        assert_eq!(ms.sort, Some(SortOrder::NameAsc));
    }

    #[test]
    fn english_videos_does_not_trigger_movies_location() {
        // lexicon bug: LOCATION_ALIASES 把 "videos" 当成 movies location 关键词。
        // "find videos modified this week" → 不应输出 location=movies。
        let intent = crate::parse("find videos modified this week");
        let SearchIntent::MediaSearch(ms) = intent else {
            panic!();
        };
        assert!(
            ms.location.is_none(),
            "\"find videos modified this week\" 不应输出 location，实际 {:?}",
            ms.location
        );
    }

    #[test]
    fn screenshot_filters_more_stopwords() {
        let intent = crate::parse("screenshots from last week in downloads");
        let SearchIntent::MediaSearch(ms) = intent else {
            panic!();
        };
        let kws = ms.keywords.unwrap_or_default();
        for stop in ["from", "last", "week", "in"] {
            assert!(
                !kws.iter().any(|k| k == stop),
                "stop word {stop:?} 不应在 keywords：{kws:?}"
            );
        }
    }

    // BETA-09 后续 fix：screenshot keywords 残留 partial 7 case 修
    // 3 个 root cause：(1) stop_words case-sensitive 漏 JPG/PNG  (2) 位置词
    // (downloads/documents/desktop) 不在 stop_words  (3) "一周" 时间词不在 stop_words
    // fixture 设计：screenshot keywords 几乎全期望 null（v05-schema-15-015 "付款二维码"
    // 是唯一真内容词例外），加 stop word 完全安全。

    /// BETA-13 收束（v09-d5-en-024）：sort 短语已被 sort 字段消费，不得漏进 keywords。
    #[test]
    fn screenshot_sorted_by_phrase_not_leaked_into_keywords() {
        let intent = crate::parse("screenshots from last month sorted by name");
        let SearchIntent::MediaSearch(ms) = intent else {
            panic!();
        };
        assert_eq!(
            ms.keywords, None,
            "sorted by name 应整短语剥除，实际 {:?}",
            ms.keywords
        );
        assert_eq!(ms.sort, Some(SortOrder::NameAsc));
        assert_eq!(ms.media_type, scout_search_backend::MediaType::Screenshot);
    }

    /// BETA-13 收束（v09-d5-en-025）：media 路径 `bigger than` size 约束不丢。
    #[test]
    fn video_bigger_than_size_extracted_on_media_path() {
        let intent = crate::parse("videos in downloads bigger than 200MB sorted by size");
        let SearchIntent::MediaSearch(ms) = intent else {
            panic!();
        };
        assert_eq!(ms.media_type, scout_search_backend::MediaType::Video);
        assert!(
            matches!(
                ms.size,
                Some(scout_search_backend::SizeExpression::GreaterThan { value, .. }) if (value - 200.0).abs() < f64::EPSILON
            ),
            "bigger than 200MB 应进 size 字段，实际 {:?}",
            ms.size
        );
        assert_eq!(ms.sort, Some(SortOrder::SizeDesc));
    }

    #[test]
    fn screenshot_jpg_png_extensions_filtered_case_insensitive() {
        // v05-schema-44-045：fixture 期望 keywords=None
        let intent = crate::parse("find JPG and PNG screenshots from yesterday");
        let SearchIntent::MediaSearch(ms) = intent else {
            panic!();
        };
        assert_eq!(
            ms.keywords, None,
            "JPG/PNG（大写扩展名词）应作 stop word 过滤掉，实际 {:?}",
            ms.keywords
        );
    }

    #[test]
    fn screenshot_location_words_dont_leak_to_keywords() {
        // v05-media-template-285/289/293/297：fixture 期望 keywords=None
        for q in [
            "find screenshots from last week in downloads",
            "find screenshots from this week in documents",
            "find screenshots from yesterday in downloads",
            "find screenshots from past 7 days in desktop",
        ] {
            let intent = crate::parse(q);
            let SearchIntent::MediaSearch(ms) = intent else {
                panic!("query={q}");
            };
            assert_eq!(
                ms.keywords, None,
                "位置词不应进 keywords：query={q}, actual={:?}",
                ms.keywords
            );
        }
    }

    #[test]
    fn screenshot_yizhou_time_phrase_dont_leak_to_keywords() {
        // v05-media-template-259/279：fixture 期望 keywords=None
        // "找最近一周截的 synthetic-receipt 截图" 之前剥 prefix 后剩 "一周截的"
        let intent = crate::parse("找最近一周截的 synthetic-receipt 截图");
        let SearchIntent::MediaSearch(ms) = intent else {
            panic!();
        };
        assert_eq!(
            ms.keywords, None,
            "\"一周\" 时间词不应留在 keywords，实际 {:?}",
            ms.keywords
        );
    }

    #[test]
    fn screenshot_real_content_keyword_kept_v05_regression() {
        // v05-schema-15-015：fixture 期望 keywords=['付款', '二维码']
        // 加 stop word 不应破坏真内容词
        let intent = crate::parse("找我昨天截的付款二维码");
        let SearchIntent::MediaSearch(ms) = intent else {
            panic!();
        };
        let kws = ms.keywords.unwrap_or_default();
        assert!(
            kws.contains(&"付款".to_owned()) && kws.contains(&"二维码".to_owned()),
            "真内容词 付款/二维码 应保留，实际 {:?}",
            kws
        );
    }

    // BETA-09 后续 fix：v05-schema-44-045 screenshot path 不输出 extensions bug

    #[test]
    fn screenshot_extracts_explicit_extensions_case_insensitive() {
        // v05-schema-44-045：fixture 期望 extensions=['jpg','png']，按出现顺序
        let intent = crate::parse("find JPG and PNG screenshots from yesterday");
        let SearchIntent::MediaSearch(ms) = intent else {
            panic!();
        };
        assert_eq!(
            ms.extensions,
            Some(vec!["jpg".to_owned(), "png".to_owned()]),
            "应识别 JPG/PNG 大写扩展名词，case-insensitive 输出小写"
        );
    }

    #[test]
    fn screenshot_without_extension_word_keeps_extensions_none() {
        // 19 fixture screenshot case 期望 extensions=null
        // 不能 over-match：query 不含明示扩展名词时不应输出 extensions
        for q in [
            "找上周截的 synthetic-receipt 截图",
            "find screenshots from last week in downloads",
            "找昨天截的付款二维码",
        ] {
            let intent = crate::parse(q);
            let SearchIntent::MediaSearch(ms) = intent else {
                panic!("query={q}");
            };
            assert_eq!(
                ms.extensions, None,
                "query 不含扩展名词时不应输出 extensions：query={q}, actual={:?}",
                ms.extensions
            );
        }
    }

    #[test]
    fn screenshot_single_extension_lowercase() {
        // 大小写混合 + 单一扩展名也工作
        let intent = crate::parse("find Png screenshots");
        let SearchIntent::MediaSearch(ms) = intent else {
            panic!();
        };
        assert_eq!(
            ms.extensions,
            Some(vec!["png".to_owned()]),
            "单一扩展名 + 大小写混合应输出小写"
        );
    }

    #[test]
    fn bare_minutes_does_not_trigger_media_v_option2() {
        // "minutes"（会议纪要）无数字上下文 → 不应漂移到 media_search。
        let intent = crate::parse("where are the minutes from the October all-hands");
        assert!(
            matches!(intent, SearchIntent::FileSearch(_)),
            "裸 minutes 应为 FileSearch，实际 {intent:?}"
        );
    }

    #[test]
    fn numeric_duration_still_triggers_media_v_option2() {
        use super::has_strong_media_signal;
        // 有数字上下文的时长词仍是强媒体信号。
        assert!(has_strong_media_signal("songs longer than 5 minutes"));
        assert!(has_strong_media_signal("找时长超过 3 小时的视频")); // "3 小时"
        assert!(has_strong_media_signal("clips under 30 minutes"));
        // 裸时长词不再是强信号。
        assert!(!has_strong_media_signal("where are the minutes"));
        assert!(!has_strong_media_signal("the minutes from the meeting"));
        assert!(!has_strong_media_signal("会议分钟纪要")); // 裸"分钟"无数字
                                                           // 真·强媒体词不受影响。
        assert!(has_strong_media_signal("audio recording"));
        assert!(has_strong_media_signal("找一首歌"));
    }
}

#[cfg(test)]
mod tests_artist_natural_phrasing {
    //! BETA-13-G Fix2：artist 自然措辞抽取修缮（剥前缀/完整EN名/夹修饰/《》/video，抑制叫-title 过抽）。
    #![allow(clippy::unwrap_used, clippy::panic)]

    #[test]
    fn artist_extraction_natural_phrasing() {
        use scout_search_backend::SearchIntent;
        let positive = [
            ("找邓紫棋的歌曲", "邓紫棋"),
            ("音乐目录里周杰伦的歌", "周杰伦"),
            ("Taylor Swift 的歌", "Taylor Swift"),
            ("王菲的爵士风格歌曲", "王菲"),
            ("周杰伦超过4分钟的无损歌曲", "周杰伦"),
            ("找五月天短于4分钟的歌", "五月天"),
            ("Coldplay 超过5分钟的歌", "Coldplay"),
            ("薛之谦《绅士》专辑", "薛之谦"),
            ("陈奕迅 浮夸 这首歌", "陈奕迅"),
            ("play the song Shape of You by Ed Sheeran", "Ed Sheeran"),
        ];
        for (q, a) in positive {
            let artist = match crate::parse(q) {
                SearchIntent::MediaSearch(m) => m.artist,
                other => panic!("{q} 应 media_search，实际 {other:?}"),
            };
            assert_eq!(artist.as_deref(), Some(a), "{q}");
        }
    }

    #[test]
    fn artist_extraction_no_false_positive() {
        use scout_search_backend::SearchIntent;
        for q in [
            "找一首叫 七里香 的歌",
            "找一首叫 Hello 的歌",
            "找一些高品质的歌",
        ] {
            if let SearchIntent::MediaSearch(m) = crate::parse(q) {
                assert_eq!(m.artist, None, "{q} 不应抽 artist，得 {:?}", m.artist);
            }
        }
    }

    #[test]
    fn songs_by_hyphenated_lowercase_artist() {
        use scout_search_backend::SearchIntent;
        // v05-media-template-286 族：小写连字符 artist（合成语料形态）。
        let SearchIntent::MediaSearch(m) = crate::parse("find songs by synthetic-artist") else {
            panic!("应 media_search")
        };
        assert_eq!(m.artist.as_deref(), Some("synthetic-artist"));
        // 反向守护：裸小写词（无连字符）不作 artist——"sorted by size" 类残句安全。
        if let SearchIntent::MediaSearch(m) = crate::parse("videos sorted by size") {
            assert_eq!(m.artist, None, "size 不应作 artist");
        }
    }

    #[test]
    fn abstract_size_mention_sorts_size_desc() {
        use scout_search_backend::{SearchIntent, SortOrder};
        // v05-media-class1-size-074：「几个 G」抽象 size 提及 → size_desc（26 锚点惯例）。
        let SearchIntent::MediaSearch(m) = crate::parse("找几个 G的视频") else {
            panic!("应 media_search")
        };
        assert_eq!(m.sort, Some(SortOrder::SizeDesc));
        // 反向守护：「找几个视频」无单位词 → 不触发 size 排序（决策 E 数量修饰路由不变）。
        let SearchIntent::MediaSearch(m) = crate::parse("找几个视频") else {
            panic!("应 media_search")
        };
        assert_ne!(m.sort, Some(SortOrder::SizeDesc));
    }

    #[test]
    fn en_dir_name_with_zh_container_location_hint() {
        use scout_search_backend::SearchIntent;
        // d5-mixed-010：「music 目录」= en 目录名 + 中文容器词，hint 取 en 形态。
        let SearchIntent::MediaSearch(m) = crate::parse("music 目录里的 lossless 歌曲")
        else {
            panic!("应 media_search")
        };
        assert_eq!(
            m.location.as_ref().and_then(|l| l.hint.as_deref()),
            Some("music")
        );
        // 反向守护：纯中文「音乐目录」仍取 zh 形态 hint。
        let SearchIntent::MediaSearch(m) = crate::parse("音乐目录里周杰伦的歌") else {
            panic!("应 media_search")
        };
        assert_eq!(
            m.location.as_ref().and_then(|l| l.hint.as_deref()),
            Some("音乐")
        );
    }

    #[test]
    fn music_video_by_artist_routes_video() {
        use scout_search_backend::{MediaType, SearchIntent};
        for (q, a) in [
            ("music videos by Adele", "Adele"),
            ("Eason 的 music video", "Eason"),
        ] {
            let SearchIntent::MediaSearch(m) = crate::parse(q) else {
                panic!("{q}")
            };
            assert_eq!(m.artist.as_deref(), Some(a), "{q}");
            assert_eq!(m.media_type, MediaType::Video, "{q} media_type");
        }
    }
}

#[cfg(test)]
mod tests_title_descriptor_reject {
    //! BETA-14 缺口盘点第 1 刀：title 兜底只收"点名"，修饰性残段不作标题。
    #![allow(clippy::unwrap_used, clippy::panic)]
    use scout_search_backend::SearchIntent;

    fn media(q: &str) -> scout_search_backend::MediaSearch {
        match crate::parse(q) {
            SearchIntent::MediaSearch(m) => m,
            other => panic!("{q} 应 media_search，实际 {other:?}"),
        }
    }

    #[test]
    fn descriptor_residue_not_title() {
        // artist 后的修饰残段（音质/流派/时长比较/泛指媒体名词）不作 title
        for q in [
            "找林俊杰的无损音乐",
            "找 Taylor Swift 的无损歌曲",
            "王菲的爵士风格歌曲",
            "周杰伦超过4分钟的无损歌曲",
            "找五月天短于4分钟的歌",
            "Coldplay 超过5分钟的歌",
            "放首李荣浩唱的",
            "周杰伦的音乐视频",
            "Eason 的 music video",
            "Adele songs from the album 25",
            "lossless songs by Taylor Swift over 4 minutes",
            "Beatles songs under 4 minutes",
        ] {
            let m = media(q);
            assert_eq!(m.title, None, "{q} 不应抽 title，得 {:?}", m.title);
            assert!(m.artist.is_some(), "{q} artist 不应丢");
        }
    }

    #[test]
    fn album_reference_residue_not_title() {
        // 《X》专辑 残段：专辑名归 album，title 应空
        for (q, album) in [
            ("周杰伦《范特西》专辑里的歌", "范特西"),
            ("薛之谦《绅士》专辑", "绅士"),
            ("周杰伦《叶惠美》专辑里超过4分钟的歌", "叶惠美"),
        ] {
            let m = media(q);
            assert_eq!(m.title, None, "{q} 不应抽 title，得 {:?}", m.title);
            assert_eq!(m.album.as_deref(), Some(album), "{q} album");
        }
    }

    #[test]
    fn duration_word_not_artist() {
        // 「时长不到3分钟的歌曲」——时长是度量词不是 artist，title 也应空
        let m = media("时长不到3分钟的歌曲");
        assert_eq!(m.artist, None, "时长 不应作 artist");
        assert_eq!(m.title, None);
        assert!(m.duration.is_some(), "时长约束应进 duration");
    }

    #[test]
    fn real_title_via_fallback_kept() {
        // v0.5 锚点：「找周华健的朋友」→ title=朋友（实义残段仍是标题）
        let m = media("找周华健的朋友");
        assert_eq!(m.artist.as_deref(), Some("周华健"));
        assert_eq!(m.title.as_deref(), Some("朋友"));
    }

    #[test]
    fn explicit_naming_paths_unaffected() {
        // 显式点名路径不经残段判定
        for (q, t) in [
            ("找一首叫 七里香 的歌", "七里香"),
            ("play the song Hello", "Hello"),
            ("找毛不易《消愁》", "消愁"),
        ] {
            let m = media(q);
            assert_eq!(m.title.as_deref(), Some(t), "{q}");
        }
    }
}

#[cfg(test)]
mod tests_artist_structure {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use scout_search_backend::SearchIntent;

    #[test]
    fn extracts_synthetic_artist_from_de_ge_structure() {
        // Bucket E: "X 的歌" 结构识别（不依赖词典）
        let intent = crate::parse("找 synthetic-artist 的歌");
        let SearchIntent::MediaSearch(ms) = intent else {
            panic!("expected MediaSearch");
        };
        assert_eq!(ms.artist.as_deref(), Some("synthetic-artist"));
    }

    #[test]
    fn extracts_artist_from_de_yinyue_structure() {
        let intent = crate::parse("找 synthetic-artist 的音乐");
        let SearchIntent::MediaSearch(ms) = intent else {
            panic!("expected MediaSearch");
        };
        assert_eq!(ms.artist.as_deref(), Some("synthetic-artist"));
    }

    #[test]
    fn extracts_artist_from_english_possessive() {
        let intent = crate::parse("find synthetic-artist's songs");
        let SearchIntent::MediaSearch(ms) = intent else {
            panic!("expected MediaSearch, got {intent:?}");
        };
        assert_eq!(ms.artist.as_deref(), Some("synthetic-artist"));
    }

    #[test]
    fn known_artist_still_works() {
        let intent = crate::parse("找周华健的朋友");
        let SearchIntent::MediaSearch(ms) = intent else {
            panic!();
        };
        assert_eq!(ms.artist.as_deref(), Some("周华健"));
    }
}

#[cfg(test)]
mod tests_media_type {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use scout_search_backend::{Language, MediaType, SearchIntent};

    fn parse_top(query: &str) -> SearchIntent {
        crate::parse(query)
    }

    #[test]
    fn screenshot_zh_disambiguates_from_audio() {
        // Bucket C: "二维码" 当前被识别为 audio；应识别为 screenshot
        let intent = parse_top("找我昨天截的付款二维码");
        let SearchIntent::MediaSearch(ms) = intent else {
            panic!("expected MediaSearch");
        };
        assert_eq!(ms.media_type, MediaType::Screenshot);
    }

    #[test]
    fn video_with_sort_word_routes_to_media_search() {
        // Bucket C: "找最大的视频" 当前走 file_search file_type=video；应走 media_search
        let intent = parse_top("找最大的视频");
        assert!(
            matches!(intent, SearchIntent::MediaSearch(_)),
            "expected MediaSearch, got {intent:?}"
        );
        if let SearchIntent::MediaSearch(ms) = intent {
            assert_eq!(ms.media_type, MediaType::Video);
        }
    }

    #[test]
    fn video_with_time_word_routes_to_media_search() {
        // v0.4 + v0.5 沿用：video + 时间词 → media_search（fixture 25 MS > 10 FS）
        let intent = parse_top("找一周内修改的视频");
        assert!(
            matches!(intent, SearchIntent::MediaSearch(_)),
            "expected MediaSearch, got {intent:?}"
        );
    }

    // v0.5 删除：`video_with_size_threshold_stays_file_search`
    // v0.4 选了"video + 具体 size → file_search"，v0.5 反转为 media_search（fixture 22 MS > 11 FS）；
    // 新契约由 [`tests_video_routing_v05::v05_video_with_concrete_size_*`] 覆盖。

    #[test]
    fn english_biggest_video_routes_to_media_search() {
        let intent = parse_top("find the biggest video");
        assert!(
            matches!(intent, SearchIntent::MediaSearch(_)),
            "expected MediaSearch, got {intent:?}"
        );
    }
}

#[cfg(test)]
mod tests_cross_category_visual_v19 {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use crate::parse;
    use scout_search_backend::{FileType, SearchIntent, SortOrder};

    #[test]
    fn cross_category_visual_media_routes_to_file_search() {
        // BETA-19 后续：「最大的图片和视频」无音频语义 → file_search file_type=[Image,Video]
        // + sort=SizeDesc（避免 media 单值 media_type 丢一类），由 BETA-19 均衡分支展示。
        let intent = parse("找最大的图片和视频");
        let SearchIntent::FileSearch(fs) = intent else {
            panic!("应路由 file_search，实得 {intent:?}");
        };
        assert_eq!(
            fs.file_type,
            Some(vec![FileType::Image, FileType::Video]),
            "BETA-13-G3/BETA-18：保留两类型，按 query 语序（图片在前）"
        );
        assert_eq!(fs.sort, Some(SortOrder::SizeDesc), "「最大」应 SizeDesc");
    }

    #[test]
    fn cross_category_visual_media_english_routes_to_file_search() {
        let intent = parse("find the biggest images and videos");
        let SearchIntent::FileSearch(fs) = intent else {
            panic!("应路由 file_search，实得 {intent:?}");
        };
        assert!(
            fs.file_type
                .as_deref()
                .is_some_and(|t| t.contains(&FileType::Image) && t.contains(&FileType::Video)),
            "应含 Image+Video，实得 {:?}",
            fs.file_type
        );
    }

    #[test]
    fn single_visual_media_still_routes_to_media_search() {
        // 回归守护：单视频类型不受影响，「最大的视频」仍 media_search。
        // BETA-13-G12 ②′：image 改为 carve-out→file_search（见 tests_beta13_g12_image_routing），
        // 故移除原「找一周内修改的图片→media」断言（决策已翻转）。
        for q in ["找最大的视频", "find the biggest video"] {
            let intent = parse(q);
            assert!(
                matches!(intent, SearchIntent::MediaSearch(_)),
                "单视频类型应仍 media：query={q}, got {intent:?}"
            );
        }
        // image-only + 约束 → file_search（不再 media）。
        assert!(
            matches!(parse("找一周内修改的图片"), SearchIntent::FileSearch(_)),
            "image+时间 应路由 file_search（②′）"
        );
    }

    #[test]
    fn cross_category_visual_with_artist_stays_media() {
        // 带 artist 的跨范畴查询在 is_media_query 上游 contains_known_artist 先命中 → 仍 media，
        // 保留 artist 语义（不被误降级到 file_search）。
        let intent = parse("找周华健的图片和视频");
        assert!(
            matches!(intent, SearchIntent::MediaSearch(_)),
            "带 artist 应仍 media：{intent:?}"
        );
    }
}

#[cfg(test)]
mod tests_strong_media_cross_category {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use crate::parse;
    use scout_search_backend::{FileType, SearchIntent};

    fn file_types(q: &str) -> Vec<FileType> {
        match parse(q) {
            SearchIntent::FileSearch(fs) => fs.file_type.unwrap_or_default(),
            other => panic!("query={q} expected FileSearch, got {other:?}"),
        }
    }

    #[test]
    fn audio_and_video_routes_to_file_search() {
        // 「音乐和视频」无 artist → file_search file_type=[Audio,Video]（避免 media 丢一类）。
        let t = file_types("找音乐和视频");
        assert!(
            t.contains(&FileType::Audio) && t.contains(&FileType::Video),
            "应含 Audio+Video，实得 {t:?}"
        );
    }

    #[test]
    fn screenshot_and_video_routes_to_file_search() {
        let t = file_types("截图和视频");
        assert!(
            t.contains(&FileType::Video)
                && (t.contains(&FileType::Screenshot) || t.contains(&FileType::Image)),
            "应含 视频 + 截图/图片，实得 {t:?}"
        );
    }

    #[test]
    fn english_music_and_videos_routes_to_file_search() {
        let t = file_types("find music and videos");
        assert!(
            t.contains(&FileType::Audio) && t.contains(&FileType::Video),
            "应含 Audio+Video，实得 {t:?}"
        );
    }

    #[test]
    fn music_video_without_conjunction_stays_media() {
        // 「音乐视频」= music video / MV（单概念，无连词）→ 仍 media_search，不误判跨范畴。
        assert!(
            matches!(parse("找音乐视频"), SearchIntent::MediaSearch(_)),
            "MV 无连词应仍 media"
        );
    }

    #[test]
    fn cross_category_with_artist_stays_media() {
        // 带 artist → 仍 media（保留 artist 语义）。
        assert!(
            matches!(parse("周华健的音乐和视频"), SearchIntent::MediaSearch(_)),
            "带 artist 应仍 media"
        );
    }

    #[test]
    fn same_category_pair_stays_media() {
        // 「音乐和歌曲」同属 audio（1 类别）→ 不算跨范畴 → 仍 media。
        assert!(
            matches!(parse("找音乐和歌曲"), SearchIntent::MediaSearch(_)),
            "同类别不应触发跨范畴"
        );
    }

    #[test]
    fn single_strong_media_unchanged() {
        // 回归守护：单强媒体词不受影响。
        assert!(matches!(parse("找音乐"), SearchIntent::MediaSearch(_)));
        assert!(matches!(
            parse("找最近的截图"),
            SearchIntent::MediaSearch(_)
        ));
    }
}

/// 简单的中文复合词分割（按常见类目词截断）。覆盖 fixture #15 的 "付款二维码"。
fn split_compound_zh_words(tokens: &[String]) -> Vec<String> {
    let split_anchors: &[&str] = &["二维码", "条形码", "发票", "号码"];
    let mut out: Vec<String> = Vec::new();
    for t in tokens {
        let mut remaining = t.as_str();
        let mut produced = false;
        for anchor in split_anchors {
            if let Some(pos) = remaining.find(anchor) {
                let prefix = &remaining[..pos];
                if !prefix.is_empty() {
                    out.push(prefix.to_owned());
                }
                out.push((*anchor).to_owned());
                remaining = &remaining[pos + anchor.len()..];
                produced = true;
            }
        }
        if !produced && !remaining.is_empty() {
            out.push(remaining.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests_content_clause_routing_beta13_g {
    //! BETA-13-G：截图 + 内容子句（按正文文字搜）→ file_search file_type=[Screenshot] + 干净关键词。
    #![allow(clippy::unwrap_used, clippy::panic)]
    use scout_search_backend::{FileType, SearchIntent};

    #[test]
    fn content_screenshot_routes_to_file_search() {
        for (q, kw) in [
            ("截图里写着已支付的", "已支付"),
            ("截图里写着订单已发货的那张", "订单已发货"),
            ("聊天截图里提到周五开会的", "周五开会"),
        ] {
            let intent = crate::parse(q);
            let SearchIntent::FileSearch(fs) = intent else {
                panic!("{q} 应路由 file_search，实际 {intent:?}");
            };
            assert_eq!(fs.file_type, Some(vec![FileType::Screenshot]), "{q}");
            assert_eq!(fs.keywords, Some(vec![kw.to_owned()]), "{q}");
        }
    }

    #[test]
    fn content_screenshot_en_routes_to_file_search() {
        for (q, kw) in [
            (
                "the screenshot that says payment successful",
                "payment successful",
            ),
            ("screenshots that mention error 404", "error 404"),
        ] {
            let intent = crate::parse(q);
            let SearchIntent::FileSearch(fs) = intent else {
                panic!("{q} 应路由 file_search，实际 {intent:?}");
            };
            assert_eq!(fs.file_type, Some(vec![FileType::Screenshot]), "{q}");
            assert_eq!(fs.keywords, Some(vec![kw.to_owned()]), "{q}");
        }
    }

    #[test]
    fn content_screenshot_both_multi_keyword() {
        use scout_search_backend::{FileType, SearchIntent};
        let cases = [
            ("截图里同时出现订单号和金额的", vec!["订单号", "金额"]),
            (
                "screenshot with both order id and tracking number",
                vec!["order id", "tracking number"],
            ),
        ];
        for (q, kws) in cases {
            let SearchIntent::FileSearch(fs) = crate::parse(q) else {
                panic!("{q} 应 file_search")
            };
            assert_eq!(
                fs.file_type,
                Some(vec![FileType::Screenshot]),
                "{q} file_type"
            );
            assert_eq!(
                fs.keywords,
                Some(kws.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
                "{q} keywords"
            );
        }
    }

    #[test]
    fn content_clause_single_not_split_on_he() {
        use scout_search_backend::SearchIntent;
        let SearchIntent::FileSearch(fs) = crate::parse("截图里写着甲方和乙方的") else {
            panic!("应 file_search")
        };
        assert_eq!(
            fs.keywords,
            Some(vec!["甲方和乙方".to_owned()]),
            "无 both 标记不拆"
        );
    }

    /// 防回归：英文祈使句（无 `that` 锚点）不应被内容子句闸门误捕为**带脏 keyword 的截图 file_search**。
    /// 此前裸 says/mentions/shows 会把 "show me all screenshots" 抽成 keyword "me all screenshots"、
    /// 把 "show screenshots sorted by size" 抽成 "screenshots sorted by size" 并改路由到 file_search。
    /// 修复后内容子句闸门不命中，自然也不会产生脏 keyword 与误路由。
    #[test]
    fn imperative_screenshots_not_misrouted_as_content_clause() {
        for q in ["show me all screenshots", "show screenshots sorted by size"] {
            // 根因：内容子句闸门不应再误触发。
            assert!(
                crate::parsers::media_search::detect_content_clause(q).is_none(),
                "{q} 不应命中内容子句闸门，实际抽出 {:?}",
                crate::parsers::media_search::detect_content_clause(q)
            );
            // 端到端：不应落成内容路由（截图 file_type + 脏 keyword）。
            if let SearchIntent::FileSearch(fs) = crate::parse(q) {
                assert!(
                    fs.file_type != Some(vec![FileType::Screenshot]) || fs.keywords.is_none(),
                    "{q} 不应被内容子句闸门误捕为带脏 keyword 的截图 file_search，实际 keywords={:?}",
                    fs.keywords
                );
            }
        }
    }

    #[test]
    fn split_content_clause_handles_separators_and_degenerate() {
        use super::split_content_clause;
        assert_eq!(split_content_clause("订单号和金额"), vec!["订单号", "金额"]);
        assert_eq!(split_content_clause("订单号、金额"), vec!["订单号", "金额"]); // 顿号
        assert_eq!(
            split_content_clause("order id and tracking number"),
            vec!["order id", "tracking number"]
        );
        assert_eq!(split_content_clause("和"), vec!["和"]); // 病态：全空段回退原短语
        assert!(split_content_clause("  ").is_empty()); // 纯空白→空
    }

    #[test]
    fn en_word_number_time_screenshot_passes() {
        use scout_search_backend::{
            MediaType, RelativeTime, SearchIntent, SortOrder, TimeExpression,
        };
        let SearchIntent::MediaSearch(m) = crate::parse("screenshots from the last three days")
        else {
            panic!("应 media_search");
        };
        assert_eq!(m.media_type, MediaType::Screenshot);
        assert_eq!(
            m.created_time,
            Some(TimeExpression::Relative {
                value: RelativeTime::Last3Days
            })
        );
        assert_eq!(m.sort, Some(SortOrder::CreatedDesc));
        assert_eq!(m.keywords, None, "数字词 three 不应漏成 keyword");
    }

    #[test]
    fn media_path_honors_name_sort() {
        use scout_search_backend::{SearchIntent, SortOrder};
        // 媒体查询显式「按名字排」→ name_asc（对齐 file_search BETA-13-G6）
        let SearchIntent::MediaSearch(m) = crate::parse("上个月的截图按名字排") else {
            panic!("应 media_search");
        };
        assert_eq!(m.sort, Some(SortOrder::NameAsc));
        // 倒序变体（用强媒体词 截图 确保走 media 路径）
        let SearchIntent::MediaSearch(m2) = crate::parse("上个月的截图按名字降序排")
        else {
            panic!("应 media_search");
        };
        assert_eq!(m2.sort, Some(SortOrder::NameDesc));
    }
}

#[cfg(test)]
mod tests_video_routing_v05 {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use crate::parse;
    use scout_search_backend::{MediaType, SearchIntent, SortOrder};

    fn assert_media_video(query: &str) {
        let intent = parse(query);
        match &intent {
            SearchIntent::MediaSearch(ms) => {
                assert_eq!(ms.media_type, MediaType::Video, "query={query}");
            }
            other => panic!("query={query} expected MediaSearch, got {other:?}"),
        }
    }

    #[test]
    fn v05_video_with_concrete_size_zh_routes_to_media() {
        // v05-media-template-243：fixture 期望 media_search
        assert_media_video("找下载目录大于 100MB的视频");
    }

    #[test]
    fn v05_video_with_concrete_size_en_routes_to_media() {
        // v05-media-template-283
        assert_media_video("find over 100MB videos in downloads");
    }

    // v0.5 修订设计：fixture 内部 dual route 分析（25 MS > 10 FS）显示 "video + 时间" 应保留
    // v0.4 的 → MediaSearch 路由。`v05_video_with_time_only_*_routes_to_file` 两个测试已删除，
    // file-template-084 / 161 等 10 个 outlier 为已知遗留，不再规划专门修复（spec §7 不做清单）。

    #[test]
    fn v05_video_with_zui_zhong_sort_routes_to_media() {
        // v05-media-class1-sort-056："最重" 应触发 media_search + sort=size_desc
        let intent = parse("找最重的视频");
        match &intent {
            SearchIntent::MediaSearch(ms) => {
                assert_eq!(ms.media_type, MediaType::Video);
                assert_eq!(ms.sort, Some(SortOrder::SizeDesc));
            }
            other => panic!("expected MediaSearch, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests_beta13_g12_image_routing {
    //! BETA-13-G12 ②′：image + 约束（time/size/location）→ file_search（file_type=image），
    //! 不进 media（§1.1 决策：video/screenshot 留 media，image 是 carve-out→file）。
    #![allow(clippy::unwrap_used, clippy::panic)]
    use crate::parse;
    use scout_search_backend::{FileType, SearchIntent, SizeExpression, SizeUnit};

    fn fs(q: &str) -> scout_search_backend::FileSearch {
        match parse(q) {
            SearchIntent::FileSearch(fs) => fs,
            other => panic!("query={q} expected FileSearch, got {other:?}"),
        }
    }

    #[test]
    fn image_with_time_routes_to_file() {
        // v09-d5-zh-015
        let f = fs("创建于上个月的图片");
        assert_eq!(f.file_type, Some(vec![FileType::Image]));
        assert!(f.created_time.is_some(), "应有 created_time，实得 {f:?}");
    }

    #[test]
    fn image_with_location_and_size_routes_to_file() {
        // v09-d5-mixed-004
        let f = fs("桌面上 smaller than 1MB 的图片");
        assert_eq!(f.file_type, Some(vec![FileType::Image]));
        assert_eq!(
            f.location.as_ref().and_then(|l| l.hint.clone()),
            Some("桌面".to_owned())
        );
        assert_eq!(
            f.size,
            Some(SizeExpression::LessThan {
                value: 1.0,
                unit: SizeUnit::Mb
            })
        );
    }

    #[test]
    fn screenshot_directory_is_location_not_media() {
        // v09-d5-zh-029：「截图目录」= 位置（截图夹），不是搜截图本身。
        let f = fs("截图目录里的图片");
        assert_eq!(f.file_type, Some(vec![FileType::Image]));
        assert_eq!(
            f.location.as_ref().and_then(|l| l.hint.clone()),
            Some("截图".to_owned())
        );
    }

    #[test]
    fn video_with_size_still_routes_to_media() {
        // 守护：§1.1 决策——video+size 仍 media（不被 image 守护误伤）。
        assert!(
            matches!(parse("找最重的视频"), SearchIntent::MediaSearch(_)),
            "video+size 应保持 media"
        );
    }
}
