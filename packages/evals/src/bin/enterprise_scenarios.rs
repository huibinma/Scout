//! BETA-40 收尾：企业三场景 daemon 端到端评测 CLI。
//!
//! 读 `test-materials/enterprise-scenarios-raw/expected/queries.tsv`，生成
//! collection 模式 daemon config（7 集合 + 5 subject token 信息墙），拉起真
//! `scoutd`（真实 GGUF embedder），逐 case 用对应 subject 的 token 走 MCP
//! `search`，按 top-K 命中 / 越权双断言评分，输出三场景 Markdown 报告
//! （`--json` 另存机读全量）。
//!
//! 依赖真实模型 + 编译了 llama-cpp 系列 feature 的 daemon binary，因此不进
//! 常跑 CI；可重复回归入口见 `tests/enterprise_scenarios_gate.rs`（fixture
//! 完整性常跑 + 环境变量门控的端到端）。
//!
//! 用法示例（Windows）：
//!
//! ```text
//! cargo run -p scout-evals --bin enterprise_scenarios -- \
//!     --daemon-binary target/release/scoutd \
//!     --model-path C:\models\embeddinggemma.gguf \
//!     --json report.json
//! ```
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::expect_used)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use reqwest::Client;
use serde_json::{json, Value};

use scout_evals::enterprise::{
    aggregate, grants_for_subject, parse_queries_tsv, render_config_toml, render_report_markdown,
    score_denied, score_hits, token_for_subject, CaseOutcome, EnterpriseCase, EnterpriseReport,
    Expectation, COLLECTIONS,
};
use scout_evals::mcp_client::{mcp_call_tool, mcp_initialize, McpSession};
use scout_evals::runner_daemon::DaemonRunner;

#[derive(Parser)]
#[command(
    name = "enterprise_scenarios",
    about = "BETA-40 企业三场景 daemon 端到端评测（真实模型 + 信息墙）"
)]
struct Cli {
    /// scoutd 可执行文件路径（需带 llama-cpp 系列 feature 编译）。
    #[arg(long)]
    daemon_binary: PathBuf,

    /// embedder GGUF 路径（或环境变量 `SCOUT_MODEL_PATH`）。
    #[arg(long, env = "SCOUT_MODEL_PATH")]
    model_path: PathBuf,

    /// 企业材料根目录（缺省 = 仓库 test-materials/enterprise-scenarios-raw）。
    #[arg(long)]
    materials_root: Option<PathBuf>,

    /// queries.tsv 路径（缺省 = <materials-root>/expected/queries.tsv）。
    #[arg(long)]
    queries: Option<PathBuf>,

    /// top-K 截断（期望路径需全部落在前 K 位）。
    #[arg(long, default_value_t = 10)]
    topk: usize,

    /// 等 daemon /health 的上限秒数（首次索引 + embed pass 用真模型、给宽松值）。
    #[arg(long, default_value_t = 600)]
    health_timeout_secs: u64,

    /// 透传 daemon --semantic-weight（缺省用 daemon 内置默认；用于权重 A/B）。
    #[arg(long)]
    semantic_weight: Option<f64>,

    /// 机读全量报告输出路径（JSON）。
    #[arg(long)]
    json: Option<PathBuf>,

    /// 通过闸门：OVERALL passed 低于该值时退非零（回归守护用）。
    #[arg(long)]
    min_overall_pass: Option<usize>,

    /// 严格闸门：任一 case 失败即退非零（2026-07-04 baseline 21/21 全过后启用；
    /// 与 --min-overall-pass 可叠加）。
    #[arg(long)]
    require_all: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("[enterprise_scenarios] 失败：{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<ExitCode> {
    let cli = Cli::parse();

    let materials_root = match &cli.materials_root {
        Some(p) => p.clone(),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-materials/enterprise-scenarios-raw"),
    };
    let materials_root = std::path::absolute(&materials_root)
        .with_context(|| format!("materials root 绝对化失败：{}", materials_root.display()))?;

    let queries_path = cli
        .queries
        .clone()
        .unwrap_or_else(|| materials_root.join("expected").join("queries.tsv"));
    let tsv = std::fs::read_to_string(&queries_path)
        .with_context(|| format!("读取 queries.tsv 失败：{}", queries_path.display()))?;
    let cases = parse_queries_tsv(&tsv)?;
    eprintln!(
        "[enterprise_scenarios] 载入 {} 条 case（{}）",
        cases.len(),
        queries_path.display()
    );

    // 生成 daemon config（绝对 roots + 合规 token）到 tempdir。
    let config_dir = tempfile::tempdir().context("创建 config tempdir 失败")?;
    let config_path = config_dir.path().join("scoutd-enterprise-eval.toml");
    std::fs::write(&config_path, render_config_toml(&materials_root)?)
        .context("写 daemon config 失败")?;

    eprintln!(
        "[enterprise_scenarios] 拉起 daemon（首次索引 + embed pass，最长等 {}s）…",
        cli.health_timeout_secs
    );
    let runner = DaemonRunner::spawn_with_config(
        &cli.daemon_binary,
        &config_path,
        &cli.model_path,
        Duration::from_secs(cli.health_timeout_secs),
        cli.semantic_weight,
    )
    .await?;

    let outcomes = run_all_cases(&runner, &cases, cli.topk).await;
    let shutdown = runner.shutdown().await;
    let outcomes = outcomes?;
    shutdown?;

    let report = EnterpriseReport {
        topk: cli.topk,
        semantic_weight: cli.semantic_weight,
        model_file: cli.model_path.file_name().map_or_else(
            || "<unknown>".to_owned(),
            |n| n.to_string_lossy().into_owned(),
        ),
        aggregates: aggregate(&outcomes),
        outcomes,
    };

    println!("{}", render_report_markdown(&report));

    if let Some(json_path) = &cli.json {
        std::fs::write(json_path, serde_json::to_string_pretty(&report)?)
            .with_context(|| format!("写 JSON 报告失败：{}", json_path.display()))?;
        eprintln!(
            "[enterprise_scenarios] JSON 报告已写：{}",
            json_path.display()
        );
    }

    let (overall_passed, overall_total) = report
        .aggregates
        .last()
        .map_or((0, 0), |a| (a.passed, a.total));
    if cli.require_all && overall_passed < overall_total {
        eprintln!(
            "[enterprise_scenarios] 严格闸门未过：{overall_passed}/{overall_total}（--require-all）"
        );
        return Ok(ExitCode::FAILURE);
    }
    if let Some(min) = cli.min_overall_pass {
        if overall_passed < min {
            eprintln!(
                "[enterprise_scenarios] 闸门未过：OVERALL passed {overall_passed} < 要求 {min}"
            );
            return Ok(ExitCode::FAILURE);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// 逐 case 评测：正样本查 top-K 命中；负样本查缺省检索零泄漏 + 显式越权全拒。
async fn run_all_cases(
    runner: &DaemonRunner,
    cases: &[EnterpriseCase],
    topk: usize,
) -> Result<Vec<CaseOutcome>> {
    let client = Client::new();
    let mut outcomes = Vec::with_capacity(cases.len());
    for case in cases {
        let session = mcp_initialize(&client, runner.addr, &token_for_subject(&case.subject))
            .await
            .context("MCP session 构造失败")?;
        let (paths, collections, degraded) =
            search_default(&client, &session, &case.query, topk, &case.id).await?;
        let outcome = match &case.expectation {
            Expectation::Hits(_) => score_hits(case, &paths, degraded),
            Expectation::AccessDenied { .. } => {
                let failures = probe_denied(&client, &session, case).await?;
                score_denied(case, &collections, paths.len(), &failures, degraded)
            }
        };
        eprintln!(
            "[case {}] {} — {}",
            outcome.id,
            if outcome.pass { "PASS" } else { "FAIL" },
            outcome.detail
        );
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// 缺省（不指名 collections）search，返回 (paths, 命中 collection ids, degraded)。
async fn search_default(
    client: &Client,
    session: &McpSession,
    query: &str,
    topk: usize,
    case_id: &str,
) -> Result<(Vec<String>, Vec<String>, bool)> {
    let resp = mcp_call_tool(
        client,
        session,
        "search",
        json!({"query": query, "limit": topk}),
    )
    .await
    .with_context(|| format!("MCP search 失败 case_id={case_id}"))?;
    if resp["isError"].as_bool().unwrap_or(false) {
        let msg = resp["content"][0]["text"].as_str().unwrap_or("<no text>");
        return Err(anyhow!("MCP search 返回 isError case_id={case_id}：{msg}"));
    }
    let payload_str = resp["content"][0]["text"]
        .as_str()
        .ok_or_else(|| anyhow!("search result.content[0].text 不是字符串 case_id={case_id}"))?;
    let payload: Value = serde_json::from_str(payload_str)
        .with_context(|| format!("search result JSON 解析失败 case_id={case_id}"))?;
    let results = payload["results"].as_array().cloned().unwrap_or_default();
    let paths = results
        .iter()
        .filter_map(|h| h["path"].as_str().map(String::from))
        .collect();
    let collections = results
        .iter()
        .filter_map(|h| h["collection"].as_str().map(String::from))
        .collect();
    let degraded = payload["degraded"].as_bool().unwrap_or(false);
    Ok((paths, collections, degraded))
}

/// 越权探针：对该 subject **未授权**的每个 collection 显式指名 search，
/// 返回「没有被拒」的 collection id 列表（应恒空）。
async fn probe_denied(
    client: &Client,
    session: &McpSession,
    case: &EnterpriseCase,
) -> Result<Vec<String>> {
    let granted = grants_for_subject(&case.subject);
    let mut failures = Vec::new();
    for (id, ..) in COLLECTIONS {
        if granted.contains(&id) {
            continue;
        }
        let resp = mcp_call_tool(
            client,
            session,
            "search",
            json!({"query": case.query, "limit": 5, "collections": [id]}),
        )
        .await
        .with_context(|| format!("越权探针调用失败 case_id={} collection={id}", case.id))?;
        if !resp["isError"].as_bool().unwrap_or(false) {
            failures.push(id.to_owned());
        }
    }
    Ok(failures)
}
