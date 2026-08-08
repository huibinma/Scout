//! PDF 页渲染层（BETA-35）。
//!
//! 把扫描版 PDF 的每一页渲染成 PNG，交给 [`crate::ocr::OcrEngine`] 识别。
//!
//! **架构约束**：`unsafe_code = forbid` 是 workspace lint，pdfium-render / mupdf-rs 这类
//! FFI crate 直接排除；沿用项目 shell-out 拿结构化输出 pattern（同 [`crate::ocr`] /
//! Everything es.exe / Spotlight mdfind）。
//!
//! **首实现 [`PopplerPdfRasterizer`]**：shell-out `pdftoppm`（poppler-utils，GPL-2/LGPL，
//! 律所场景可用；mupdf `mutool` / ghostscript 均 AGPL-3，红牌排除）。用户装机方式与
//! Tesseract 同 pattern（BETA-31 onboarding 引导步）。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use crate::IndexError;

/// 单份 PDF 页渲染超时（页多的扫描 PDF 可能数十秒，比单图 OCR 长）。
const RASTERIZE_TIMEOUT: Duration = Duration::from_secs(120);

/// 默认渲染 DPI（200 平衡 OCR 精度 vs 单页耗时，spec 期草案值；装机验证再复核）。
const DEFAULT_DPI: u32 = 200;

/// PDF 页渲染引擎。跨平台 + 跨实现（首实现 Poppler，可扩其他）。
pub trait PdfRasterizer: Send + Sync + std::fmt::Debug {
    /// 把 PDF 每页渲染成 PNG，返回 [`RasterizedPdf`]（RAII 管临时目录）。
    ///
    /// 失败（解码错 / 引擎错 / 超时 / 进程缺失）返回 [`IndexError::Tag`]，
    /// 由增量循环计 failed、跳过、不中断整轮（同 OCR 语义）。
    fn render_pages(&self, pdf: &Path) -> Result<RasterizedPdf, IndexError>;

    /// 引擎名（trace / 诊断用）。
    fn name(&self) -> &'static str;
}

/// 已渲染的 PDF 页集合。`Drop` 时自动删除临时目录（RAII，防临时 PNG 泄漏）。
///
/// 使用者遍历 [`pages()`](Self::pages) 拿 `(page_no, png_path)`，对每张调 OCR。
/// 整个 batch 处理完毕后 drop 本对象，临时目录自动清理。
#[derive(Debug)]
pub struct RasterizedPdf {
    pages: Vec<(u32, PathBuf)>,
    _tempdir: TempDir,
}

impl RasterizedPdf {
    /// 已渲染页列表：`(page_no from 1, png_path)`，按 `page_no` 升序。
    #[must_use]
    pub fn pages(&self) -> &[(u32, PathBuf)] {
        &self.pages
    }

    /// 页数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    /// 是否为空（未产出任何页——正常不该发生，rasterize 已失败即 Err）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }
}

/// 构造 [`IndexError::Tag`]（PDF rasterize 是按文件粒度的失败语义）。
fn tag_err(path: &Path, detail: impl Into<String>) -> IndexError {
    IndexError::Tag {
        path: path.to_string_lossy().into_owned(),
        detail: detail.into(),
    }
}

/// 给 `Command` 加 `CREATE_NO_WINDOW`（Windows）避免 spawn 时闪现控制台黑框；其他平台 no-op。
///
/// 与 [`crate::ocr`] 里的同名 helper 语义完全一致，本处独立一份是刻意保
/// ocr.rs 零改（BETA-35 spec 承诺）。
fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// spawn 外部进程、超时 kill、成功返回 `()`。
///
/// 与 [`crate::ocr::spawn_capture_stdout`] 相似但不 capture stdout（pdftoppm 写文件、
/// 不吐正文到 stdout）。失败统一映射为按 PDF 粒度的 [`IndexError::Tag`]。
fn spawn_and_wait(mut cmd: Command, pdf: &Path) -> Result<(), IndexError> {
    // stdin 必须显式关闭：某些 poppler 构建对加密 PDF 会在 stdin 上等密码输入，
    // 默认继承的父进程 stdin（桌面 GUI 进程，非交互终端）既不给 EOF 也不给数据，
    // 读会一直挂到 RASTERIZE_TIMEOUT 才被杀——是"pdftoppm 超时"批量出现的真根因之一
    // （另一半根因是加密 PDF 被误判成扫描版才走到这里，见 doc_extract.rs
    // `is_pdf_decryption_error` 的前置快速失败）。null 后 poppler 读到空/EOF 会立即报错退出。
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    no_window(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| tag_err(pdf, format!("spawn pdftoppm 进程失败: {e}")))?;
    let start = Instant::now();

    loop {
        if child
            .try_wait()
            .map_err(|e| tag_err(pdf, e.to_string()))?
            .is_some()
        {
            let output = child
                .wait_with_output()
                .map_err(|e| tag_err(pdf, e.to_string()))?;
            if output.status.success() {
                return Ok(());
            }
            return Err(tag_err(
                pdf,
                format!(
                    "pdftoppm 进程失败: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ));
        }
        if start.elapsed() >= RASTERIZE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(tag_err(pdf, "pdftoppm 超时"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ===== 引擎选择（纯逻辑，便于单测，不真调系统） =====

/// 引擎优先级判定结果。
#[derive(Debug, PartialEq, Eq)]
enum RasterizerPick {
    Poppler,
    None,
}

/// 纯优先级逻辑：Poppler 可用 → 用；否则 None（BETA-35 第一版只支持 Poppler，
/// 后续如加 macOS `qlmanage` / 其他兜底再扩枚举）。
fn pick_rasterizer(poppler_available: bool) -> RasterizerPick {
    if poppler_available {
        RasterizerPick::Poppler
    } else {
        RasterizerPick::None
    }
}

/// 选默认 PDF 渲染引擎：PATH 上有 `pdftoppm` → [`PopplerPdfRasterizer`]；
/// 否则 `None`（扫描版 PDF 优雅跳过，走增量 failed 语义）。
#[must_use]
pub fn default_pdf_rasterizer() -> Option<Box<dyn PdfRasterizer>> {
    let poppler_available = PopplerPdfRasterizer::detect();
    match pick_rasterizer(poppler_available) {
        RasterizerPick::Poppler => Some(Box::new(PopplerPdfRasterizer::new())),
        RasterizerPick::None => None,
    }
}

// ===== Poppler pdftoppm =====

/// 探测某个 `pdftoppm` 候选（PATH 裸名或绝对路径）能否起进程。
///
/// `pdftoppm -v` 把版本号写到 stderr、退出码 0；poppler 老版本可能非 0 但仍能跑——
/// 只要能起进程就认可用（`Command::status()` Ok 即通过）。
fn probe_pdftoppm(exe: &Path) -> bool {
    let mut cmd = Command::new(exe);
    cmd.arg("-v").stdout(Stdio::null()).stderr(Stdio::null());
    no_window(&mut cmd);
    cmd.status().is_ok()
}

/// 解析可用的 `pdftoppm`：先 PATH 裸名，再各平台已知安装位置兜底。
///
/// **为什么需要兜底**（v0.9.10 装机验证实锤）：GUI 进程的环境变量在启动时继承，
/// winget 装 poppler 改的是**注册表用户 PATH**，正在运行的 app 看不到——onboarding
/// 的"3s 自动重检"若只靠 PATH 会一直失败，被迫重启 app。兜底直接探测 winget
/// portable 包的落盘位置（Windows）与 Homebrew 前缀（macOS，GUI app 同样拿不到
/// shell 的 PATH 追加），命中后用**绝对路径** spawn。
fn resolve_pdftoppm() -> Option<PathBuf> {
    let bare = PathBuf::from("pdftoppm");
    if probe_pdftoppm(&bare) {
        return Some(bare);
    }
    known_install_candidates()
        .into_iter()
        .find(|p| p.is_file() && probe_pdftoppm(p))
}

/// Windows 已知安装位置：winget Links 目录 + winget portable 包落盘目录
/// （`Packages/oschwartz10612.Poppler_*/<ver>/Library/bin/pdftoppm.exe`，
/// onboarding 引导的推荐安装方式）。
#[cfg(windows)]
fn known_install_candidates() -> Vec<PathBuf> {
    let Ok(local) = std::env::var("LOCALAPPDATA") else {
        return Vec::new();
    };
    let winget = Path::new(&local).join("Microsoft").join("WinGet");
    let mut out = vec![winget.join("Links").join("pdftoppm.exe")];
    if let Ok(pkgs) = std::fs::read_dir(winget.join("Packages")) {
        for pkg in pkgs.flatten() {
            if !pkg
                .file_name()
                .to_string_lossy()
                .starts_with("oschwartz10612.Poppler")
            {
                continue;
            }
            if let Ok(vers) = std::fs::read_dir(pkg.path()) {
                for v in vers.flatten() {
                    out.push(v.path().join("Library").join("bin").join("pdftoppm.exe"));
                }
            }
        }
    }
    out
}

/// macOS / Linux 已知安装位置：Homebrew 两前缀（GUI app 拿不到 shell PATH 追加）。
#[cfg(not(windows))]
fn known_install_candidates() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/opt/homebrew/bin/pdftoppm"),
        PathBuf::from("/usr/local/bin/pdftoppm"),
    ]
}

/// Poppler PDF 页渲染引擎（shell-out `pdftoppm`，需用户装 poppler-utils）。
///
/// **调用**：`pdftoppm -r <dpi> -png <pdf> <temp_prefix>` → 产 `<temp_prefix>-1.png`
/// `<temp_prefix>-2.png` ...；本 impl 扫描临时目录、按 `page-N.png` 命名回读页码。
#[derive(Debug)]
pub struct PopplerPdfRasterizer {
    /// 渲染 DPI（默认 [`DEFAULT_DPI`] = 200）。
    dpi: u32,
    /// 已解析的 pdftoppm 可执行（PATH 裸名或已知安装位置绝对路径）。
    exe: PathBuf,
}

impl PopplerPdfRasterizer {
    /// 探测：本机是否有可用的 `pdftoppm`（PATH 或已知安装位置，见 [`resolve_pdftoppm`]）。
    #[must_use]
    pub fn detect() -> bool {
        resolve_pdftoppm().is_some()
    }

    /// 构造（默认 DPI 200）。
    #[must_use]
    pub fn new() -> Self {
        Self::with_dpi(DEFAULT_DPI)
    }

    /// 构造（自定义 DPI，装机验证或 evals 调参用）。
    ///
    /// 未解析到 pdftoppm 时退回 PATH 裸名（render 时报 spawn 失败、按 failed 语义走）。
    #[must_use]
    pub fn with_dpi(dpi: u32) -> Self {
        Self {
            dpi,
            exe: resolve_pdftoppm().unwrap_or_else(|| PathBuf::from("pdftoppm")),
        }
    }
}

impl Default for PopplerPdfRasterizer {
    fn default() -> Self {
        Self::new()
    }
}

impl PdfRasterizer for PopplerPdfRasterizer {
    fn render_pages(&self, pdf: &Path) -> Result<RasterizedPdf, IndexError> {
        let tempdir =
            tempfile::tempdir().map_err(|e| tag_err(pdf, format!("创建临时目录失败: {e}")))?;
        let prefix = tempdir.path().join("page");

        let mut cmd = Command::new(&self.exe);
        cmd.arg("-r")
            .arg(self.dpi.to_string())
            .arg("-png")
            .arg(pdf)
            .arg(&prefix);
        spawn_and_wait(cmd, pdf)?;

        let mut pages: Vec<(u32, PathBuf)> = Vec::new();
        let read_dir =
            std::fs::read_dir(tempdir.path()).map_err(|e| tag_err(pdf, e.to_string()))?;
        for entry in read_dir {
            let entry = entry.map_err(|e| tag_err(pdf, e.to_string()))?;
            let path = entry.path();
            if let Some(page_no) = parse_page_no(&path) {
                pages.push((page_no, path));
            }
        }
        pages.sort_by_key(|(n, _)| *n);

        if pages.is_empty() {
            return Err(tag_err(
                pdf,
                "pdftoppm 未产出任何 PNG（PDF 可能损坏或加密）",
            ));
        }

        Ok(RasterizedPdf {
            pages,
            _tempdir: tempdir,
        })
    }

    fn name(&self) -> &'static str {
        "poppler-pdftoppm"
    }
}

/// 从 pdftoppm 产出的文件名回读页码：`page-1.png` / `page-01.png` / `page-001.png`。
///
/// pdftoppm 自动把页码补零到 PDF 总页数宽度：4 页文档产 `page-1..page-4`，
/// 12 页文档产 `page-01..page-12`，123 页文档产 `page-001..page-123`。
fn parse_page_no(path: &Path) -> Option<u32> {
    let stem = path.file_stem()?.to_str()?;
    let ext = path.extension()?.to_str()?;
    if !ext.eq_ignore_ascii_case("png") {
        return None;
    }
    let n_str = stem.strip_prefix("page-")?;
    n_str.parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn pick_rasterizer_priority() {
        assert_eq!(pick_rasterizer(true), RasterizerPick::Poppler);
        assert_eq!(pick_rasterizer(false), RasterizerPick::None);
    }

    #[test]
    fn poppler_name() {
        assert_eq!(PopplerPdfRasterizer::new().name(), "poppler-pdftoppm");
    }

    #[test]
    fn poppler_default_dpi_is_200() {
        assert_eq!(PopplerPdfRasterizer::new().dpi, DEFAULT_DPI);
        assert_eq!(PopplerPdfRasterizer::with_dpi(300).dpi, 300);
    }

    #[cfg(windows)]
    #[test]
    fn known_install_candidates_cover_winget_locations() {
        // 兜底列表必须含 winget Links；有 poppler 包落盘时还应含其 Library/bin。
        let cands = known_install_candidates();
        assert!(
            cands
                .iter()
                .any(|p| p.to_string_lossy().contains("WinGet") && p.ends_with("pdftoppm.exe")),
            "应含 winget 位置候选，实得 {cands:?}"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn known_install_candidates_cover_homebrew_prefixes() {
        let cands = known_install_candidates();
        assert!(cands.contains(&PathBuf::from("/opt/homebrew/bin/pdftoppm")));
        assert!(cands.contains(&PathBuf::from("/usr/local/bin/pdftoppm")));
    }

    #[test]
    fn parse_page_no_various_widths() {
        // pdftoppm 补零到总页数宽度，全部形式都要覆盖。
        assert_eq!(parse_page_no(Path::new("page-1.png")), Some(1));
        assert_eq!(parse_page_no(Path::new("page-01.png")), Some(1));
        assert_eq!(parse_page_no(Path::new("page-001.png")), Some(1));
        assert_eq!(parse_page_no(Path::new("page-12.png")), Some(12));
        assert_eq!(parse_page_no(Path::new("page-123.png")), Some(123));
    }

    #[test]
    fn parse_page_no_full_path() {
        // 加临时目录前缀不影响。
        let p = std::path::PathBuf::from("/tmp/xyz/page-7.png");
        assert_eq!(parse_page_no(&p), Some(7));
    }

    #[test]
    fn parse_page_no_rejects_non_png() {
        assert_eq!(parse_page_no(Path::new("page-1.jpg")), None);
        assert_eq!(parse_page_no(Path::new("page-1.txt")), None);
    }

    #[test]
    fn parse_page_no_rejects_wrong_prefix() {
        // 不是 page- 前缀 → 不认。
        assert_eq!(parse_page_no(Path::new("foo-1.png")), None);
        assert_eq!(parse_page_no(Path::new("slide-1.png")), None);
        assert_eq!(parse_page_no(Path::new("1.png")), None);
    }

    #[test]
    fn parse_page_no_rejects_non_numeric() {
        assert_eq!(parse_page_no(Path::new("page-abc.png")), None);
        assert_eq!(parse_page_no(Path::new("page-.png")), None);
    }

    #[test]
    fn parse_page_no_case_insensitive_ext() {
        // Windows 上大小写扩展名都可能出现。
        assert_eq!(parse_page_no(Path::new("page-1.PNG")), Some(1));
        assert_eq!(parse_page_no(Path::new("page-1.Png")), Some(1));
    }

    #[test]
    fn rasterized_pdf_len_and_empty() {
        // 构造一个仅逻辑上有效的 RasterizedPdf（不真渲染，只测 accessor）。
        let tempdir = tempfile::tempdir().unwrap();
        let png = tempdir.path().join("page-1.png");
        std::fs::write(&png, b"fake png").unwrap();
        let r = RasterizedPdf {
            pages: vec![(1, png)],
            _tempdir: tempdir,
        };
        assert_eq!(r.len(), 1);
        assert!(!r.is_empty());
        assert_eq!(r.pages().len(), 1);
        assert_eq!(r.pages()[0].0, 1);
    }

    #[test]
    fn rasterized_pdf_drop_cleans_tempdir() {
        // 关键 RAII 语义：drop 后临时目录不再存在。
        let path = {
            let tempdir = tempfile::tempdir().unwrap();
            let saved_path = tempdir.path().to_path_buf();
            let r = RasterizedPdf {
                pages: vec![],
                _tempdir: tempdir,
            };
            drop(r);
            saved_path
        };
        assert!(!path.exists(), "RasterizedPdf drop 后临时目录应已删除");
    }
}
