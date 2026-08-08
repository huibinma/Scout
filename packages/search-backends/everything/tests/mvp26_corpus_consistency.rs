//! MVP-26 跨平台一致性（后端结果集层，Everything 侧）。
//!
//! 对 PROTO-05A 合成语料（与 macOS Spotlight / Windows Search 测试同一套）跑文件名可解析的
//! 代表性查询，断言 [`EverythingBackend`] 经 `es.exe` 返回语义正确的预期文件。Everything 是
//! **纯文件名 / 路径**引擎，故只覆盖扩展名 + 文件名关键词子串查询（与能力路由「内容查询走
//! Windows Search」分工一致）；与 Windows Search 侧的 `mvp26_corpus_consistency.rs` 合起来构成
//! 后端层跨平台一致性证据。
//!
//! 同时覆盖两个真机点：
//!   1. `location.include` 递归限定到含**子目录**的语料根（`path_under` 递归 scope 修复）。
//!   2. CJK 文件名（合成-预算 / 周华健）经 `-export-txt -utf8-bom` 正确还原（BETA-15C CJK 修复）。
//!
//! 默认 `#[ignore]`。运行前置（装好 Everything + ES CLI 的 Windows 真机）：
//!   1. `cargo run -p scout-evals --bin fixtures -- --dir <CORPUS> generate`
//!      （CORPUS 任意可被 Everything 索引的目录，如 `%USERPROFILE%\scout-mvp26-corpus`）
//!   2. `set SCOUT_MVP26_CORPUS=<CORPUS>` 后 `cargo test -p scout-search-backend-everything
//!      --test mvp26_corpus_consistency -- --ignored`
#![cfg(target_os = "windows")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use futures_util::StreamExt;
use scout_search_backend::{
    BackendKind, CancellationToken, SearchBackend, SearchIntent, SearchResult,
};
use scout_search_backend_everything::EverythingBackend;

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let waker = futures_util::task::noop_waker();
    let mut context = std::task::Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        std::thread::yield_now();
    }
}

/// 把 `location.include` 注入语料目录后经 `EverythingBackend` 执行查询。
fn search(corpus: &str, mut intent_json: serde_json::Value) -> Vec<SearchResult> {
    intent_json["location"] = serde_json::json!({ "include": [corpus] });
    let backend = EverythingBackend::new().expect("construct backend");
    assert!(
        backend.is_available(),
        "es.exe 应在 PATH 上可用（装 voidtools.Everything.Cli）"
    );
    let intent: SearchIntent = serde_json::from_value(intent_json).expect("intent");
    let stream = block_on(backend.search(&intent, CancellationToken::new())).expect("search ok");
    block_on(stream.collect::<Vec<_>>())
        .into_iter()
        .collect::<Result<_, _>>()
        .expect("collect results")
}

fn names(results: &[SearchResult]) -> Vec<String> {
    results.iter().map(|result| result.name.clone()).collect()
}

#[test]
#[ignore = "requires Everything + ES CLI + generated+indexed corpus; set SCOUT_MVP26_CORPUS"]
fn corpus_queries_return_expected_files() {
    let corpus = std::env::var("SCOUT_MVP26_CORPUS")
        .expect("set SCOUT_MVP26_CORPUS to the generated+indexed corpus dir");

    // 1) 扩展名 pdf —— 语料内唯一 pdf（位于 Downloads/ 子目录 → 验证递归 scope）
    let pdf = search(
        &corpus,
        serde_json::json!({"schema_version":"1.0","intent":"file_search","extensions":["pdf"]}),
    );
    let pdf_names = names(&pdf);
    assert!(
        pdf_names
            .iter()
            .any(|n| n == "synthetic-received-last-week.pdf"),
        "pdf 查询应命中唯一 pdf: {pdf_names:?}"
    );
    for result in &pdf {
        assert_eq!(result.source, BackendKind::Everything);
        assert!(
            result.path.exists(),
            "返回路径应真实存在: {}",
            result.path.display()
        );
    }

    // 2) 扩展名 docx —— 两个 docx 都应命中（含 CJK 文件名，分布在 Desktop/ 与 Documents/）
    let docx = names(&search(
        &corpus,
        serde_json::json!({"schema_version":"1.0","intent":"file_search","extensions":["docx"]}),
    ));
    assert!(
        docx.iter().any(|n| n == "synthetic-word-doc.docx"),
        "docx: {docx:?}"
    );
    assert!(
        docx.iter().any(|n| n == "合成-预算-2026.docx"),
        "docx（CJK 文件名 utf8-bom 还原）: {docx:?}"
    );

    // 3) 扩展名 pptx —— 三个 pptx 都应命中（分布在 Desktop/ 与 Documents/、Documents/2025/）
    let pptx = names(&search(
        &corpus,
        serde_json::json!({"schema_version":"1.0","intent":"file_search","extensions":["pptx"]}),
    ));
    for expected in [
        "synthetic-budget.pptx",
        "synthetic-presentation-2025.pptx",
        "budget-plan.pptx",
    ] {
        assert!(
            pptx.iter().any(|n| n == expected),
            "pptx 缺 {expected}（递归 scope 应覆盖子目录）: {pptx:?}"
        );
    }

    // 4) 文件名关键词 预算（CJK 子串） —— Everything 按文件名匹配
    let kw = names(&search(
        &corpus,
        serde_json::json!({"schema_version":"1.0","intent":"file_search","keywords":["预算"]}),
    ));
    assert!(
        kw.iter().any(|n| n == "合成-预算-2026.docx"),
        "关键词 预算（文件名子串）: {kw:?}"
    );

    // 5) 文件名关键词 周华健 —— 两个音乐文件（Music/ 子目录，CJK）
    let artist = names(&search(
        &corpus,
        serde_json::json!({"schema_version":"1.0","intent":"file_search","keywords":["周华健"]}),
    ));
    assert!(
        artist.iter().filter(|n| n.contains("周华健")).count() >= 2,
        "关键词 周华健 应命中 ≥2: {artist:?}"
    );

    eprintln!(
        "MVP-26 Everything 语料一致性: 5 类文件名可解析查询全部命中预期文件（含递归子目录 + CJK）"
    );
}
