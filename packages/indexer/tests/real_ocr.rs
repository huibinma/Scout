//! 真机 OCR 集成测试（BETA-03）。`#[ignore]`，仅 Windows 真机 + 装了 OCR 语言包时手动跑：
//! `cargo test -p scout-indexer --test real_ocr -- --ignored`。
//!
//! fixture `tests/fixtures/ocr_cjk.png` 是合成图片（白底黑字「会议纪要测试 OCR123」，
//! 无真实用户数据，符合 CONVENTIONS §7）。断言识别结果含关键子串（容忍 OCR 噪声）。

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::path::PathBuf;

use scout_indexer::{OcrEngine, WindowsOcrEngine};

#[test]
#[ignore = "需 Windows 真机 + 已装 OCR 识别语言（如 zh-Hans-CN）"]
fn windows_ocr_recognizes_cjk_fixture() {
    if !WindowsOcrEngine::detect() {
        eprintln!("跳过：本机无可用 OCR 识别语言");
        return;
    }
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("ocr_cjk.png");
    let engine = WindowsOcrEngine::new();
    let text = engine.recognize(&fixture).expect("OCR 应成功");
    // CJK 已折叠空格（normalize_ocr_text），整串无空格应可命中。
    assert!(
        text.contains("会议纪要测试"),
        "应识别出「会议纪要测试」（含 CJK 空格折叠），实得: {text:?}"
    );
}
