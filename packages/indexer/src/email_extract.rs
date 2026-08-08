//! BETA-37 邮件格式提取：eml 正文 + from/to/date/subject headers 基础字段 + 附件递交现有提取管线。
//!
//! - 字段映射零 schema 变更：Subject → `DocumentEntry.title`、From → `DocumentEntry.author`
//!   （显示名 + 地址）；From/To/Date/Subject 同时以文本头块拼进 body 开头，全部可 FTS 检索；
//!   `modified_time` 仍用文件 mtime（增量锚点语义不混）。
//! - 附件解码后按原扩展名写临时文件、递归 [`crate::doc_extract::extract_document`] 复用
//!   全部现有提取器（扫描 PDF 附件自然继承 BETA-35 OCR 管线）；文本以「[附件 文件名]」
//!   段并入邮件 body，不单独成 documents 行（避免磁盘上不存在的幽灵 path）。
//! - 深度限 1：附件里的 eml（含 message/rfc822 内嵌）只提 headers + 正文，不再展开其附件。
//! - 单附件提取失败（不支持类型 / 损坏 / 超限）只留标记行（文件名本身可检索），
//!   tracing warn 后继续，整封邮件不计 failed。

use std::fs;
use std::path::Path;

use mail_parser::{Address, Message, MessageParser, MessagePart, MimeHeaders};

use crate::doc_extract::{tag_err, Extracted};
use crate::IndexError;

/// 单附件解码后字节上限；超限不提取内容、只留文件名标记行（冷归档常见超大附件）。
const MAX_ATTACHMENT_BYTES: usize = 32 * 1024 * 1024;
/// 单封邮件提取内容的附件数上限；超出的附件只留文件名标记行。
const MAX_ATTACHMENTS: usize = 32;
/// text body 段数上限（正常邮件 1 段；防御异常 MIME 结构）。
const MAX_TEXT_BODIES: usize = 8;
/// 头块中 To 收件人列出上限（归档邮件常见大抄送列表，防头块膨胀）。
const MAX_HEADER_ADDRS: usize = 8;

/// eml 提取入口。`expand_attachments=false` 是深度限 1 的第二层：附件只留文件名标记。
pub(crate) fn extract_eml(path: &Path, expand_attachments: bool) -> Result<Extracted, IndexError> {
    let raw = fs::read(path).map_err(|e| tag_err(path, e.to_string()))?;
    let msg = MessageParser::default()
        .parse(&raw[..])
        .ok_or_else(|| tag_err(path, "eml 解析失败（非法 MIME 结构）"))?;
    let (title, author, body) = render_message(&msg, path, expand_attachments);
    Ok((title, author, body, None, Vec::new(), Vec::new()))
}

/// 把一封（顶层或内嵌）邮件渲染为 `(title, author, body)`。
///
/// body 布局（spec §4.2）：`From/To/Date/Subject` 头块 → 空行 → 正文（text/plain 优先，
/// HTML-only 时 mail-parser 转文本）→ 每个附件一段 `[附件 文件名]` + 提取文本。
fn render_message(
    msg: &Message,
    path: &Path,
    expand_attachments: bool,
) -> (Option<String>, Option<String>, String) {
    let title = msg
        .subject()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let author = address_display(msg.from());

    let mut body = String::new();
    if let Some(from) = &author {
        push_header_line(&mut body, "From", from);
    }
    if let Some(to) = address_display(msg.to()) {
        push_header_line(&mut body, "To", &to);
    }
    if let Some(date) = msg.date() {
        push_header_line(&mut body, "Date", &date.to_rfc3339());
    }
    if let Some(subject) = &title {
        push_header_line(&mut body, "Subject", subject);
    }

    for pos in 0..MAX_TEXT_BODIES {
        let Some(text) = msg.body_text(pos) else {
            break;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        body.push('\n');
        body.push_str(text);
        body.push('\n');
    }

    for (idx, part) in msg.attachments().enumerate() {
        let name = part
            .attachment_name()
            .map_or_else(|| format!("attachment-{}", idx + 1), sanitize_file_name);
        body.push('\n');
        body.push_str("[附件 ");
        body.push_str(&name);
        body.push_str("]\n");
        if !expand_attachments {
            continue;
        }
        if idx >= MAX_ATTACHMENTS {
            tracing::warn!(
                path = %path.display(),
                attachment = %name,
                "附件数超上限 {MAX_ATTACHMENTS}，跳过内容提取（保留文件名标记）"
            );
            continue;
        }
        if part.contents().len() > MAX_ATTACHMENT_BYTES {
            tracing::warn!(
                path = %path.display(),
                attachment = %name,
                bytes = part.contents().len(),
                "附件超 {MAX_ATTACHMENT_BYTES} 字节上限，跳过内容提取（保留文件名标记）"
            );
            continue;
        }
        match attachment_text(part, &name) {
            Ok(text) => {
                let text = text.trim();
                if !text.is_empty() {
                    body.push_str(text);
                    body.push('\n');
                }
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    attachment = %name,
                    error = %e,
                    "附件提取失败（保留文件名标记，不中断邮件）"
                );
            }
        }
    }

    (title, author, body)
}

/// 追加一行 `Key: Value` 头块文本。
fn push_header_line(body: &mut String, key: &str, value: &str) {
    body.push_str(key);
    body.push_str(": ");
    body.push_str(value);
    body.push('\n');
}

/// 地址头显示串：`显示名 <地址>`（缺一取一）；多地址逗号连接、超上限截断加省略。
fn address_display(addr: Option<&Address>) -> Option<String> {
    let addr = addr?;
    let mut parts: Vec<String> = Vec::new();
    for a in addr.iter().take(MAX_HEADER_ADDRS) {
        let display = match (a.name().map(str::trim), a.address().map(str::trim)) {
            (Some(name), Some(address)) if !name.is_empty() => format!("{name} <{address}>"),
            (_, Some(address)) => address.to_string(),
            (Some(name), None) if !name.is_empty() => name.to_string(),
            _ => continue,
        };
        parts.push(display);
    }
    if addr.iter().count() > MAX_HEADER_ADDRS {
        parts.push("…".to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

/// 提取单个附件的文本：eml/内嵌邮件走 [`render_message`]（不展开其附件，深度限 1）；
/// 其余类型写临时文件递交 [`crate::doc_extract::extract_document`] 复用现有提取器。
fn attachment_text(part: &MessagePart, name: &str) -> Result<String, IndexError> {
    // message/rfc822 内嵌邮件：mail-parser 已就地解析，直接渲染。
    if let Some(nested) = part.message() {
        let (_, _, nested_body) = render_message(nested, Path::new(name), false);
        return Ok(nested_body);
    }
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    // 附件名声明 .eml 但按普通字节传输：同样按邮件解析（不展开其附件）。
    if ext == "eml" {
        let nested = MessageParser::default()
            .parse(part.contents())
            .ok_or_else(|| tag_err(Path::new(name), "eml 附件解析失败（非法 MIME 结构）"))?;
        let (_, _, nested_body) = render_message(&nested, Path::new(name), false);
        return Ok(nested_body);
    }
    // 其余类型：临时文件（RAII，出作用域自动清理）→ 现有提取管线。
    let dir = tempfile::tempdir().map_err(|e| tag_err(Path::new(name), e.to_string()))?;
    let tmp = dir.path().join(name);
    fs::write(&tmp, part.contents()).map_err(|e| tag_err(&tmp, e.to_string()))?;
    let doc = crate::doc_extract::extract_document(&tmp, 0)?;
    Ok(doc.body)
}

/// 附件文件名净化：只取 basename（防 `../` 逃逸临时目录）、替换非法字符、空名兜底。
fn sanitize_file_name(name: &str) -> String {
    // Windows/macOS 分隔符都斩（macOS 上 Path 不认 `\`，手动统一）。
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| match c {
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim_matches(['.', ' ']).to_string();
    if cleaned.is_empty() {
        "attachment".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::path::PathBuf;

    fn write_eml(dir: &tempfile::TempDir, name: &str, content: &str) -> PathBuf {
        let path = dir.path().join(name);
        // 邮件规范行尾 CRLF。
        fs::write(&path, content.replace('\n', "\r\n")).unwrap();
        path
    }

    #[test]
    fn subject_to_title_from_to_author_headers_into_body() {
        let dir = tempfile::tempdir().unwrap();
        // Subject 是「离职交接安排」的 RFC 2047 encoded-word（对齐 BETA-41 fixture 形态）。
        let path = write_eml(
            &dir,
            "a.eml",
            "From: 张三 <zhangsan@example.com>\nTo: team@example.com\nDate: Thu, 2 Jul 2026 10:00:00 +0800\nSubject: =?UTF-8?B?56a76IGM5Lqk5o6l5a6J5o6S?=\nMIME-Version: 1.0\nContent-Type: text/plain; charset=utf-8\n\n交接清单见知识库。\n",
        );
        let (title, author, body, page_count, passages, failed_pages) =
            extract_eml(&path, true).unwrap();
        assert_eq!(title.as_deref(), Some("离职交接安排"));
        assert_eq!(author.as_deref(), Some("张三 <zhangsan@example.com>"));
        assert!(body.contains("From: 张三 <zhangsan@example.com>"));
        assert!(body.contains("To: team@example.com"));
        assert!(body.contains("Date: 2026-07-02T10:00:00+08:00"));
        assert!(body.contains("Subject: 离职交接安排"));
        assert!(body.contains("交接清单见知识库"));
        assert_eq!(page_count, None);
        assert!(
            passages.is_empty(),
            "邮件不产生 passages（页概念仅扫描 PDF）"
        );
        assert!(failed_pages.is_empty());
    }

    #[test]
    fn html_only_email_converted_to_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_eml(
            &dir,
            "h.eml",
            "From: a@example.com\nSubject: html mail\nMIME-Version: 1.0\nContent-Type: text/html; charset=utf-8\n\n<html><body><p>合同已归档，编号 <b>HT-2026</b>。</p></body></html>\n",
        );
        let (_, _, body, ..) = extract_eml(&path, true).unwrap();
        assert!(body.contains("合同已归档"));
        assert!(body.contains("HT-2026"));
        assert!(!body.contains("<p>"), "HTML 标签应被转文本剥离: {body}");
    }

    #[test]
    fn txt_attachment_extracted_into_body_with_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_eml(
            &dir,
            "m.eml",
            "From: a@example.com\nTo: b@example.com\nSubject: report\nMIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"B\"\n\n--B\nContent-Type: text/plain; charset=utf-8\n\n正文第一段。\n--B\nContent-Type: text/plain; charset=utf-8; name=\"notes.txt\"\nContent-Disposition: attachment; filename=\"notes.txt\"\n\n附件里的巡检记录内容。\n--B--\n",
        );
        let (_, _, body, ..) = extract_eml(&path, true).unwrap();
        assert!(body.contains("正文第一段"));
        assert!(body.contains("[附件 notes.txt]"));
        assert!(body.contains("附件里的巡检记录内容"));
    }

    #[test]
    fn attachments_not_expanded_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_eml(
            &dir,
            "m.eml",
            "From: a@example.com\nSubject: report\nMIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"B\"\n\n--B\nContent-Type: text/plain; charset=utf-8\n\n正文。\n--B\nContent-Type: text/plain; name=\"notes.txt\"\nContent-Disposition: attachment; filename=\"notes.txt\"\n\n附件内容不该出现。\n--B--\n",
        );
        let (_, _, body, ..) = extract_eml(&path, false).unwrap();
        assert!(body.contains("[附件 notes.txt]"), "标记行应保留: {body}");
        assert!(!body.contains("附件内容不该出现"));
    }

    #[test]
    fn nested_message_rfc822_depth_limited_to_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_eml(
            &dir,
            "fwd.eml",
            "From: a@example.com\nSubject: fwd\nMIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"B\"\n\n--B\nContent-Type: text/plain; charset=utf-8\n\n外层正文。\n--B\nContent-Type: message/rfc822\nContent-Disposition: attachment; filename=\"inner.eml\"\n\nFrom: c@example.com\nSubject: inner\nMIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"C\"\n\n--C\nContent-Type: text/plain; charset=utf-8\n\n内层正文可检索。\n--C\nContent-Type: text/plain; name=\"deep.txt\"\nContent-Disposition: attachment; filename=\"deep.txt\"\n\n深层附件不该被展开。\n--C--\n--B--\n",
        );
        let (_, _, body, ..) = extract_eml(&path, true).unwrap();
        assert!(body.contains("外层正文"));
        assert!(
            body.contains("内层正文可检索"),
            "内嵌邮件正文应提取: {body}"
        );
        assert!(
            body.contains("[附件 deep.txt]"),
            "深层附件应留标记行: {body}"
        );
        assert!(
            !body.contains("深层附件不该被展开"),
            "深度限 1：内嵌邮件的附件内容不应展开: {body}"
        );
    }

    #[test]
    fn unsupported_attachment_keeps_marker_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_eml(
            &dir,
            "b.eml",
            "From: a@example.com\nSubject: bin\nMIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"B\"\n\n--B\nContent-Type: text/plain; charset=utf-8\n\n正文。\n--B\nContent-Type: application/octet-stream; name=\"data.bin\"\nContent-Disposition: attachment; filename=\"data.bin\"\nContent-Transfer-Encoding: base64\n\nAAECAwQF\n--B--\n",
        );
        let (_, _, body, ..) = extract_eml(&path, true).unwrap();
        assert!(
            body.contains("[附件 data.bin]"),
            "不支持类型应留文件名标记: {body}"
        );
        assert!(body.contains("正文"));
    }

    #[test]
    fn missing_subject_yields_none_title() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_eml(
            &dir,
            "n.eml",
            "From: a@example.com\nMIME-Version: 1.0\nContent-Type: text/plain; charset=utf-8\n\n只有正文。\n",
        );
        let (title, author, body, ..) = extract_eml(&path, true).unwrap();
        assert_eq!(title, None);
        assert_eq!(author.as_deref(), Some("a@example.com"));
        assert!(body.contains("只有正文"));
        assert!(!body.contains("Subject:"), "无主题不应出现 Subject 头块行");
    }

    #[test]
    fn sanitize_file_name_strips_traversal_and_invalid_chars() {
        assert_eq!(sanitize_file_name("../../evil.txt"), "evil.txt");
        assert_eq!(sanitize_file_name("..\\..\\evil.txt"), "evil.txt");
        assert_eq!(sanitize_file_name("a:b*c?.txt"), "a_b_c_.txt");
        assert_eq!(sanitize_file_name(""), "attachment");
        assert_eq!(sanitize_file_name("..."), "attachment");
        assert_eq!(sanitize_file_name("报告.docx"), "报告.docx");
    }

    #[test]
    fn multiple_recipients_joined_in_header_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_eml(
            &dir,
            "t.eml",
            "From: a@example.com\nTo: 王五 <wangwu@example.com>, li@example.com\nSubject: s\nMIME-Version: 1.0\nContent-Type: text/plain; charset=utf-8\n\nx\n",
        );
        let (_, _, body, ..) = extract_eml(&path, true).unwrap();
        assert!(body.contains("To: 王五 <wangwu@example.com>, li@example.com"));
    }
}
