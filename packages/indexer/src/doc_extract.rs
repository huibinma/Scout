//! 文档正文提取（BETA-02）：docx / pptx / xlsx / pdf / html / md / txt / eml（BETA-37）。
//!
//! 每格式抽取「标题 / 作者 / 正文 / 页(节)数」，正文交给 [`crate::doc_db`] 入 FTS。
//! 提取失败（损坏 / 不支持）返回 [`IndexError::Tag`]，由增量循环计 failed 跳过、不中断。

use std::fs;
use std::io::{Read, Write};
use std::path::Path;
#[cfg(windows)]
use std::process::{Command, Stdio};
#[cfg(windows)]
use std::time::{Duration, Instant};

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::model::{DocumentEntry, ExtractedDoc, PageFailure, PagePassage};
use crate::IndexError;

/// 正文字符上限（防超大文档撑爆 FTS 行）。
const MAX_BODY_CHARS: usize = 1024 * 1024;

/// Windows PDF 文本层提取 helper 的内部命令行标记。
///
/// `pdf-extract`/`lopdf` 会递归解析 PDF 对象图；真实卷宗中的成环/恶意对象图可触发
/// `STATUS_STACK_OVERFLOW`（0xC00000FD）。Windows 栈溢出是不可恢复的 SEH 异常，
/// `catch_unwind` 和加大线程栈都只能缓解、不能隔离。因此 Windows 生产路径把解析放进
/// 当前可执行文件的 helper 模式，helper 崩溃只让单个 PDF 降级 OCR，不再杀掉桌面进程。
pub const PDF_EXTRACT_WORKER_ARG: &str = "--scout-pdf-extract-worker";

/// helper 最长运行时间。文本层解析正常应远低于此值；成环但尚未溢栈的解析也必须被终止。
#[cfg(windows)]
const PDF_EXTRACT_WORKER_TIMEOUT: Duration = Duration::from_secs(60);

/// BETA-64 T4：单份 PDF 内多页 OCR 的并发上限，独立于 `scan.rs` 的文件级提取并发预算。
/// 此前逐页 OCR 是 `.iter().map()` 纯串行——一份大页数扫描件即便机器多核也只能单核逐页
/// 跑完 OCR 子进程。本值刻意保守（默认 2）：单文件页级并发会叠加在文件级并发预算之上
/// （`scan::extract_parallelism_for` 重量级默认 4），worst case 总并发子进程数
/// ≈ 文件级并发 × 本值，避免叠加后重现 v0.9.27「17 个 pdftoppm 并发打爆机器」故障。
/// 可用 `SCOUT_PAGE_OCR_PARALLELISM` env 覆盖。
const PAGE_OCR_PARALLELISM: usize = 2;
const PAGE_OCR_PARALLELISM_ENV: &str = "SCOUT_PAGE_OCR_PARALLELISM";

fn page_ocr_parallelism() -> usize {
    std::env::var(PAGE_OCR_PARALLELISM_ENV)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(PAGE_OCR_PARALLELISM)
}

/// 进程级共享的页 OCR 线程池（懒初始化一次，容量固定不随运行期变化）。构建失败（极罕见）
/// → `None`，调用方退回 rayon 全局池（与 `scan.rs` 提取池同款兜底策略）。
fn page_ocr_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(page_ocr_parallelism())
            .build()
            .ok()
    })
    .as_ref()
}

/// 各格式提取器的通用返回：`(title, author, body, page_count, passages, failed_pages)`。
///
/// 除 PDF 扫描版路径（BETA-35 cycle 4）外，各格式一律返回空 `passages` / `failed_pages`
/// vec——从而落库时只走 `documents` + `documents_fts`，与 tuple 时代逐字节等价
/// （BETA-27 byte-equal 保护）。
pub(crate) type Extracted = (
    Option<String>,
    Option<String>,
    String,
    Option<u32>,
    Vec<PagePassage>,
    Vec<PageFailure>,
);

pub(crate) fn tag_err(path: &Path, detail: impl Into<String>) -> IndexError {
    IndexError::Tag {
        path: path.to_string_lossy().into_owned(),
        detail: detail.into(),
    }
}

/// 从单个文档提取 metadata + 正文（+ 扫描 PDF 时的段落 / 失败页）。
///
/// 返回 [`ExtractedDoc`]（BETA-35 cycle 4 从 tuple 升级）：`body` 进 FTS，
/// `passages` / `failed_pages` 空时行为与 cycle 3 之前完全相同。
pub fn extract_document(path: &Path, modified_time: i64) -> Result<ExtractedDoc, IndexError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    let (title, author, body, page_count, passages, failed_pages): Extracted = match ext.as_str() {
        "docx" => extract_docx(path)?,
        "pptx" => extract_pptx(path)?,
        "xlsx" | "xls" | "ods" => extract_spreadsheet(path)?,
        "pdf" => extract_pdf(path)?,
        "html" | "htm" => extract_html(path)?,
        "md" | "markdown" => extract_md(path)?,
        // csv/tsv 按纯文本提取（BETA-40 评测暴露的企业归档覆盖缺口：权限清单 / 台账导出）。
        "txt" | "csv" | "tsv" => extract_txt(path)?,
        // BETA-37：eml 邮件（headers + 正文 + 附件递交现有提取管线，附件展开深度 1）。
        "eml" => crate::email_extract::extract_eml(path, true)?,
        other => return Err(tag_err(path, format!("不支持的扩展名: {other}"))),
    };

    let body = truncate_chars(&body, MAX_BODY_CHARS);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();

    // BETA-38 doc identity：文件原始字节指纹（读取失败降级 None，不阻断索引）。
    let content_hash = crate::embed::file_identity_hash(path).ok();
    let entry = DocumentEntry {
        path: path.to_string_lossy().into_owned(),
        file_name,
        title,
        author,
        doc_type: normalize_doc_type(&ext),
        page_count,
        modified_time,
        content_hash,
    };
    Ok(ExtractedDoc {
        entry,
        body,
        passages,
        failed_pages,
    })
}

fn normalize_doc_type(ext: &str) -> String {
    match ext {
        "htm" => "html".to_string(),
        "markdown" => "md".to_string(),
        other => other.to_string(),
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

// ============================================================
// 通用 XML 文本收集
// ============================================================

/// 收集 XML 中所有 Text 事件（以空格连接）；`skip_tags`（local name）内的内容跳过。
fn collect_xml_text(xml: &str, skip_tags: &[&[u8]]) -> String {
    let mut reader = Reader::from_str(xml);
    let mut body = String::new();
    let mut buf_depth = 0usize; // 当前是否在 skip 元素内
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let local = e.local_name();
                if buf_depth > 0 || skip_tags.contains(&local.as_ref()) {
                    buf_depth += 1;
                }
            }
            Ok(Event::End(_)) => {
                buf_depth = buf_depth.saturating_sub(1);
            }
            Ok(Event::Text(e)) if buf_depth == 0 => {
                if let Ok(t) = e.decode() {
                    let t = t.trim();
                    if !t.is_empty() {
                        body.push_str(t);
                        body.push(' ');
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    body
}

/// 取 XML 中某 local-name 元素的首个文本（用于 core.xml 的 dc:title / dc:creator）。
fn first_element_text(xml: &str, want_local: &[u8]) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    let mut capture = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == want_local => capture = true,
            Ok(Event::Text(e)) if capture => {
                if let Ok(t) = e.decode() {
                    let t = t.trim();
                    if !t.is_empty() {
                        return Some(t.to_string());
                    }
                }
                capture = false;
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == want_local => capture = false,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    None
}

// ============================================================
// OOXML（docx / pptx）
// ============================================================

fn read_zip_entry<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<String> {
    let mut file = zip.by_name(name).ok()?;
    let mut s = String::new();
    file.read_to_string(&mut s).ok()?;
    Some(s)
}

/// docProps/core.xml 的 `dc:title` / `dc:creator` + `cp:lastModifiedBy`（BETA-55）。
///
/// `author` 字段合并「创建者 + 最后保存者」（去重、按序空格连接），两者皆进 FTS `author`
/// 列可被子串检索——支撑审计取证 / 离职归档场景「谁最后改的这份文件」的检索诉求
/// （此前只抽 `dc:creator`、`cp:lastModifiedBy` 完全没入索引）。
fn read_core_props<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> (Option<String>, Option<String>) {
    let Some(xml) = read_zip_entry(zip, "docProps/core.xml") else {
        return (None, None);
    };
    let title = first_element_text(&xml, b"title");
    let creator = first_element_text(&xml, b"creator");
    let last_modified_by = first_element_text(&xml, b"lastModifiedBy");
    (title, combine_authors(creator, last_modified_by))
}

/// 合并创建者与最后保存者为单个 `author` 串（去重、按出现顺序空格连接）；两者皆空 → `None`。
/// 二者相同（同一人创建并最后保存）时只留一份，避免 FTS 里重复词。
fn combine_authors(creator: Option<String>, last_modified_by: Option<String>) -> Option<String> {
    let mut names: Vec<String> = Vec::new();
    for n in [creator, last_modified_by].into_iter().flatten() {
        let n = n.trim().to_string();
        if !n.is_empty() && !names.contains(&n) {
            names.push(n);
        }
    }
    if names.is_empty() {
        None
    } else {
        Some(names.join(" "))
    }
}

/// OLE2/复合文档容器（CFB）文件签名。加密 OOXML（Word/Excel/PowerPoint 设了打开密码）
/// 用此容器包一层 `EncryptedPackage` 流，本身不是 zip；旧版二进制 `.doc`/`.xls`/`.ppt`
/// 误加 `.docx`/`.xlsx`/`.pptx` 扩展名同样是此签名。命中即可判定"非 zip 的具体原因"，
/// 不需要真解出是否加密。
const CFB_SIGNATURE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// `zip::ZipArchive::new` 失败时的诊断 detail：CFB 容器（加密文档 / 老版二进制格式）
/// 给可读原因，其余（截断 / 非 office 文件）保留原始 zip 报错文本。
fn zip_open_err_detail(path: &Path, raw: &str) -> String {
    let Ok(mut f) = fs::File::open(path) else {
        return raw.to_string();
    };
    let mut head = [0u8; 8];
    if f.read_exact(&mut head).is_ok() && head == CFB_SIGNATURE {
        return "文档疑似已加密或为旧版二进制格式（非 OOXML zip），无法读取正文".to_string();
    }
    raw.to_string()
}

/// 从路径按 OOXML zip 读 `docProps/core.xml`（xlsx 走 calamine 不暴露 core props，另开 zip 补）。
/// 非 zip（老 .xls BIFF）/ 无 core.xml（.ods 用 meta.xml）→ 降级 `(None, None)`，不阻断索引。
fn read_ooxml_core_props_from_path(path: &Path) -> (Option<String>, Option<String>) {
    let Ok(file) = fs::File::open(path) else {
        return (None, None);
    };
    let Ok(mut zip) = zip::ZipArchive::new(file) else {
        return (None, None);
    };
    read_core_props(&mut zip)
}

fn extract_docx(path: &Path) -> Result<Extracted, IndexError> {
    let file = fs::File::open(path).map_err(|e| tag_err(path, e.to_string()))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| tag_err(path, zip_open_err_detail(path, &e.to_string())))?;
    let xml = read_zip_entry(&mut zip, "word/document.xml")
        .ok_or_else(|| tag_err(path, "缺少 word/document.xml"))?;
    let body = collect_xml_text(&xml, &[]);
    let (title, author) = read_core_props(&mut zip);
    Ok((title, author, body, None, Vec::new(), Vec::new()))
}

fn extract_pptx(path: &Path) -> Result<Extracted, IndexError> {
    let file = fs::File::open(path).map_err(|e| tag_err(path, e.to_string()))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| tag_err(path, zip_open_err_detail(path, &e.to_string())))?;

    // 收集 ppt/slides/slideN.xml（排除 slideLayout / slideMaster / notesSlide）。
    let slide_names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
        .filter(|n| {
            n.starts_with("ppt/slides/slide")
                && std::path::Path::new(n)
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("xml"))
                && !n.contains("slideLayout")
                && !n.contains("slideMaster")
        })
        .collect();

    let mut body = String::new();
    for name in &slide_names {
        if let Some(xml) = read_zip_entry(&mut zip, name) {
            body.push_str(&collect_xml_text(&xml, &[]));
        }
    }
    let page_count = u32::try_from(slide_names.len()).ok();
    let (title, author) = read_core_props(&mut zip);
    Ok((title, author, body, page_count, Vec::new(), Vec::new()))
}

// ============================================================
// 电子表格（calamine：xlsx / xls / ods）
// ============================================================

fn extract_spreadsheet(path: &Path) -> Result<Extracted, IndexError> {
    use calamine::Reader;
    let mut wb = calamine::open_workbook_auto(path).map_err(|e| tag_err(path, e.to_string()))?;
    let sheet_names = wb.sheet_names().clone();
    let mut body = String::new();
    for name in &sheet_names {
        if let Ok(range) = wb.worksheet_range(name) {
            for row in range.rows() {
                for cell in row {
                    let s = cell.to_string();
                    if !s.is_empty() {
                        body.push_str(&s);
                        body.push(' ');
                    }
                }
            }
        }
    }
    let page_count = u32::try_from(sheet_names.len()).ok();
    // BETA-55：xlsx/xlsm 是 OOXML zip，calamine 不给 core props——另开 zip 读 title/author
    //（含最后保存者）；.xls/.ods 读不到即降级 None。
    let (title, author) = read_ooxml_core_props_from_path(path);
    Ok((title, author, body, page_count, Vec::new(), Vec::new()))
}

// ============================================================
// PDF
// ============================================================

/// 扫描版 PDF 判定阈值（cycle 2 简版：整文档字符数 < 阈值 → 扫描版）。
///
/// **策略选择说明**：spec §4.2 原写"每页平均 <10 字符"公式需要 `page_count`；
/// 当前 pdf-extract 未返回 PDF 页数，若拿则需引入 pdfinfo shell-out 或 lopdf
/// 依赖——留 cycle 3 pipeline 整合 `PdfRasterizer` 时同步升级。cycle 2 用整
/// 文档阈值覆盖「完全无文本层 / 极稀薄」这一极端情形（≈"每页平均 <10 chars"
/// 在典型 10 页文档上等价），其他 PDF 一律走原路径不动、保 BETA-27 byte-equal。
const SCANNED_TEXT_FLOOR_CHARS: usize = 100;

/// 判定 pdf-extract 抽出的文本是否属于扫描版 PDF（文本层稀薄/为空）。
///
/// 纯函数，便于单测；由 [`extract_pdf`] 与 cycle 3 pipeline 整合后共用。
fn is_scanned_pdf(text_len_chars: usize) -> bool {
    text_len_chars < SCANNED_TEXT_FLOOR_CHARS
}

#[derive(Debug)]
enum PdfTextFailure {
    Error(String),
    Panic,
}

fn extract_pdf(path: &Path) -> Result<Extracted, IndexError> {
    // pdf-extract 对部分文本层编码不支持时会 **panic**（如中文 CID 字体 CMap
    // `UniGB-UCS2-H`——企业中文卷宗大量使用）或返 Err。这类 PDF 文本层存在但读不出，
    // 按「无可用文本层」降级 rasterize + OCR 管线兜底，而不是整份计 failed 静默丢
    // （BETA-40 企业三场景排查实锤：合成中文文本层 PDF 全数因此 panic 未落库）。
    // panic 静默：scan.rs 增量循环的 catch_extract 已装 quiet hook；直接调用方
    // （单测 / 诊断）会看到一条 panic 打印，可接受。
    match extract_pdf_text_guarded(path) {
        Ok(body) => classify_pdf_extraction(path, body),
        Err(PdfTextFailure::Error(error)) => {
            if is_pdf_decryption_error(&error) {
                // 加密 PDF：pdf-extract 用空密码解密必然失败，误判成"扫描版"会白跑一次
                // pdftoppm 渲染 + OCR（还可能因 pdftoppm 等 stdin 密码输入卡满
                // RASTERIZE_TIMEOUT）。这里直接快速失败，不进 rasterize + OCR 管线。
                return Err(tag_err(path, "PDF 已加密，无法读取正文"));
            }
            tracing::warn!(
                path = %path.display(),
                %error,
                "pdf-extract 文本层提取失败，降级 rasterize + OCR 管线"
            );
            extract_scanned_pdf_via_ocr(path)
        }
        Err(PdfTextFailure::Panic) => {
            tracing::warn!(
                path = %path.display(),
                "pdf-extract panic（不支持的编码 / 畸形文件），降级 rasterize + OCR 管线"
            );
            extract_scanned_pdf_via_ocr(path)
        }
    }
}

/// `pdf-extract` 内部对加密 PDF 会用空密码尝试 `Document::decrypt`（`maybe_decrypt`），
/// 失败后冒泡出 `lopdf::Error::Decryption`，经 `OutputError` 的 `Display`
/// （`"PDF error: decryption error: ..."`）固定含 `"decryption error"` 子串；
/// Windows worker 路径下这段文本被原样透传进 stderr、再拼进 helper 退出消息，
/// 子串依然保留。命中即可判定"加密 PDF"，不需要额外引入 `lopdf` 直接解析
/// （直接在本进程调 `lopdf::Document::load` 会绕开 Windows worker 隔离，
/// 重新引入 `PDF_EXTRACT_WORKER_ARG` 本要规避的成环对象图栈溢出风险）。
fn is_pdf_decryption_error(message: &str) -> bool {
    message.contains("decryption error")
}

/// PDF 文本层提取的故障边界。
///
/// Windows 必须走子进程隔离：`STATUS_STACK_OVERFLOW` 会直接终止发生异常的进程，无法在
/// Rust 线程内捕获。其他平台暂时保留原 `catch_unwind` 路径，避免改变既有行为。
#[cfg(windows)]
fn extract_pdf_text_guarded(path: &Path) -> Result<String, PdfTextFailure> {
    let exe = std::env::current_exe().ok();
    let worker_capable = exe
        .as_deref()
        .and_then(Path::file_stem)
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(is_pdf_worker_executable_stem);
    if worker_capable {
        extract_pdf_text_in_worker(path).map_err(|e| PdfTextFailure::Error(e.to_string()))
    } else {
        // cargo test 的 current_exe 是测试 harness，不具备本 helper 入口；测试环境保留
        // 旧的进程内 catch_unwind 路径，避免 Windows CI 把内部参数交给 libtest 误报。
        extract_pdf_text_in_process(path)
    }
}

#[cfg(windows)]
fn is_pdf_worker_executable_stem(stem: &str) -> bool {
    ["Scout", "scout-desktop", "scoutd"]
        .iter()
        .any(|expected| stem.eq_ignore_ascii_case(expected))
}

#[cfg(windows)]
fn format_windows_exit_code(code: i32) -> String {
    format!("0x{:08X}", u32::from_ne_bytes(code.to_ne_bytes()))
}

#[cfg(not(windows))]
fn extract_pdf_text_guarded(path: &Path) -> Result<String, PdfTextFailure> {
    extract_pdf_text_in_process(path)
}

fn extract_pdf_text_in_process(path: &Path) -> Result<String, PdfTextFailure> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pdf_extract::extract_text(path)
    })) {
        Ok(Ok(body)) => Ok(body),
        Ok(Err(error)) => Err(PdfTextFailure::Error(error.to_string())),
        Err(_) => Err(PdfTextFailure::Panic),
    }
}

/// 启动当前 exe 的 PDF helper 模式并收集结果。
///
/// stdout/stderr 必须并行排空，否则大正文可能填满 pipe、令父进程 `wait` 与子进程写入
/// 互相等待。helper 写出前已按 [`MAX_BODY_CHARS`] 截断，内存仍有确定上限。
#[cfg(windows)]
fn extract_pdf_text_in_worker(path: &Path) -> Result<String, IndexError> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let exe = std::env::current_exe()
        .map_err(|e| tag_err(path, format!("定位 PDF 提取 helper 失败: {e}")))?;
    let mut child = Command::new(exe)
        .arg(PDF_EXTRACT_WORKER_ARG)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| tag_err(path, format!("启动 PDF 提取 helper 失败: {e}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| tag_err(path, "PDF 提取 helper 无 stdout 管道"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| tag_err(path, "PDF 提取 helper 无 stderr 管道"))?;
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = std::io::BufReader::new(stdout).read_to_end(&mut bytes);
        bytes
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = std::io::BufReader::new(stderr).read_to_end(&mut bytes);
        bytes
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < PDF_EXTRACT_WORKER_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(tag_err(
                    path,
                    format!(
                        "PDF 提取 helper 超时（{} 秒），已终止",
                        PDF_EXTRACT_WORKER_TIMEOUT.as_secs()
                    ),
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(tag_err(path, format!("等待 PDF 提取 helper 失败: {error}")));
            }
        }
    };

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        let exit = status
            .code()
            .map_or_else(|| "unknown".to_owned(), format_windows_exit_code);
        let detail = String::from_utf8_lossy(&stderr);
        return Err(tag_err(
            path,
            format!("PDF 提取 helper 异常退出（code={exit}）: {}", detail.trim()),
        ));
    }
    String::from_utf8(stdout)
        .map_err(|e| tag_err(path, format!("PDF 提取 helper 返回非 UTF-8 文本: {e}")))
}

/// 二进制入口应在初始化 Tauri/Tokio/CLI 之前调用本函数。
///
/// 普通启动立即返回；命中 [`PDF_EXTRACT_WORKER_ARG`] 时执行一次原始 `pdf-extract`，
/// 写 stdout 后直接退出。若解析触发栈溢出，只会终止这个 helper 子进程。
pub fn run_pdf_extract_worker_if_requested() {
    let mut args = std::env::args_os();
    let _exe = args.next();
    if args.next().as_deref() != Some(std::ffi::OsStr::new(PDF_EXTRACT_WORKER_ARG)) {
        return;
    }
    let Some(path) = args.next() else {
        let _ = writeln!(std::io::stderr(), "PDF helper 缺少文件路径");
        std::process::exit(2);
    };
    let path = Path::new(&path);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pdf_extract::extract_text(path)
    }));
    match result {
        Ok(Ok(body)) => {
            let body = truncate_chars(&body, MAX_BODY_CHARS);
            if std::io::stdout().write_all(body.as_bytes()).is_err() {
                std::process::exit(4);
            }
            std::process::exit(0);
        }
        Ok(Err(error)) => {
            let _ = writeln!(std::io::stderr(), "{error}");
            std::process::exit(3);
        }
        Err(_) => {
            let _ = writeln!(std::io::stderr(), "pdf-extract panic");
            std::process::exit(5);
        }
    }
}

/// 分支路径：拿到 pdf-extract 文本后，按 [`is_scanned_pdf`] 判定走原路径还是走
/// 扫描版分支。抽出纯逻辑 helper 便于单测（extract_pdf 依赖真 PDF fixture）。
///
/// - **文本层充分** → 走原路径，`passages` / `failed_pages` 皆空，落库时只写
///   documents + FTS（BETA-27 byte-equal 不动）
/// - **扫描版** → [`extract_scanned_pdf_via_ocr`] 真跑 rasterize + OCR pipeline，
///   passages / failed_pages 携带页级结果
fn classify_pdf_extraction(path: &Path, body: String) -> Result<Extracted, IndexError> {
    if is_scanned_pdf(body.chars().count()) {
        tracing::warn!(
            path = %path.display(),
            text_len = body.chars().count(),
            "扫描版 PDF 检出，走 rasterize + OCR 管线"
        );
        return extract_scanned_pdf_via_ocr(path);
    }
    Ok((None, None, body, None, Vec::new(), Vec::new()))
}

/// 扫描版 PDF 真跑管线：`default_pdf_rasterizer()` → `render_pages` → 逐页
/// `default_ocr_engine().recognize()` → [`aggregate_page_ocr_results`]。
///
/// **依赖不可用时的降级**：
/// - pdftoppm 未装（`default_pdf_rasterizer` 返 None）→ [`IndexError::Tag`] 计 failed，
///   onboarding 会引导用户装 poppler（BETA-35 cycle 6）；
/// - OCR 引擎均不可用（Windows.Media.Ocr + Tesseract 都无）→ 同上，onboarding 已引导。
///
/// **成本注意**：`default_*` 工厂 per-call 都会跑一次 detect（spawn 探测进程）。
/// cycle 3 阶段 per-doc 一次 detect 可接受；cycle 4-5 pipeline 大改时会把
/// detect 提到 scan.rs 增量循环外做一次并缓存。
fn extract_scanned_pdf_via_ocr(path: &Path) -> Result<Extracted, IndexError> {
    let rasterizer = crate::pdf_rasterizer::default_pdf_rasterizer().ok_or_else(|| {
        tag_err(
            path,
            "扫描版 PDF 需 pdftoppm（poppler-utils）渲染页，未装 → 跳过（onboarding 会引导装）",
        )
    })?;
    let ocr = crate::ocr::default_ocr_engine().ok_or_else(|| {
        tag_err(
            path,
            "扫描版 PDF 需 OCR 引擎（Windows.Media.Ocr / Tesseract），均不可用 → 跳过",
        )
    })?;
    let rendered = rasterizer.render_pages(path)?;
    // BETA-64 T4：逐页 OCR 改并行（此前 `.iter().map()` 纯串行），经容量受限的
    // `page_ocr_pool()` 执行，避免与 `scan.rs` 文件级并发叠加后子进程数失控。
    let run_pages = || {
        use rayon::prelude::*;
        rendered
            .pages()
            .par_iter()
            .map(|(page_no, png)| (*page_no, ocr.recognize(png)))
            .collect()
    };
    let page_ocr_results: Vec<(u32, Result<String, IndexError>)> = match page_ocr_pool() {
        Some(pool) => pool.install(run_pages),
        None => run_pages(),
    };
    // rendered 在此 drop → 临时 PNG 自动清理（RasterizedPdf RAII 语义）。
    aggregate_page_ocr_results(path, page_ocr_results)
}

/// 聚合逐页 OCR 结果为 [`Extracted`]（BETA-35 cycle 4 升级：保留 page_no 结构）：
/// - **body**：拼接所有非空白成功页文本，每页之间 `\n` 分隔（进 FTS）；
/// - **passages**：每个成功且非空白页对应一段 [`PagePassage`]（`seq=0`；后续可
///   在页内切分），带 `page_no` 落 `document_passages` 表——**命中回页**（验收 ②）
///   数据源；
/// - **failed_pages**：失败页 → [`PageFailure`]（含 `reason`），落
///   `document_failed_pages` 表——**失败页不静默丢**（验收 ③）；
/// - **全失败**（`success_count == 0`）→ [`IndexError::Tag`] 让整份 PDF 计 failed。
///
/// **cycle 4 简化**：全空白 OCR 结果算 success 但**不生成 passage**（page 有 body 但无检索价值）；
/// 若整份 PDF 全部页都空白，虽然 success_count > 0 但 passages 会为空、body 也为空——
/// 这种情况通常是 OCR 引擎完全没识别出字，我们**仍算 success**（不进 failed 表），让
/// 上层的 [`crate::embed::is_embed_worthy`] 挡向量污染。
fn aggregate_page_ocr_results(
    path: &Path,
    results: Vec<(u32, Result<String, IndexError>)>,
) -> Result<Extracted, IndexError> {
    let total_pages = results.len();
    let mut body = String::new();
    let mut passages: Vec<PagePassage> = Vec::new();
    let mut failed_pages: Vec<PageFailure> = Vec::new();
    let mut success_count: usize = 0;
    for (page_no, res) in results {
        match res {
            Ok(text) => {
                success_count += 1;
                if !text.trim().is_empty() {
                    if !body.is_empty() {
                        body.push('\n');
                    }
                    body.push_str(&text);
                    passages.push(PagePassage {
                        page_no,
                        seq: 0,
                        text,
                    });
                }
            }
            Err(e) => {
                let reason = e.to_string();
                tracing::warn!(
                    path = %path.display(),
                    page_no,
                    error = %reason,
                    "扫描版 PDF 单页 OCR 失败（落库 document_failed_pages）"
                );
                failed_pages.push(PageFailure { page_no, reason });
            }
        }
    }
    if success_count == 0 {
        return Err(tag_err(
            path,
            format!("扫描版 PDF 全部 {total_pages} 页 OCR 失败"),
        ));
    }
    let page_count = u32::try_from(total_pages).ok();
    Ok((None, None, body, page_count, passages, failed_pages))
}

// ============================================================
// HTML / Markdown / TXT
// ============================================================

fn extract_html(path: &Path) -> Result<Extracted, IndexError> {
    let raw = fs::read(path).map_err(|e| tag_err(path, e.to_string()))?;
    let xml = String::from_utf8_lossy(&raw);
    let title = first_element_text(&xml, b"title");
    let body = collect_xml_text(&xml, &[b"script".as_slice(), b"style".as_slice()]);
    Ok((title, None, body, None, Vec::new(), Vec::new()))
}

fn extract_md(path: &Path) -> Result<Extracted, IndexError> {
    use pulldown_cmark::{Event as MdEvent, Parser};
    let text = fs::read_to_string(path).map_err(|e| tag_err(path, e.to_string()))?;
    let mut body = String::new();
    for ev in Parser::new(&text) {
        if let MdEvent::Text(t) | MdEvent::Code(t) = ev {
            body.push_str(&t);
            body.push(' ');
        }
    }
    Ok((None, None, body, None, Vec::new(), Vec::new()))
}

fn extract_txt(path: &Path) -> Result<Extracted, IndexError> {
    let raw = fs::read(path).map_err(|e| tag_err(path, e.to_string()))?;
    let body = String::from_utf8_lossy(&raw).into_owned();
    Ok((None, None, body, None, Vec::new(), Vec::new()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::io::Write;

    #[cfg(windows)]
    #[test]
    fn pdf_worker_recognizes_production_binaries_not_test_harnesses() {
        assert!(is_pdf_worker_executable_stem("Scout"));
        assert!(is_pdf_worker_executable_stem("SCOUT-DESKTOP"));
        assert!(is_pdf_worker_executable_stem("scoutd"));
        assert!(
            !is_pdf_worker_executable_stem("real_pdf-d2ab30e07d0e0eee"),
            "Windows cargo test harness 必须保留进程内测试路径，不能误启动自身"
        );
    }

    #[cfg(windows)]
    #[test]
    fn stack_overflow_exit_code_is_logged_as_unsigned_ntstatus() {
        let signed = i32::from_ne_bytes(0xC000_00FD_u32.to_ne_bytes());
        assert_eq!(format_windows_exit_code(signed), "0xC00000FD");
    }

    #[test]
    fn pdf_decryption_error_detected_from_pdf_extract_display_string() {
        // 进程内路径：`OutputError::PdfError` 的 Display（"PDF error: decryption error: ..."）。
        assert!(is_pdf_decryption_error(
            "PDF error: decryption error: invalid password"
        ));
        // Windows worker 路径：同一段文本被拼进 helper 退出消息，子串仍保留。
        assert!(is_pdf_decryption_error(
            "PDF 提取 helper 异常退出（code=3）: PDF error: decryption error: invalid password"
        ));
    }

    #[test]
    fn pdf_decryption_error_not_confused_with_other_pdf_errors() {
        assert!(!is_pdf_decryption_error(
            "PDF error: couldn't parse input: unexpected EOF"
        ));
        assert!(!is_pdf_decryption_error("IO error: 拒绝访问"));
    }

    #[test]
    fn collect_text_from_docx_xml() {
        let xml = r"<w:document><w:body><w:p><w:r><w:t>季度</w:t></w:r><w:r><w:t>预算</w:t></w:r></w:p></w:body></w:document>";
        let body = collect_xml_text(xml, &[]);
        assert!(body.contains("季度"));
        assert!(body.contains("预算"));
    }

    #[test]
    fn collect_text_from_pptx_xml() {
        let xml = r"<p:sld><p:cSld><p:spTree><a:p><a:r><a:t>Slide One</a:t></a:r></a:p></p:spTree></p:cSld></p:sld>";
        let body = collect_xml_text(xml, &[]);
        assert!(body.contains("Slide One"));
    }

    #[test]
    fn collect_text_skips_script_style() {
        let html = r"<html><head><style>.x{color:red}</style></head><body><script>alert('bad')</script><p>Hello World</p></body></html>";
        let body = collect_xml_text(html, &[b"script".as_slice(), b"style".as_slice()]);
        assert!(body.contains("Hello World"));
        assert!(!body.contains("alert"), "script 内容应被跳过: {body}");
        assert!(!body.contains("color"), "style 内容应被跳过: {body}");
    }

    #[test]
    fn first_element_text_reads_core_props() {
        let xml = r#"<cp:coreProperties xmlns:cp="c" xmlns:dc="x"><dc:title>季度报告</dc:title><dc:creator>张三</dc:creator><cp:lastModifiedBy>燎原</cp:lastModifiedBy></cp:coreProperties>"#;
        assert_eq!(
            first_element_text(xml, b"title").as_deref(),
            Some("季度报告")
        );
        assert_eq!(first_element_text(xml, b"creator").as_deref(), Some("张三"));
        // BETA-55：最后保存者 local-name 命中。
        assert_eq!(
            first_element_text(xml, b"lastModifiedBy").as_deref(),
            Some("燎原")
        );
        assert_eq!(first_element_text(xml, b"subject"), None);
    }

    #[test]
    fn combine_authors_dedups_and_joins() {
        // 创建者 + 最后保存者不同 → 两者皆保留、空格连接（皆可被 FTS 子串命中）。
        assert_eq!(
            combine_authors(Some("张三".into()), Some("燎原".into())).as_deref(),
            Some("张三 燎原")
        );
        // 同一人创建并最后保存 → 去重，不重复词。
        assert_eq!(
            combine_authors(Some("燎原".into()), Some("燎原".into())).as_deref(),
            Some("燎原")
        );
        // 单侧存在。
        assert_eq!(
            combine_authors(Some("张三".into()), None).as_deref(),
            Some("张三")
        );
        assert_eq!(
            combine_authors(None, Some("燎原".into())).as_deref(),
            Some("燎原")
        );
        // 空白/全空。
        assert_eq!(
            combine_authors(Some("  ".into()), Some("燎原".into())).as_deref(),
            Some("燎原")
        );
        assert_eq!(combine_authors(None, None), None);
    }

    #[test]
    fn extract_txt_reads_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        fs::write(&path, "hello 季度预算 world").unwrap();
        let ExtractedDoc { entry, body, .. } = extract_document(&path, 42).unwrap();
        assert_eq!(entry.doc_type, "txt");
        assert_eq!(entry.modified_time, 42);
        assert!(body.contains("季度预算"));
    }

    #[test]
    fn extract_csv_tsv_as_plain_text() {
        // BETA-40 评测暴露：企业归档权限清单 / 台账导出常为 csv/tsv，按纯文本提取。
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("accounts.csv");
        fs::write(&csv, "账号,用途\nsvc_kunpeng,数据库只读账号\n").unwrap();
        let ExtractedDoc { entry, body, .. } = extract_document(&csv, 0).unwrap();
        assert_eq!(entry.doc_type, "csv");
        assert!(body.contains("数据库只读账号"));

        let tsv = dir.path().join("ledger.tsv");
        fs::write(&tsv, "科目\t金额\n维护费\t1200\n").unwrap();
        let ExtractedDoc { entry, body, .. } = extract_document(&tsv, 0).unwrap();
        assert_eq!(entry.doc_type, "tsv");
        assert!(body.contains("维护费"));
    }

    #[test]
    fn extract_md_strips_syntax() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("readme.md");
        fs::write(&path, "# 标题\n\nHello **world** and `code`.").unwrap();
        let ExtractedDoc { entry, body, .. } = extract_document(&path, 0).unwrap();
        assert_eq!(entry.doc_type, "md");
        assert!(body.contains("标题"));
        assert!(body.contains("Hello"));
        assert!(body.contains("world"));
        assert!(body.contains("code"));
        assert!(!body.contains('#'), "markdown 语法应被剥离: {body}");
        assert!(!body.contains('*'));
    }

    #[test]
    fn extract_html_title_and_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("page.html");
        fs::write(
            &path,
            r"<html><head><title>报告页</title></head><body><script>x()</script><p>正文内容</p></body></html>",
        )
        .unwrap();
        let ExtractedDoc { entry, body, .. } = extract_document(&path, 0).unwrap();
        assert_eq!(entry.doc_type, "html");
        assert_eq!(entry.title.as_deref(), Some("报告页"));
        assert!(body.contains("正文内容"));
        assert!(!body.contains("x()"));
    }

    #[test]
    fn extract_docx_end_to_end_via_minimal_zip() {
        use zip::write::SimpleFileOptions;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.docx");
        // 构造最小 docx：仅 word/document.xml + docProps/core.xml。
        let f = fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = SimpleFileOptions::default();
        zw.start_file("word/document.xml", opts).unwrap();
        zw.write_all(
            r"<w:document><w:body><w:p><w:r><w:t>季度预算分析</w:t></w:r></w:p></w:body></w:document>".as_bytes(),
        )
        .unwrap();
        zw.start_file("docProps/core.xml", opts).unwrap();
        zw.write_all(
            r"<cp:coreProperties><dc:title>Q1 报告</dc:title><dc:creator>李四</dc:creator></cp:coreProperties>".as_bytes(),
        )
        .unwrap();
        zw.finish().unwrap();

        let ExtractedDoc { entry, body, .. } = extract_document(&path, 7).unwrap();
        assert_eq!(entry.doc_type, "docx");
        assert_eq!(entry.title.as_deref(), Some("Q1 报告"));
        // 仅创建者、无最后保存者 → author = 创建者（combine 单侧回归）。
        assert_eq!(entry.author.as_deref(), Some("李四"));
        assert!(body.contains("季度预算分析"));
    }

    #[test]
    fn extract_docx_cfb_container_reports_encrypted_hint() {
        // 加密 docx / 老版二进制 .doc 误加 .docx 扩展名 → OLE2/CFB 容器，非 zip。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("encrypted.docx");
        let mut bytes = CFB_SIGNATURE.to_vec();
        bytes.extend_from_slice(&[0u8; 32]); // 补足字节，签名判定只看开头 8 字节。
        fs::write(&path, &bytes).unwrap();

        let err = extract_document(&path, 0).unwrap_err();
        let IndexError::Tag { detail, .. } = err else {
            panic!("expected Tag error, got {err:?}");
        };
        assert!(
            detail.contains("加密"),
            "CFB 容器应给出加密/旧格式提示而非原始 zip 报错，实得: {detail:?}"
        );
    }

    #[test]
    fn extract_docx_plain_corrupt_file_keeps_raw_zip_error() {
        // 非 CFB 的随意损坏内容（如截断下载）→ 仍走原始 zip 报错文本，不误判成加密。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.docx");
        fs::write(&path, b"not a zip at all").unwrap();

        let err = extract_document(&path, 0).unwrap_err();
        let IndexError::Tag { detail, .. } = err else {
            panic!("expected Tag error, got {err:?}");
        };
        assert!(
            !detail.contains("加密"),
            "非 CFB 文件不应判成加密, 实得: {detail:?}"
        );
    }

    #[test]
    fn extract_docx_author_includes_last_modified_by() {
        // BETA-55：docx 的最后保存者（cp:lastModifiedBy）并入 author、可被检索。
        use zip::write::SimpleFileOptions;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.docx");
        let f = fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = SimpleFileOptions::default();
        zw.start_file("word/document.xml", opts).unwrap();
        zw.write_all(
            r"<w:document><w:body><w:p><w:r><w:t>会议纪要</w:t></w:r></w:p></w:body></w:document>"
                .as_bytes(),
        )
        .unwrap();
        zw.start_file("docProps/core.xml", opts).unwrap();
        zw.write_all(
            r"<cp:coreProperties><dc:creator>张三</dc:creator><cp:lastModifiedBy>燎原</cp:lastModifiedBy></cp:coreProperties>".as_bytes(),
        )
        .unwrap();
        zw.finish().unwrap();

        let ExtractedDoc { entry, .. } = extract_document(&path, 0).unwrap();
        let author = entry.author.as_deref().expect("应有 author");
        assert!(author.contains("张三"), "author 应含创建者: {author}");
        assert!(author.contains("燎原"), "author 应含最后保存者: {author}");
    }

    #[test]
    fn extract_pptx_counts_slides() {
        use zip::write::SimpleFileOptions;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deck.pptx");
        let f = fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = SimpleFileOptions::default();
        for (i, word) in ["第一页", "第二页"].iter().enumerate() {
            zw.start_file(format!("ppt/slides/slide{}.xml", i + 1), opts)
                .unwrap();
            zw.write_all(
                format!(r"<p:sld><a:p><a:r><a:t>{word}</a:t></a:r></a:p></p:sld>").as_bytes(),
            )
            .unwrap();
        }
        // 干扰项：layout 不应计入。
        zw.start_file("ppt/slideLayouts/slideLayout1.xml", opts)
            .unwrap();
        zw.write_all(r"<x><a:t>布局噪声</a:t></x>".as_bytes())
            .unwrap();
        zw.finish().unwrap();

        let ExtractedDoc { entry, body, .. } = extract_document(&path, 0).unwrap();
        assert_eq!(entry.doc_type, "pptx");
        assert_eq!(entry.page_count, Some(2), "应只数 2 张幻灯片");
        assert!(body.contains("第一页"));
        assert!(body.contains("第二页"));
        assert!(!body.contains("布局噪声"));
    }

    #[test]
    fn unsupported_extension_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.zip");
        fs::write(&path, b"x").unwrap();
        assert!(matches!(
            extract_document(&path, 0),
            Err(IndexError::Tag { .. })
        ));
    }

    #[test]
    fn corrupt_docx_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.docx");
        fs::write(&path, b"not a zip").unwrap();
        assert!(matches!(
            extract_document(&path, 0),
            Err(IndexError::Tag { .. })
        ));
    }

    #[test]
    fn truncate_chars_caps_length() {
        let s = "a".repeat(10);
        assert_eq!(truncate_chars(&s, 4).chars().count(), 4);
        assert_eq!(truncate_chars(&s, 100).chars().count(), 10);
    }

    // ===== BETA-35 cycle 2：扫描版 PDF 判定 + extract_pdf 分支 =====

    #[test]
    fn is_scanned_pdf_empty_treated_as_scanned() {
        // 完全无文本层（0 字符）→ 扫描版。
        assert!(is_scanned_pdf(0));
    }

    #[test]
    fn is_scanned_pdf_below_floor_treated_as_scanned() {
        // 阈值以下 → 扫描版（文本层过于稀薄，几乎肯定需要 OCR）。
        assert!(is_scanned_pdf(1));
        assert!(is_scanned_pdf(50));
        assert!(is_scanned_pdf(99));
        assert!(is_scanned_pdf(SCANNED_TEXT_FLOOR_CHARS - 1));
    }

    #[test]
    fn is_scanned_pdf_at_or_above_floor_treated_as_text_layer() {
        // 阈值上方（含边界）→ 文本层充分，走原路径。
        assert!(!is_scanned_pdf(SCANNED_TEXT_FLOOR_CHARS));
        assert!(!is_scanned_pdf(101));
        assert!(!is_scanned_pdf(1_000));
        assert!(!is_scanned_pdf(10_000));
    }

    #[test]
    fn classify_pdf_text_layer_returns_body_unchanged() {
        // 分支①：非扫描版 → 原路径，byte-equal 关键点（passages/failed_pages 皆空）。
        let body = "a".repeat(200);
        let (title, author, out_body, page_count, passages, failed_pages) =
            classify_pdf_extraction(Path::new("/tmp/fake-text-layer.pdf"), body.clone()).unwrap();
        assert_eq!(title, None);
        assert_eq!(author, None);
        assert_eq!(out_body, body);
        assert_eq!(page_count, None);
        assert!(
            passages.is_empty(),
            "文本层 PDF 走原路径不应产生 passages（byte-equal 保护）"
        );
        assert!(
            failed_pages.is_empty(),
            "文本层 PDF 走原路径不应产生 failed_pages"
        );
    }

    // 注：cycle 2 阶段的 `classify_pdf_scanned_returns_tag_err_with_hint` /
    // `classify_pdf_empty_body_treated_as_scanned` 单测已被 cycle 3 pipeline 整合替换——
    // 扫描版分支现在走真 rasterize + OCR pipeline，结果依赖本机 pdftoppm/OCR 装机状态，
    // 不再是稳定的纯逻辑；扫描版检测本身由 `is_scanned_pdf_*` 单测覆盖，pipeline 聚合
    // 由下方 `aggregate_page_ocr_results_*` 单测覆盖。

    // ===== BETA-35 cycle 3：aggregate_page_ocr_results 纯逻辑 =====

    fn fake_ocr_err(page: u32) -> IndexError {
        IndexError::Tag {
            path: "/tmp/fake.pdf".to_string(),
            detail: format!("模拟第 {page} 页 OCR 失败"),
        }
    }

    #[test]
    fn aggregate_all_success_concatenates_pages_with_newline_and_page_no_kept() {
        let results = vec![
            (1, Ok("第一页 hello".to_string())),
            (2, Ok("第二页 world".to_string())),
            (3, Ok("第三页 done".to_string())),
        ];
        let (title, author, body, page_count, passages, failed_pages) =
            aggregate_page_ocr_results(Path::new("/tmp/x.pdf"), results).unwrap();
        assert_eq!(title, None);
        assert_eq!(author, None);
        assert!(body.contains("第一页"));
        assert!(body.contains("第二页"));
        assert!(body.contains("第三页"));
        // 三页应有 2 个换行分隔。
        assert_eq!(body.matches('\n').count(), 2);
        assert_eq!(page_count, Some(3));
        // BETA-35 cycle 4：page_no 结构保留在 passages 里，命中回页数据源。
        assert_eq!(passages.len(), 3);
        assert_eq!(passages[0].page_no, 1);
        assert_eq!(passages[0].seq, 0);
        assert_eq!(passages[0].text, "第一页 hello");
        assert_eq!(passages[2].page_no, 3);
        assert!(failed_pages.is_empty());
    }

    #[test]
    fn aggregate_partial_success_failed_page_recorded_with_reason() {
        // 3 页，1 失败：body/passages 只含成功页，failed_pages 记录失败页 + reason（验收 ③）。
        let results = vec![
            (1, Ok("有效文本".to_string())),
            (2, Err(fake_ocr_err(2))),
            (3, Ok("第三页".to_string())),
        ];
        let (_, _, body, page_count, passages, failed_pages) =
            aggregate_page_ocr_results(Path::new("/tmp/x.pdf"), results).unwrap();
        assert!(body.contains("有效文本"));
        assert!(body.contains("第三页"));
        assert!(!body.contains("模拟"), "失败页错误内容不应进 body");
        assert_eq!(
            page_count,
            Some(3),
            "page_count 应为总页数 3（含失败页），不是成功页数 2"
        );
        // passages 只含成功页。
        assert_eq!(passages.len(), 2);
        assert_eq!(passages[0].page_no, 1);
        assert_eq!(passages[1].page_no, 3);
        // failed_pages 记 (page_no, reason)——验收 ③。
        assert_eq!(failed_pages.len(), 1);
        assert_eq!(failed_pages[0].page_no, 2);
        assert!(failed_pages[0].reason.contains("模拟第 2 页 OCR 失败"));
    }

    #[test]
    fn aggregate_all_failure_returns_tag_err() {
        let results = vec![(1, Err(fake_ocr_err(1))), (2, Err(fake_ocr_err(2)))];
        let err = aggregate_page_ocr_results(Path::new("/tmp/x.pdf"), results).unwrap_err();
        match err {
            IndexError::Tag { detail, .. } => {
                assert!(
                    detail.contains("全部") && detail.contains("2 页"),
                    "全失败提示应含总页数: {detail}"
                );
            }
            other => panic!("应返回 IndexError::Tag，实际: {other:?}"),
        }
    }

    #[test]
    fn aggregate_empty_pages_returns_tag_err() {
        // 空 vec：视同全失败（0 页成功）。
        let err = aggregate_page_ocr_results(Path::new("/tmp/x.pdf"), vec![]).unwrap_err();
        assert!(matches!(err, IndexError::Tag { .. }));
    }

    #[test]
    fn aggregate_skips_whitespace_only_pages_from_body_and_passages() {
        // OCR 引擎返回全空白（页面识别正常但没字）→ 算 success 但不加入 body，也不生成 passage。
        let results = vec![
            (1, Ok("   \n  ".to_string())),
            (2, Ok("有效内容".to_string())),
        ];
        let (_, _, body, page_count, passages, failed_pages) =
            aggregate_page_ocr_results(Path::new("/tmp/x.pdf"), results).unwrap();
        assert_eq!(body, "有效内容", "空白页不进 body、不留分隔换行");
        assert_eq!(page_count, Some(2));
        // 空白页不生成 passage（检索价值为零）。
        assert_eq!(passages.len(), 1);
        assert_eq!(passages[0].page_no, 2);
        assert!(failed_pages.is_empty());
    }

    #[test]
    fn aggregate_first_page_whitespace_no_leading_newline() {
        // 首页空白 + 次页有内容 → body 应无前导换行。
        let results = vec![(1, Ok("   ".to_string())), (2, Ok("正文".to_string()))];
        let (_, _, body, _, passages, _) =
            aggregate_page_ocr_results(Path::new("/tmp/x.pdf"), results).unwrap();
        assert_eq!(body, "正文");
        assert_eq!(passages.len(), 1);
        assert_eq!(passages[0].page_no, 2);
    }
}
