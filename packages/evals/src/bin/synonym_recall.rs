//! BETA-15A 同义词召回评测 bin。
//! 加载 ship 词典 + 合成 corpus/cases，跑 parse→expand→匹配，输出报告 + 门槛退出码。
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use scout_evals::recall::{
    check_integrity, load_cases, load_corpus, run_recall, FP_GATE, RECALL_GATE,
};
use scout_harness::YamlSynonymExpander;

#[derive(Parser)]
#[command(name = "synonym_recall", about = "BETA-15A 同义词召回评测")]
struct Cli {
    /// 输出 JSON 报告
    #[arg(long)]
    json: bool,
    /// 仅打印未达标(漏命中/有假阳)的 case
    #[arg(long)]
    only_failures: bool,
}

/// 把 `(key, rate)` 对列表渲染成合法 JSON 对象字符串（无外层依赖）。
fn pairs_to_json(pairs: &[(String, f64)]) -> String {
    let body: Vec<String> = pairs
        .iter()
        .map(|(k, v)| {
            format!(
                "{}:{:.4}",
                serde_json::to_string(k).unwrap_or_else(|_| "\"\"".into()),
                v
            )
        })
        .collect();
    format!("{{{}}}", body.join(","))
}

fn workspace_path(rel: &str) -> PathBuf {
    // CARGO_MANIFEST_DIR = packages/evals → 仓库根是 ../..
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let zh = workspace_path("resources/synonyms/zh.yaml");
    let en = workspace_path("resources/synonyms/en.yaml");
    let expander = match YamlSynonymExpander::from_paths(&zh, &en) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("词典加载失败: {err}");
            return ExitCode::from(2);
        }
    };

    let corpus_path = workspace_path("packages/evals/fixtures/synonym-recall/corpus.json");
    let cases_path = workspace_path("packages/evals/fixtures/synonym-recall/cases.json");
    let corpus = match load_corpus(&corpus_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let cases = match load_cases(&cases_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = check_integrity(&corpus, &cases) {
        eprintln!("fixture 完整性校验失败: {e}");
        return ExitCode::from(2);
    }

    let report = run_recall(&expander, &corpus, &cases);

    if cli.json {
        let by_bucket = pairs_to_json(&report.recall_by(|o| o.bucket.as_str()));
        let by_lang = pairs_to_json(&report.recall_by(|o| o.language.as_str()));
        println!(
            "{{\"recall\":{:.4},\"false_positive\":{:.4},\"cases\":{},\"corpus\":{},\"by_bucket\":{by_bucket},\"by_language\":{by_lang}}}",
            report.recall_rate(),
            report.false_positive_rate(),
            report.outcomes.len(),
            report.corpus_size,
        );
    } else {
        println!("== BETA-15A 同义词召回评测 ==");
        println!(
            "cases={} corpus={}",
            report.outcomes.len(),
            report.corpus_size
        );
        println!("总召回率   : {:.1}%", report.recall_rate() * 100.0);
        println!("总假阳率   : {:.1}%", report.false_positive_rate() * 100.0);
        println!("-- 按桶 --");
        for (k, v) in report.recall_by(|o| o.bucket.as_str()) {
            println!("  {k:<10} {:.1}%", v * 100.0);
        }
        println!("-- 按语言 --");
        for (k, v) in report.recall_by(|o| o.language.as_str()) {
            println!("  {k:<10} {:.1}%", v * 100.0);
        }
        for o in &report.outcomes {
            let failed = !o.missing.is_empty() || !o.extra.is_empty();
            if cli.only_failures && !failed {
                continue;
            }
            if failed {
                println!(
                    "  [FAIL] {} (bucket={}) 漏={:?} 假阳={:?}",
                    o.case_id, o.bucket, o.missing, o.extra
                );
            } else if !cli.only_failures {
                println!("  [ ok ] {}", o.case_id);
            }
        }
    }

    if report.passes_gate() {
        eprintln!(
            "门槛通过: recall {:.1}% >= {:.0}% 且 fp {:.1}% <= {:.0}%",
            report.recall_rate() * 100.0,
            RECALL_GATE * 100.0,
            report.false_positive_rate() * 100.0,
            FP_GATE * 100.0
        );
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "门槛未过: recall {:.1}% (需 >= {:.0}%) / fp {:.1}% (需 <= {:.0}%)",
            report.recall_rate() * 100.0,
            RECALL_GATE * 100.0,
            report.false_positive_rate() * 100.0,
            FP_GATE * 100.0
        );
        ExitCode::from(1)
    }
}
