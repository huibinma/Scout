#![forbid(unsafe_code)]
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use futures_util::StreamExt;
use scout_platform_macos::MacOsLocationResolver;
use scout_search_backend::{
    BackendKind, CancellationToken, Location, SearchBackend, SearchError, SearchIntent,
    SearchResult,
};
use scout_search_backend_spotlight::{translate_intent, SpotlightBackend};

#[derive(Debug, Parser)]
#[command(name = "scout-cli")]
#[command(about = "Scout 原型 CLI：用自然语言搜索本机文件", long_about = None)]
struct Cli {
    /// 输出 `SearchIntent` JSON 和结果数组 JSON。
    #[arg(long)]
    json: bool,

    /// 只输出 `SearchIntent` JSON，不执行搜索。
    #[arg(long)]
    intent_only: bool,

    /// 限制搜索范围；可重复使用。相对路径会按当前工作目录转成绝对路径。
    #[arg(long, value_name = "PATH")]
    onlyin: Vec<PathBuf>,

    /// 自然语言搜索查询。
    query: String,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let runtime = match tokio::runtime::Builder::new_current_thread().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(4);
        }
    };

    match runtime.block_on(run(&cli)) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(4)
        }
    }
}

async fn run(cli: &Cli) -> Result<ExitCode, CliError> {
    let mut intent = scout_intent_parser::parse(&cli.query);
    inject_onlyin(&mut intent, &cli.onlyin)?;

    if cli.intent_only {
        print_json(&intent)?;
        return Ok(ExitCode::SUCCESS);
    }

    match &intent {
        SearchIntent::Clarify(clarify) => {
            println!("{}", clarify.question);
            if let Some(options) = clarify.options.as_ref() {
                for option in options {
                    println!("- {option}");
                }
            }
            Ok(ExitCode::from(2))
        }
        SearchIntent::Refine(_) | SearchIntent::FileAction(_) => {
            println!("此 intent 类型在 v0.1 CLI 中暂不支持端到端执行。");
            Ok(ExitCode::from(3))
        }
        SearchIntent::FileSearch(_) | SearchIntent::MediaSearch(_) => {
            if cli.json {
                print_json_line(&intent)?;
            }
            let result_count = execute_search(&intent, cli.json).await?;
            if cli.json {
                // 结果已在流式消费时逐行输出。
            }

            if result_count == 0 {
                Ok(ExitCode::from(1))
            } else {
                Ok(ExitCode::SUCCESS)
            }
        }
    }
}

async fn execute_search(intent: &SearchIntent, json: bool) -> Result<usize, CliError> {
    let trace_resolver = MacOsLocationResolver::new()
        .map_err(|error| SearchError::BackendUnavailable {
            reason: error.to_string(),
        })
        .map_err(CliError::Search)?;
    let query = translate_intent(intent, &trace_resolver).map_err(CliError::Search)?;
    eprintln!("backend: {}", backend_name(BackendKind::Spotlight));
    // 适配 TranslatedQuery 双查询结构（BETA-15D）
    eprintln!("predicate(Q1): {}", query.q1.predicate);
    if let Some(q2) = &query.q2 {
        eprintln!("predicate(Q2): {}", q2.predicate);
    }
    for path in &query.q1.only_in {
        eprintln!("onlyin: {}", path.display());
    }

    let backend = SpotlightBackend::new().map_err(CliError::Search)?;
    if !backend.is_available() {
        return Err(CliError::Search(SearchError::BackendUnavailable {
            reason: "mdfind not found in PATH".to_owned(),
        }));
    }

    let cancel = CancellationToken::new();
    let mut stream = backend
        .search(intent, cancel)
        .await
        .map_err(CliError::Search)?;
    let mut count = 0;

    while let Some(item) = stream.next().await {
        let result = item.map_err(CliError::Search)?;
        count += 1;
        if json {
            print_json_line(&result)?;
        } else {
            print_human_result(&result);
        }
    }

    Ok(count)
}

fn inject_onlyin(intent: &mut SearchIntent, onlyin: &[PathBuf]) -> Result<(), CliError> {
    if onlyin.is_empty() {
        return Ok(());
    }

    let paths = onlyin
        .iter()
        .map(absolute_path)
        .collect::<Result<Vec<_>, _>>()?;

    match intent {
        SearchIntent::FileSearch(search) => {
            append_include(&mut search.location, paths);
        }
        SearchIntent::MediaSearch(search) => {
            append_include(&mut search.location, paths);
        }
        SearchIntent::Refine(refine) => {
            append_include(&mut refine.delta.location, paths);
        }
        SearchIntent::FileAction(_) | SearchIntent::Clarify(_) => {}
    }

    Ok(())
}

fn append_include(location: &mut Option<Location>, paths: Vec<String>) {
    let location = location.get_or_insert_with(Location::default);
    location.include.get_or_insert_with(Vec::new).extend(paths);
}

fn absolute_path(path: &PathBuf) -> Result<String, CliError> {
    let absolute = if path.is_absolute() {
        path.clone()
    } else {
        std::env::current_dir()?.join(path)
    };

    Ok(absolute.to_string_lossy().into_owned())
}

fn print_human_result(result: &SearchResult) {
    let modified_time = result
        .metadata
        .modified_time
        .map_or_else(|| "-".to_owned(), |time| time.to_rfc3339());
    println!("{}\t{modified_time}", result.path.display());
}

fn print_json<T>(value: &T) -> Result<(), CliError>
where
    T: serde::Serialize,
{
    let json = serde_json::to_string_pretty(value)?;
    println!("{json}");
    Ok(())
}

fn print_json_line<T>(value: &T) -> Result<(), CliError>
where
    T: serde::Serialize,
{
    let json = serde_json::to_string(value)?;
    println!("{json}");
    Ok(())
}

const fn backend_name(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Spotlight => "spotlight",
        BackendKind::WindowsSearch => "windows_search",
        BackendKind::Everything => "everything",
        BackendKind::NativeIndex => "native_index",
        // BETA-15B：语义向量后端。
        BackendKind::SemanticIndex => "semantic_index",
    }
}

#[derive(Debug)]
enum CliError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Search(SearchError),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Json(error) => write!(formatter, "{error}"),
            Self::Search(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for CliError {}

impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for CliError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}
