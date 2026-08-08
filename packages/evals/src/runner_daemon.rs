//! BETA-32 T12：daemon-mode evals runner。
//!
//! 拉起 `scoutd` 子进程 + 走 MCP client 跑评测 case + top-K 集合等价比对。
//!
//! 设计要点：
//!
//! - **端口**：用 `tokio::net::TcpListener::bind("127.0.0.1:0")` 取一个 OS 分配
//!   的可用端口，立刻 drop listener、然后把端口透给 daemon 的 `--bind` 参数。
//!   race window 极窄（drop 与 daemon bind 之间），实践 OK；CI 并发友好。
//!   workspace 没有 portpicker crate、这是最轻量方式。
//! - **token**：用固定明文常量（≥ 32 char，与 `preflight::check_token` 下限对
//!   齐），daemon binary 用 `--token` flag 直接收，不走环境变量（避免 evals
//!   多进程跑互相覆盖）。
//! - **`TempDir`**：data-dir 用 `tempfile::TempDir`，handle drop 时清；root 由
//!   调用方提供（评测用的是用户给的语料根，不在本模块创建）。
//! - **关停**：`kill_on_drop(true)` + 显式 `child.kill().await` 双保险。daemon
//!   收到 SIGKILL 立刻退，不走 graceful shutdown —— 评测场景没必要等。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};

use crate::mcp_client::{mcp_call_tool, mcp_initialize, wait_for_health, McpSession};
use crate::Case;

/// 给 daemon 用的固定 bearer token（≥ 32 字符、与 `preflight::check_token` 下限
/// 对齐）。本进程内常量，evals 单跑场景没有泄漏风险。
const EVALS_TOKEN: &str = "evals-token-32-chars-minimum-length";

/// 单条 case 在 daemon mode 下的输出：top-K 命中文件 path 列表。
#[derive(Debug, Clone)]
pub struct DaemonCaseResult {
    /// case ID（与 `Case::id` 一致）。
    pub id: String,
    /// 原始 query（便于失败时打印对照）。
    pub query: String,
    /// daemon `search` 返回的 top-K paths，按 daemon 排序顺序。
    pub paths: Vec<String>,
}

/// daemon 子进程句柄 + 监听地址 + token + tempdir。
#[derive(Debug)]
pub struct DaemonRunner {
    /// 子进程句柄；`kill_on_drop=true` 保证 panic 路径也能回收。
    pub child: Child,
    /// daemon `--bind` 用的真实地址（OS 分配端口后塞回的）。
    pub addr: SocketAddr,
    /// bearer token（明文）。
    pub token: String,
    /// `--data-dir` 的 [`TempDir`] 句柄，drop 时清；外部不需访问，仅靠所有
    /// 权挂在 `DaemonRunner` 上让 drop 清目录。
    _data: TempDir,
}

impl DaemonRunner {
    /// spawn daemon 子进程，等 `/health` 200。
    ///
    /// `binary`：`scoutd` 可执行文件路径（一般是 `target/release/scoutd`
    /// 或同款 debug）。
    /// `root`：索引根（评测语料）。
    /// `model_path`：embedder GGUF。
    /// `health_timeout`：等 `/health` 的上限；首次全量索引耗时不可预测、给宽
    ///   松默认（60s）。
    pub async fn spawn(
        binary: &Path,
        root: &Path,
        model_path: &Path,
        health_timeout: Duration,
    ) -> Result<Self> {
        if !binary.exists() {
            return Err(anyhow!("daemon 可执行文件不存在：{}", binary.display()));
        }
        if !root.exists() {
            return Err(anyhow!("索引根不存在：{}", root.display()));
        }
        if !model_path.exists() {
            return Err(anyhow!("模型文件不存在：{}", model_path.display()));
        }

        let data = tempfile::tempdir().context("创建 data-dir tempdir 失败")?;

        // OS 分配空闲端口：bind 127.0.0.1:0 → 拿到 addr → 立刻 drop listener →
        // 把 port 透给 daemon。race window 在 drop 与 daemon bind 之间，实践 OK。
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind 127.0.0.1:0 取空闲端口失败")?;
        let addr = listener
            .local_addr()
            .context("已 bind 的 listener 必有 addr")?;
        drop(listener);

        let token = EVALS_TOKEN.to_owned();

        let child = Command::new(binary)
            .args([
                "--root",
                root.to_str().ok_or_else(|| anyhow!("root 路径非 UTF-8"))?,
                "--bind",
                &addr.to_string(),
                "--data-dir",
                data.path()
                    .to_str()
                    .ok_or_else(|| anyhow!("data-dir 路径非 UTF-8"))?,
                "--model-path",
                model_path
                    .to_str()
                    .ok_or_else(|| anyhow!("model-path 非 UTF-8"))?,
                "--token",
                &token,
                "--log-level",
                "warn",
            ])
            // reviewer Important #3：piped + 无 reader 会让 daemon 在 ~64KB pipe
            // buffer 满时阻塞写 stdout/stderr。评测路径下没人读，最稳是直接 null。
            // trade-off：daemon stderr 全丢、spawn 失败 debug 时不方便；若未来
            // 真需要捕获、follow-up 走 tokio::io::copy 到 buffer 的 option (b)。
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("spawn scoutd 子进程失败")?;

        let client = Client::new();
        wait_for_health(&client, addr, health_timeout)
            .await
            .context("daemon 起来等 /health 超时")?;

        Ok(Self {
            child,
            addr,
            token,
            _data: data,
        })
    }

    /// collection 模式 spawn（BETA-40 企业评测）：`--config <TOML>` 替代
    /// `--root`/`--token`；token 在 TOML `[[tokens]]` 里逐 subject 声明，
    /// 因此返回句柄的 `token` 字段为空串——调用方按 subject 自备 token。
    ///
    /// `semantic_weight`：`Some(w)` 时透传 `--semantic-weight`（A/B 融合权重）。
    /// stderr 走 inherit（daemon `--log-level warn`），首次索引 / embed pass
    /// 的告警对评测诊断有价值；stdout 保持 null。
    ///
    /// # Errors
    ///
    /// 可执行文件 / config / 模型缺失、spawn 失败、`/health` 超时。
    pub async fn spawn_with_config(
        binary: &Path,
        config_path: &Path,
        model_path: &Path,
        health_timeout: Duration,
        semantic_weight: Option<f64>,
    ) -> Result<Self> {
        if !binary.exists() {
            return Err(anyhow!("daemon 可执行文件不存在：{}", binary.display()));
        }
        if !config_path.exists() {
            return Err(anyhow!("daemon config 不存在：{}", config_path.display()));
        }
        if !model_path.exists() {
            return Err(anyhow!("模型文件不存在：{}", model_path.display()));
        }

        let data = tempfile::tempdir().context("创建 data-dir tempdir 失败")?;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind 127.0.0.1:0 取空闲端口失败")?;
        let addr = listener
            .local_addr()
            .context("已 bind 的 listener 必有 addr")?;
        drop(listener);

        let mut cmd = Command::new(binary);
        cmd.args([
            "--config",
            config_path
                .to_str()
                .ok_or_else(|| anyhow!("config 路径非 UTF-8"))?,
            "--bind",
            &addr.to_string(),
            "--data-dir",
            data.path()
                .to_str()
                .ok_or_else(|| anyhow!("data-dir 路径非 UTF-8"))?,
            "--model-path",
            model_path
                .to_str()
                .ok_or_else(|| anyhow!("model-path 非 UTF-8"))?,
            "--log-level",
            "warn",
        ]);
        if let Some(w) = semantic_weight {
            cmd.args(["--semantic-weight", &w.to_string()]);
        }
        let child = cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("spawn scoutd 子进程失败（collection 模式）")?;

        let client = Client::new();
        wait_for_health(&client, addr, health_timeout)
            .await
            .context("daemon 起来等 /health 超时")?;

        Ok(Self {
            child,
            addr,
            token: String::new(),
            _data: data,
        })
    }

    /// 显式关停子进程（SIGKILL + wait）。Drop 路径不必调，但成功路径调一下让
    /// stderr 体面收尾、避免端口 `TIME_WAIT`。
    pub async fn shutdown(mut self) -> Result<()> {
        self.child.kill().await.context("kill 子进程失败")?;
        self.child.wait().await.context("wait 子进程退出失败")?;
        Ok(())
    }
}

/// 跑一组 case：对每条 query 调 MCP `search` tool，收集 top-K paths。
pub async fn run_cases(
    runner: &DaemonRunner,
    cases: &[Case],
    limit: usize,
) -> Result<Vec<DaemonCaseResult>> {
    let client = Client::new();
    let session: McpSession = mcp_initialize(&client, runner.addr, &runner.token)
        .await
        .context("MCP initialize 失败")?;

    let mut results = Vec::with_capacity(cases.len());
    for case in cases {
        let resp = mcp_call_tool(
            &client,
            &session,
            "search",
            json!({"query": case.query, "limit": limit}),
        )
        .await
        .with_context(|| format!("MCP search 失败 case_id={}", case.id))?;

        // reviewer Important #1：tool-level error 走 `CallToolResult::error(...)` →
        // 协议层 result 含 `isError: true`，content[0].text 是 plain 错误文本（不是
        // JSON）。`mcp.rs::ToolError::InvalidParams` / `Internal` 两条都走这条
        // 路径。先看 isError、否则下面 from_str(plain text) 会以"JSON 解析失败"
        // 掩盖真正错误。
        if resp["isError"].as_bool().unwrap_or(false) {
            let msg = resp["content"][0]["text"].as_str().unwrap_or("<no text>");
            return Err(anyhow!(
                "MCP search tool 返回 isError case_id={}：{msg}",
                case.id
            ));
        }

        // tools/call result 的 content[0].text 是 SearchOutput 的 JSON 字符串。
        let payload_str = resp["content"][0]["text"]
            .as_str()
            .ok_or_else(|| anyhow!("search result.content[0].text 不是字符串"))?;
        let payload: Value = serde_json::from_str(payload_str)
            .with_context(|| format!("search result JSON 解析失败 case_id={}", case.id))?;

        let paths: Vec<String> = payload["results"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|h| h["path"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        results.push(DaemonCaseResult {
            id: case.id.clone(),
            query: case.query.clone(),
            paths,
        });
    }
    Ok(results)
}

/// top-K 集合等价比对器的输出。
#[derive(Debug, Clone)]
pub struct TopKEquivalenceReport {
    /// 比对的 case 数（两侧按 id 对齐后）。
    pub total: usize,
    /// 集合（不计顺序）等价的 case 数。
    pub set_equal: usize,
    /// 集合不等的 case 列表（保留细节用于调试 / 打印）。
    pub mismatches: Vec<TopKMismatch>,
}

/// 集合不等的单条 case 详情。
#[derive(Debug, Clone)]
pub struct TopKMismatch {
    pub id: String,
    pub query: String,
    /// 只在 desktop 出现的 path（按字典序）。
    pub only_in_desktop: Vec<String>,
    /// 只在 daemon 出现的 path（按字典序）。
    pub only_in_daemon: Vec<String>,
}

impl TopKEquivalenceReport {
    /// 是否通过等价闸门：`mismatches.len() <= tolerance`。
    #[must_use]
    pub fn passes(&self, tolerance: usize) -> bool {
        self.mismatches.len() <= tolerance
    }
}

/// 比较 desktop 与 daemon 的 top-K 输出，按 path 集合等价（不计顺序）。
///
/// 两侧按 case id 对齐：只有同 id 的两条会比较；任一侧缺 id 的 case 不计入
/// `total` 也不报 mismatch（调用方应当先确认 case 集对齐）。
#[must_use]
pub fn topk_set_equivalent(
    desktop: &[DaemonCaseResult],
    daemon: &[DaemonCaseResult],
) -> TopKEquivalenceReport {
    use std::collections::BTreeMap;
    let desktop_by_id: BTreeMap<&str, &DaemonCaseResult> =
        desktop.iter().map(|r| (r.id.as_str(), r)).collect();
    let daemon_by_id: BTreeMap<&str, &DaemonCaseResult> =
        daemon.iter().map(|r| (r.id.as_str(), r)).collect();

    let mut total = 0usize;
    let mut set_equal = 0usize;
    let mut mismatches = Vec::new();

    for (id, d_res) in &desktop_by_id {
        let Some(daemon_res) = daemon_by_id.get(id) else {
            continue;
        };
        total += 1;
        let d_set: std::collections::BTreeSet<&str> =
            d_res.paths.iter().map(String::as_str).collect();
        let m_set: std::collections::BTreeSet<&str> =
            daemon_res.paths.iter().map(String::as_str).collect();
        if d_set == m_set {
            set_equal += 1;
            continue;
        }
        let only_in_desktop: Vec<String> =
            d_set.difference(&m_set).map(|s| (*s).to_owned()).collect();
        let only_in_daemon: Vec<String> =
            m_set.difference(&d_set).map(|s| (*s).to_owned()).collect();
        mismatches.push(TopKMismatch {
            id: (*id).to_owned(),
            query: d_res.query.clone(),
            only_in_desktop,
            only_in_daemon,
        });
    }

    TopKEquivalenceReport {
        total,
        set_equal,
        mismatches,
    }
}

/// daemon-mode 评测入口的参数集合。供 evals 主 binary 与外部测试共用。
#[derive(Debug)]
pub struct DaemonModeArgs {
    pub daemon_binary: PathBuf,
    pub root: PathBuf,
    pub model_path: PathBuf,
    pub limit: usize,
    /// 等 `/health` 的上限；默认建议 60s（首次全量索引耗时不可预测）。
    pub health_timeout: Duration,
}

/// 一键完成 daemon-mode 评测：spawn → 跑 case → shutdown。
pub async fn run_daemon_mode(
    args: &DaemonModeArgs,
    cases: &[Case],
) -> Result<Vec<DaemonCaseResult>> {
    let runner = DaemonRunner::spawn(
        &args.daemon_binary,
        &args.root,
        &args.model_path,
        args.health_timeout,
    )
    .await?;
    let results = run_cases(&runner, cases, args.limit).await;
    // 即使 run_cases 错也要尝试关停子进程、避免端口悬挂。
    let shutdown = runner.shutdown().await;
    let results = results?;
    shutdown?;
    Ok(results)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn case(id: &str) -> DaemonCaseResult {
        DaemonCaseResult {
            id: id.to_owned(),
            query: format!("q-{id}"),
            paths: Vec::new(),
        }
    }

    fn case_with(id: &str, paths: &[&str]) -> DaemonCaseResult {
        DaemonCaseResult {
            id: id.to_owned(),
            query: format!("q-{id}"),
            paths: paths.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn topk_equivalent_when_same_set_different_order() {
        // path 顺序不同但集合一致 → 等价。
        let desktop = vec![case_with("c1", &["/a", "/b", "/c"])];
        let daemon = vec![case_with("c1", &["/c", "/a", "/b"])];
        let report = topk_set_equivalent(&desktop, &daemon);
        assert_eq!(report.total, 1);
        assert_eq!(report.set_equal, 1);
        assert!(report.mismatches.is_empty());
        assert!(report.passes(0));
    }

    #[test]
    fn topk_mismatch_records_both_sides() {
        let desktop = vec![case_with("c1", &["/a", "/b"])];
        let daemon = vec![case_with("c1", &["/a", "/c"])];
        let report = topk_set_equivalent(&desktop, &daemon);
        assert_eq!(report.set_equal, 0);
        assert_eq!(report.mismatches.len(), 1);
        let m = &report.mismatches[0];
        assert_eq!(m.id, "c1");
        assert_eq!(m.only_in_desktop, vec!["/b".to_owned()]);
        assert_eq!(m.only_in_daemon, vec!["/c".to_owned()]);
        assert!(!report.passes(0));
        assert!(report.passes(1));
    }

    #[test]
    fn topk_skips_unaligned_ids() {
        // daemon 缺 c2、desktop 缺 c3 —— 都不计入比对。
        let desktop = vec![case_with("c1", &["/a"]), case_with("c2", &["/b"])];
        let daemon = vec![case_with("c1", &["/a"]), case_with("c3", &["/d"])];
        let report = topk_set_equivalent(&desktop, &daemon);
        assert_eq!(report.total, 1, "只对齐 c1");
        assert_eq!(report.set_equal, 1);
    }

    #[test]
    fn topk_empty_paths_both_sides_equivalent() {
        let desktop = vec![case("c1")];
        let daemon = vec![case("c1")];
        let report = topk_set_equivalent(&desktop, &daemon);
        assert_eq!(report.set_equal, 1);
    }
}
