//! 真机集成测试（`#[ignore]`，CI 不跑）。BETA-01A 发现层。
//!
//! Windows 需 Everything CLI（`winget install voidtools.Everything.Cli`）+ Everything 服务运行；
//! macOS 需 Spotlight。运行：
//! `cargo test -p scout-indexer --test real_discovery -- --ignored --nocapture`

#![allow(clippy::print_stderr)]

use scout_indexer::default_audio_discovery;

#[test]
#[ignore = "需真机 Everything(Win)/Spotlight(macOS)"]
fn discover_audio_smoke() {
    let Some(disc) = default_audio_discovery() else {
        eprintln!("当前平台无默认发现器，跳过");
        return;
    };
    match disc.discover_audio() {
        Ok(paths) => {
            eprintln!("发现 {} 条音频路径", paths.len());
            for p in paths.iter().take(5) {
                eprintln!("  {}", p.display());
            }
            assert!(
                !paths.is_empty(),
                "真机应发现至少 1 条音频（若确实无音频可忽略）"
            );
        }
        Err(e) => {
            eprintln!("发现失败/不可用（确认 Everything CLI 已装并运行）: {e}");
        }
    }
}
