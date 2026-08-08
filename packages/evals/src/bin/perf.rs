#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_precision_loss,
    clippy::format_push_string,
    clippy::too_many_lines
)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use scout_evals::{load_cases, Case};
use scout_intent_parser::fallback::resolve_intent;
use scout_search_backend::{LocationResolveError, LocationResolver, SearchIntent};
use scout_search_backend_spotlight::translate_intent;
use serde::Serialize;

const DEFAULT_RUNS: usize = 20;
const DEFAULT_WARMUP: usize = 3;
const PERF_FIXTURE_DIR: &str = "/tmp/scout-evals-perf-fixtures";
const DOC_REPORT: &str = "target/mvp-27-perf.md";

#[derive(Debug, Parser)]
#[command(name = "perf")]
#[command(about = "Scout MVP-27 性能基准", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// 每条 fixture 重复次数。
    #[arg(long, default_value_t = DEFAULT_RUNS)]
    runs: usize,

    /// 每条 fixture 剔除的冷启动次数。
    #[arg(long, default_value_t = DEFAULT_WARMUP)]
    warmup: usize,

    /// CLI 完整搜索的 onlyin 目录。
    #[arg(long, default_value = PERF_FIXTURE_DIR)]
    onlyin: PathBuf,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// query -> SearchIntent，不打 backend。
    Parser,
    /// query -> `SearchIntent` -> Spotlight predicate，不执行 mdfind。
    Translate,
    /// CLI binary 端到端，分别测 intent-only 与完整 mdfind。
    Cli {
        /// CLI 测试模式。
        #[arg(long, value_enum, default_value_t = CliMode::Both)]
        mode: CliMode,
    },
    /// 依次运行 parser / translate / cli，并写 `target/mvp-27-perf.md`。
    All,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliMode {
    IntentOnly,
    Search,
    Both,
}

#[derive(Debug, Serialize)]
struct PerfReport {
    date: String,
    environment: Environment,
    spotlight_status: String,
    runs_per_case: usize,
    warmup_per_case: usize,
    stages: Vec<StageReport>,
}

#[derive(Debug, Serialize)]
struct Environment {
    os: String,
    model: String,
    ram: String,
    rustc: String,
    fixture_dir: String,
}

#[derive(Debug, Serialize)]
struct StageReport {
    stage: String,
    sample_count: usize,
    failure_count: usize,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    bucket_by_variant: Vec<BucketReport>,
    bucket_by_language: Vec<BucketReport>,
    bucket_by_complexity: Vec<BucketReport>,
    outliers: Vec<Outlier>,
}

#[derive(Debug, Serialize)]
struct BucketReport {
    bucket: String,
    sample_count: usize,
    p95_ms: f64,
}

#[derive(Debug, Serialize)]
struct Outlier {
    case_id: String,
    query: String,
    variant: String,
    language: String,
    max_ms: f64,
}

#[derive(Debug, Clone)]
struct TimedSample {
    case: Case,
    duration: Duration,
}

#[derive(Debug, Clone)]
struct BenchCase {
    case: Case,
    expected_intent: SearchIntent,
}

#[derive(Debug)]
struct BenchSet {
    parser_cases: Vec<BenchCase>,
    search_cases: Vec<BenchCase>,
}

#[derive(Debug)]
struct MockResolver {
    root: PathBuf,
}

impl LocationResolver for MockResolver {
    fn resolve_hint(&self, hint: &str) -> std::result::Result<Vec<PathBuf>, LocationResolveError> {
        let path = match hint {
            "下载" | "downloads" | "Downloads" => self.root.join("Downloads"),
            "桌面" | "desktop" | "Desktop" => self.root.join("Desktop"),
            "文档" | "documents" | "Documents" => self.root.join("Documents"),
            "音乐" | "music" | "Music" => self.root.join("Music"),
            "图片" | "pictures" | "Pictures" | "截屏" | "screenshots" => {
                self.root.join("Pictures")
            }
            "视频" | "movies" | "Movies" => self.root.join("Movies"),
            other => {
                return Err(LocationResolveError::UnsupportedHint {
                    hint: other.to_owned(),
                });
            }
        };
        Ok(vec![path])
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    validate_runs(cli.runs, cli.warmup)?;
    let bench_set = load_bench_set()?;
    let env = collect_environment(&cli.onlyin);
    let spotlight_status = spotlight_status(&cli.onlyin);

    match cli.command {
        Commands::Parser => {
            let stage = bench_parser(&bench_set.parser_cases, cli.runs, cli.warmup);
            write_stage_outputs(
                "parser",
                &env,
                &spotlight_status,
                cli.runs,
                cli.warmup,
                vec![stage],
            )?;
        }
        Commands::Translate => {
            let stage = bench_translate(&bench_set.search_cases, cli.runs, cli.warmup, &cli.onlyin);
            write_stage_outputs(
                "translate",
                &env,
                &spotlight_status,
                cli.runs,
                cli.warmup,
                vec![stage],
            )?;
        }
        Commands::Cli { mode } => {
            ensure_perf_fixtures(&cli.onlyin)?;
            let stages = bench_cli(
                &bench_set.search_cases,
                cli.runs,
                cli.warmup,
                &cli.onlyin,
                mode,
            )?;
            write_stage_outputs("cli", &env, &spotlight_status, cli.runs, cli.warmup, stages)?;
        }
        Commands::All => {
            ensure_perf_fixtures(&cli.onlyin)?;
            let mut stages = Vec::new();
            stages.push(bench_parser(&bench_set.parser_cases, cli.runs, cli.warmup));
            stages.push(bench_translate(
                &bench_set.search_cases,
                cli.runs,
                cli.warmup,
                &cli.onlyin,
            ));
            stages.extend(bench_cli(
                &bench_set.search_cases,
                cli.runs,
                cli.warmup,
                &cli.onlyin,
                CliMode::Both,
            )?);
            write_stage_outputs("all", &env, &spotlight_status, cli.runs, cli.warmup, stages)?;
        }
    }

    Ok(())
}

fn validate_runs(runs: usize, warmup: usize) -> Result<()> {
    anyhow::ensure!(runs > warmup, "runs 必须大于 warmup");
    anyhow::ensure!(runs > 0, "runs 必须大于 0");
    Ok(())
}

fn load_bench_set() -> Result<BenchSet> {
    let mut parser_cases = Vec::new();
    for case in load_cases("v0.1")? {
        parser_cases.push(bench_case(case)?);
    }

    for case in load_cases("v0.5")?
        .into_iter()
        .filter(is_proto05a_subset)
        .take(60)
    {
        parser_cases.push(bench_case(case)?);
    }

    let search_cases = parser_cases
        .iter()
        .filter(|case| is_search_intent(&case.expected_intent))
        .cloned()
        .collect();

    Ok(BenchSet {
        parser_cases,
        search_cases,
    })
}

fn bench_case(case: Case) -> Result<BenchCase> {
    let expected_intent = serde_json::from_value(case.expected_intent.clone())
        .with_context(|| format!("case {} expected_intent 反序列化失败", case.id))?;
    Ok(BenchCase {
        case,
        expected_intent,
    })
}

fn is_proto05a_subset(case: &Case) -> bool {
    matches!(case.variant.as_str(), "FileSearch" | "MediaSearch")
}

fn is_search_intent(intent: &SearchIntent) -> bool {
    matches!(
        intent,
        SearchIntent::FileSearch(_) | SearchIntent::MediaSearch(_)
    )
}

fn bench_parser(cases: &[BenchCase], runs: usize, warmup: usize) -> StageReport {
    let mut samples = Vec::new();
    for case in cases {
        for run_index in 0..runs {
            let start = Instant::now();
            let _intent = resolve_intent(&case.case.query, None);
            let elapsed = start.elapsed();
            if run_index >= warmup {
                samples.push(TimedSample {
                    case: case.case.clone(),
                    duration: elapsed,
                });
            }
        }
    }
    summarize_stage("parser-only", &samples, 0)
}

fn bench_translate(
    cases: &[BenchCase],
    runs: usize,
    warmup: usize,
    fixture_dir: &Path,
) -> StageReport {
    let resolver = MockResolver {
        root: fixture_dir.to_path_buf(),
    };
    let mut samples = Vec::new();
    for case in cases {
        for run_index in 0..runs {
            let start = Instant::now();
            if let Ok(resolved) = resolve_intent(&case.case.query, None) {
                let _query = translate_intent(&resolved.intent, &resolver);
            }
            let elapsed = start.elapsed();
            if run_index >= warmup {
                samples.push(TimedSample {
                    case: case.case.clone(),
                    duration: elapsed,
                });
            }
        }
    }
    summarize_stage("translate", &samples, 0)
}

fn bench_cli(
    cases: &[BenchCase],
    runs: usize,
    warmup: usize,
    fixture_dir: &Path,
    mode: CliMode,
) -> Result<Vec<StageReport>> {
    let binary = ensure_cli_binary()?;
    let mut stages = Vec::new();
    if matches!(mode, CliMode::IntentOnly | CliMode::Both) {
        let (cold, warm) = bench_cli_mode(cases, runs, warmup, fixture_dir, &binary, true)?;
        stages.push(summarize_stage(
            "cli-intent-only-cold",
            &cold.samples,
            cold.failures,
        ));
        stages.push(summarize_stage(
            "cli-intent-only-warm",
            &warm.samples,
            warm.failures,
        ));
    }
    if matches!(mode, CliMode::Search | CliMode::Both) {
        let (cold, warm) = bench_cli_mode(cases, runs, warmup, fixture_dir, &binary, false)?;
        stages.push(summarize_stage(
            "cli-search-cold",
            &cold.samples,
            cold.failures,
        ));
        stages.push(summarize_stage(
            "cli-search-warm",
            &warm.samples,
            warm.failures,
        ));
    }
    Ok(stages)
}

#[derive(Debug)]
struct CliSamples {
    samples: Vec<TimedSample>,
    failures: usize,
}

fn bench_cli_mode(
    cases: &[BenchCase],
    runs: usize,
    warmup: usize,
    fixture_dir: &Path,
    binary: &Path,
    intent_only: bool,
) -> Result<(CliSamples, CliSamples)> {
    let mut cold = CliSamples {
        samples: Vec::new(),
        failures: 0,
    };
    let mut warm = CliSamples {
        samples: Vec::new(),
        failures: 0,
    };
    for case in cases {
        for run_index in 0..runs {
            let start = Instant::now();
            let status = run_cli(binary, fixture_dir, &case.case.query, intent_only)?;
            let elapsed = start.elapsed();
            let failed = !status.success() && !matches!(status.code(), Some(1 | 2));
            let sample = TimedSample {
                case: case.case.clone(),
                duration: elapsed,
            };
            if run_index < warmup {
                if failed {
                    cold.failures += 1;
                }
                cold.samples.push(sample);
            } else {
                if failed {
                    warm.failures += 1;
                }
                warm.samples.push(sample);
            }
        }
    }
    Ok((cold, warm))
}

fn run_cli(
    binary: &Path,
    fixture_dir: &Path,
    query: &str,
    intent_only: bool,
) -> Result<std::process::ExitStatus> {
    let mut command = Command::new(binary);
    if intent_only {
        command.arg("--intent-only");
    } else {
        command.arg("--onlyin").arg(fixture_dir);
    }
    command
        .arg(query)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.status().context("执行 scout-cli 失败")
}

fn ensure_cli_binary() -> Result<PathBuf> {
    let binary = PathBuf::from("target/debug/scout-cli");
    if binary.is_file() {
        return Ok(binary);
    }
    let status = Command::new("cargo")
        .args(["build", "-p", "scout-cli"])
        .status()
        .context("构建 scout-cli 失败")?;
    anyhow::ensure!(status.success(), "cargo build -p scout-cli 未通过");
    Ok(binary)
}

fn summarize_stage(stage: &str, samples: &[TimedSample], failure_count: usize) -> StageReport {
    let durations = samples
        .iter()
        .map(|sample| sample.duration)
        .collect::<Vec<_>>();
    StageReport {
        stage: stage.to_owned(),
        sample_count: samples.len(),
        failure_count,
        p50_ms: percentile_ms(&durations, 50),
        p95_ms: percentile_ms(&durations, 95),
        p99_ms: percentile_ms(&durations, 99),
        bucket_by_variant: bucket_p95(samples, |case| case.variant.clone()),
        bucket_by_language: bucket_p95(samples, |case| case.language.clone()),
        bucket_by_complexity: bucket_p95(samples, complexity_bucket),
        outliers: outliers(samples),
    }
}

fn bucket_p95<F>(samples: &[TimedSample], bucket_of: F) -> Vec<BucketReport>
where
    F: Fn(&Case) -> String,
{
    let mut buckets: BTreeMap<String, Vec<Duration>> = BTreeMap::new();
    for sample in samples {
        buckets
            .entry(bucket_of(&sample.case))
            .or_default()
            .push(sample.duration);
    }
    buckets
        .into_iter()
        .map(|(bucket, durations)| BucketReport {
            bucket,
            sample_count: durations.len(),
            p95_ms: percentile_ms(&durations, 95),
        })
        .collect()
}

fn complexity_bucket(case: &Case) -> String {
    let field_count = case
        .expected_intent
        .as_object()
        .map_or(0, |object| object.len().saturating_sub(3));
    match field_count {
        0..=2 => "simple".to_owned(),
        3..=5 => "medium".to_owned(),
        _ => "complex".to_owned(),
    }
}

fn outliers(samples: &[TimedSample]) -> Vec<Outlier> {
    let mut by_case: BTreeMap<String, (&TimedSample, Duration)> = BTreeMap::new();
    for sample in samples {
        by_case
            .entry(sample.case.id.clone())
            .and_modify(|(_, max)| {
                if sample.duration > *max {
                    *max = sample.duration;
                }
            })
            .or_insert((sample, sample.duration));
    }

    let mut outliers = by_case
        .into_values()
        .map(|(sample, max)| Outlier {
            case_id: sample.case.id.clone(),
            query: sample.case.query.clone(),
            variant: sample.case.variant.clone(),
            language: sample.case.language.clone(),
            max_ms: duration_ms(max),
        })
        .collect::<Vec<_>>();
    outliers.sort_by(|a, b| b.max_ms.total_cmp(&a.max_ms));
    outliers.truncate(10);
    outliers
}

fn percentile_ms(durations: &[Duration], percentile: usize) -> f64 {
    if durations.is_empty() {
        return 0.0;
    }
    let mut values = durations.to_vec();
    values.sort_unstable();
    let rank = (percentile * values.len()).div_ceil(100);
    let index = rank.saturating_sub(1).min(values.len() - 1);
    duration_ms(values[index])
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn write_stage_outputs(
    stage_name: &str,
    env: &Environment,
    spotlight_status: &str,
    runs: usize,
    warmup: usize,
    stages: Vec<StageReport>,
) -> Result<()> {
    let report = PerfReport {
        date: today(),
        environment: Environment {
            os: env.os.clone(),
            model: env.model.clone(),
            ram: env.ram.clone(),
            rustc: env.rustc.clone(),
            fixture_dir: env.fixture_dir.clone(),
        },
        spotlight_status: spotlight_status.to_owned(),
        runs_per_case: runs,
        warmup_per_case: warmup,
        stages,
    };
    let json_path = format!("/tmp/mvp-27-perf-{stage_name}.json");
    let md_path = format!("/tmp/mvp-27-perf-{stage_name}.md");
    fs::write(&json_path, serde_json::to_string_pretty(&report)?)?;
    let markdown = render_markdown(&report);
    fs::write(&md_path, &markdown)?;
    if stage_name == "all" {
        fs::write(DOC_REPORT, markdown)?;
    }
    println!("JSON: {json_path}");
    println!("Markdown: {md_path}");
    if stage_name == "all" {
        println!("Report: {DOC_REPORT}");
    }
    print_console_summary(&report);
    Ok(())
}

fn render_markdown(report: &PerfReport) -> String {
    let mut md = String::new();
    md.push_str("# MVP-27 性能基准报告\n\n");
    md.push_str(&format!(
        "> 日期：{}  \n> 运行机：{}，{}，{}  \n> Spotlight 索引状态：{}\n\n",
        report.date,
        report.environment.os,
        report.environment.model,
        report.environment.ram,
        report.spotlight_status
    ));
    md.push_str("## 1. 三档延迟分布\n\n");
    md.push_str("| 档位 | p50 | p95 | p99 | 样本量 | 非成功退出 | 出场阈值（§6.2） | 结果 |\n");
    md.push_str("|---|---:|---:|---:|---:|---:|---|---|\n");
    for stage in &report.stages {
        let (threshold, result) = threshold_result(stage, &report.spotlight_status);
        md.push_str(&format!(
            "| {} | {:.3} ms | {:.3} ms | {:.3} ms | {} | {} | {} | {} |\n",
            stage.stage,
            stage.p50_ms,
            stage.p95_ms,
            stage.p99_ms,
            stage.sample_count,
            stage.failure_count,
            threshold,
            result
        ));
    }

    md.push_str("\n## 2. 分桶分析\n\n");
    md.push_str("复杂度分桶为 MVP-27 临时口径：按 expected intent 顶层约束字段数量分为 simple / medium / complex。\n\n");
    for stage in &report.stages {
        md.push_str(&format!("### {}\n\n", stage.stage));
        append_bucket_table(&mut md, "intent variant", &stage.bucket_by_variant);
        append_bucket_table(&mut md, "language", &stage.bucket_by_language);
        append_bucket_table(&mut md, "fixture complexity", &stage.bucket_by_complexity);
    }

    md.push_str("## 3. 与 PROTO-09 对比\n\n");
    md.push_str("PROTO-09 出场报告记录 CLI release `--intent-only` 单条查询约 4ms（含 fork + binary 加载）。本次基准默认使用 debug binary，且 CLI 档覆盖多条 fixture 与真实 mdfind 进程调用，预计高一个量级。\n\n");

    md.push_str("## 4. 瓶颈定位\n\n");
    for stage in &report.stages {
        md.push_str(&format!("### {}\n\n", stage.stage));
        md.push_str("| case | max | variant | language | query | 初步归因 |\n");
        md.push_str("|---|---:|---|---|---|---|\n");
        for outlier in &stage.outliers {
            md.push_str(&format!(
                "| {} | {:.3} ms | {} | {} | {} | {} |\n",
                outlier.case_id,
                outlier.max_ms,
                outlier.variant,
                outlier.language,
                escape_md_cell(&outlier.query),
                outlier_reason(stage.stage.as_str())
            ));
        }
        md.push('\n');
    }

    md.push_str("## 5. 出场建议\n\n");
    md.push_str("- parser-only 对照 §6.2「简单查询响应（规则解析路径）p95 < 500ms」。\n");
    md.push_str("- CLI 完整搜索 warm 对照 §6.2「简单查询响应」的交互体感阈值 p95 < 1500ms；cold 样本仅用于观察进程启动与索引预热影响。\n");
    md.push_str("- translate 档暂无 §6.2 硬阈值，作为定位 parser 与 backend process spawn 之间的纯 CPU 基线。\n");
    md.push_str("- 本机 `mdutil` 显示 Spotlight server disabled；CLI 完整搜索存在非成功退出，当前 CLI 搜索数字只能作为本机观测，不作为正式出场通过证据。\n");
    md.push_str("- 若 CLI warm 超阈值，下一步应落到 Spotlight process 启动/索引预热与 CLI release 构建基准复测；若 parser-only 超阈值，才进入 parser 规则路径优化。\n\n");
    md.push_str("## 附录：运行参数\n\n");
    md.push_str(&format!(
        "- runs_per_case：{}\n- warmup_per_case：{}\n- fixture_dir：`{}`\n- rustc：`{}`\n",
        report.runs_per_case,
        report.warmup_per_case,
        report.environment.fixture_dir,
        report.environment.rustc
    ));
    md
}

fn append_bucket_table(md: &mut String, label: &str, buckets: &[BucketReport]) {
    md.push_str(&format!("按 {label}：\n\n"));
    md.push_str("| bucket | p95 | 样本量 |\n");
    md.push_str("|---|---:|---:|\n");
    for bucket in buckets {
        md.push_str(&format!(
            "| {} | {:.3} ms | {} |\n",
            bucket.bucket, bucket.p95_ms, bucket.sample_count
        ));
    }
    md.push('\n');
}

fn threshold_result(stage: &StageReport, spotlight_status: &str) -> (&'static str, &'static str) {
    if stage.stage.starts_with("cli-search")
        && (stage.failure_count > 0 || spotlight_status.contains("disabled"))
    {
        return ("< 1500ms p95（需 Spotlight 完整）", "⚠️");
    }
    match stage.stage.as_str() {
        "parser-only" => (
            "< 500ms p95",
            if stage.p95_ms < 500.0 {
                "✅"
            } else {
                "⚠️"
            },
        ),
        "cli-search-warm" => (
            "< 1500ms p95",
            if stage.p95_ms < 1500.0 {
                "✅"
            } else {
                "⚠️"
            },
        ),
        "cli-intent-only-warm" => (
            "< 500ms p95（参考）",
            if stage.p95_ms < 500.0 {
                "✅"
            } else {
                "⚠️"
            },
        ),
        _ => ("-", "-"),
    }
}

fn outlier_reason(stage: &str) -> &'static str {
    match stage {
        "parser-only" | "translate" => "规则分支 / 谓词字符串构造",
        "cli-intent-only-cold" | "cli-search-cold" => "binary 加载 / process spawn / 冷缓存",
        "cli-search-warm" => "mdfind process spawn / Spotlight 索引命中",
        "cli-intent-only-warm" => "CLI process spawn",
        _ => "待分析",
    }
}

fn print_console_summary(report: &PerfReport) {
    println!("=== MVP-27 perf {} ===", report.date);
    for stage in &report.stages {
        println!(
            "{:<22} p50 {:>8.3} ms  p95 {:>8.3} ms  p99 {:>8.3} ms  n {}",
            stage.stage, stage.p50_ms, stage.p95_ms, stage.p99_ms, stage.sample_count
        );
        if stage.failure_count > 0 {
            println!("  non_success: {}", stage.failure_count);
        }
    }
}

fn collect_environment(fixture_dir: &Path) -> Environment {
    Environment {
        os: command_output("sw_vers", &["-productVersion"]).map_or_else(
            || command_output("uname", &["-sr"]).unwrap_or_else(|| "unknown".to_owned()),
            |version| format!("macOS {version}"),
        ),
        model: command_output("sysctl", &["-n", "hw.model"])
            .unwrap_or_else(|| "unknown".to_owned()),
        ram: command_output("sysctl", &["-n", "hw.memsize"])
            .and_then(|bytes| bytes.parse::<u64>().ok())
            .map_or_else(
                || "unknown RAM".to_owned(),
                |bytes| {
                    let gib = 1_073_741_824;
                    format!("{}GB RAM", (bytes + (gib / 2)) / gib)
                },
            ),
        rustc: command_output("rustc", &["--version"]).unwrap_or_else(|| "unknown".to_owned()),
        fixture_dir: fixture_dir.display().to_string(),
    }
}

fn spotlight_status(path: &Path) -> String {
    command_output("mdutil", &["-s", &path.display().to_string()]).map_or_else(
        || "未知（mdutil 不可用或目录尚未建立）".to_owned(),
        |output| {
            if output.contains("Indexing enabled") {
                "完整或进行中（mdutil: Indexing enabled）".to_owned()
            } else if output.contains("Indexing disabled") {
                "部分（mdutil: Indexing disabled）".to_owned()
            } else {
                format!("未知（{}）", output.replace('\n', " / "))
            }
        },
    )
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn ensure_perf_fixtures(root: &Path) -> Result<()> {
    let files = [
        "Desktop/synthetic-budget.pptx",
        "Desktop/synthetic-word-doc.docx",
        "Downloads/synthetic-excel-recent.xlsx",
        "Downloads/synthetic-video-large.mp4",
        "Documents/合成-预算-2026.docx",
        "Documents/合成-会议纪要-001.md",
        "Music/Eric Clapton - Wonderful Tonight.mp3",
        "Music/周华健 - 朋友.flac",
        "Pictures/Screenshots/Screenshot 2026-05-24 10-00-00.png",
        "Movies/synthetic-movie-long.mp4",
    ];
    for relative in files {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !path.exists() {
            fs::write(path, format!("Scout synthetic perf fixture: {relative}\n"))?;
        }
    }
    let _ = Command::new("mdimport").arg(root).status();
    Ok(())
}

fn today() -> String {
    command_output("date", &["+%F"]).unwrap_or_else(|| "2026-05-26".to_owned())
}

fn escape_md_cell(value: &str) -> String {
    value.replace('|', "\\|")
}
