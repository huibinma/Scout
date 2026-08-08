//! 词典与同义词表。
//!
//! 集中存放扩展名 / 文件类型 / 位置 / 排序 / 媒体类型 / 文件操作的关键词映射。
//! 输入做了小写化（ASCII 部分）后再用 [`contains`] 类匹配。

use scout_search_backend::{FileActionKind, FileType, MediaType, SortOrder};

// ============================================================
// 扩展名 / 文件类型
// ============================================================

/// 一组关键词命中后，应输出的扩展名列表 + 文件类型。
#[derive(Debug)]
pub struct ExtensionAlias {
    pub keywords: &'static [&'static str],
    pub extensions: &'static [&'static str],
    pub file_type: FileType,
}

/// 关键词 → 扩展名 / 文件类型表。
///
/// 命中顺序：从前到后，第一条命中即返回（更具体的放前面）。
pub const EXTENSION_ALIASES: &[ExtensionAlias] = &[
    ExtensionAlias {
        keywords: &["pptx"],
        extensions: &["pptx"],
        file_type: FileType::Presentation,
    },
    ExtensionAlias {
        keywords: &["ppt", "powerpoint"],
        extensions: &["ppt", "pptx"],
        file_type: FileType::Presentation,
    },
    ExtensionAlias {
        // BETA-13-G3：中文/范畴类型词（非字面扩展名）只给 file_type，不带具体扩展名
        // （覆盖标注约定：类型词表达范畴，扩展名由 file_type 驱动）。「ppt/pptx」等
        // 字面格式词仍走上面带扩展名的 alias（v0.5 锚点）。
        keywords: &[
            "演示文稿",
            "幻灯片",
            "presentation",
            "presentations",
            "slides",
            "slide",
        ],
        extensions: &[],
        file_type: FileType::Presentation,
    },
    ExtensionAlias {
        keywords: &["xlsx"],
        extensions: &["xlsx"],
        file_type: FileType::Spreadsheet,
    },
    ExtensionAlias {
        keywords: &["xls", "excel"],
        extensions: &["xls", "xlsx"],
        file_type: FileType::Spreadsheet,
    },
    ExtensionAlias {
        // BETA-13-G3：中文/范畴类型词 → file_type，不带扩展名
        keywords: &["电子表格", "表格", "spreadsheet", "spreadsheets"],
        extensions: &[],
        file_type: FileType::Spreadsheet,
    },
    ExtensionAlias {
        keywords: &["docx"],
        extensions: &["docx"],
        file_type: FileType::Document,
    },
    ExtensionAlias {
        keywords: &["doc", "word", "word 文档"],
        extensions: &["doc", "docx"],
        file_type: FileType::Document,
    },
    ExtensionAlias {
        // BETA-13-G12：补英文复数 `pdfs`（word_present 词边界使 `pdf` 不匹配 `pdfs`）。
        keywords: &["pdf", "pdfs"],
        extensions: &["pdf"],
        file_type: FileType::Document,
    },
    ExtensionAlias {
        keywords: &["md", "markdown"],
        extensions: &["md"],
        file_type: FileType::Document,
    },
    ExtensionAlias {
        keywords: &["txt"],
        extensions: &["txt"],
        file_type: FileType::Document,
    },
    ExtensionAlias {
        keywords: &["zip"],
        extensions: &["zip"],
        file_type: FileType::Archive,
    },
    ExtensionAlias {
        // BETA-13-G12：补英文复数 `archives`（v0.5 无此词，byte-equal 安全）。
        keywords: &["rar", "7z", "tar", "gz", "压缩包", "archive", "archives"],
        extensions: &[],
        file_type: FileType::Archive,
    },
    ExtensionAlias {
        keywords: &["mp4"],
        extensions: &["mp4"],
        file_type: FileType::Video,
    },
    ExtensionAlias {
        keywords: &[
            "mov", "avi", "mkv", "视频", "video", "videos", "影片", "movies",
        ],
        extensions: &[],
        file_type: FileType::Video,
    },
    ExtensionAlias {
        keywords: &["mp3", "flac", "wav", "m4a", "ape", "ogg", "aac"],
        extensions: &[],
        file_type: FileType::Audio,
    },
    ExtensionAlias {
        keywords: &["音乐", "歌", "歌曲", "music", "audio", "song"],
        extensions: &[
            "mp3", "flac", "wav", "m4a", "ape", "ogg", "aac", "wma", "aiff",
        ],
        file_type: FileType::Audio,
    },
    ExtensionAlias {
        // BETA-13-G3：「音频」自然类型词 → file_type，不带具体扩展名
        // （区别于上面「音乐/歌」带扩展名的 media 词）。
        keywords: &["音频"],
        extensions: &[],
        file_type: FileType::Audio,
    },
    ExtensionAlias {
        keywords: &["png"],
        extensions: &["png"],
        file_type: FileType::Image,
    },
    ExtensionAlias {
        keywords: &["jpg", "jpeg"],
        extensions: &["jpg"],
        file_type: FileType::Image,
    },
    ExtensionAlias {
        // BETA-13-G3：补「照片/相片/photos/photo」自然类型词 + 「image」单数
        keywords: &[
            "图片", "image", "images", "pictures", "照片", "相片", "photos", "photo",
        ],
        extensions: &[],
        file_type: FileType::Image,
    },
    ExtensionAlias {
        keywords: &["截图", "截屏", "screenshot", "screenshots"],
        extensions: &[],
        file_type: FileType::Screenshot,
    },
    ExtensionAlias {
        // BETA-13-G3：「代码/代码文件」自然类型词 → Code。英文用短语形式
        // （"code file(s)" / "source code"）规避「verification code / QR code」误命中。
        keywords: &[
            "代码",
            "代码文件",
            "源代码",
            "code file",
            "code files",
            "source code",
        ],
        extensions: &[],
        file_type: FileType::Code,
    },
    ExtensionAlias {
        // BETA-13-G3：「可执行/可执行文件」自然类型词 → Executable。
        keywords: &[
            "可执行文件",
            "可执行程序",
            "可执行",
            "executable",
            "executables",
        ],
        extensions: &[],
        file_type: FileType::Executable,
    },
    ExtensionAlias {
        // BETA-09 后续 fix：移除英文 "document" / "documents" — 它们更常作位置词
        // （fixture 0 case 把英文 documents 当 file_type，5 个 case 把它当
        // location）。中文 "文档" 是真 file_type trigger，保留
        // （fixture v05-schema-7-007 期望）。
        keywords: &["文档"],
        extensions: &[],
        file_type: FileType::Document,
    },
];

// ============================================================
// 位置 hint
// ============================================================

/// 一组关键词命中后，应输出的位置 hint。
/// 中文 / 英文两条 canonical，由调用方按 language 选择
/// （与 ROADMAP §4.3 hint→include 解析表对齐）。
#[derive(Debug)]
pub struct LocationAlias {
    pub keywords: &'static [&'static str],
    pub zh_hint: &'static str,
    pub en_hint: &'static str,
}

pub const LOCATION_ALIASES: &[LocationAlias] = &[
    LocationAlias {
        keywords: &["下载目录", "下载", "downloads", "download folder"],
        zh_hint: "下载",
        en_hint: "downloads",
    },
    LocationAlias {
        keywords: &["桌面", "desktop"],
        zh_hint: "桌面",
        en_hint: "desktop",
    },
    LocationAlias {
        keywords: &["文稿", "文档目录", "documents"],
        zh_hint: "文稿",
        en_hint: "documents",
    },
    LocationAlias {
        // BETA-13-G14 B2：补「图片文件夹」folder 形（v0.5 无此串→byte-equal 安全）。
        keywords: &["图片目录", "图片文件夹", "pictures"],
        zh_hint: "图片",
        en_hint: "pictures",
    },
    LocationAlias {
        // 注：移除 "videos" / "movies" — 它们更常作为 media_type 触发词
        // （fixture 中 "find videos modified..." 期望不出 location）。
        // "影片目录" 是 macOS 默认 ~/Movies 中文名，作为显式 location 词保留。
        // BETA-13-G14 B2：补「影片文件夹」folder 形。
        keywords: &["影片目录", "影片文件夹", "movies folder"],
        zh_hint: "影片",
        en_hint: "movies",
    },
    LocationAlias {
        // 「music 目录」：en 目录名 + 中文容器词的 mixed 形态（d5-mixed-010 锚），
        // hint 形态由 common::alias_name_part_is_ascii 判定（名字部分 ascii → en_hint）。
        keywords: &["音乐目录", "music 目录", "music folder"],
        zh_hint: "音乐",
        en_hint: "music",
    },
    LocationAlias {
        keywords: &["截屏目录", "screenshots folder"],
        zh_hint: "截屏",
        en_hint: "screenshots",
    },
    LocationAlias {
        // BETA-13-G12 ②′：「截图目录/截图文件夹/截图夹」= 截图所在文件夹（location），
        // 区别于「截屏目录」(zh_hint 截屏)；coverage v09-d5-zh-029 期望 hint=截图。
        keywords: &["截图目录", "截图文件夹", "截图夹"],
        zh_hint: "截图",
        en_hint: "screenshots",
    },
];

// ============================================================
// 排序
// ============================================================

/// 排序关键词。v0.2.1 起加入"最大的 / 最重 / biggest / 最新 / 最旧" 等
/// Class A 同义词最高级排序词；命中优先级高于上下文默认（time / size 字段
/// 隐含的 sort），由 `decide_sort` 在 lib.rs 中先查 [`SORT_ALIASES`] 再走默认。
pub const SORT_ALIASES: &[(&[&str], SortOrder)] = &[
    // 大小排序：用户明确说"最大的 / 最重 / biggest"
    (
        &[
            "最大的",
            "最大",
            "最重",
            "体积最大",
            "biggest",
            "largest",
            "按大小倒序",
            "by size desc",
            "sort by size desc",
        ],
        SortOrder::SizeDesc,
    ),
    (
        &["最小", "最小的", "smallest", "按大小正序", "by size asc"],
        SortOrder::SizeAsc,
    ),
    // 时间排序：用户明确说"最新 / 最近的 / newest / 最旧 / oldest"
    (
        &[
            "最新",
            "最新的",
            "最近的",
            "最近编辑",
            "最近修改",
            "newest",
            "most recent",
        ],
        SortOrder::ModifiedDesc,
    ),
    (
        // BETA-13-G14 C3：用户拍板「oldest = 创建时间升序」（文件「年龄」最自然=创建时间）。
        // v0.5 零 oldest 锚点 → byte-equal 安全。
        &["最旧", "最旧的", "oldest", "earliest"],
        SortOrder::CreatedAsc,
    ),
    // 名称排序
    (&["按名称", "by name"], SortOrder::NameAsc),
];

// ============================================================
// 媒体类型词
// ============================================================

pub const MEDIA_TYPE_KEYWORDS: &[(&[&str], MediaType)] = &[
    (
        &["截图", "截屏", "screenshot", "screenshots"],
        MediaType::Screenshot,
    ),
    // BETA-13-G2：「视频」类放在 audio 之前——「音乐视频/music video」是 video 单概念，
    // 应判 Video 而非 Audio。
    (
        &[
            "音乐视频",
            "music video",
            "视频",
            "video",
            "videos",
            "影片",
            "movies",
        ],
        MediaType::Video,
    ),
    (
        &[
            "音乐", "music", "歌曲", "歌", "song", "songs", "audio", "track", "tracks",
        ],
        MediaType::Audio,
    ),
    (&["图片", "images", "pictures"], MediaType::Image),
];

// ============================================================
// 媒体质量
// ============================================================

pub const QUALITY_KEYWORDS: &[(&[&str], scout_search_backend::Quality)] = &[
    (
        &["无损", "lossless"],
        scout_search_backend::Quality::Lossless,
    ),
    // BETA-13-G2：高品质 / high quality → High
    (
        &["高品质", "高质量", "high quality", "hi-res", "hires"],
        scout_search_backend::Quality::High,
    ),
];

// ============================================================
// 音乐流派（BETA-13-G2）
// ============================================================

/// 关键词 → 规范流派名。命中即作为 `genre` 字段。中文词较具体、可作路由触发词；
/// 英文短词（rock/pop/rap…）有歧义，路由由调用方加 "music/songs/some" 上下文门控。
pub const GENRE_KEYWORDS: &[(&[&str], &str)] = &[
    (&["爵士"], "爵士"),
    (&["摇滚"], "摇滚"),
    (&["古典"], "古典"),
    (&["民谣"], "民谣"),
    (&["轻音乐"], "轻音乐"),
    (&["说唱"], "说唱"),
    (&["嘻哈"], "嘻哈"),
    (&["电子乐"], "电子"),
    (&["蓝调"], "蓝调"),
    (&["乡村音乐"], "乡村"),
    (&["金属乐"], "金属"),
    (&["hip hop", "hip-hop"], "hip hop"),
    (&["jazz"], "jazz"),
    (&["rock"], "rock"),
    (&["classical"], "classical"),
    (&["folk"], "folk"),
    (&["blues"], "blues"),
    (&["country"], "country"),
    (&["metal"], "metal"),
    (&["electronic"], "electronic"),
    (&["rap"], "rap"),
    (&["pop"], "pop"),
];

// ============================================================
// 文件操作
// ============================================================

pub const FILE_ACTION_KEYWORDS: &[(&[&str], FileActionKind)] = &[
    (
        &[
            "在访达里显示",
            "在访达中显示",
            "show in finder",
            "show ... in finder",
            "reveal in finder",
        ],
        FileActionKind::Locate,
    ),
    (&["打开", "open"], FileActionKind::Open),
    (&["复制到", "copy to"], FileActionKind::Copy),
    (
        &["改名为", "改名", "重命名", "rename"],
        FileActionKind::Rename,
    ),
    (&["移动到", "move to"], FileActionKind::Move),
    (
        &["删除", "删掉", "delete", "remove"],
        FileActionKind::Delete,
    ),
];
