//! 真机集成测试：需要安装 Everything 并启用 ES CLI（`es.exe` 在 PATH 中）。
//!
//! 默认 `#[ignore]`，仅在装好 Everything + ES 的 Windows 真机上 `cargo test -- --ignored` 运行。
//! 验证 [`EsCliExecutor`] 端到端：`SearchIntent` → `es.exe` 参数 → stdout 路径 → `SearchResult`。
#![cfg(target_os = "windows")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use futures_util::StreamExt;
use scout_search_backend::{
    BackendKind, CancellationToken, ExpandedSearchIntent, KeywordGroup, SearchBackend, SearchIntent,
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

#[test]
#[ignore = "requires Everything + ES CLI (es.exe on PATH)"]
fn returns_existing_paths_for_extension_search() {
    let backend = EverythingBackend::new().expect("construct backend");
    assert!(
        backend.is_available(),
        "es.exe should be discoverable on PATH for this test"
    );

    let intent: SearchIntent = serde_json::from_value(serde_json::json!({
        "schema_version": "1.0",
        "intent": "file_search",
        "extensions": ["pdf"],
        "limit": 5
    }))
    .expect("intent");

    let stream = block_on(backend.search(&intent, CancellationToken::new())).expect("search ok");
    let results: Vec<_> = block_on(stream.collect::<Vec<_>>())
        .into_iter()
        .collect::<Result<_, _>>()
        .expect("collect results");

    eprintln!(
        "everything extension search returned {} results",
        results.len()
    );
    // 真机有 PDF 时后端必须返回非空——可捕获「es.exe 参数误吞搜索项」一类回归
    // （如早期 `-path` 误用导致 0 结果）。
    assert!(
        !results.is_empty(),
        "expected at least one pdf via Everything on a machine with indexed pdfs"
    );
    for result in &results {
        assert_eq!(result.source, BackendKind::Everything);
        assert!(
            result.path.exists(),
            "returned path should exist on disk: {}",
            result.path.display()
        );
    }
}

#[test]
#[ignore = "requires Everything + ES CLI (es.exe on PATH)"]
fn expanded_search_matches_synonyms_via_or() {
    // 同义词组 {工作汇报, 述职, 工作总结}：用户搜 head「工作汇报」，
    // 应经组内 `|` OR 展开命中只含 synonym「述职」/「工作总结」的文件——这是
    // BETA-15C 在真 es.exe 上的端到端验证（绕开「es 参数误吞」「OR 优先级」类回归）。
    let dir = std::env::temp_dir().join(format!("scout-es-expanded-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");

    let report = dir.join("年度工作汇报.txt");
    let zhishi = dir.join("述职2024.txt");
    let summary = dir.join("工作总结Q4.txt");
    let unrelated = dir.join("无关文件.txt");
    for path in [&report, &zhishi, &summary, &unrelated] {
        std::fs::write(path, b"x").expect("write test file");
    }
    // 给 Everything 索引新文件留出时间。
    std::thread::sleep(std::time::Duration::from_secs(2));

    let backend = EverythingBackend::new().expect("construct backend");
    assert!(
        backend.is_available(),
        "es.exe should be discoverable on PATH for this test"
    );

    let base: SearchIntent = serde_json::from_value(serde_json::json!({
        "schema_version": "1.0",
        "intent": "file_search",
        "keywords": ["工作汇报"],
        "location": { "include": [dir.to_string_lossy()] }
    }))
    .expect("intent");
    let expanded = ExpandedSearchIntent {
        base,
        keyword_groups: vec![KeywordGroup {
            head: "工作汇报".to_owned(),
            synonyms: vec!["述职".to_owned(), "工作总结".to_owned()],
        }],
        match_mode: scout_search_backend::MatchMode::default(),
    };

    let stream = block_on(backend.search_expanded(&expanded, CancellationToken::new()))
        .expect("search_expanded ok");
    let results: Vec<_> = block_on(stream.collect::<Vec<_>>())
        .into_iter()
        .collect::<Result<_, _>>()
        .expect("collect results");
    let names: Vec<&str> = results.iter().map(|result| result.name.as_str()).collect();
    eprintln!("expanded synonym search returned: {names:?}");

    // 组内三个同义词文件全部命中（含只有 synonym 的文件）。
    assert!(
        names.contains(&"年度工作汇报.txt"),
        "应命中 head 文件: {names:?}"
    );
    assert!(
        names.contains(&"述职2024.txt"),
        "应经 OR 命中只含 synonym「述职」的文件: {names:?}"
    );
    assert!(
        names.contains(&"工作总结Q4.txt"),
        "应经 OR 命中只含 synonym「工作总结」的文件: {names:?}"
    );
    // 无关文件不被误召回（OR 扩展不过度）。
    assert!(
        !names.contains(&"无关文件.txt"),
        "OR 扩展不应误召回无关文件: {names:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "requires Everything + ES CLI (es.exe on PATH)"]
fn cross_category_file_type_unions_extensions() {
    // BETA-18 跨范畴多类型真机验证：`file_type: ["presentation","document"]`（ppt 与 pdf 异范畴）
    // 应展开为两类扩展名并集，同时命中 .ppt 与 .pdf。旧实现退回首范畴会丢其一。
    let dir = std::env::temp_dir().join(format!("scout-es-crosscat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");

    let slides = dir.join("季度汇报.ppt");
    let doc = dir.join("年度预算.pdf");
    let noise = dir.join("照片.png");
    for path in [&slides, &doc, &noise] {
        std::fs::write(path, b"x").expect("write test file");
    }
    std::thread::sleep(std::time::Duration::from_secs(2));

    let backend = EverythingBackend::new().expect("construct backend");
    assert!(backend.is_available(), "es.exe should be on PATH");

    // file_type 用数组形式（scalar-or-vec serde 接受）。
    let intent: SearchIntent = serde_json::from_value(serde_json::json!({
        "schema_version": "1.0",
        "intent": "file_search",
        "file_type": ["presentation", "document"],
        "location": { "include": [dir.to_string_lossy()] }
    }))
    .expect("intent");

    let stream = block_on(backend.search(&intent, CancellationToken::new())).expect("search ok");
    let results: Vec<_> = block_on(stream.collect::<Vec<_>>())
        .into_iter()
        .collect::<Result<_, _>>()
        .expect("collect results");
    let names: Vec<&str> = results.iter().map(|r| r.name.as_str()).collect();
    eprintln!("cross-category search returned: {names:?}");

    assert!(
        names.contains(&"季度汇报.ppt"),
        "应命中 Presentation 的 .ppt: {names:?}"
    );
    assert!(
        names.contains(&"年度预算.pdf"),
        "应命中 Document 的 .pdf（旧实现退回首范畴会丢）: {names:?}"
    );
    assert!(
        !names.contains(&"照片.png"),
        "Image 不在 file_type 集合，不应命中: {names:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
