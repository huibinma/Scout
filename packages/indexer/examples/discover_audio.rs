//! 原型（spike）：全盘音频发现 → 提取 → 入库 → 跨目录搜索。
//!
//! 流程：
//!   1. 发现：spawn `es.exe`（Everything CLI）按扩展名枚举**所有盘**的音频路径，
//!      经 `-export-txt -utf8-bom` 导出（规避中文 Windows GBK stdout 破坏 CJK 路径）。
//!   2. 提取：对每条路径用 `scout_indexer::extract_metadata`（lofty）读标签。
//!   3. 入库：写进临时文件的 `MusicIndex`（真 SQLite + FTS5）。
//!   4. 诊断：统计耗时 / 标签覆盖率 / 失败样本（探明 OneDrive 占位符是否是坑）。
//!   5. 搜索：跑一条真实 FTS 查询，证明跨目录命中。
//!
//! 运行：`cargo run -p scout-indexer --example discover_audio [es.exe路径]`

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
use std::process::Command;
use std::time::Instant;

use scout_indexer::{extract_metadata, MusicIndex, MusicQuery, NoopProgress};

const AUDIO_QUERY: &str = "ext:mp3;flac;m4a;aac;ogg;opus;wav;wma;aiff;aif;ape";
const ES_FALLBACK: &str = r"C:\Users\alice\AppData\Local\Microsoft\WinGet\Packages\voidtools.Everything.Cli_Microsoft.Winget.Source_8wekyb3d8bbwe\es.exe";

fn main() {
    let es = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "es.exe".to_string());

    // ---------- 1. 发现 ----------
    let export = std::env::temp_dir().join("scout_audio_discovery.txt");
    println!("【发现】调用 Everything 枚举全盘音频…");
    let t0 = Instant::now();
    let paths = discover(&es, &export).or_else(|| discover(ES_FALLBACK, &export));
    let Some(paths) = paths else {
        eprintln!("无法调用 es.exe（Everything CLI）。请确认已安装并在 PATH 中。");
        std::process::exit(1);
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

/// 调用 es.exe 导出全盘音频路径。失败返回 None。
fn discover(es: &str, export: &PathBuf) -> Option<Vec<PathBuf>> {
    let status = Command::new(es)
        .args([
            AUDIO_QUERY,
            "-export-txt",
            &export.to_string_lossy(),
            "-utf8-bom",
        ])
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let bytes = std::fs::read(export).ok()?;
    // 去 UTF-8 BOM。
    let text = String::from_utf8_lossy(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes));
    Some(
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect(),
    )
}
