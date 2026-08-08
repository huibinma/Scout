//! 真机集成测试：需要可用的 Windows Search 索引（`WSearch` 服务运行 + 已索引文件）。
//!
//! 默认 `#[ignore]`，仅在 Windows 真机上显式 `cargo test -- --ignored` 运行。验证执行层
//! 端到端：`SearchIntent` → SQL → `Search.CollatorDSO` → `System.ItemUrl` → 真实路径还原。
#![cfg(target_os = "windows")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use futures_util::StreamExt;
use scout_search_backend::{BackendKind, CancellationToken, SearchBackend, SearchIntent};
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

fn run(intent_json: serde_json::Value) -> Vec<scout_search_backend::SearchResult> {
    let backend = WindowsSearchBackend::new().expect("construct backend");
    let intent: SearchIntent = serde_json::from_value(intent_json).expect("intent");
    let stream = block_on(backend.search(&intent, CancellationToken::new())).expect("search ok");
    block_on(stream.collect::<Vec<_>>())
        .into_iter()
        .collect::<Result<_, _>>()
        .expect("collect results")
}

#[test]
#[ignore = "requires a live Windows Search index"]
fn returns_existing_paths_for_extension_search() {
    let results = run(serde_json::json!({
        "schema_version": "1.0",
        "intent": "file_search",
        "extensions": ["pdf"],
        "limit": 5
    }));

    eprintln!("extension search returned {} results", results.len());
    for result in &results {
        assert_eq!(result.source, BackendKind::WindowsSearch);
        // 关键正确性断言：System.ItemUrl 还原出的路径必须真实存在于磁盘上，
        // 证明未误用本地化的 System.ItemPathDisplay。
        assert!(
            result.path.exists(),
            "returned path should exist on disk: {}",
            result.path.display()
        );
    }
}

#[test]
#[ignore = "requires a live Windows Search index"]
fn relative_time_query_executes_without_provider_error() {
    // 相对时间在执行器内解析为绝对 ISO 字面量；本用例确保不再触发
    // `Search.CollatorDSO` 的 DATEADD/GETDATE 语法错误（HRESULT 0x80040E14）。
    let results = run(serde_json::json!({
        "schema_version": "1.0",
        "intent": "file_search",
        "extensions": ["pdf"],
        "modified_time": { "type": "relative", "value": "last_30_days" },
        "limit": 5
    }));

    eprintln!("relative-time search returned {} results", results.len());
    for result in &results {
        assert!(
            result.path.exists(),
            "returned path should exist on disk: {}",
            result.path.display()
        );
    }
}
