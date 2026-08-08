#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_precision_loss,
    clippy::unwrap_used,
    clippy::uninlined_format_args
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use scout_evals::runner_daemon::{run_daemon_mode, DaemonModeArgs};
use scout_evals::{
    evaluate_case_with_context, generate_summary, is_fallback_candidate, latency_percentiles,
    load_cases, result_rank, variant_confusion_matrix, CaseReport, EvalContext, EvalResult,
};
use scout_intent_parser::fallback::ModelFallback;
use scout_intent_parser::SEARCH_INTENT_GBNF;
use scout_model_runtime::{ModelDaemon, ModelLoadParams};

/// BETA-32 T12：evals 跑评测的两种模式。
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// 默认：纯 in-process 评测（parser / 可选 `ModelFallback`）。
    Desktop,
    /// BETA-32：spawn scoutd 子进程 + 走 MCP `search` 取 top-K paths。
    /// 本模式输出 top-K paths（json 形式打到 stdout），不参与 parser 级评测；
    /// 用来做与 desktop 模式的 top-K 集合等价闸门（T14 红线 4）。
    Daemon,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
#[allow(clippy::struct_excessive_bools)]
struct Args {
    /// 跑特定一条用例（按 ID）
    #[arg(short, long)]
    case: Option<String>,

    /// 输出 JSON 报告
    #[arg(long)]
    json: bool,

    /// 仅看失败（fail 与 partial）
    #[arg(long)]
    only_failures: bool,

    /// fixture 版本：v0.1 或 v0.5
    #[arg(long, default_value = "v0.1")]
    fixtures: String,

    /// MVP-17：启用模型 fallback（按 Class 3 触发器决策是否调模型）。
    /// 默认 feature build 走 StubLoader（不产合法 intent），仅验证 wiring；
    /// `cargo build --features model-fallback` 启用 llama.cpp 真模型。
    #[arg(long)]
    with_fallback: bool,

    /// GGUF 模型路径。未提供时按顺序尝试：CLI --model-path → 环境变量
    /// `SCOUT_MODEL_PATH` → `models/qwen3-0.6b-q4_k_m.gguf`（BETA-17 winner）。
    #[arg(long)]
    model_path: Option<PathBuf>,

    /// GPU 层数（Metal/CUDA）。默认 999 = 全部到 GPU。
    #[arg(long, default_value_t = 999)]
    gpu_layers: u32,

    /// 模型上下文窗口大小（token）。
    #[arg(long, default_value_t = 2048)]
    context_size: u32,

    /// 只跑 fallback 候选子集，避免在 500 条全量上慢跑。
    #[arg(long)]
    fallback_subset: bool,

    /// MVP-17 v0.2：启用 GBNF 受限解码（强制模型输出符合 schema v1.0 的 JSON）。
    /// 仅在 --with-fallback 时生效；llama-cpp 后端启用，stub/candle 忽略。
    #[arg(long)]
    grammar: bool,

    /// MVP-17 v0.3：启用混合架构（parser 锁 variant + 模型只填字段补丁）。
    /// 仅在 --with-fallback 时生效。预期能减少"模型推翻 parser 对的判断"类 regression。
    #[arg(long)]
    hybrid: bool,

    /// 自动与一份已有的 JSON 报告对比，产出 diff。
    #[arg(long)]
    baseline: Option<PathBuf>,

    /// 只跑前 N 条用例（冒烟门快速验证用）。
    #[arg(long)]
    limit: Option<usize>,

    /// BETA-23：只报告 fallback 触发率（按 reason 分桶），不评测。纯 parser，无需模型。
    #[arg(long)]
    fire_rate: bool,

    /// BETA-32 T12：评测模式（desktop = 默认 in-process；daemon = spawn scoutd 子进程走 MCP）。
    #[arg(long, value_enum, default_value_t = Mode::Desktop)]
    mode: Mode,

    /// BETA-32 T12：daemon 模式必填，scoutd 可执行文件路径。
    #[arg(long)]
    daemon_binary: Option<PathBuf>,

    /// BETA-32 T12：daemon 模式必填，索引根目录（透传给 daemon `--root`）。
    #[arg(long)]
    root: Option<PathBuf>,

    /// BETA-32 T12：daemon 模式每条 case 的 top-K 取数。默认 20，与 spec §4.1 SearchInput.limit 默认对齐。
    #[arg(long, default_value_t = 20)]
    topk: usize,

    /// BETA-32 T12：daemon 起来等 `/health` 200 的上限秒数。默认 60s（首次全量索引耗时不可预测）。
    #[arg(long, default_value_t = 60)]
    health_timeout_secs: u64,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cases = load_cases(&args.fixtures)?;

    let mut filtered_cases = if let Some(id) = &args.case {
        cases
            .into_iter()
            .filter(|c| c.id == *id)
            .collect::<Vec<_>>()
    } else {
        cases
    };

    if args.fallback_subset {
        let before = filtered_cases.len();
        filtered_cases.retain(is_fallback_candidate);
        eprintln!(
            "[fallback-subset] 过滤后 {} / {} case",
            filtered_cases.len(),
            before
        );
    }

    if let Some(n) = args.limit {
        filtered_cases.truncate(n);
    }

    if filtered_cases.is_empty() {
        if let Some(id) = &args.case {
            eprintln!("未找到 ID 为 {id} 的用例");
        } else {
            eprintln!("未找到任何用例");
        }
        return Ok(());
    }

    // BETA-32 T12：daemon mode 在 in-process 评测路径之前分流（fire_rate / fallback
    // 都不适用 daemon mode）。
    if args.mode == Mode::Daemon {
        return run_mode_daemon(&args, &filtered_cases);
    }

    if args.fire_rate {
        report_fire_rate(&filtered_cases);
        return Ok(());
    }

    // 模型 fallback 初始化（lazy；仅 --with-fallback 时加载）。
    let fallback_holder = if args.with_fallback {
        Some(load_fallback(&args)?)
    } else {
        None
    };

    let ctx = match &fallback_holder {
        Some(f) => EvalContext::with_fallback(f),
        None => EvalContext::parser_only(),
    };

    let reports: Vec<CaseReport> = filtered_cases
        .iter()
        .map(|c| evaluate_case_with_context(c, &ctx))
        .collect();

    if args.json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
        return Ok(());
    }

    let baseline = if let Some(path) = &args.baseline {
        Some(load_baseline(path)?)
    } else {
        None
    };

    print_report(
        &reports,
        args.only_failures,
        &args.fixtures,
        args.with_fallback,
        args.grammar,
        args.hybrid,
        baseline.as_ref(),
    );

    Ok(())
}

/// BETA-32 T12：daemon-mode 评测分支 —— spawn scoutd 子进程、跑 case、打印
/// top-K 输出（JSON 形式）。
///
/// daemon mode 不评 parser 字段正确性、只取 top-K paths（评测语义层留给上层
/// 与 desktop 模式做集合等价比对，见 `runner_daemon::topk_set_equivalent`）。
fn run_mode_daemon(args: &Args, cases: &[scout_evals::Case]) -> anyhow::Result<()> {
    let daemon_binary = args
        .daemon_binary
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--mode daemon 必须配 --daemon-binary"))?;
    let root = args
        .root
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--mode daemon 必须配 --root（索引根目录）"))?;
    let model_path = args
        .model_path
        .clone()
        .or_else(|| std::env::var_os("SCOUT_MODEL_PATH").map(PathBuf::from))
        .ok_or_else(|| {
            anyhow::anyhow!("--mode daemon 必须配 --model-path（或 SCOUT_MODEL_PATH 环境变量）")
        })?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("构建 tokio runtime 失败：{e}"))?;

    let daemon_args = DaemonModeArgs {
        daemon_binary,
        root,
        model_path,
        limit: args.topk,
        health_timeout: Duration::from_secs(args.health_timeout_secs),
    };

    eprintln!(
        "[daemon] spawning {} root={} model={} cases={}",
        daemon_args.daemon_binary.display(),
        daemon_args.root.display(),
        daemon_args.model_path.display(),
        cases.len()
    );

    let results = runtime.block_on(run_daemon_mode(&daemon_args, cases))?;

    // daemon-mode 输出永远是 JSON（不走 print_report，因为没有 parser 评测维度）。
    // 字段：id / query / paths（top-K）。供外部脚本 / `topk_set_equivalent`
    // 比对器二次消化。
    let payload: Vec<serde_json::Value> = results
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "query": r.query,
                "paths": r.paths,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

fn load_fallback(args: &Args) -> anyhow::Result<ModelFallback> {
    let model_path = args
        .model_path
        .clone()
        .or_else(|| std::env::var_os("SCOUT_MODEL_PATH").map(PathBuf::from))
        // BETA-17 winner = Qwen3-0.6B（与 v1 准确率对等、更小更快）。SCOUT_MODEL_PATH 优先。
        .unwrap_or_else(|| PathBuf::from("models/qwen3-0.6b-q4_k_m.gguf"));

    eprintln!("[fallback] loading model: {}", model_path.display());
    let start = std::time::Instant::now();
    let daemon = ModelDaemon::load_blocking(
        &model_path,
        ModelLoadParams {
            gpu_layers: args.gpu_layers,
            context_size: args.context_size,
        },
    )
    .map_err(|err| anyhow::anyhow!("加载模型失败 ({}): {err}", model_path.display()))?;
    eprintln!("[fallback] model loaded in {:?}", start.elapsed());

    let mut fallback = ModelFallback::new(Arc::new(daemon));
    if args.grammar {
        eprintln!("[fallback] GBNF grammar enabled (search-intent.v1)");
        fallback = fallback.with_grammar(SEARCH_INTENT_GBNF);
    }
    if args.hybrid {
        eprintln!("[fallback] hybrid mode enabled (parser locks variant, model fills fields)");
        fallback = fallback.with_hybrid_mode();
    }
    Ok(fallback)
}

fn load_baseline(path: &Path) -> anyhow::Result<HashMap<String, CaseReport>> {
    let content = std::fs::read_to_string(path)?;
    let reports: Vec<CaseReport> = serde_json::from_str(&content)?;
    Ok(reports
        .into_iter()
        .map(|r| (r.case.id.clone(), r))
        .collect())
}

#[allow(clippy::too_many_lines, clippy::cast_possible_wrap)]
fn print_diff(reports: &[CaseReport], baseline: &HashMap<String, CaseReport>, path: &Path) {
    println!("=== diff vs baseline ({}) ===", path.display());

    let mut b_pass: usize = 0;
    let mut b_partial: usize = 0;
    let mut b_fail: usize = 0;
    let mut c_pass: usize = 0;
    let mut c_partial: usize = 0;
    let mut c_fail: usize = 0;

    let mut changed_cases = Vec::new();

    for r in reports {
        if let Some(br) = baseline.get(&r.case.id) {
            match br.result {
                EvalResult::Pass => b_pass += 1,
                EvalResult::Partial { .. } => b_partial += 1,
                EvalResult::Fail { .. } => b_fail += 1,
            }
            match r.result {
                EvalResult::Pass => c_pass += 1,
                EvalResult::Partial { .. } => c_partial += 1,
                EvalResult::Fail { .. } => c_fail += 1,
            }

            if result_rank(&r.result) != result_rank(&br.result) {
                changed_cases.push((br, r));
            }
        }
    }

    println!("总览变化:");
    println!(
        "  pass:    {:>3} → {:<3} ({:+})",
        b_pass,
        c_pass,
        c_pass as i64 - b_pass as i64
    );
    println!(
        "  partial: {:>3} → {:<3} ({:+})",
        b_partial,
        c_partial,
        c_partial as i64 - b_partial as i64
    );
    println!(
        "  fail:    {:>3} → {:<3} ({:+})",
        b_fail,
        c_fail,
        c_fail as i64 - b_fail as i64
    );
    println!();

    if !changed_cases.is_empty() {
        println!("per-case 变化（只列 result 桶变化的 case）:");
        for (br, r) in changed_cases {
            let b_label = match br.result {
                EvalResult::Pass => "Pass",
                EvalResult::Partial { .. } => "Partial",
                EvalResult::Fail { .. } => "Fail",
            };
            let c_label = match r.result {
                EvalResult::Pass => "Pass",
                EvalResult::Partial { .. } => "Partial",
                EvalResult::Fail { .. } => "Fail",
            };

            println!("  {} \"{}\"", r.case.id, r.case.query);
            print!("    {b_label} → {c_label}");

            match (&br.result, &r.result) {
                (
                    EvalResult::Fail {
                        actual_variant: b_v,
                    },
                    EvalResult::Fail {
                        actual_variant: c_v,
                    },
                ) if b_v != c_v => {
                    print!(" (variant: {b_v} → {c_v})");
                }
                (
                    EvalResult::Fail {
                        actual_variant: b_v,
                    },
                    _,
                ) => {
                    print!(" (variant: {b_v} → {})", r.case.variant);
                }
                (
                    _,
                    EvalResult::Fail {
                        actual_variant: c_v,
                    },
                ) => {
                    print!(" (variant: {} → {c_v})", r.case.variant);
                }
                (EvalResult::Partial { diff: b_d }, EvalResult::Partial { diff: c_d }) => {
                    let mut b_keys: Vec<_> = b_d.keys().cloned().collect();
                    let mut c_keys: Vec<_> = c_d.keys().cloned().collect();
                    b_keys.sort();
                    c_keys.sort();
                    if b_keys == c_keys {
                        print!(" (diff fields: {})", b_keys.join(", "));
                    } else {
                        print!(" (diff fields changed)");
                    }
                }
                _ => {}
            }
            println!();
        }
        println!();
    }
}

#[allow(clippy::too_many_lines, clippy::fn_params_excessive_bools)]
fn print_report(
    reports: &[CaseReport],
    only_failures: bool,
    fixtures: &str,
    with_fallback: bool,
    grammar: bool,
    hybrid: bool,
    baseline: Option<&HashMap<String, CaseReport>>,
) {
    let summary = generate_summary(reports);

    if !only_failures {
        let mode_label = if with_fallback {
            match (grammar, hybrid) {
                (true, true) => " (with-fallback + GBNF + hybrid)",
                (true, false) => " (with-fallback + GBNF)",
                (false, true) => " (with-fallback + hybrid)",
                (false, false) => " (with-fallback)",
            }
        } else {
            ""
        };
        println!("=== Scout evals {fixtures}{mode_label} ===\n");
        println!("总览：");
        println!(
            "  variant 命中率:      {:>3} / {:>3}  ({:.1}%)",
            summary.variant_hits(),
            summary.total,
            (summary.variant_hits() as f64 / summary.total as f64) * 100.0
        );
        println!(
            "  字段级精确匹配率:  {:>3} / {:>3}  ({:.1}%)",
            summary.field_exact_matches(),
            summary.total,
            (summary.field_exact_matches() as f64 / summary.total as f64) * 100.0
        );
        println!(
            "  pass:              {:>3} / {:>3}  ({:.1}%)",
            summary.pass,
            summary.total,
            (summary.pass as f64 / summary.total as f64) * 100.0
        );
        println!(
            "  partial:           {:>3} / {:>3}  ({:.1}%)",
            summary.partial,
            summary.total,
            (summary.partial as f64 / summary.total as f64) * 100.0
        );
        println!(
            "  fail:              {:>3} / {:>3}  ({:.1}%)\n",
            summary.fail,
            summary.total,
            (summary.fail as f64 / summary.total as f64) * 100.0
        );

        if with_fallback {
            println!("MVP-17 fallback 指标：");
            println!(
                "  fallback 触发数:        {:>3} / {:>3}",
                summary.fallback_invoked, summary.total
            );
            println!(
                "  模型 valid intent:      {:>3} / {:>3}  ({})",
                summary.fallback_valid_intent,
                summary.fallback_invoked,
                if summary.fallback_invoked == 0 {
                    "—".to_owned()
                } else {
                    format!(
                        "{:.1}%",
                        (summary.fallback_valid_intent as f64 / summary.fallback_invoked as f64)
                            * 100.0
                    )
                }
            );
            println!("  parser → fallback 救回:");
            println!("    rescued_to_pass:      {:>3}", summary.rescued_to_pass);
            println!(
                "    rescued_to_partial:   {:>3}",
                summary.rescued_to_partial
            );
            println!("    regressed:            {:>3}", summary.regressed);
            let parser_fails = reports
                .iter()
                .filter(|r| {
                    r.parser_result
                        .as_ref()
                        .is_some_and(|r| matches!(r, EvalResult::Fail { .. }))
                })
                .count();
            if parser_fails > 0 {
                let rescued = summary.rescued_to_pass + summary.rescued_to_partial;
                println!(
                    "    救回率 = (pass+partial) / parser_fail = {} / {} = {:.1}%",
                    rescued,
                    parser_fails,
                    (rescued as f64 / parser_fails as f64) * 100.0
                );
            }
            let (p50_all, p95_all) = latency_percentiles(&summary.latencies_all_ms);
            let (p50_fb, p95_fb) = latency_percentiles(&summary.latencies_fallback_ms);
            println!("  端到端延迟（全 case，含 parser-only）:");
            println!("    p50: {p50_all} ms");
            println!("    p95: {p95_all} ms");
            println!("  端到端延迟（仅 fallback 触发的 case）:");
            println!("    p50: {p50_fb} ms");
            println!("    p95: {p95_fb} ms\n");

            let confusion = variant_confusion_matrix(reports);
            if !confusion.is_empty() {
                println!("variant confusion (fail cases only):");
                for ((expected, actual), count) in confusion {
                    println!("  {expected} → {actual}: {count}");
                }
                println!();
            }
        }

        println!("按 variant 分桶：");
        let mut v_keys: Vec<_> = summary.variant_stats.keys().collect();
        v_keys.sort();
        for v in v_keys {
            let (p, pt, f) = summary.variant_stats[v];
            println!("  {v:<12}  pass {p:>2}  partial {pt:>2}  fail {f:>2}");
        }
        println!();

        println!("按 language 分桶：");
        let mut l_keys: Vec<_> = summary.language_stats.keys().collect();
        l_keys.sort();
        for l in l_keys {
            let (p, pt, f) = summary.language_stats[l];
            println!("  {l:<8}  pass {p:>2}  partial {pt:>2}  fail {f:>2}");
        }
        println!();

        if let Some(baseline) = baseline {
            print_diff(reports, baseline, Path::new("baseline"));
        }
    }

    let failures: Vec<_> = reports
        .iter()
        .filter(|r| !matches!(r.result, EvalResult::Pass))
        .collect();

    if !failures.is_empty() {
        println!("失败详情（fail 与 partial）：");
        for r in failures {
            match &r.result {
                EvalResult::Fail { actual_variant } => {
                    println!("  case #{:<8} \"{}\"：", r.case.id, r.case.query);
                    println!(
                        "    fail: variant mismatch (expected {}, actual {actual_variant})",
                        r.case.variant
                    );
                    if r.fallback_invoked {
                        println!(
                            "    fallback: invoked, valid_intent={}",
                            r.fallback_valid_intent
                        );
                    }
                }
                EvalResult::Partial { diff } => {
                    println!("  case #{:<8} \"{}\"：", r.case.id, r.case.query);
                    if r.fallback_invoked {
                        println!(
                            "    fallback: invoked, valid_intent={}",
                            r.fallback_valid_intent
                        );
                    }
                    println!("    diff:");
                    let mut keys: Vec<_> = diff.keys().collect();
                    keys.sort();
                    for k in keys {
                        let (expected, actual) = diff.get(k).unwrap();
                        println!("      .{k}: expected {expected}, actual {actual}");
                    }
                }
                EvalResult::Pass => unreachable!(),
            }
            println!();
        }
    } else if only_failures {
        println!("没有失败的用例。");
    }
}

/// BETA-23：统计 `should_invoke_model` 在数据集上的触发率与 reason 分布。
fn report_fire_rate(cases: &[scout_evals::Case]) {
    use scout_intent_parser::fallback::{
        parse_with_signals, should_invoke_model, FallbackDecision, FallbackReason,
    };
    let mut clarified = 0usize;
    let mut omission_fields: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    let mut triggered_ids: Vec<&str> = Vec::new();
    for case in cases {
        let parsed = parse_with_signals(&case.query);
        match should_invoke_model(&parsed) {
            FallbackDecision::UseParser => {}
            FallbackDecision::InvokeModel(reason) => {
                triggered_ids.push(&case.id);
                match reason {
                    FallbackReason::ParserClarified => clarified += 1,
                    FallbackReason::StructuralOmission { fields } => {
                        for f in fields {
                            *omission_fields.entry(f).or_insert(0) += 1;
                        }
                    }
                }
            }
        }
    }
    let total = cases.len();
    let fired = triggered_ids.len();
    #[allow(clippy::cast_precision_loss)]
    let rate = if total == 0 {
        0.0
    } else {
        fired as f64 * 100.0 / total as f64
    };
    println!("fire-rate: {fired}/{total} ({rate:.1}%)");
    println!("  clarified: {clarified}");
    for (field, n) in &omission_fields {
        println!("  omission.{field}: {n}");
    }
    println!("triggered ids: {}", triggered_ids.join(", "));
}
