//! BETA-43 验收 ①②：出处定位纯函数——关键词上下文窗口（片段级返回）+
//! `document_passages` 命中回页（复用 BETA-35 来源映射）。
//!
//! 全部走 **Rust 字符级 contains** 而非 FTS5 `snippet()`：
//! - trigram tokenizer 对 2 字 CJK 词结构性 0 命中（BETA-42 背景），字符级匹配无此限制；
//! - 纯函数无 db 依赖，可直接单测（页级定位不必造真扫描 PDF）。
//!
//! 大小写折叠用**逐字符首映射**（`char::to_lowercase().next()`）保持与原文 1:1
//! 对齐——`str::to_lowercase` 可能改变字符数（ß→ss），窗口切片会错位。

use scout_indexer::PagePassage;
use scout_search_backend::KeywordGroup;

/// 单个命中窗口两侧的上下文字符数（search 出处片段用，短）。
pub const SEARCH_CONTEXT_CHARS: usize = 80;
/// `read_document` 片段模式的窗口上下文字符数（读取场景给更宽窗口）。
pub const READ_CONTEXT_CHARS: usize = 200;
/// `read_document` 片段模式最多返回的窗口数。
pub const MAX_READ_WINDOWS: usize = 5;
/// 命中页号列表上限（防超长扫描件刷屏）。
pub const MAX_PAGES: usize = 10;

/// 候选查询路径：原值 + 去除 Windows 扩展长度前缀（`\\?\` / `\\?\UNC\`）的形式。
/// 镜像 desktop `search/preview.rs::lookup_candidates`——本地索引 `SearchResult`
/// 的 path 经 `canonicalize` 产出（Windows 带扩展长度前缀），而 `documents.path`
/// 存的是扫描时原始路径，精确匹配查库必须逐候选尝试。
#[must_use]
pub fn lookup_candidates(path: &str) -> Vec<String> {
    let mut out = vec![path.to_string()];
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        out.push(format!(r"\\{rest}"));
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        out.push(rest.to_string());
    }
    out
}

/// 页级命中（`read_document` 片段模式输出）：页号 + 该页命中摘录。
#[derive(Debug, serde::Serialize)]
pub struct PageHit {
    /// 页号（起于 1，对齐 `document_passages.page_no`）。
    pub page_no: u32,
    /// 该页第一个命中窗口摘录。
    pub excerpt: String,
}

/// 从查询词组抽检索词条：head + synonyms，multi-word head 再按空白拆 token。
/// 词组为空（parser 未抽出 keyword）→ 回退 query 空白 token；仍为空 → 整个
/// trimmed query 作为单词条。全部原样保留（匹配时做大小写折叠）、去重去空。
#[must_use]
pub fn query_terms(query: &str, groups: &[KeywordGroup]) -> Vec<String> {
    fn push(terms: &mut Vec<String>, t: &str) {
        let t = t.trim();
        if !t.is_empty() && !terms.iter().any(|x| x == t) {
            terms.push(t.to_string());
        }
    }
    let mut terms: Vec<String> = Vec::new();
    for g in groups {
        for word in g.head.split_whitespace() {
            push(&mut terms, word);
        }
        for s in &g.synonyms {
            push(&mut terms, s);
        }
    }
    if terms.is_empty() {
        for word in query.split_whitespace() {
            push(&mut terms, word);
        }
    }
    if terms.is_empty() {
        push(&mut terms, query);
    }
    terms
}

/// 逐字符首映射小写（与原文 1:1 对齐，见模块注释）。
fn fold_chars(s: &str) -> Vec<char> {
    s.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

/// 在 `haystack`（已折叠字符序列）中找 `needle` 的所有起点（char 下标，朴素扫描）。
fn find_all(haystack: &[char], needle: &[char], cap: usize) -> Vec<usize> {
    let mut out = Vec::new();
    if needle.is_empty() || haystack.len() < needle.len() {
        return out;
    }
    let mut i = 0;
    while i + needle.len() <= haystack.len() && out.len() < cap {
        if haystack[i..i + needle.len()] == *needle {
            out.push(i);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

/// 从正文抽命中上下文窗口：每个词条的命中位置 ±`context_chars`，重叠窗口合并，
/// 最多 `max_windows` 个；被裁剪的边用 `…` 标记。无命中 → 空 vec（**绝不回退全文**）。
#[must_use]
pub fn snippet_windows(
    body: &str,
    terms: &[String],
    max_windows: usize,
    context_chars: usize,
) -> Vec<String> {
    let chars: Vec<char> = body.chars().collect();
    if chars.is_empty() || terms.is_empty() || max_windows == 0 {
        return Vec::new();
    }
    let folded = fold_chars(body);

    // 收集全部命中区间 [start, end)（词条本身跨度）。
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for term in terms {
        let needle = fold_chars(term);
        // 每词条命中数上限 = max_windows：后面反正要合并/截断。
        for start in find_all(&folded, &needle, max_windows) {
            spans.push((start, start + needle.len()));
        }
    }
    if spans.is_empty() {
        return Vec::new();
    }
    spans.sort_unstable();

    // 扩上下文 + 合并重叠。
    let mut windows: Vec<(usize, usize)> = Vec::new();
    for (s, e) in spans {
        let ws = s.saturating_sub(context_chars);
        let we = (e + context_chars).min(chars.len());
        match windows.last_mut() {
            Some((_, prev_end)) if ws <= *prev_end => *prev_end = (*prev_end).max(we),
            _ => windows.push((ws, we)),
        }
    }
    windows.truncate(max_windows);

    windows
        .into_iter()
        .map(|(s, e)| {
            let mut out = String::new();
            if s > 0 {
                out.push('…');
            }
            out.extend(&chars[s..e]);
            if e < chars.len() {
                out.push('…');
            }
            out
        })
        .collect()
}

/// 命中回页（BETA-43 验收 ①）：返回文本含任一词条的 `page_no` 去重升序列表，
/// 上限 `cap`。非扫描件（无 passages）→ 空 vec。
#[must_use]
pub fn matching_pages(passages: &[PagePassage], terms: &[String], cap: usize) -> Vec<u32> {
    let needles: Vec<Vec<char>> = terms.iter().map(|t| fold_chars(t)).collect();
    let mut pages: Vec<u32> = Vec::new();
    for p in passages {
        if pages.len() >= cap {
            break;
        }
        if pages.last() == Some(&p.page_no) || pages.contains(&p.page_no) {
            continue;
        }
        let folded = fold_chars(&p.text);
        if needles.iter().any(|n| !find_all(&folded, n, 1).is_empty()) {
            pages.push(p.page_no);
        }
    }
    pages
}

/// 页级命中摘录（`read_document` 片段模式）：命中页各取第一个上下文窗口。
#[must_use]
pub fn matching_page_excerpts(
    passages: &[PagePassage],
    terms: &[String],
    cap: usize,
    context_chars: usize,
) -> Vec<PageHit> {
    let mut out: Vec<PageHit> = Vec::new();
    for p in passages {
        if out.len() >= cap {
            break;
        }
        if out.iter().any(|h| h.page_no == p.page_no) {
            continue;
        }
        if let Some(excerpt) = snippet_windows(&p.text, terms, 1, context_chars).pop() {
            out.push(PageHit {
                page_no: p.page_no,
                excerpt,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn terms(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    fn passage(page_no: u32, text: &str) -> PagePassage {
        PagePassage {
            page_no,
            seq: 0,
            text: text.to_string(),
        }
    }

    #[test]
    fn query_terms_from_groups_splits_and_dedups() {
        let mut g = KeywordGroup::singleton("BETA-32 daemon".to_string());
        g.synonyms.push("守护进程".to_string());
        let t = query_terms("原始 query", &[g.clone(), g]);
        assert_eq!(t, vec!["BETA-32", "daemon", "守护进程"]);
    }

    #[test]
    fn query_terms_falls_back_to_query_tokens_then_whole_query() {
        assert_eq!(query_terms("hello world", &[]), vec!["hello", "world"]);
        assert_eq!(query_terms("违约金条款", &[]), vec!["违约金条款"]);
        assert!(query_terms("   ", &[]).is_empty(), "纯空白 query 应返空");
    }

    #[test]
    fn snippet_windows_extracts_context_and_marks_clipping() {
        let body = format!(
            "{}违约金条款约定为合同总价的百分之十{}",
            "前".repeat(100),
            "后".repeat(100)
        );
        let out = snippet_windows(&body, &terms(&["违约金"]), 5, 10);
        assert_eq!(out.len(), 1);
        let w = &out[0];
        assert!(
            w.starts_with('…') && w.ends_with('…'),
            "两侧被裁应有省略号：{w}"
        );
        assert!(w.contains("违约金条款"), "窗口应含命中词及其后文：{w}");
        // 10 上下文 + 3 词长 + 2 省略号 = 25 char 内。
        assert!(
            w.chars().count() <= 25,
            "窗口超限：{}（{w}）",
            w.chars().count()
        );
    }

    #[test]
    fn snippet_windows_merges_overlapping_and_caps_count() {
        let body = "alpha beta alpha beta alpha";
        // 上下文窗口大 → 全部命中合并为 1 个窗口。
        let merged = snippet_windows(body, &terms(&["alpha"]), 5, 50);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0], body, "窗口覆盖全文时无省略号");
        // 上下文 0 → 每个命中一个窗口，cap=2 截断。
        let capped = snippet_windows(body, &terms(&["alpha"]), 2, 0);
        assert_eq!(capped.len(), 2);
    }

    #[test]
    fn snippet_windows_case_insensitive_and_no_match_returns_empty() {
        let out = snippet_windows("Payroll Ledger Notes", &terms(&["payroll"]), 5, 10);
        assert_eq!(out.len(), 1, "大小写折叠应命中");
        assert!(
            snippet_windows("完全无关的正文", &terms(&["违约金"]), 5, 10).is_empty(),
            "无命中绝不回退全文"
        );
    }

    #[test]
    fn snippet_windows_two_char_cjk_matches() {
        // BETA-42 背景：trigram FTS 对 2 字 CJK 0 命中；字符级窗口必须可达。
        let out = snippet_windows("本页含判决主文与违约金说明", &terms(&["判决"]), 5, 4);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("判决"));
    }

    #[test]
    fn lookup_candidates_strips_windows_extended_prefix() {
        assert_eq!(lookup_candidates("/a/b.txt"), vec!["/a/b.txt".to_string()]);
        assert_eq!(
            lookup_candidates(r"\\?\C:\Users\x\a.docx"),
            vec![
                r"\\?\C:\Users\x\a.docx".to_string(),
                r"C:\Users\x\a.docx".to_string(),
            ]
        );
        assert_eq!(
            lookup_candidates(r"\\?\UNC\server\share\a.txt"),
            vec![
                r"\\?\UNC\server\share\a.txt".to_string(),
                r"\\server\share\a.txt".to_string(),
            ]
        );
    }

    #[test]
    fn matching_pages_dedups_and_caps() {
        let ps = vec![
            passage(1, "无关内容"),
            passage(2, "违约金条款正文"),
            passage(2, "违约金再次出现（同页第二段）"),
            passage(5, "违约金尾部引用"),
        ];
        assert_eq!(matching_pages(&ps, &terms(&["违约金"]), 10), vec![2, 5]);
        assert_eq!(matching_pages(&ps, &terms(&["违约金"]), 1), vec![2]);
        assert!(matching_pages(&ps, &terms(&["不存在"]), 10).is_empty());
    }

    #[test]
    fn matching_page_excerpts_returns_page_and_window() {
        let ps = vec![passage(3, "第三页开头。违约金条款约定见附件。结尾。")];
        let hits = matching_page_excerpts(&ps, &terms(&["违约金"]), 10, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].page_no, 3);
        assert!(hits[0].excerpt.contains("违约金"));
    }
}
