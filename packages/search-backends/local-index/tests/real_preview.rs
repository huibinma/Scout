//! BETA-20 真机集成测试：对**真实 index.db**（后台索引产物）跑 `LocalIndexBackend::preview`，
//! 验证文档正文 / 音频元数据 / FTS 命中高亮端到端可取。
//!
//! 默认 `#[ignore]`（CI 无真实库）。真机跑：
//! `cargo test -p scout-local-index-backend --test real_preview -- --ignored --nocapture`

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::path::PathBuf;

use scout_indexer::{DocumentIndex, DocumentQuery, MusicIndex, MusicQuery};
use scout_local_index_backend::{fts_match_for_groups, LocalIndexBackend, LocalPreview};
use scout_search_backend::{KeywordGroup, MatchMode};

/// 真实索引库路径：`data_dir()/Scout/index.db`（Windows = `%APPDATA%/Scout`）。
fn real_db_path() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share"));
    let p = base?.join("Scout").join("index.db");
    p.exists().then_some(p)
}

#[test]
#[ignore = "需真实 index.db（后台索引产物）"]
fn preview_real_document_returns_body_and_highlight() {
    let Some(db) = real_db_path() else {
        eprintln!("跳过：未找到真实 index.db");
        return;
    };
    let backend = LocalIndexBackend::new(&db);

    // 取一条真实文档（含正文）。
    let docs = DocumentIndex::open(&db).unwrap();
    let hits = docs
        .query(&DocumentQuery {
            limit: Some(20),
            ..Default::default()
        })
        .unwrap();
    if hits.is_empty() {
        eprintln!("跳过：真实库无文档记录");
        return;
    }

    // 找一条 preview body 非空的文档（部分占位/空文档可能 body 为空）。
    let mut checked = 0usize;
    for hit in &hits {
        let path = &hit.entry.path;
        let preview = backend.preview(path, None).unwrap();
        let Some(LocalPreview::Document(d)) = preview else {
            continue;
        };
        checked += 1;
        eprintln!(
            "文档预览 OK: {} (type={}, body_chars={})",
            d.entry.file_name,
            d.entry.doc_type,
            d.body.chars().count()
        );
        if d.body.chars().count() >= 3 {
            // 用正文前几个字符构造命中表达式，验证 snippet 高亮哨兵注入。
            let needle: String = d.body.chars().take(3).collect();
            let fts = fts_match_for_groups(&[KeywordGroup::singleton(&needle)], MatchMode::All);
            let hl = backend.preview(path, fts.as_deref()).unwrap();
            if let Some(LocalPreview::Document(d2)) = hl {
                if let Some(snip) = d2.snippet {
                    assert!(
                        snip.contains('\u{2}') && snip.contains('\u{3}'),
                        "命中片段应含高亮哨兵: {snip:?}"
                    );
                    eprintln!("  命中高亮 OK: {snip:?}");
                    return; // 验证到一条带高亮的即可
                }
            }
        }
    }
    assert!(checked > 0, "应至少有一条文档可预览");
}

#[test]
#[ignore = "需真实 index.db（后台索引产物）"]
fn preview_real_music_returns_metadata() {
    let Some(db) = real_db_path() else {
        eprintln!("跳过：未找到真实 index.db");
        return;
    };
    let backend = LocalIndexBackend::new(&db);

    let music = MusicIndex::open(&db).unwrap();
    let entries = music
        .query(&MusicQuery {
            limit: Some(5),
            ..Default::default()
        })
        .unwrap();
    if entries.is_empty() {
        eprintln!("跳过：真实库无音乐记录");
        return;
    }
    let path = &entries[0].path;
    let preview = backend.preview(path, None).unwrap();
    match preview {
        Some(LocalPreview::Music(e)) => {
            eprintln!(
                "音频预览 OK: {} (artist={:?}, title={:?}, dur={:?})",
                e.file_name, e.artist, e.title, e.duration_secs
            );
        }
        other => panic!("应返回音频预览，实得 {other:?}"),
    }
}
