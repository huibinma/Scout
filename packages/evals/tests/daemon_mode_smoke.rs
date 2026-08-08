//! BETA-32 T12：evals --mode daemon 接通 smoke 测试。
//!
//! 本 task 不跑全量 v0.9 真模型评测（成本太高、留 T14 红线 4）。本 smoke 只验证
//! mode 接通：
//!
//! 1. `--mode daemon` 必填参数缺失时退出非零；
//! 2. （可选）若环境提供 `SCOUT_MODEL_PATH` 与 `SCOUT_DAEMON_BIN`，跑一
//!    条最小 case 端到端拿 top-K。无环境变量时 skip——CI 不能也不应当假定有 GGUF
//!    模型可用。
//!
//! 真模型端到端 smoke 留 T14 红线 4 阶段在专门 evals/CI 环境跑。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    clippy::print_stdout
)]

use std::process::Command;

fn evals_bin() -> String {
    env!("CARGO_BIN_EXE_evals").to_owned()
}

#[test]
fn daemon_mode_requires_daemon_binary() {
    // 缺 --daemon-binary / --root / --model-path → 应当退非零、stderr 提示。
    let output = Command::new(evals_bin())
        .args(["--mode", "daemon", "--fixtures", "v0.5", "--limit", "1"])
        .output()
        .expect("evals binary 应当能跑起来");
    assert!(
        !output.status.success(),
        "daemon mode 缺参数应当退非零，实得 status={:?} stdout={:?} stderr={:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--daemon-binary")
            || stderr.contains("daemon-binary")
            || stderr.contains("--root")
            || stderr.contains("--model-path"),
        "stderr 应当提示缺哪个必填参数，实得：{stderr}"
    );
}

#[test]
fn daemon_mode_help_lists_new_flags() {
    // CLI --help 应当列出 BETA-32 T12 新加的 flag（防 clap derive 漏改）。
    let output = Command::new(evals_bin())
        .arg("--help")
        .output()
        .expect("evals --help 应当能跑");
    assert!(output.status.success(), "evals --help 应当退 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for needle in [
        "--mode",
        "--daemon-binary",
        "--root",
        "--topk",
        "--health-timeout-secs",
    ] {
        assert!(
            stdout.contains(needle),
            "--help 应当含 {needle} flag，实得：{stdout}"
        );
    }
}

/// 仅 `SCOUT_DAEMON_BIN` + `SCOUT_MODEL_PATH` 都提供时跑：spawn daemon →
/// 索引一个空目录 → 跑 1 条 case → 关停。
///
/// 关注的是 wiring 接通（CLI 解析 → tokio runtime → daemon spawn → MCP search →
/// shutdown），不是召回质量。
#[test]
fn daemon_mode_end_to_end_when_env_provided() {
    let Ok(daemon_bin) = std::env::var("SCOUT_DAEMON_BIN") else {
        eprintln!("[skip] 未设 SCOUT_DAEMON_BIN，跳过 daemon end-to-end smoke");
        return;
    };
    let Ok(model_path) = std::env::var("SCOUT_MODEL_PATH") else {
        eprintln!("[skip] 未设 SCOUT_MODEL_PATH，跳过 daemon end-to-end smoke");
        return;
    };

    let root = tempfile::tempdir().expect("tempdir root 应当能建");
    std::fs::write(
        root.path().join("hello.txt"),
        "competitive analysis content",
    )
    .expect("写 hello.txt 应当成功");

    let output = Command::new(evals_bin())
        .args([
            "--mode",
            "daemon",
            "--fixtures",
            "v0.5",
            "--limit",
            "1",
            "--daemon-binary",
            &daemon_bin,
            "--root",
            root.path().to_str().unwrap(),
            "--model-path",
            &model_path,
            "--topk",
            "5",
            "--health-timeout-secs",
            "120",
        ])
        .output()
        .expect("evals daemon mode 应当能跑");
    assert!(
        output.status.success(),
        "evals daemon mode 应当退 0，status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    // stdout 应当是 JSON 数组（至少含 1 个 case）。
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("daemon mode stdout 应当是合法 JSON");
    let arr = parsed.as_array().expect("stdout 应当是 JSON 数组");
    assert_eq!(arr.len(), 1, "limit=1 → 1 条 case，实得 {arr:?}");
    assert!(arr[0]["id"].is_string());
    assert!(arr[0]["paths"].is_array());
}
