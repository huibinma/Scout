//! 原型（spike）：全盘音频发现 → 提取 → 入库 → 跨目录搜索。
//!
//! 流程：
//!   1. 发现：[`scout_indexer::default_audio_discovery`]（Windows 内置 MFT/USN 原生索引、
//!      macOS Spotlight）按扩展名枚举全盘音频路径。
//!   2. 提取：对每条路径用 `scout_indexer::extract_metadata`（lofty）读标签。
//!   3. 入库：写进临时文件的 `MusicIndex`（真 SQLite + FTS5）。
//!   4. 诊断：统计耗时 / 标签覆盖率 / 失败样本（探明 OneDrive 占位符是否是坑）。
//!   5. 搜索：跑一条真实 FTS 查询，证明跨目录命中。
//!
//! 运行：`cargo run -p scout-indexer --example discover_audio`
//! （Windows 需以管理员权限运行终端——内置原生索引打开 NTFS 卷句柄的 Win32 硬性要求）

// 诊断型 demo binary：println/expect/cast 是其本职，统一允许。
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::doc_markdown,
    clippy::too_many_lines
)]

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use scout_indexer::{
    default_audio_discovery, extract_metadata, MusicIndex, MusicQuery, NoopProgress,
};

fn main() {
    // ---------- 1. 发现 ----------
    println!("【发现】枚举全盘音频…");
    let t0 = Instant::now();
    let Some(discovery) = default_audio_discovery() else {
        eprintln!("当前平台无默认发现器。");
        std::process::exit(1);
    };
    let paths = match discovery.discover_audio() {
        Ok(paths) => paths,
        Err(err) => {
            eprintln!("发现失败/不可用：{err}（Windows 请以管理员权限运行）。");
            std::process::exit(1);
        }
    };
    println!(
        "【发现】{} 条音频路径，耗时 {:?}",
        paths.len(),
        t0.elapsed()
    );
    if paths.is_empty() {
        return;
    }

    // ---------- 2+3. 提取 + 入库 ----------
    let db = std::env::temp_dir().join("scout_spike_audio.db");
    let _ = std::fs::remove_file(&db); // 干净重建
    let idx = MusicIndex::open(&db).expect("打开索引库");

    println!("【提取+入库】lofty 逐文件读标签（单线程）…");
    let t1 = Instant::now();
    let stats = idx.index_paths(&paths, &NoopProgress).expect("index_paths");
    let elapsed = t1.elapsed();
    println!(
        "【提取+入库】扫描 {} / 新增 {} / 更新 {} / 跳过 {} / 失败 {}，耗时 {:?}",
        stats.scanned, stats.added, stats.updated, stats.skipped, stats.failed, elapsed
    );
    if stats.scanned > 0 {
        let per = elapsed.as_secs_f64() / stats.scanned as f64 * 1000.0;
        println!("        平均每文件 {per:.1} ms");
    }

    // ---------- 4. 诊断：标签覆盖率 ----------
    let all = idx
        .query(&MusicQuery {
            limit: Some(100_000),
            ..Default::default()
        })
        .expect("query all");
    let with_artist = all.iter().filter(|e| e.artist.is_some()).count();
    let with_title = all.iter().filter(|e| e.title.is_some()).count();
    let with_album = all.iter().filter(|e| e.album.is_some()).count();
    println!(
        "【标签覆盖】入库 {} 条中：有 artist {} / 有 title {} / 有 album {}",
        all.len(),
        with_artist,
        with_title,
        with_album
    );
    println!("【样本】前 8 条有标签的记录：");
    for e in all
        .iter()
        .filter(|e| e.artist.is_some() || e.title.is_some())
        .take(8)
    {
        println!(
            "    [{}] {} - {} | {}",
            e.format.as_deref().unwrap_or("?"),
            e.artist.as_deref().unwrap_or("（无）"),
            e.title.as_deref().unwrap_or("（无）"),
            e.file_name
        );
    }

    // ---------- 4b. 诊断失败原因（探 OneDrive 占位符） ----------
    if stats.failed > 0 {
        let indexed: HashSet<&str> = all.iter().map(|e| e.path.as_str()).collect();
        let missing: Vec<&PathBuf> = paths
            .iter()
            .filter(|p| !indexed.contains(p.to_string_lossy().as_ref()))
            .take(5)
            .collect();
        println!(
            "【失败诊断】抽样 {} 个未入库文件，重读看原因：",
            missing.len()
        );
        for p in missing {
            match extract_metadata(p, 0) {
                Ok(_) => println!("    （重读成功，可能首次为占位符已水合）{}", p.display()),
                Err(err) => println!("    {err}"),
            }
        }
    }

    // ---------- 5. 跨目录搜索 demo ----------
    println!("\n【搜索 demo】跨目录命中：");
    // 用第一条有 artist 的记录的 artist 做一次真实 FTS 查询。
    if let Some(sample_artist) = all.iter().find_map(|e| e.artist.clone()) {
        let hits = idx
            .query(&MusicQuery {
                text: Some(sample_artist.clone()),
                limit: Some(5),
                ..Default::default()
            })
            .expect("query by artist");
        println!(
            "    查询 artist=\"{sample_artist}\" → {} 条命中：",
            hits.len()
        );
        for h in &hits {
            println!("      {}", h.path);
        }
    } else {
        println!(
            "    （所有文件都无 artist 标签 → FTS 无内容可搜，跨目录搜索需靠文件名/系统后端）"
        );
    }

    let _ = std::fs::remove_file(&db);
    println!("\n完成。");
}
