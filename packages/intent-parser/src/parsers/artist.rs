//! artist 抽取簇：从 `MediaSearch` 查询里识别/提取艺人名（从 media_search.rs 拆出，零行为变化）。
#![allow(clippy::pedantic, clippy::expect_used, clippy::while_let_on_iterator)]

use crate::lexicon;

/// 是否含自由 artist 结构（用于路由 bool 判定，不抽取具体值）。
pub(crate) fn has_free_artist_structure(lower: &str) -> bool {
    use regex::Regex;
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\p{Han}{2,4}\s*(?:的歌|的歌曲|唱的)|(?:songs?|tracks?|music)\s+by\s+\w")
            .expect("regex")
    });
    re.is_match(lower)
}

const KNOWN_ARTISTS_CN: &[&str] = &["周华健"];
const KNOWN_ARTISTS_EN: &[&str] = &["eric clapton"];

pub(crate) fn contains_known_artist(lower: &str) -> bool {
    KNOWN_ARTISTS_CN.iter().any(|a| lower.contains(a))
        || KNOWN_ARTISTS_EN.iter().any(|a| lower.contains(a))
}

pub(crate) fn extract_artist(input: &str, lower: &str) -> Option<String> {
    for a in KNOWN_ARTISTS_CN {
        if input.contains(a) {
            return Some((*a).to_owned());
        }
    }
    for a in KNOWN_ARTISTS_EN {
        if lower.contains(a) {
            return Some(title_case(a));
        }
    }
    // Bucket E: 结构化识别 "X 的歌/音乐/歌曲" / "X's songs/music"
    extract_artist_by_structure(input)
}

/// 结构化提取 artist：从 "X 的歌/音乐/歌曲" / "X's songs/music" 等模式提取 X。
///
/// X 必须是连续的非空白字符片段（允许 `-` / `_`），且不在停用词列表里。
fn extract_artist_by_structure(input: &str) -> Option<String> {
    use regex::Regex;
    use std::sync::OnceLock;

    // BETA-13-G Fix2 (C)：「叫 X（的歌）」抑制——X 是歌名/标题，不是 artist。
    // 命中即整体放弃结构化 artist 抽取（避免「找一首叫 七里香 的歌」误抽 七里香）。
    // 注：`叫\s*\S` 是 unanchored（不锚定句首/上下文），故 failure mode 是"漏抽 artist"
    // （benign，标题路径仍可兜）而非"误抽"，因此这种宽匹配可接受。
    static RE_JIAO: OnceLock<Regex> = OnceLock::new();
    let re_jiao = RE_JIAO.get_or_init(|| Regex::new(r"叫\s*\S").expect("regex"));
    if re_jiao.is_match(input) {
        return None;
    }

    // BETA-13-G Fix2 (A)：抽取前剥前缀。先剥句首动词/数量词（lead），再剥位置前缀。
    // 位置前缀只用于「的歌」类规则；`《》` 规则用未剥位置的 lead 串，避免 "专辑里" 的「里」被误剥。
    let lead = strip_lead_prefix(input);
    let cleaned = strip_location_prefix(&lead);
    let cleaned = cleaned.as_str();

    // BETA-13-G Fix2 (D)：英文 "the song … by X"（"play the song Shape of You by Ed Sheeran"）。
    // 取 by 后连续首字母大写词串。须在 RE_EN_BY 之前——此句无 songs/tracks/music 紧邻 by。
    static RE_EN_SONG_BY: OnceLock<Regex> = OnceLock::new();
    let re_en_song_by = RE_EN_SONG_BY.get_or_init(|| {
        Regex::new(r"(?i)the song\b.+?\bby\s+([A-Z][A-Za-z]*(?:\s+[A-Z][A-Za-z]*)*)")
            .expect("regex")
    });
    if let Some(cap) = re_en_song_by.captures(input) {
        let candidate = cap[1].trim();
        if !is_stopword_artist(candidate) {
            return Some(candidate.to_owned());
        }
    }

    // BETA-13-G2：英文 "songs/tracks/music by X"（含 Fix2 (G) "music videos by X"）。
    // X 为一到多个首字母大写的词（"Ed Sheeran" / "Taylor Swift" / "Coldplay" / "Adele"），
    // 或**含连字符的小写 ASCII 标识符**（"synthetic-artist"，v0.5 合成语料形态）——必须
    // 含连字符，裸小写词（"sorted by size" 类残句的 size/name/year）不命中。
    static RE_EN_BY: OnceLock<Regex> = OnceLock::new();
    let re_en_by = RE_EN_BY.get_or_init(|| {
        Regex::new(
            r"(?:songs?|tracks?|music|videos?)\s+by\s+([A-Z][A-Za-z]*(?:\s+[A-Z][A-Za-z]*)*|[a-z0-9_]+(?:-[a-z0-9_]+)+)",
        )
        .expect("regex")
    });
    if let Some(cap) = re_en_by.captures(input) {
        let candidate = cap[1].trim();
        if !is_stopword_artist(candidate) {
            return Some(candidate.to_owned());
        }
    }

    // 英文："X's songs/music/tracks"
    static RE_EN: OnceLock<Regex> = OnceLock::new();
    let re_en = RE_EN
        .get_or_init(|| Regex::new(r"(\S+)'s\s+(?:songs?|music|tracks?|albums?)").expect("regex"));
    if let Some(cap) = re_en.captures(input) {
        let candidate = cap[1].trim();
        if !is_stopword_artist(candidate) {
            return Some(candidate.to_owned());
        }
    }

    // BETA-13-G Fix2 (G)：英文 "X 的 music video(s)" / "X的音乐视频" —— X 在 media 词前。
    // （中文「的」+ 英文/中文 video 词；artist 为前面的连续大写词串或汉字名。）
    static RE_EN_DE_MV: OnceLock<Regex> = OnceLock::new();
    let re_en_de_mv = RE_EN_DE_MV.get_or_init(|| {
        Regex::new(r"([A-Z][A-Za-z]*(?:\s+[A-Z][A-Za-z]*)*)\s*的\s*(?:music videos?|音乐视频)")
            .expect("regex")
    });
    if let Some(cap) = re_en_de_mv.captures(input) {
        let candidate = cap[1].trim();
        if !is_stopword_artist(candidate) {
            return Some(candidate.to_owned());
        }
    }

    // BETA-13-G2 / Fix2 (B)：中文自由 artist —— "X的（…修饰…）歌/歌曲/音乐" / "X唱的"。
    // X 直接接「的」（或「唱的」）；「的」与「歌」之间允许夹修饰（「的爵士风格歌曲」「的无损歌曲」）。
    // X 为完整英文名（连续首字母大写词串）/ ASCII identifier / 2-4 汉字（排除 的歌曲音乐）。
    // 仅在 cleaned 串**句首**锚定 X，避免吞前缀。
    static RE_ZH_FREE: OnceLock<Regex> = OnceLock::new();
    let re_zh_free = RE_ZH_FREE.get_or_init(|| {
        Regex::new(
            r"^\s*([A-Z][A-Za-z]*(?:\s+[A-Z][A-Za-z]*)*|[A-Za-z0-9_\-]+|[\p{Han}&&[^的歌曲音乐]]{2,4})\s*(?:的[^歌]{0,8}?(?:歌曲|歌|音乐)|唱的)",
        )
        .expect("regex")
    });
    if let Some(cap) = re_zh_free.captures(cleaned) {
        let candidate = cap[1].trim();
        if !is_stopword_artist(candidate) {
            return Some(candidate.to_owned());
        }
    }

    // BETA-13-G Fix2 (B)：中文 "X<时长/质量修饰>…的歌"——X 与「的」之间夹 duration/quality 短语
    // （"周杰伦超过4分钟的无损歌曲" / "五月天短于4分钟的歌" / "Coldplay 超过5分钟的歌"）。
    // 锚 X 至**修饰标记词**之前（超过/短于/大于…），再要求其后含「的…歌」。
    static RE_ZH_NAME_MOD: OnceLock<Regex> = OnceLock::new();
    let re_zh_name_mod = RE_ZH_NAME_MOD.get_or_init(|| {
        Regex::new(
            r"^\s*([A-Z][A-Za-z]*(?:\s+[A-Z][A-Za-z]*)*|[A-Za-z0-9_\-]+|[\p{Han}&&[^的歌曲音乐]]{2,4})\s*(?:超过|超出|多于|大于|短于|少于|不到|不足|长于|over|under|longer than|shorter than|more than|less than)[^的]{0,12}?的[^歌]{0,8}?(?:歌曲|歌|音乐)",
        )
        .expect("regex")
    });
    if let Some(cap) = re_zh_name_mod.captures(cleaned) {
        let candidate = cap[1].trim();
        if !is_stopword_artist(candidate) {
            return Some(candidate.to_owned());
        }
    }

    // BETA-13-G Fix2 (E)：中文 "X《…》"（含 "X《…》专辑" / "X《…》专辑里的歌"）—— artist=X。
    static RE_ZH_BRACKET: OnceLock<Regex> = OnceLock::new();
    let re_zh_bracket = RE_ZH_BRACKET
        .get_or_init(|| Regex::new(r"^([\p{Han}]{2,4}|[A-Za-z][A-Za-z ]*?)《").expect("regex"));
    if let Some(cap) = re_zh_bracket.captures(&lead) {
        let candidate = cap[1].trim();
        if !is_stopword_artist(candidate) {
            return Some(candidate.to_owned());
        }
    }

    // BETA-13-G Fix2 (F)：中文 "X 浮夸 这首歌" —— ^X 空格 标题 空格? 这首歌 → artist=X。
    static RE_ZH_THIS_SONG: OnceLock<Regex> = OnceLock::new();
    let re_zh_this_song = RE_ZH_THIS_SONG
        .get_or_init(|| Regex::new(r"^([\p{Han}]{2,4})\s+\S+\s*这首歌").expect("regex"));
    if let Some(cap) = re_zh_this_song.captures(cleaned) {
        let candidate = cap[1].trim();
        if !is_stopword_artist(candidate) {
            return Some(candidate.to_owned());
        }
    }

    // BETA-13-G2：句首 "X songs ..." —— "Adele songs from the album 25" / "Beatles songs under 4 minutes"
    static RE_EN_LEAD: OnceLock<Regex> = OnceLock::new();
    let re_en_lead = RE_EN_LEAD.get_or_init(|| {
        Regex::new(r"^([A-Z][A-Za-z]*(?:\s+[A-Z][A-Za-z]*)*)\s+(?:songs?|tracks?)\b")
            .expect("regex")
    });
    if let Some(cap) = re_en_lead.captures(input.trim()) {
        let candidate = cap[1].trim();
        if !is_stopword_artist(candidate) {
            return Some(candidate.to_owned());
        }
    }

    None
}

/// BETA-13-G Fix2 (A)：剥离 artist 抽取的句首前缀——播放/检索动词 + 数量/指示词。
/// 例："帮我找一下周杰伦的歌"→"周杰伦的歌"、"find synthetic-artist 的歌"→"synthetic-artist 的歌"。
/// 仅用于结构化 artist 抽取，不影响其他字段。**不剥位置前缀**（见 `strip_location_prefix`）。
fn strip_lead_prefix(input: &str) -> String {
    let mut s = input.trim();
    // 句首动词（长词优先，循环剥多重前缀如 "帮我找一下"）。
    const LEAD_VERBS: &[&str] = &[
        "帮我找一下",
        "帮我找",
        "我想找",
        "找一下",
        "找一首",
        "放一首",
        "来一首",
        "找找",
        "想找",
        "搜索",
        "列出",
        "来点",
        "放点",
        "放首",
        "播放",
        "找",
        "搜",
    ];
    // 英文句首动词（大小写不敏感，仅剥词边界，避免吞掉 "finder" 之类）。
    const LEAD_VERBS_EN: &[&str] = &[
        "search for ",
        "find me ",
        "get me ",
        "show me ",
        "play me ",
        "find ",
        "show ",
        "play ",
        "list ",
        "get ",
    ];
    loop {
        let mut stripped = false;
        for v in LEAD_VERBS {
            if let Some(rest) = s.strip_prefix(v) {
                s = rest.trim_start();
                stripped = true;
                break;
            }
        }
        if !stripped {
            let low = s.to_ascii_lowercase();
            for v in LEAD_VERBS_EN {
                if low.starts_with(v) {
                    s = s[v.len()..].trim_start();
                    stripped = true;
                    break;
                }
            }
        }
        if !stripped {
            break;
        }
    }
    // 句首数量/指示词（"一些 / 一首 / 这首 / 几首"）——它们不是 artist 也不是位置。
    const QUANT: &[&str] = &[
        "一些", "一首", "这首", "那首", "几首", "几张", "一张", "这张", "那张",
    ];
    loop {
        let mut stripped = false;
        for q in QUANT {
            if let Some(rest) = s.strip_prefix(q) {
                s = rest.trim_start();
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }
    s.to_owned()
}

/// BETA-13-G Fix2 (A)：剥离位置前缀——取最后一个「目录里/文件夹里/里」之后的片段
/// （"音乐目录里周杰伦的歌"→"周杰伦的歌"）。
/// 排除 "专辑里"/"这里"/"那里"/"哪里" 等非容器用法的「里」，避免误剥
/// （如 "周杰伦《范特西》专辑里的歌" 不应被裸「里」截断）。
fn strip_location_prefix(input: &str) -> String {
    let mut result = input.to_owned();
    // 容器尾词（长词优先）。
    for sep in ["目录里", "文件夹里"] {
        if let Some(pos) = result.rfind(sep) {
            result = result[pos + sep.len()..].trim_start().to_owned();
        }
    }
    // 裸「里」：仅当其前一个字符不是 专辑/这/那/哪 时才剥（避免 "专辑里"、"这里" 误判）。
    if let Some(pos) = result.rfind('里') {
        let before = result[..pos].chars().next_back();
        if !matches!(
            before,
            Some('专') | Some('辑') | Some('这') | Some('那') | Some('哪')
        ) {
            result = result[pos + '里'.len_utf8()..].trim_start().to_owned();
        }
    }
    result
}

pub(crate) fn is_stopword_artist(token: &str) -> bool {
    // 结构 / 代词 / 非质量非流派类停用词（lexicon 不覆盖，手列）。
    const STOP: &[&str] = &[
        "我",
        "你",
        "他",
        "她",
        "的",
        "了",
        "我的",
        "你的",
        "他的",
        "她的",
        "i",
        "me",
        "you",
        "his",
        "her",
        "find",
        "show",
        // 质量 / 流派词由下方 lexicon 单一来源过滤（QUALITY_KEYWORDS / GENRE_KEYWORDS），
        // 此处仅保留 lexicon 未覆盖的衍生形/相关词，避免误判为 artist。
        "无损格式",
        "格式",
        "高清",
        "经典",
        "好听",
        "喜欢",
        "推荐",
        "最近",
        "所有",
        "全部",
        "这些",
        "那些",
        "一些",
        "一首",
        "这首",
        "那首",
        "分钟",
        "小时",
        "分",
        "秒",
        "钟头",
        "音频",
        "时长",
    ];
    if STOP.iter().any(|s| s.eq_ignore_ascii_case(token)) {
        return true;
    }
    // 质量词（无损 / 高品质 / 高质量 / lossless / high quality / hi-res…）单一来源：lexicon::QUALITY_KEYWORDS。
    if lexicon::QUALITY_KEYWORDS
        .iter()
        .any(|(kws, _)| kws.iter().any(|k| k.eq_ignore_ascii_case(token)))
    {
        return true;
    }
    // 流派词（轻音乐 / 爵士 / 摇滚 / rock / pop…）单一来源：lexicon::GENRE_KEYWORDS。
    if lexicon::GENRE_KEYWORDS
        .iter()
        .any(|(kws, _)| kws.iter().any(|k| k.eq_ignore_ascii_case(token)))
    {
        return true;
    }
    false
}

fn title_case(s: &str) -> String {
    s.split(' ')
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
