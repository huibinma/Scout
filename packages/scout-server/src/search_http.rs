//! BETA-78：`POST /search` + `POST /search/quick`——除 `/mcp` 外新增的 REST
//! 检索入口，供桌面瘦客户端用（不需要走 MCP JSON-RPC framing，直接普通
//! JSON request/response）。两个 handler 分别薄薄包一层
//! [`crate::tools::search::execute_search`] / [`crate::quick_search::execute_quick_search`]，
//! 与 `search` MCP tool / 桌面既有 `quick_search` 共用同一份核心逻辑——避免
//! REST 与 MCP 两条路径各自维护一份 intent 解析/融合/排序代码而漂移。
//!
//! 错误映射（[`ToolError`] → HTTP status）：`InvalidParams` → 400、`Denied` →
//! 403、`Internal` → 500（body 不含内部错误细节，PRIVACY CONTRACT 同
//! `reindex.rs`——可能含本机绝对路径，只进 tracing log）。

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, Extension, Json};
use futures_util::StreamExt as _;
use scout_search_backend::{BackendKind, CancellationToken, ExpandedSearchIntent, SearchResult};
use serde::{Deserialize, Serialize};

use crate::auth::AuthedPrincipal;
use crate::config::ServerCtx;
use crate::quick_search::{execute_quick_search, QuickSearchInput, QuickSearchOutput};
use crate::tools::search::{execute_search, resolve_target_ids, SearchInput, SearchOutput};
use crate::tools::ToolError;

fn map_tool_error(e: ToolError) -> StatusCode {
    match e {
        ToolError::InvalidParams(_) => StatusCode::BAD_REQUEST,
        ToolError::Denied(_) => StatusCode::FORBIDDEN,
        ToolError::Internal(msg) => {
            tracing::error!(error = %msg, "search REST handler internal error");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// `POST /search`：完整 NL intent 解析 + fallback/fanout + rank 全链路（与
/// `search` MCP tool 同一实现）。
pub(crate) async fn search(
    State(ctx): State<Arc<ServerCtx>>,
    Extension(principal): Extension<Arc<AuthedPrincipal>>,
    Json(input): Json<SearchInput>,
) -> Result<Json<SearchOutput>, StatusCode> {
    execute_search(ctx, principal, input)
        .await
        .map(Json)
        .map_err(map_tool_error)
}

/// `POST /search/quick`：跳过 NL 解析的精简检索（桌面快速查找下拉专用）。
pub(crate) async fn quick(
    State(ctx): State<Arc<ServerCtx>>,
    Extension(principal): Extension<Arc<AuthedPrincipal>>,
    Json(input): Json<QuickSearchInput>,
) -> Result<Json<QuickSearchOutput>, StatusCode> {
    execute_quick_search(ctx, principal, input)
        .await
        .map(Json)
        .map_err(map_tool_error)
}

/// `POST /backend/search` 请求体（BETA-78）。
#[derive(Deserialize)]
pub(crate) struct BackendSearchInput {
    /// `"search.local"` | `"search.semantic"` | `"search.native_file_index"`。
    pub(crate) tool_id: String,
    pub(crate) expanded: ExpandedSearchIntent,
    /// 目标 collection；缺省 = token 授权的第一个（个人模式恒单 collection）。
    #[serde(default)]
    pub(crate) collection: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct BackendSearchOutput {
    pub(crate) results: Vec<SearchResult>,
}

/// `POST /backend/search`（BETA-78）：桌面瘦客户端专用的**单 backend**检索代理——
/// 桌面保留自己完整的本地 harness 管线（policy / refine / 同义词扩展 / 能力感知
/// 路由 / fan-out / tracer 事件全部不变，见 `apps/desktop/src-tauri/src/search.rs`
/// 文档），只是三个原本在桌面进程内直接跑的 backend（`search.local`/
/// `search.semantic`/`search.native_file_index`）现在改为经这个端点向 scoutd 借
/// 一次 `SearchBackend::search_expanded()` 调用——这正是把索引/原生 MFT 访问挪进
/// 特权服务、同时不动桌面既有产品体验的关键接口，比让桌面直接调用完整的
/// `/search`（会丢弃 refine/adhoc 同义词/多类型均衡等桌面独有能力）更贴合实际
/// 需求。**不做 collection 信息墙以外的额外过滤**——桌面只在个人模式单
/// collection 下使用本端点。
///
/// `search.native_file_index` 是卷范围、不属于任何 collection——同 `/search/quick`
/// 的安全边界，只有 admin token 能触达。
pub(crate) async fn backend_search(
    State(ctx): State<Arc<ServerCtx>>,
    Extension(principal): Extension<Arc<AuthedPrincipal>>,
    Json(input): Json<BackendSearchInput>,
) -> Result<Json<BackendSearchOutput>, StatusCode> {
    execute_backend_search(ctx, principal, input)
        .await
        .map(Json)
        .map_err(map_tool_error)
}

async fn execute_backend_search(
    ctx: Arc<ServerCtx>,
    principal: Arc<AuthedPrincipal>,
    input: BackendSearchInput,
) -> Result<BackendSearchOutput, ToolError> {
    let cancel = CancellationToken::new();

    if input.tool_id == "search.native_file_index" {
        if !principal.admin {
            return Err(ToolError::Denied("search.native_file_index".to_string()));
        }
        return Ok(BackendSearchOutput {
            results: query_native_file_index(&input.expanded, cancel).await,
        });
    }

    let backend_kind = match input.tool_id.as_str() {
        "search.local" => BackendKind::NativeIndex,
        "search.semantic" => BackendKind::SemanticIndex,
        other => {
            return Err(ToolError::InvalidParams(format!(
                "未知 backend id: {other}"
            )))
        }
    };

    let requested = input.collection.as_ref().map(std::slice::from_ref);
    let target_ids = resolve_target_ids(&ctx, &principal, requested)?;
    for id in &target_ids {
        let Some(rt) = ctx.collection(id) else {
            continue;
        };
        let Some(tool) = rt
            .search_candidates
            .iter()
            .find(|t| t.capability().backend_kind == Some(backend_kind))
        else {
            continue;
        };
        let stream = tool
            .search_expanded(&input.expanded, cancel)
            .await
            .map_err(|e| ToolError::Internal(e.to_string()))?;
        let results: Vec<_> = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flatten()
            .collect();
        return Ok(BackendSearchOutput { results });
    }
    Ok(BackendSearchOutput {
        results: Vec::new(),
    })
}

#[cfg(windows)]
async fn query_native_file_index(
    expanded: &ExpandedSearchIntent,
    cancel: CancellationToken,
) -> Vec<SearchResult> {
    use scout_search_backend::SearchBackend as _;
    let Ok(backend) = scout_native_index::backend::NativeIndexBackend::new() else {
        return Vec::new();
    };
    if !backend.is_available() {
        return Vec::new();
    }
    let Ok(stream) = backend.search_expanded(expanded, cancel).await else {
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
    _expanded: &ExpandedSearchIntent,
    _cancel: CancellationToken,
) -> Vec<SearchResult> {
    Vec::new()
}
