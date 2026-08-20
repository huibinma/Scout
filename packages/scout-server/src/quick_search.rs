//! `POST /search/quick`（BETA-78）——桌面「快速查找」下拉专用精简检索：跳过
//! NL intent 解析 / policy / 同义词扩展，把输入直接当文件名关键词，并发查
//! 目标 collection 的本地 FTS 候选（`BackendKind::NativeIndex`，即 `scout-indexer`
//! 自建 `SQLite` FTS5，命名与 `NativeFileIndex`/MFT 那个刻意不同——见
//! [`scout_search_backend::BackendKind`] 文档）+（Windows 且 admin token 时）
//! 系统级原生文件名索引（MFT/USN 全盘覆盖）。
//!
//! 与桌面端既有 `apps/desktop/src-tauri/src/search/quick.rs` 的
//! `quick_search_impl` 是同一套设计——本模块是它的 service 端落地：桌面重构
//! 为瘦客户端后，这条快速查找逻辑整体搬到这里，桌面只剩 HTTP 调用。
//!
//! **安全边界**：原生文件名索引查询卷范围文件名，不受任何 collection root
//! 限制——只有 `admin=true` 的 token 能触达它（个人模式单 token 恒 admin，
//! 不受影响；企业 collection 限权 token 绝不能借这条精简路径绕过信息墙拿到
//! 整卷文件名）。本地 FTS 候选仍按 `resolve_target_ids` 做与 `/search` 一致的
//! collection 级授权收窄。

use std::collections::HashSet;
use std::sync::Arc;

use futures_util::StreamExt as _;
use scout_search_backend::{
    BackendKind, CancellationToken, FileSearch, SchemaVersion, SearchIntent,
};
use serde::{Deserialize, Serialize};

use crate::auth::AuthedPrincipal;
use crate::config::ServerCtx;
use crate::tools::search::resolve_target_ids;
use crate::tools::ToolError;

/// 前端下拉列表展示上限（镜像桌面端 `QUICK_LIMIT`）。
const QUICK_LIMIT: usize = 30;
/// 单个来源内部取回上限。
const PER_SOURCE_LIMIT: u32 = 40;

#[derive(Deserialize)]
pub(crate) struct QuickSearchInput {
    pub(crate) query: String,
    #[serde(default)]
    pub(crate) collections: Option<Vec<String>>,
}

#[derive(Serialize)]
pub(crate) struct QuickResult {
    path: String,
    name: String,
    /// 命中所属 collection；系统级原生索引命中（卷范围，不属于任何 collection）时为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    collection: Option<String>,
    /// "local" | "`native_file_index`"。
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    modified_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
}

#[derive(Serialize)]
pub(crate) struct QuickSearchOutput {
    pub(crate) results: Vec<QuickResult>,
}

fn quick_intent(query: &str) -> SearchIntent {
    SearchIntent::FileSearch(FileSearch {
        schema_version: SchemaVersion::V1,
        language: None,
        keywords: Some(vec![query.to_owned()]),
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
    })
}

pub(crate) async fn execute_quick_search(
    ctx: Arc<ServerCtx>,
    principal: Arc<AuthedPrincipal>,
    input: QuickSearchInput,
) -> Result<QuickSearchOutput, ToolError> {
    let q = input.query.trim();
    if q.is_empty() {
        return Ok(QuickSearchOutput {
            results: Vec::new(),
        });
    }

    let target_ids = resolve_target_ids(&ctx, &principal, input.collections.as_deref())?;
    let intent = quick_intent(q);
    let cancel = CancellationToken::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut merged: Vec<QuickResult> = Vec::new();

    for id in &target_ids {
        let Some(rt) = ctx.collection(id) else {
            continue;
        };
        let Some(tool) = rt
            .search_candidates
            .iter()
            .find(|t| t.capability().backend_kind == Some(BackendKind::NativeIndex))
        else {
            continue;
        };
        let Ok(stream) = tool.search(&intent, cancel.clone()).await else {
            continue;
        };
        let results: Vec<_> = stream.collect::<Vec<_>>().await;
        for r in results.into_iter().flatten() {
            let key = r.path.to_string_lossy().to_lowercase();
            if !seen.insert(key) {
                continue; // 跨来源命中同一文件，保留先入者
            }
            merged.push(QuickResult {
                path: scout_search_backend::user_facing_path(&r.path),
                name: r.name,
                collection: Some(id.clone()),
                source: "local",
                modified_time: r.metadata.modified_time.map(|t| t.to_rfc3339()),
                size_bytes: r.metadata.size_bytes,
            });
        }
    }

    if principal.admin {
        for r in query_native_file_index(&intent, cancel.clone()).await {
            let key = r.path.to_string_lossy().to_lowercase();
            if !seen.insert(key) {
                continue;
            }
            merged.push(QuickResult {
                path: scout_search_backend::user_facing_path(&r.path),
                name: r.name,
                collection: None,
                source: "native_file_index",
                modified_time: r.metadata.modified_time.map(|t| t.to_rfc3339()),
                size_bytes: r.metadata.size_bytes,
            });
        }
    }

    sort_by_match_tightness(&mut merged, q);
    merged.truncate(QUICK_LIMIT);
    Ok(QuickSearchOutput { results: merged })
}

#[cfg(windows)]
async fn query_native_file_index(
    intent: &SearchIntent,
    cancel: CancellationToken,
) -> Vec<scout_search_backend::SearchResult> {
    use scout_search_backend::SearchBackend as _;
    let Ok(backend) = scout_native_index::backend::NativeIndexBackend::new() else {
        return Vec::new();
    };
    if !backend.is_available() {
        return Vec::new();
    }
    let Ok(stream) = backend.search(intent, cancel).await else {
        return Vec::new();
    };
    stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .flatten()
        .collect()
}

#[cfg(not(windows))]
#[allow(clippy::unused_async)]
async fn query_native_file_index(
    _intent: &SearchIntent,
    _cancel: CancellationToken,
) -> Vec<scout_search_backend::SearchResult> {
    Vec::new()
}

/// 粗排：文件名与查询词相等 > 前缀匹配 > 其余子串匹配（镜像桌面端同名函数）。
fn sort_by_match_tightness(results: &mut [QuickResult], query: &str) {
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
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn result(name: &str) -> QuickResult {
        QuickResult {
            path: format!("/a/{name}"),
            name: name.to_owned(),
            collection: Some("default".to_owned()),
            source: "local",
            modified_time: None,
            size_bytes: None,
        }
    }

    #[test]
    fn sort_ranks_exact_then_prefix_then_substring() {
        let mut results = vec![
            result("预算说明.docx"),
            result("预算.docx"),
            result("2024预算.docx"),
        ];
        sort_by_match_tightness(&mut results, "预算");
        assert_eq!(
            results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["预算.docx", "预算说明.docx", "2024预算.docx"],
        );
    }

    #[tokio::test]
    async fn empty_query_short_circuits() {
        let ctx = crate::test_support::build_test_ctx_inmem();
        let principal = Arc::new(AuthedPrincipal {
            subject: "t".to_string(),
            grant: crate::collections::CollectionGrant::All,
            admin: true,
        });
        let out = execute_quick_search(
            ctx,
            principal,
            QuickSearchInput {
                query: "   ".to_string(),
                collections: None,
            },
        )
        .await
        .unwrap();
        assert!(out.results.is_empty());
    }
}
