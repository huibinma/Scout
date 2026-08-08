//! 用户输入语言识别（zh / en / mixed / unknown）。

use scout_search_backend::Language;

/// 按字符统计判断语言。zh+常见 ASCII 技术名词（文件扩展名 / 大小单位 / 已知短词）
/// 不再触发 mixed —— 这些 token 在中文输入里属于专有名词性质，不算外语成分。
///
/// 规则：
/// - 把输入中匹配 [`ASCII_NEUTRAL_TOKENS`] 的子串先替换为空
/// - 再扫描剩余字符：含 CJK + 含 ASCII 字母 → Mixed；只含 CJK → Zh；只含 ASCII 字母 → En；都没有 → Unknown
#[must_use]
pub fn detect(input: &str) -> Language {
    let scrubbed = scrub_neutral_tokens(input);

    let mut has_cjk = false;
    let mut has_ascii_letter = false;

    for ch in scrubbed.chars() {
        if is_cjk(ch) {
            has_cjk = true;
        } else if ch.is_ascii_alphabetic() {
            has_ascii_letter = true;
        }
        if has_cjk && has_ascii_letter {
            return Language::Mixed;
        }
    }

    match (has_cjk, has_ascii_letter) {
        (true, true) => Language::Mixed,
        (true, false) => Language::Zh,
        (false, true) => Language::En,
        (false, false) => Language::Unknown,
    }
}

/// 在 zh + ASCII 混合判定前，把这些 token 视为"中性词"（不算外语成分）。
/// 范围严格控制：文件扩展名 / 大小单位 / 一组高频技术专名。
/// 添加新词需先在 evals 上量化是否会把真实 mixed query 误判为 zh。
const ASCII_NEUTRAL_TOKENS: &[&str] = &[
    // 文件扩展名 / 文件类型词
    "ppt",
    "pptx",
    "doc",
    "docx",
    "xls",
    "xlsx",
    "pdf",
    "txt",
    "md",
    "zip",
    "rar",
    "mp4",
    "mp3",
    "mov",
    "avi",
    "png",
    "jpg",
    "jpeg",
    "markdown",
    "excel",
    "word",
    "powerpoint",
    // 大小单位（含数字粘连后的纯字母部分；数字会被独立跳过）
    "kb",
    "mb",
    "gb",
    "tb",
];

fn scrub_neutral_tokens(input: &str) -> String {
    use regex::Regex;
    use std::sync::OnceLock;

    let mut s = input.to_lowercase();
    // v0.5：先去掉 hyphenated ASCII identifier（如 synthetic-place / synthetic-artist
    // / project-final-v2）。这类 token 通常是占位符 / 名字 / 路径片段，不算英文内容词，
    // 不应让 zh query 误判 mixed。单连字符 + 全字母即匹配。
    static RE_HYPHENATED: OnceLock<Regex> = OnceLock::new();
    let re = RE_HYPHENATED.get_or_init(|| Regex::new(r"[a-z]+(?:-[a-z]+)+").expect("regex valid"));
    s = re.replace_all(&s, " ").into_owned();

    // 再按长 token 顺序替换 ASCII 白名单避免短词覆盖长词
    let mut tokens: Vec<&&str> = ASCII_NEUTRAL_TOKENS.iter().collect();
    tokens.sort_by_key(|t| std::cmp::Reverse(t.len()));
    for t in tokens {
        s = s.replace(t, " ");
    }
    // 再把"100mb"残留的纯数字 + 残留空白都过掉
    s.chars().filter(|c| !c.is_ascii_digit()).collect()
}

/// 简化版 CJK 范围（统一表意 / 扩展 A / 兼容表意 / 假名 / 谚文）。
fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x4E00..=0x9FFF        // CJK Unified Ideographs
        | 0x3400..=0x4DBF      // CJK Extension A
        | 0xF900..=0xFAFF      // CJK Compatibility Ideographs
        | 0x3040..=0x309F      // Hiragana
        | 0x30A0..=0x30FF      // Katakana
        | 0xAC00..=0xD7AF      // Hangul Syllables
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn chinese_only_is_zh() {
        assert_eq!(detect("查找昨天编辑过的"), Language::Zh);
    }

    #[test]
    fn english_only_is_en() {
        assert_eq!(detect("find ppt yesterday"), Language::En);
    }

    #[test]
    fn mixed_returns_mixed() {
        assert_eq!(detect("找我 yesterday 改过的 ppt"), Language::Mixed);
    }

    #[test]
    fn pure_digits_is_unknown() {
        assert_eq!(detect("12345"), Language::Unknown);
    }

    #[test]
    fn zh_with_file_extension_token_is_zh() {
        assert_eq!(detect("查找昨天编辑过的 ppt"), Language::Zh);
        assert_eq!(detect("找最近三天修改的 Excel"), Language::Zh);
        assert_eq!(detect("找桌面上的 word 文档"), Language::Zh);
        assert_eq!(detect("找上周收到的 pdf"), Language::Zh);
    }

    #[test]
    fn zh_with_size_unit_token_is_zh() {
        assert_eq!(detect("找下载目录中大于 100MB 的视频"), Language::Zh);
        assert_eq!(detect("找过去一个月里大于 1GB 的视频"), Language::Zh);
    }

    #[test]
    fn zh_with_unknown_english_word_stays_mixed() {
        // budget / final 是 English content word（无连字符），仍按 mixed 处理
        assert_eq!(detect("找名字里有 budget 的文件"), Language::Mixed);
        assert_eq!(detect("把第三个改名为 final"), Language::Mixed);
    }

    #[test]
    fn zh_with_hyphenated_identifier_is_zh_v05() {
        // v0.5：带连字符的 ASCII token（synthetic-* / 任何 hyphenated identifier）视为占位 token，
        // 不触发 mixed —— fixture 模板生成器用这种命名约定区分"标识符 vs 内容词"。
        assert_eq!(detect("找 synthetic-place 里的文件"), Language::Zh);
        assert_eq!(detect("找 synthetic-artist 的歌"), Language::Zh);
        assert_eq!(detect("把第5个改名为 synthetic-final"), Language::Zh);
        // 带全角引号的 hyphenated
        assert_eq!(
            detect("找上周桌面名字里有「synthetic-plan」的文件"),
            Language::Zh
        );
    }

    #[test]
    fn pure_english_hyphenated_unaffected() {
        // 纯英文 query 含 hyphenated identifier 仍 en
        assert_eq!(detect("find synthetic-place files"), Language::En);
    }

    #[test]
    fn pure_english_unaffected() {
        assert_eq!(detect("find ppt yesterday"), Language::En);
        assert_eq!(detect("show files larger than 100mb"), Language::En);
    }
}
