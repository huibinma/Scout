//! 真机集成测试（`#[ignore]`，CI 不跑，仿 windows-search `real_*` 模式）。
//!
//! 在有文档目录的真机上手动运行：
//! `cargo test -p scout-indexer --test real_documents -- --ignored --nocapture`

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use scout_indexer::{default_document_roots, DocumentIndex, DocumentQuery};

#[test]
#[ignore = "需真机文档目录"]
fn index_default_document_roots_smoke() {
    let roots = default_document_roots();
    if roots.is_empty() {
        eprintln!("跳过：无法确定系统文档目录");
        return;
    }
    eprintln!("索引根目录: {roots:?}");

    let dir = tempfile::tempdir().unwrap();
    let idx = DocumentIndex::open(&dir.path().join("documents.db")).unwrap();
    let stats = idx.index_dirs(&roots).unwrap();
    eprintln!("索引统计: {stats:?}");

    let total = idx.count().unwrap();
    eprintln!("记录总数: {total}");
    if total == 0 {
        eprintln!("文档目录为空或无支持格式，跳过查询断言");
        return;
    }

    let any = idx
        .query(&DocumentQuery {
            limit: Some(5),
            ..Default::default()
        })
        .unwrap();
    assert!(!any.is_empty(), "有记录时无过滤查询应非空");
    for h in &any {
        eprintln!(
            "  {} | type={} title={:?} author={:?} pages={:?}",
            h.entry.file_name, h.entry.doc_type, h.entry.title, h.entry.author, h.entry.page_count
        );
    }
}
