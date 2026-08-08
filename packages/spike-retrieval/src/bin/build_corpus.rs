//! BETA-26 探针 Task 2：遍历 home 目录冻结语料到 fixtures/corpus.jsonl。
//!
//! 设计要点（与原计划单目录版不同，按本机实际：个人文档目录近乎为空，真语料在整个 home）：
//! - 排除 build/cache/依赖/系统噪声目录，用 walkdir filter_entry 提前剪枝整棵子树（快）。
//! - 分层抽样：稀缺格式（office/pdf/html）全收，不抽样；bulk（md/txt）等距 stride 抽样补足
//!   到 MAX_DOCS。早期全局等距抽样会把 office/pdf 这类稀有格式淹没（实测 0 个 office、3 个 pdf），
//!   故按格式分层，确保稀缺格式存活。两组都用确定性 排序+等距 stride，无 RNG，可复现。
//! - panic 安全：home 内含损坏 PDF 会让 pdf-extract panic；extract_document 直调不带
//!   catch_unwind（其守护在 indexer scan.rs 的另一条路径），故此处逐文件 catch_unwind 包裹，
//!   panic / Err / 空白正文 三种情况都计为 skipped 跳过。
//!
//! 一次性丢弃代码，GO/NO-GO 后可删。

use anyhow::Result;
use indexer::extract_document;
use spike_retrieval::CorpusDoc;
use std::io::{BufWriter, Write};
use std::path::Path;
use walkdir::{DirEntry, WalkDir};

/// 需排除的目录名（build/cache/依赖/系统噪声）；命中即剪掉整棵子树。
const EXCLUDE: &[&str] = &[
    "Library",
    "node_modules",
    ".git",
    "target",
    ".cargo",
    ".rustup",
    ".venv",
    "venv",
    "__pycache__",
    "dist",
    "build",
    ".next",
    "Pods",
    ".gradle",
    ".Trash",
    "vendor",
    ".cache",
    "DerivedData",
];

/// 稀缺格式扩展名（office/pdf/html，小写比较）：数量少（~100），全收不抽样。
const SCARCE_EXTS: &[&str] = &["docx", "pptx", "xlsx", "xls", "pdf", "html", "htm"];

/// bulk 格式扩展名（md/txt，小写比较）：数量大，等距 stride 抽样补足余额。
const BULK_EXTS: &[&str] = &["md", "txt"];

/// 该目录项是否为被排除目录（仅对目录有意义，供 filter_entry 剪枝用）。
fn is_excluded_dir(e: &DirEntry) -> bool {
    e.file_type().is_dir()
        && e.file_name()
            .to_str()
            .map(|n| EXCLUDE.contains(&n))
            .unwrap_or(false)
}

/// 取小写扩展名。
fn lower_ext(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// 抽取单文件正文（panic 安全 + 空白跳过），成功则写出并自增 kept。
/// 返回 true 表示已写出（kept+1），false 表示跳过（skipped+1）。
fn extract_and_write<W: Write>(
    path_str: &str,
    writer: &mut W,
    kept: &mut usize,
    skipped: &mut usize,
) -> Result<()> {
    let path = Path::new(path_str);
    // mtime：取不到就用 0，不影响正文抽取。
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let extracted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        extract_document(path, mtime)
    }));

    let body = match extracted {
        // BETA-35 cycle 4：extract_document 返 ExtractedDoc（原 tuple 升级）；仍只取 body。
        Ok(Ok(doc)) => doc.body,
        // Err（抽取失败）或 panic（catch_unwind 捕获）均跳过。
        _ => {
            *skipped += 1;
            return Ok(());
        }
    };

    if body.trim().is_empty() {
        *skipped += 1;
        return Ok(());
    }

    let doc = CorpusDoc {
        id: format!("d{kept:05}"),
        path: path_str.to_owned(),
        text: body,
    };
    writeln!(writer, "{}", serde_json::to_string(&doc)?)?;
    *kept += 1;

    if kept.is_multiple_of(200) {
        eprintln!("  …已冻结 {kept} 篇（跳过 {skipped}）");
    }
    Ok(())
}

fn main() -> Result<()> {
    // 抑制 pdf-extract 等库 panic 时刷屏的 stderr（运行后恢复默认 hook）。
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let result = run();

    std::panic::set_hook(prev_hook);
    result
}

fn run() -> Result<()> {
    let root = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("HOME").ok())
        .ok_or_else(|| anyhow::anyhow!("未提供 root 参数且 HOME 环境变量缺失"))?;

    let max_docs: usize = std::env::var("MAX_DOCS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5000);

    eprintln!("📂 遍历根目录: {root}（MAX_DOCS={max_docs}）");

    // 1) 收齐候选路径并按格式分流（filter_entry 提前剪掉噪声目录子树）。
    let mut scarce: Vec<String> = Vec::new();
    let mut bulk: Vec<String> = Vec::new();
    for entry in WalkDir::new(&root)
        .into_iter()
        .filter_entry(|e| !is_excluded_dir(e))
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext) = lower_ext(path) else { continue };
        if SCARCE_EXTS.contains(&ext.as_str()) {
            scarce.push(path.to_string_lossy().into_owned());
        } else if BULK_EXTS.contains(&ext.as_str()) {
            bulk.push(path.to_string_lossy().into_owned());
        }
    }

    // 2) 排序（WalkDir 顺序不保证稳定，排序保可复现）。
    scarce.sort();
    bulk.sort();
    let candidate_count = scarce.len() + bulk.len();
    eprintln!(
        "🔍 候选文件: {candidate_count} 篇（稀缺 office/pdf/html {} + bulk md/txt {}）",
        scarce.len(),
        bulk.len()
    );

    // 3) 分层抽样：稀缺格式全收；bulk 等距 stride 抽样补足余额到 MAX_DOCS（可复现，无 RNG）。
    let remainder = max_docs.saturating_sub(scarce.len());
    let sampled_bulk: Vec<&String> = if bulk.len() > remainder {
        // remainder==0 时 checked_div 返回 None，跳过 bulk（空集）。
        match bulk.len().checked_div(remainder) {
            Some(step) => (0..remainder).map(|i| &bulk[i * step]).collect(),
            None => Vec::new(),
        }
    } else {
        bulk.iter().collect()
    };
    eprintln!(
        "🎯 抽样后待抽取: {} 篇（稀缺全收 {} + bulk 抽样 {}）",
        scarce.len() + sampled_bulk.len(),
        scarce.len(),
        sampled_bulk.len()
    );

    // 4) 准备输出。
    let out_dir = "packages/spike-retrieval/fixtures";
    std::fs::create_dir_all(out_dir)?;
    let out_path = format!("{out_dir}/corpus.jsonl");
    let file = std::fs::File::create(&out_path)?;
    let mut writer = BufWriter::new(file);

    // 5) 逐文件 panic 安全抽取，仅写非空正文。先处理稀缺组再处理 bulk 组，id 顺序自增。
    let mut kept = 0usize;
    let mut skipped = 0usize;

    let scarce_before = kept;
    for path_str in &scarce {
        extract_and_write(path_str, &mut writer, &mut kept, &mut skipped)?;
    }
    let scarce_kept = kept - scarce_before;

    let bulk_before = kept;
    for path_str in &sampled_bulk {
        extract_and_write(path_str, &mut writer, &mut kept, &mut skipped)?;
    }
    let bulk_kept = kept - bulk_before;

    writer.flush()?;
    eprintln!(
        "✅ 冻结 {kept} 篇（office/pdf/html {scarce_kept} 篇 + md/txt {bulk_kept} 篇）→ {out_path}（候选 {candidate_count}，跳过 {skipped}）"
    );
    Ok(())
}
