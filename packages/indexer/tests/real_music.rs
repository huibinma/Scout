//! 真机集成测试（`#[ignore]`，CI 不跑，仿 windows-search `real_*` 模式）。
//!
//! 在有音乐目录的真机上手动运行：
//! `cargo test -p scout-indexer --test real_music -- --ignored --nocapture`

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use scout_indexer::{default_music_roots, MusicIndex, MusicQuery};

#[test]
#[ignore = "需真机音乐目录"]
fn index_default_music_roots_smoke() {
    let roots = default_music_roots();
    if roots.is_empty() {
        eprintln!("跳过：无法确定系统音乐目录");
        return;
    }
    eprintln!("索引根目录: {roots:?}");

    let dir = tempfile::tempdir().unwrap();
    let idx = MusicIndex::open(&dir.path().join("music.db")).unwrap();
    let stats = idx.index_dirs(&roots).unwrap();
    eprintln!("索引统计: {stats:?}");

    let total = idx.count().unwrap();
    eprintln!("记录总数: {total}");
    if total == 0 {
        eprintln!("音乐目录为空或无支持的音频格式，跳过查询断言");
        return;
    }

    // 抽样：无过滤查询应返回非空。
    let any = idx
        .query(&MusicQuery {
            limit: Some(5),
            ..Default::default()
        })
        .unwrap();
    assert!(!any.is_empty(), "有记录时无过滤查询应非空");
    for e in &any {
        eprintln!(
            "  {} | artist={:?} title={:?} dur={:?}s fmt={:?}",
            e.file_name, e.artist, e.title, e.duration_secs, e.format
        );
    }
}
