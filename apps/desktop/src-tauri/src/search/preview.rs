//! BETA-20 结果预览面板数据源。
//!
//! 硬约束（Codex 风险点）：
//! - **只读已索引数据**——正文 / 元数据全部来自本地索引 DB，不读磁盘原文件，
//!   故不触发 OneDrive 占位符水合（`is_online_only` 在纯文本路径无关）。
//! - **预览正文与完整路径不进 trace**——本模块不调 `deps.tracer`，按构造保证零污染。

use scout_intent_parser::fallback::resolve_intent;
use scout_local_index_backend::{fts_match_for_groups, LocalIndexBackend, LocalPreview};
use serde::Serialize;

use super::SearchDeps;

/// 预览正文 / 片段最大字符数（限 IPC 体积，按 char 边界截断）。
const PREVIEW_MAX_CHARS: usize = 4000;

/// 扫描版 PDF 每段 OCR 文本的展示字符上限（限 IPC 体积；段本来就短，取小值防个别巨页撑爆）。
const SCANNED_PAGE_TEXT_MAX_CHARS: usize = 800;

/// BETA-35 cycle 5：扫描版 PDF 一段 OCR（前端展示"第 N 页 · OCR"标签）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScannedPageInfo {
    pub page_no: u32,
    pub seq: u32,
    /// 段文本（截断到 [`SCANNED_PAGE_TEXT_MAX_CHARS`]）。
    pub text: String,
    pub text_truncated: bool,
}

/// BETA-35 cycle 5：扫描版 PDF 一页 OCR 失败留痕（验收 ③——不静默丢）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FailedPageInfo {
    pub page_no: u32,
    pub reason: String,
}

/// 序列化给前端的预览数据（`kind` 标签区分）。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PreviewPayload {
    /// 音频元数据。
    Music {
        artist: Option<String>,
        title: Option<String>,
        album: Option<String>,
        duration_secs: Option<f64>,
        format: Option<String>,
        bitrate: Option<u32>,
    },
    /// 文档 / OCR 图片正文。`doc_type` 为图片扩展名时前端按「图片 OCR」呈现。
    Document {
        doc_type: String,
        title: Option<String>,
        author: Option<String>,
        page_count: Option<u32>,
        /// 正文摘录（截断到 [`PREVIEW_MAX_CHARS`]）。
        body: String,
        /// 正文是否被截断。
        body_truncated: bool,
        /// 命中片段（命中词以 `\u{2}` / `\u{3}` 哨兵包裹，前端转 `<mark>`）；无查询 / 无命中 → `None`。
        snippet: Option<String>,
        /// BETA-35 cycle 5：扫描版 PDF 逐页 OCR 段（非扫描 PDF / 非 PDF → 空 vec）。
        /// 前端据此展示"扫描版 · N 页 · 第 M 页 · OCR"标签（验收 ② + ④）。
        #[serde(default)]
        scanned_pages: Vec<ScannedPageInfo>,
        /// BETA-35 cycle 5：扫描版 PDF 逐页 OCR 失败记录（验收 ③）。
        /// UI 顶部展示"K 页 OCR 失败"提示；BETA-40 playbook 里用作取证复核入口。
        #[serde(default)]
        failed_pages: Vec<FailedPageInfo>,
    },
    /// 该文件不在本地索引（仅系统后端命中）→ 前端回退展示结果行已有的文件信息。
    Unindexed,
}

/// BETA-15B-5：语义命中的高亮段落区间（字符偏移，针对截断后 body）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExplainPayload {
    pub passages: Vec<ExplainPassage>,
}

/// 单个高亮段：`start`/`end` 为 body 的字符偏移（`end` 不含），`score` 为真 cosine。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExplainPassage {
    pub start: usize,
    pub end: usize,
    pub score: f32,
}

/// `get_preview` 命令实现（可单测）。**纯读索引**，失败时回退 [`PreviewPayload::Unindexed`]
/// （预览是锦上添花，绝不让取数失败影响主搜索流程）。
///
/// BETA-35 cycle 5：文档类型为 PDF 时额外拉扫描版 OCR 段落 / 失败页（`document_passages`
/// + `document_failed_pages` 表），非扫描 PDF 表为空 → 前端展示逻辑关。
pub(crate) fn get_preview_impl(
    path: &str,
    query: Option<&str>,
    deps: &SearchDeps,
) -> PreviewPayload {
    let backend = LocalIndexBackend::new(crate::local_index_db_path());
    // 高亮用的 FTS 表达式：复用与搜索同款的 parser + 同义词扩展 → 词组 → FTS5 表达式。
    let fts = query.and_then(|q| fts_for_query(q, deps));

    // 路径归一：本地索引结果的 path 经 `canonicalize` 产出（Windows 带 `\\?\` 扩展长度前缀），
    // 而索引 DB 存的是原始路径 → 逐候选尝试，命中即返。
    for cand in lookup_candidates(path) {
        match backend.preview(&cand, fts.as_deref()) {
            Ok(Some(preview)) => {
                // BETA-35 cycle 5：扫描 PDF 追加段落 / 失败页（其他类型两次调用返空 vec、无额外成本）。
                let (scanned_pages, failed_pages) = fetch_scanned_pdf_side(&backend, &cand);
                return to_payload(preview, scanned_pages, failed_pages);
            }
            Ok(None) => {}
            // 读 DB 失败 → 不在索引中按处理，回退文件信息（不抛错中断 UI）。
            Err(_) => return PreviewPayload::Unindexed,
        }
    }
    PreviewPayload::Unindexed
}

/// BETA-35 cycle 5：拉指定 path 的扫描版 PDF 段落 / 失败页（预览面板展示"第 N 页 · OCR"用）。
/// 后端错误静默返空 vec——预览是锦上添花，绝不因侧信息取数失败拦截主 preview。
fn fetch_scanned_pdf_side(
    backend: &LocalIndexBackend,
    path: &str,
) -> (Vec<ScannedPageInfo>, Vec<FailedPageInfo>) {
    let passages = backend
        .passages_for_doc(path)
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            let (text, text_truncated) = truncate_chars(&p.text, SCANNED_PAGE_TEXT_MAX_CHARS);
            ScannedPageInfo {
                page_no: p.page_no,
                seq: p.seq,
                text,
                text_truncated,
            }
        })
        .collect();
    let failed = backend
        .failed_pages_for_doc(path)
        .unwrap_or_default()
        .into_iter()
        .map(|f| FailedPageInfo {
            page_no: f.page_no,
            reason: f.reason,
        })
        .collect();
    (passages, failed)
}

/// 对选中的语义命中结果，算出 body 中与 query 语义最相似的段落区间。
/// **只读已索引正文**（复用 [`get_preview_impl`]，不读原文件、不触发水合）、**不调 tracer**。
/// 非文档 / 无模型 / feature 关 → 空 payload（前端无高亮，逐字节等价于现状）。
///
/// BETA-33 cycle 4：**图片 doc_type 默认直接返空**——与 `embed_pending` 图片跳过口径一致，
/// 兜底防旧图片向量仍在库时 UI 侧展示虚高段落级 cosine（v0.9.4 用户「作文」case）。
/// BETA-39：「图片语义索引」opt-in 开启时图片放开，段级门槛用图片专属 ratio（0.75）——
/// 防同一乱码 case 在段落级复现。
pub(crate) fn explain_semantic_hit_impl(
    path: &str,
    query: &str,
    deps: &SearchDeps,
) -> ExplainPayload {
    let PreviewPayload::Document { body, doc_type, .. } = get_preview_impl(path, None, deps) else {
        return ExplainPayload {
            passages: Vec::new(),
        };
    };
    // 图片 OCR 类型（doc_type 由 image_entry 写入时已小写）：opt-in 关 → 返空（现状）；
    // 开 → 走图片专属段级门槛。非图片沿用通用门槛。
    let is_image = scout_indexer::IMAGE_EXTS.contains(&doc_type.as_str());
    if is_image && !deps.image_semantics_enabled() {
        return ExplainPayload {
            passages: Vec::new(),
        };
    }
    let min_ratio = if is_image {
        scout_indexer::embed::IMAGE_MEANINGFUL_RATIO_FLOOR
    } else {
        scout_indexer::embed::MEANINGFUL_CHAR_RATIO_FLOOR
    };
    let embedder = deps.embedding();
    let ranges = scout_search_backend_semantic::explain::explain_passages_with_ratio(
        &body,
        query,
        embedder.as_ref(),
        min_ratio,
    );
    ExplainPayload {
        passages: ranges
            .into_iter()
            .map(|(start, end, score)| ExplainPassage { start, end, score })
            .collect(),
    }
}

/// 把原始 query 解析为内容词组的 FTS5 表达式（parser-only，不调模型；任何失败 → `None`）。
fn fts_for_query(query: &str, deps: &SearchDeps) -> Option<String> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    let resolved = resolve_intent(q, None).ok()?;
    let expanded = deps.synonym_expander().expand(resolved.intent, q);
    // 与 search_impl 同口径：读同一份全局 match_mode 配置，保证高亮与实际搜索命中一致。
    fts_match_for_groups(&expanded.keyword_groups, deps.match_mode())
}

/// 候选查询路径：原值 + 去除 Windows 扩展长度前缀（`\\?\` / `\\?\UNC\`）的形式。
fn lookup_candidates(path: &str) -> Vec<String> {
    let mut out = vec![path.to_string()];
    let user_facing = scout_search_backend::strip_windows_verbatim_prefix(path);
    if user_facing != path {
        out.push(user_facing.into_owned());
    }
    out
}

fn to_payload(
    preview: LocalPreview,
    scanned_pages: Vec<ScannedPageInfo>,
    failed_pages: Vec<FailedPageInfo>,
) -> PreviewPayload {
    match preview {
        LocalPreview::Music(e) => PreviewPayload::Music {
            artist: e.artist,
            title: e.title,
            album: e.album,
            duration_secs: e.duration_secs,
            format: e.format,
            bitrate: e.bitrate,
        },
        LocalPreview::Document(d) => {
            let (body, body_truncated) = truncate_chars(&d.body, PREVIEW_MAX_CHARS);
            PreviewPayload::Document {
                doc_type: d.entry.doc_type,
                title: d.entry.title,
                author: d.entry.author,
                page_count: d.entry.page_count,
                body,
                body_truncated,
                snippet: d.snippet.map(|s| truncate_chars(&s, PREVIEW_MAX_CHARS).0),
                scanned_pages,
                failed_pages,
            }
        }
    }
}

/// 按 char 边界截断到 `max` 个字符。返回 `(截断后文本, 是否发生截断)`。
fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        (s.to_string(), false)
    } else {
        (s.chars().take(max).collect(), true)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

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
    fn truncate_chars_respects_char_boundary() {
        // 纯 CJK，按字符（非字节）截断。
        let (s, truncated) = truncate_chars("一二三四五", 3);
        assert_eq!(s, "一二三");
        assert!(truncated);
        let (s2, t2) = truncate_chars("ab", 5);
        assert_eq!(s2, "ab");
        assert!(!t2);
    }

    #[test]
    fn document_payload_truncates_and_keeps_snippet() {
        let long = "字".repeat(PREVIEW_MAX_CHARS + 100);
        let payload = to_payload(
            LocalPreview::Document(scout_indexer::DocumentPreview {
                entry: scout_indexer::DocumentEntry {
                    path: "/d/a.docx".into(),
                    file_name: "a.docx".into(),
                    title: Some("标题".into()),
                    author: Some("作者".into()),
                    doc_type: "docx".into(),
                    page_count: Some(3),
                    modified_time: 0,
                    content_hash: None,
                },
                body: long,
                snippet: Some("命中\u{2}片段\u{3}".into()),
            }),
            Vec::new(),
            Vec::new(),
        );
        match payload {
            PreviewPayload::Document {
                body,
                body_truncated,
                snippet,
                doc_type,
                scanned_pages,
                failed_pages,
                ..
            } => {
                assert_eq!(doc_type, "docx");
                assert_eq!(body.chars().count(), PREVIEW_MAX_CHARS);
                assert!(body_truncated);
                assert_eq!(snippet.as_deref(), Some("命中\u{2}片段\u{3}"));
                assert!(
                    scanned_pages.is_empty(),
                    "非扫描 PDF payload 不应有 scanned_pages"
                );
                assert!(
                    failed_pages.is_empty(),
                    "非扫描 PDF payload 不应有 failed_pages"
                );
            }
            _ => panic!("应为 Document"),
        }
    }

    #[test]
    fn music_payload_maps_fields() {
        let payload = to_payload(
            LocalPreview::Music(scout_indexer::MusicEntry {
                path: "/m/a.mp3".into(),
                file_name: "a.mp3".into(),
                artist: Some("周华健".into()),
                title: Some("朋友".into()),
                album: Some("专辑".into()),
                duration_secs: Some(240.0),
                format: Some("MP3".into()),
                bitrate: Some(320),
                modified_time: 0,
            }),
            Vec::new(),
            Vec::new(),
        );
        match payload {
            PreviewPayload::Music {
                artist,
                duration_secs,
                bitrate,
                ..
            } => {
                assert_eq!(artist.as_deref(), Some("周华健"));
                assert_eq!(duration_secs, Some(240.0));
                assert_eq!(bitrate, Some(320));
            }
            _ => panic!("应为 Music"),
        }
    }

    #[test]
    fn document_payload_scanned_pdf_carries_page_info() {
        // BETA-35 cycle 5：扫描 PDF payload 应把 scanned_pages / failed_pages 透传出去（含截断）。
        let long_ocr = "页".repeat(SCANNED_PAGE_TEXT_MAX_CHARS + 50);
        let scanned = vec![ScannedPageInfo {
            page_no: 1,
            seq: 0,
            text: long_ocr,
            text_truncated: true,
        }];
        let failed = vec![FailedPageInfo {
            page_no: 2,
            reason: "OCR 引擎错".into(),
        }];
        let payload = to_payload(
            LocalPreview::Document(scout_indexer::DocumentPreview {
                entry: scout_indexer::DocumentEntry {
                    path: "/scan/a.pdf".into(),
                    file_name: "a.pdf".into(),
                    title: None,
                    author: None,
                    doc_type: "pdf".into(),
                    page_count: Some(3),
                    modified_time: 0,
                    content_hash: None,
                },
                body: String::from("拼接后的 OCR 正文"),
                snippet: None,
            }),
            scanned.clone(),
            failed.clone(),
        );
        match payload {
            PreviewPayload::Document {
                scanned_pages,
                failed_pages,
                page_count,
                ..
            } => {
                assert_eq!(page_count, Some(3));
                assert_eq!(scanned_pages, scanned);
                assert_eq!(failed_pages, failed);
                assert_eq!(scanned_pages[0].page_no, 1);
                assert_eq!(failed_pages[0].page_no, 2);
            }
            _ => panic!("应为 Document"),
        }
    }
}
