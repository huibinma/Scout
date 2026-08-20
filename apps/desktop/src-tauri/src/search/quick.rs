//! "快速查找"命令（重构：找文件搜索框的双模式之一）。
//!
//! 与"深度检索"（[`crate::search::search`]，完整 NL intent 解析 + policy + 同义词
//! 扩展 + fan-out/fallback + 语义召回）分工——本命令**跳过 NL 解析、policy、同义词
//! 扩展**，直接把用户当前输入当文件名关键词，并发查两个已注册 backend：
//! Windows 上的 `search.native_file_index`（内置 MFT/USN 原生索引，全盘、常驻内存，
//! 类 Everything 的"输入即出结果"体验）与跨平台的 `search.local`（本地 SQLite
//! 索引，命中已索引文档/音乐的文件名/标题/作者/艺术家）。两路结果合并去重后按
//! 匹配紧密度（相等 > 前缀 > 子串）粗排，供输入框下方的实时下拉列表使用。
//!
//! 回车后前端切换到 [`crate::search::search`] 的完整深度检索结果，本命令的结果
//! 不进 `ContextMemory`/审计/trace——它只是"看一眼"，不是一次正式查询。

use std::collections::HashSet;

use futures_util::StreamExt;
use scout_search_backend::{CancellationToken, FileSearch, SchemaVersion, SearchIntent};
use serde::Serialize;

use super::SearchDeps;

/// 单条快速查找结果（字段刻意精简于 [`super::SearchResultJson`]——下拉列表不需要
/// score/sources/match_type 这些深度检索才有意义的字段）。
#[derive(Debug, Clone, Serialize)]
pub struct QuickResultJson {
    pub path: String,
    pub name: String,
    /// 命中来源："native_file_index" | "local"。
    pub source: String,
    pub modified_time: Option<String>,
    pub size_bytes: Option<u64>,
}

/// 前端下拉列表展示上限。
const QUICK_LIMIT: usize = 30;
/// 单个 backend 内部取回上限（略大于展示上限，供合并去重后仍有富余）。
const PER_SOURCE_LIMIT: u32 = 40;

/// 快速查找会尝试的 backend id（按此顺序发起，结果合并去重、不分先后展示）。
/// Windows 独有 `search.native_file_index`；`search.local` 全平台共用。
const QUICK_TOOL_IDS: &[&str] = &["search.local", "search.native_file_index"];

/// 命令主体（便于单测注入 `SearchDeps`，不依赖 `tauri::State`）。thin wrapper
/// `search::quick_search`（`#[tauri::command]`）在 `search.rs` 里，与本 crate
/// 其余搜索类命令（`search`/`get_preview` 等）同一模式——`generate_handler!`
/// 要求宏生成的隐藏 sibling 项与 `#[tauri::command]` fn 同模块，故不能把命令
/// 属性直接放在子模块里再 `pub use` 出去。
pub(crate) async fn quick_search_impl(query: &str, deps: &SearchDeps) -> Vec<QuickResultJson> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }

    let intent = SearchIntent::FileSearch(FileSearch {
        schema_version: SchemaVersion::V1,
        language: None,
        keywords: Some(vec![q.to_owned()]),
        extensions: None,
        file_type: None,
        location: None,
        modified_time: None,
        created_time: None,
        accessed_time: None,
        size: None,
        exclude_extensions: None,
        exclude_file_type: None,
        sort: None,
        limit: Some(PER_SOURCE_LIMIT),
    });

    let cancel = CancellationToken::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut merged: Vec<QuickResultJson> = Vec::new();

    for &id in QUICK_TOOL_IDS {
        let Some(tool) = deps.registry().find_search_tool(id) else {
            continue;
        };
        // is_available() 在原生索引首次调用时会触发一次全盘 MFT 枚举（若启动期后台
        // 预热尚未完成，这一次调用会同步等它跑完）——不理想但正确；预热见
        // `main.rs` `.setup()` 里的 native-index 后台 `spawn_blocking`，正常情况下
        // 用户开始打字前已就绪。
        if !tool.is_available() {
            continue;
        }
        let Ok(stream) = tool.search(&intent, cancel.clone()).await else {
            continue;
        };
        let results: Vec<_> = stream.collect::<Vec<_>>().await;
        for result in results.into_iter().flatten() {
            let key = result.path.to_string_lossy().to_lowercase();
            if !seen.insert(key) {
                continue; // 跨 backend 命中同一文件，保留先入者
            }
            merged.push(QuickResultJson {
                path: scout_search_backend::user_facing_path(&result.path),
                name: result.name,
                source: format!("{:?}", result.source).to_lowercase(),
                modified_time: result.metadata.modified_time.map(|t| t.to_rfc3339()),
                size_bytes: result.metadata.size_bytes,
            });
        }
    }

    sort_by_match_tightness(&mut merged, q);
    merged.truncate(QUICK_LIMIT);
    merged
}

/// 粗排：文件名与查询词相等 > 前缀匹配 > 其余子串匹配；同档内按文件名排序，
/// 保证多次相同输入结果顺序稳定（HashMap 遍历顺序本身不稳定）。
fn sort_by_match_tightness(results: &mut [QuickResultJson], query: &str) {
    let needle = query.to_lowercase();
    let rank = |name: &str| -> u8 {
        let lower = name.to_lowercase();
        if lower == needle {
            0
        } else if lower.starts_with(needle.as_str()) {
            1
        } else {
            2
        }
    };
    results.sort_by(|a, b| {
        rank(&a.name)
            .cmp(&rank(&b.name))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn result(path: &str, name: &str) -> QuickResultJson {
        QuickResultJson {
            path: path.to_owned(),
            name: name.to_owned(),
            source: "local".to_owned(),
            modified_time: None,
            size_bytes: None,
        }
    }

    #[test]
    fn empty_query_short_circuits_without_touching_registry() {
        // 用真实的空 SearchDeps 亦可（不需要注册任何 backend），验证空输入直接返回空。
        let deps = SearchDeps::new(
            std::sync::Arc::new(scout_harness::ToolRegistry::new()),
            std::sync::Arc::new(scout_harness::PolicyEngine::new()),
            std::sync::Arc::new(scout_harness::Tracer::with_hooks(Vec::new())),
            std::sync::Arc::new(std::sync::Mutex::new(
                scout_harness::context::ContextMemory::new(),
            )),
            std::sync::Arc::new(scout_harness::file_action_tool::FileActionTool::new(
                std::sync::Arc::new(scout_harness::file_action_tool::LocalFileActionExecutor),
                scout_harness::PolicyEngine::new(),
            )),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(scout_harness::NoopExpander)
                as std::sync::Arc<dyn scout_harness::SynonymExpander>,
        );
        let out = futures_executor::block_on(quick_search_impl("   ", &deps));
        assert!(out.is_empty());
    }

    #[test]
    fn sort_ranks_exact_then_prefix_then_substring() {
        let mut results = vec![
            result("/a/预算说明.docx", "预算说明.docx"),
            result("/a/预算.docx", "预算.docx"),
            result("/a/2024预算.docx", "2024预算.docx"),
        ];
        sort_by_match_tightness(&mut results, "预算");
        assert_eq!(
            results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["预算.docx", "预算说明.docx", "2024预算.docx"],
        );
    }
}
