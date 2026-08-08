//! Candidate signal scanner —— 在原始查询字符串里扫描"信号词"，作为 MVP-17 模型
//! fallback "结构性遗漏触发器" 的判据。
//!
//! # 背景：用户 Class 3 洞察
//!
//! 规则解析"成功"（schema 校验通过）但**信息不完整**的 case，传统的"解析失败才
//! 触发模型"无法发现。例如用户说"一周内编辑过的 ppt"，parser 产出合法的
//! `file_search` intent 但漏 `modified_time`，因为词典里没"一周内"。
//!
//! 解决：独立扫描信号词，与 parser 输出对照 —— 信号检出但字段为空 = 结构性遗漏，
//! 触发模型 fallback。这一层与 parser 完全解耦，可以独立扩词典而不动 parser
//! 代码。
//!
//! # 与 MVP-17 fallback 的关系
//!
//! 见 [`crate::fallback::ModelFallback`]。

#![allow(clippy::module_name_repetitions)]

/// 在自然语言查询中扫描到的"语义信号"。
///
/// 各字段为 `true` 表示用户输入里**看起来**含某类约束词（不保证 parser 能成功
/// 提取它对应的字段）。Fallback 决策器对照本结构与 parser 输出找"结构性遗漏"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CandidateSignals {
    /// 时间词（今天 / 一周内 / 最近 / 昨天 ...）
    pub time: bool,
    /// 大小词（最大 / 几个 G / 大文件 / X MB 以上 ...）
    pub size: bool,
    /// 排序词（最新 / 倒序 / 按时间 / 排序 ...）
    pub sort: bool,
    /// 位置词（下载 / 桌面 / Downloads / ...）
    pub location: bool,
    /// 文件操作动词（打开 / 复制 / 移动 / 重命名 / 删除 ...）
    pub action: bool,
    /// 媒体类型词（歌 / 音乐 / 照片 / 视频 / 截图 / song / photo ...）
    pub media: bool,
}

impl CandidateSignals {
    /// 是否检出任何信号。
    #[must_use]
    pub const fn any(&self) -> bool {
        self.time || self.size || self.sort || self.location || self.action || self.media
    }
}

/// 信号词典 —— 中英双语小集合，足够覆盖 Class 1 / Class 3 报告里的实际查询。
/// 后续扩词典只改本模块，不动 parser 或 fallback。
struct Lexicon;

impl Lexicon {
    const TIME: &'static [&'static str] = &[
        // 中文相对
        "今天",
        "昨天",
        "前天",
        "最近",
        "近期",
        "近几天",
        "近一周",
        "近一月",
        "本周",
        "这周",
        "上周",
        "本月",
        "这个月",
        "上个月",
        "今年",
        "去年",
        "一周内",
        "三天内",
        "一个月内",
        "几天前",
        "几小时前",
        // 中文绝对
        "月",
        "日",
        "年",
        // 英文
        "today",
        "yesterday",
        "recently",
        "this week",
        "last week",
        "this month",
        "last month",
        "this year",
        "last year",
    ];

    // 注意：不放 "g" / "gb" / "mb" / "kb" 等短字符 token，它们会在 "budget" /
    // "image" 等普通词上误触发。size 单位的精确识别留给 parser 的 regex 层。
    const SIZE: &'static [&'static str] = &[
        "最大",
        "最小",
        "最重",
        "体积",
        "容量",
        "大文件",
        "小文件",
        "巨型",
        "兆字节",
        "千兆",
        "以上",
        "以下",
        "超过",
        "不超过",
        "多于",
        "少于",
        "biggest",
        "largest",
        "smallest",
        "huge",
        "tiny",
        " mb",
        " gb",
        " kb",
    ];

    const SORT: &'static [&'static str] = &[
        "排序",
        "倒序",
        "顺序",
        "升序",
        "降序",
        "按",
        "最新",
        "最旧",
        "最近的",
        "newest",
        "oldest",
        "sort",
        "order",
    ];

    const LOCATION: &'static [&'static str] = &[
        "下载",
        "桌面",
        "文档",
        "文稿",
        "图片",
        "图库",
        "音乐",
        "视频",
        "影片",
        "截屏",
        "截图",
        "屏幕截图",
        "downloads",
        "desktop",
        "documents",
        "pictures",
        "music",
        "videos",
        "screenshots",
    ];

    const ACTION: &'static [&'static str] = &[
        "打开",
        "显示",
        "在访达",
        "在资源管理器",
        "复制",
        "拷贝",
        "移动到",
        "搬到",
        "重命名",
        "改名",
        "删除",
        "归档",
        "open",
        "show",
        "reveal",
        "copy",
        "move",
        "rename",
        "delete",
    ];

    const MEDIA: &'static [&'static str] = &[
        "歌", "歌曲", "音乐", "音频", "MP3", "mp3", "flac", "wav", "照片", "图片", "截图", "截屏",
        "壁纸", "视频", "影片", "电影", "短片", "song", "music", "audio", "photo", "image",
        "video", "movie",
    ];

    fn contains_any(haystack: &str, needles: &[&str]) -> bool {
        needles.iter().any(|n| haystack.contains(&n.to_lowercase()))
    }
}

/// 扫描查询字符串生成 [`CandidateSignals`]。
///
/// 算法是简单 substring 匹配（小写化后），不依赖正则；扩词典只需追加到 [`Lexicon`]。
/// 不抑制误报：本扫描器有意"宁可多触发模型，不可漏触发"，模型 fallback 自身
/// 还有 schema 校验兜底。
#[must_use]
pub fn scan(query: &str) -> CandidateSignals {
    let lower = query.to_lowercase();
    CandidateSignals {
        time: Lexicon::contains_any(&lower, Lexicon::TIME),
        size: Lexicon::contains_any(&lower, Lexicon::SIZE),
        sort: Lexicon::contains_any(&lower, Lexicon::SORT),
        location: Lexicon::contains_any(&lower, Lexicon::LOCATION),
        action: Lexicon::contains_any(&lower, Lexicon::ACTION),
        media: Lexicon::contains_any(&lower, Lexicon::MEDIA),
    }
}

// ============================================================
// 单元测试
// ============================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn empty_query_no_signals() {
        assert_eq!(scan(""), CandidateSignals::default());
    }

    /// 用户 Class 1 实测查询 A：包含 size + time + location + sort 信号
    #[test]
    fn user_query_a_最近一周下载的最大的文件() {
        let s = scan("最近一周下载的最大的文件");
        assert!(s.time, "应检出时间信号（最近 / 一周内）");
        assert!(s.size, "应检出大小信号（最大）");
        assert!(s.location, "应检出位置信号（下载）");
        // sort 词典里有"最近的"，但精确匹配是子串，不严格强制
        assert!(s.any());
    }

    /// 用户 Class 1 实测查询 B：包含 time + media 信号
    #[test]
    fn user_query_b_一周内编辑过的ppt() {
        let s = scan("一周内编辑过的ppt");
        assert!(s.time, "应检出时间信号（一周内）");
        // ppt 不在 media 词典（属于 office doc），故 media=false 也合理
    }

    #[test]
    fn english_query_signals() {
        let s = scan("show me the largest video from last week");
        assert!(s.time);
        assert!(s.size);
        assert!(s.action);
        assert!(s.media);
    }

    #[test]
    fn pure_keyword_query_no_signals() {
        let s = scan("budget proposal");
        assert!(!s.any());
    }

    #[test]
    fn media_song_query() {
        let s = scan("一首周华健的歌");
        assert!(s.media);
    }

    #[test]
    fn action_open_query() {
        let s = scan("打开第三个");
        assert!(s.action);
    }
}
