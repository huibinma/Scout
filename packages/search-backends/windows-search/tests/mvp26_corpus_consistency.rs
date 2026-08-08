//! MVP-26 跨平台一致性（后端结果集层，Windows 侧）。
//!
//! 对 PROTO-05A 合成语料（与 macOS Spotlight 测试同一套）跑代表性查询，断言 `WindowsSearchBackend`
//! 返回语义正确的预期文件。结合 parser 层双平台 evals 0pp 一致性，构成后端层跨平台一致性证据。
//!
//! 默认 `#[ignore]`。运行前置（Windows 真机）：
//!   1. `cargo run -p scout-evals --bin fixtures -- --dir <CORPUS> generate`（CORPUS 须在
//!      Windows Search 索引范围内，如 `%USERPROFILE%\Documents\scout-mvp26-corpus`）
//!   2. 等待 Windows Search 索引完成（查询 SCOPE='file:<CORPUS>' 返回 ~26 项）
//!   3. `set SCOUT_MVP26_CORPUS=<CORPUS>` 后 `cargo test -p scout-search-backend-windows-search
//!      --test mvp26_corpus_consistency -- --ignored`
#![cfg(target_os = "windows")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use futures_util::StreamExt;
use scout_search_backend::{CancellationToken, SearchBackend, SearchIntent, SearchResult};
use scout_search_backend_windows_search::WindowsSearchBackend;

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

/// 把 `location.include` 注入语料目录后经 `WindowsSearchBackend` 执行查询。
fn search(corpus: &str, mut intent_json: serde_json::Value) -> Vec<SearchResult> {
    intent_json["location"] = serde_json::json!({ "include": [corpus] });
    let backend = WindowsSearchBackend::new().expect("construct backend");
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
#[ignore = "requires generated+indexed corpus; set SCOUT_MVP26_CORPUS"]
fn corpus_queries_return_expected_files() {
    let corpus = std::env::var("SCOUT_MVP26_CORPUS")
        .expect("set SCOUT_MVP26_CORPUS to the generated+indexed corpus dir");

    // 1) 扩展名 pdf —— 语料内唯一 pdf
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
        assert!(
            result.path.exists(),
            "返回路径应真实存在（SCOPE 限定 + ItemUrl 还原）: {}",
            result.path.display()
        );
    }

    // 2) 扩展名 docx —— 两个 docx 都应命中
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
        "docx: {docx:?}"
    );

    // 3) 扩展名 pptx —— 三个 pptx 都应命中
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
            "pptx 缺 {expected}: {pptx:?}"
        );
    }

    // 4) 关键词 预算（中文文件名子串）
    let kw = names(&search(
        &corpus,
        serde_json::json!({"schema_version":"1.0","intent":"file_search","keywords":["预算"]}),
    ));
    assert!(
        kw.iter().any(|n| n == "合成-预算-2026.docx"),
        "关键词 预算: {kw:?}"
    );

    // 5) 关键词 周华健 —— 两个音乐文件
    let artist = names(&search(
        &corpus,
        serde_json::json!({"schema_version":"1.0","intent":"file_search","keywords":["周华健"]}),
    ));
    assert!(
        artist.iter().filter(|n| n.contains("周华健")).count() >= 2,
        "关键词 周华健 应命中 ≥2: {artist:?}"
    );

    eprintln!("MVP-26 corpus consistency: 5 类代表性查询全部命中预期文件");
}
