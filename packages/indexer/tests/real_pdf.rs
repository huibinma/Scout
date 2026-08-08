//! 真机 PDF 页渲染集成测试（BETA-35）+ BETA-41 fixture 端到端。
//!
//! 三层：
//! 1. **常跑（无装机依赖）**：BETA-41 fixture 的文本层 PDF 走原路径、扫描 PDF
//!    必进扫描分支（结果二分：无 poppler/OCR → 报"需 pdftoppm/OCR"错；装机 →
//!    产出带页码 passages。两者都证明 `is_scanned_pdf` 判定生效）。
//! 2. **`#[ignore]` 探测层**：装机验证 `PopplerPdfRasterizer::detect` /
//!    `default_pdf_rasterizer` / `SCOUT_TEST_PDF` rasterize 通路。
//! 3. **`#[ignore]` 端到端**：装了 pdftoppm + OCR 引擎的机器上，对 BETA-41 全部
//!    扫描 fixture 跑 rasterize + OCR，断言页码映射与合成关键词命中：
//!    `cargo test -p scout-indexer --test real_pdf -- --ignored`。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    clippy::panic
)]

use std::path::PathBuf;

use scout_indexer::{
    default_ocr_engine, default_pdf_rasterizer, extract_document, PdfRasterizer,
    PopplerPdfRasterizer,
};

/// BETA-41 fixture 文件根（`packages/evals/fixtures/enterprise-recall/files/`）。
fn fixture_files(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../evals/fixtures/enterprise-recall/files")
        .join(rel)
}

/// 扫描版 fixture 清单：(相对路径, 期望页数, OCR 关键词——命中任一即可，容忍识别误差)。
const SCANNED_FIXTURES: &[(&str, u32, &[&str])] = &[
    ("lawfirm/e00005-judgment-scan.pdf", 2, &["判决", "违约金"]),
    ("lawfirm/e00006-judgment-rescan.pdf", 2, &["判决", "违约金"]),
    (
        "lawfirm/e00007-judgment-copy-scan.pdf",
        2,
        &["判决", "违约金"],
    ),
    (
        "lawfirm/e00001-hearing-transcript-scan.pdf",
        1,
        &["开庭", "冲压设备"],
    ),
    ("lawfirm/e00002-contract-scan.pdf", 2, &["预付款", "交货期"]),
    ("audit/e00041-invoice-scan.pdf", 1, &["发票", "一体机"]),
    (
        "audit/e00043-goods-receipt-scan.pdf",
        1,
        &["验收", "二十台"],
    ),
    ("offboarding/e00078-nda-scan.pdf", 1, &["保密", "李示例"]),
    (
        "offboarding/e00079-handover-confirm-scan.pdf",
        1,
        &["交接", "王样本"],
    ),
];

#[test]
fn beta41_text_layer_pdf_takes_original_path() {
    // 文本层对照（英文 Helvetica）：走 BETA-27 原路径——body 非空、passages/failed 皆空。
    let doc = extract_document(
        &fixture_files("lawfirm/e00019-supply-agreement-summary.pdf"),
        0,
    )
    .expect("文本层 PDF 提取应成功（pdf-extract，无装机依赖）");
    assert!(
        doc.body.contains("Northridge"),
        "文本层正文应含合成关键词，实得: {}",
        doc.body.chars().take(120).collect::<String>()
    );
    assert!(doc.body.chars().count() >= 100, "文本层不应被判为扫描版");
    assert!(doc.passages.is_empty(), "原路径 passages 应为空");
    assert!(doc.failed_pages.is_empty(), "原路径 failed_pages 应为空");
}

#[test]
fn cjk_cmap_text_layer_pdf_falls_back_to_ocr_not_panic() {
    // BETA-40 收尾回归：中文 CID 字体 CMap（UniGB-UCS2-H）文本层 PDF 会让
    // pdf-extract **panic**——修复后 extract_pdf 内层 catch_unwind 降级 OCR 管线。
    // CI 安全的二分断言（与 beta41_scanned_fixtures_enter_scanned_branch 同款）：
    // 未装 poppler/OCR → Err 指向依赖；装机 → Ok 带 passages。两者都证明不再 panic 丢文件。
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../../test-materials/enterprise-scenarios-raw/real-formats/lawfirm/blueharbor-judgment-text-layer.pdf",
    );
    match extract_document(&path, 0) {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("pdftoppm") || msg.contains("OCR"),
                "UniGB PDF 应因缺 pdftoppm/OCR 失败（证明降级到 OCR 分支），实为: {msg}"
            );
        }
        Ok(doc) => {
            assert!(
                !doc.passages.is_empty(),
                "UniGB PDF Ok 时应带 OCR passages（降级链产物）"
            );
        }
    }
}

#[test]
fn beta41_scanned_fixtures_enter_scanned_branch() {
    // 无装机依赖的二分守卫：扫描 fixture 必进扫描分支——
    // 未装 poppler/OCR → Err 且错误信息指向依赖；装机 → Ok 且带页码 passages。
    // 若 is_scanned_pdf 判定失效（走了原路径），会 Ok 但 passages 为空 → 此测试失败。
    for (rel, _pages, _kw) in SCANNED_FIXTURES {
        match extract_document(&fixture_files(rel), 0) {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("pdftoppm") || msg.contains("OCR"),
                    "{rel} 应因缺 pdftoppm/OCR 失败（证明进了扫描分支），实为: {msg}"
                );
            }
            Ok(doc) => {
                assert!(
                    !doc.passages.is_empty(),
                    "{rel} Ok 却无 passages——扫描判定未生效（走了原路径）"
                );
            }
        }
    }
}

#[test]
#[ignore = "需装 poppler（pdftoppm）+ OCR 引擎（Windows.Media.Ocr / Tesseract），装机手动跑"]
fn beta41_scanned_fixtures_ocr_end_to_end() {
    if !PopplerPdfRasterizer::detect() {
        eprintln!("跳过：本机无 pdftoppm");
        return;
    }
    if default_ocr_engine().is_none() {
        eprintln!("跳过：本机无可用 OCR 引擎");
        return;
    }
    for (rel, expect_pages, keywords) in SCANNED_FIXTURES {
        let doc = extract_document(&fixture_files(rel), 0)
            .unwrap_or_else(|e| panic!("{rel} 端到端提取应成功: {e}"));
        assert!(!doc.passages.is_empty(), "{rel} 应至少 1 页 OCR 成功");
        for p in &doc.passages {
            assert!(
                p.page_no >= 1 && p.page_no <= *expect_pages,
                "{rel} passage 页码 {} 越界（共 {expect_pages} 页）",
                p.page_no
            );
        }
        assert!(
            keywords.iter().any(|kw| doc.body.contains(kw)),
            "{rel} OCR 文本未命中任一合成关键词 {keywords:?}，实得前 200 字: {}",
            doc.body.chars().take(200).collect::<String>()
        );
        eprintln!(
            "{rel}: {} 页成功 / {} 页失败，body {} chars ✓",
            doc.passages.len(),
            doc.failed_pages.len(),
            doc.body.chars().count()
        );
    }
}

#[test]
#[ignore = "需装 poppler-utils（pdftoppm），装机手动跑"]
fn poppler_detect_and_factory_produce_engine() {
    if !PopplerPdfRasterizer::detect() {
        eprintln!("跳过：本机无 pdftoppm（poppler-utils）");
        return;
    }
    let engine = default_pdf_rasterizer().expect("装了 pdftoppm 时工厂应返 Some");
    assert_eq!(engine.name(), "poppler-pdftoppm");
}

#[test]
#[ignore = "需装 poppler + 一份 PDF 供 env SCOUT_TEST_PDF 指向"]
fn poppler_renders_pages_from_env_supplied_pdf() {
    if !PopplerPdfRasterizer::detect() {
        eprintln!("跳过：本机无 pdftoppm");
        return;
    }
    let Ok(pdf_path) = std::env::var("SCOUT_TEST_PDF") else {
        eprintln!("跳过：未设 SCOUT_TEST_PDF 指向测试 PDF 路径");
        return;
    };
    let engine = PopplerPdfRasterizer::new();
    let rendered = engine
        .render_pages(&PathBuf::from(&pdf_path))
        .expect("render_pages 应成功（若 PDF 损坏 / 加密请换一份）");
    assert!(!rendered.is_empty(), "至少应有 1 页 PNG 产出");
    for (page_no, png_path) in rendered.pages() {
        assert!(*page_no >= 1, "page_no 从 1 起");
        assert!(png_path.exists(), "临时 PNG 应存在（drop 前）");
        assert_eq!(
            png_path.extension().and_then(|e| e.to_str()),
            Some("png"),
            "产出应为 PNG"
        );
    }
    // 让 rendered drop → 临时目录自动清理（RAII 语义）。
    let saved_first_png = rendered.pages()[0].1.clone();
    drop(rendered);
    assert!(
        !saved_first_png.exists(),
        "RasterizedPdf drop 后临时 PNG 应被清理"
    );
}
