//! HTTP admin endpoints —— `/health`（无鉴权）+ `/admin/reindex`（bearer + admin 标志）。
//!
//! BETA-36：admin 端点在 bearer 鉴权（401）之上加 **admin 标志门**——token 合法但
//! `admin=false` → **403**（验收 ④ 的 REST 侧路径）；reindex 支持
//! `?collection=<id>` 指名单集合。

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::audit::{AuditAction, AuditRecord};
use crate::auth::AuthedPrincipal;
use crate::config::ServerCtx;
use crate::reindex::{trigger_reindex, ReindexError};

/// admin 标志门：非 admin token → 403 + audit denied 留痕（验收 ④ REST 侧）。
fn require_admin(
    ctx: &ServerCtx,
    principal: &AuthedPrincipal,
    endpoint: &str,
) -> Result<(), StatusCode> {
    if principal.admin {
        return Ok(());
    }
    tracing::warn!(subject = %principal.subject, endpoint, "非 admin token 调用 admin 端点，拒绝（403）");
    ctx.audit.record(
        &AuditRecord::now(&principal.subject, AuditAction::Denied, Vec::new())
            .with_denied_reason(&format!("admin endpoint {endpoint}")),
    );
    Err(StatusCode::FORBIDDEN)
}

/// `/health` 响应体。
#[derive(Debug, Serialize)]
pub struct HealthResp {
    /// 健康标识；恒为 `"ok"`。
    pub status: &'static str,
    /// 当前 crate 版本（编译期注入）。
    pub version: &'static str,
}

/// `/health`：无鉴权探活端点。
#[allow(clippy::unused_async)] // axum handler 签名要求 async。
pub async fn health() -> Json<HealthResp> {
    Json(HealthResp {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

/// `/admin/reindex` 查询参数。
#[derive(Debug, Deserialize)]
pub struct ReindexParams {
    /// 指名重建的 collection id；省略 = 全部非只读集合。
    pub collection: Option<String>,
}

/// `/admin/reindex` 响应体。
#[derive(Debug, Serialize)]
pub struct ReindexResp {
    /// 状态串；成功恒为 `"completed"`。
    pub status: &'static str,
    /// 本次实际重建的 collection id 列表。
    pub collections: Vec<String>,
    /// 本次 reindex 后目标集合文档数合计。
    pub doc_count: u64,
    /// 本次 reindex 耗时（毫秒）。
    pub duration_ms: u128,
}

/// `/admin/reindex`：触发 reindex；同一集合同时只允许一个在跑。
///
/// 错误映射：
/// - 非 admin token → HTTP 403 Forbidden（验收 ④）
/// - [`ReindexError::UnknownCollection`] → HTTP 404
/// - [`ReindexError::ReadOnly`] / [`ReindexError::InFlight`] → HTTP 409 Conflict
/// - 其它 → HTTP 500
pub async fn admin_reindex(
    State(ctx): State<Arc<ServerCtx>>,
    Extension(principal): Extension<Arc<AuthedPrincipal>>,
    Query(params): Query<ReindexParams>,
) -> Result<Json<ReindexResp>, StatusCode> {
    require_admin(&ctx, &principal, "/admin/reindex")?;
    match trigger_reindex(ctx.clone(), params.collection.as_deref()).await {
        Ok(r) => {
            ctx.audit.record(&AuditRecord::now(
                &principal.subject,
                AuditAction::Reindex,
                r.collections.clone(),
            ));
            Ok(Json(r))
        }
        Err(ReindexError::UnknownCollection(_)) => Err(StatusCode::NOT_FOUND),
        Err(ReindexError::ReadOnly(_) | ReindexError::InFlight(_)) => Err(StatusCode::CONFLICT),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// `/admin/audit` 查询参数。
#[derive(Debug, Deserialize)]
pub struct AuditQueryParams {
    /// 返回最近 N 条（缺省 100、上限 1000）。
    pub tail: Option<usize>,
}

/// `GET /admin/audit`：读取最近 N 条 audit 记录（取证导出入口，admin token 专用）。
pub async fn admin_audit(
    State(ctx): State<Arc<ServerCtx>>,
    Extension(principal): Extension<Arc<AuthedPrincipal>>,
    Query(params): Query<AuditQueryParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_admin(&ctx, &principal, "/admin/audit")?;
    let tail = params.tail.unwrap_or(100).min(1000);
    let entries = ctx.audit.read_tail(tail).map_err(|e| {
        tracing::error!(error = %e, "读取 audit 文件失败");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(json!({ "entries": entries })))
}

/// `/admin/audit/report` 查询参数（BETA-43 验收 ③）。
#[derive(Debug, Deserialize)]
pub struct AuditReportParams {
    /// `md`（缺省）或 `csv`。
    pub format: Option<String>,
    /// 精确匹配 subject。
    pub subject: Option<String>,
    /// 记录涉及此 collection id。
    pub collection: Option<String>,
    /// 时间下界：RFC 3339 或 `YYYY-MM-DD`（当日 00:00 UTC）。
    pub from: Option<String>,
    /// 时间上界：RFC 3339 或 `YYYY-MM-DD`（当日 23:59:59 UTC，含当日）。
    pub to: Option<String>,
}

/// `GET /admin/audit/report`：`audit.jsonl` → 人读合规报告（md / csv），
/// 按 subject / collection / 时间范围过滤——客户合规人员不必自行 parse jsonl。
/// 参数不合法（未知 format / 坏时间）→ 400。
pub async fn admin_audit_report(
    State(ctx): State<Arc<ServerCtx>>,
    Extension(principal): Extension<Arc<AuthedPrincipal>>,
    Query(params): Query<AuditReportParams>,
) -> Result<axum::response::Response, StatusCode> {
    use axum::response::IntoResponse;

    use crate::audit_report::{
        filter_entries, parse_time_param, render_csv, render_markdown, ReportFilter, ReportFormat,
    };

    require_admin(&ctx, &principal, "/admin/audit/report")?;
    let format = ReportFormat::parse(params.format.as_deref()).ok_or(StatusCode::BAD_REQUEST)?;
    let parse_bound = |s: &Option<String>, end_of_day: bool| match s {
        // 外层 Option = 参数是否给出；内层 Option = 解析是否成功（失败 → 400）。
        Some(raw) => parse_time_param(raw, end_of_day).map(Some).ok_or(()),
        None => Ok(None),
    };
    let filter = ReportFilter {
        subject: params.subject.clone(),
        collection: params.collection.clone(),
        from: parse_bound(&params.from, false).map_err(|()| StatusCode::BAD_REQUEST)?,
        to: parse_bound(&params.to, true).map_err(|()| StatusCode::BAD_REQUEST)?,
    };
    let entries = ctx.audit.read_all().map_err(|e| {
        tracing::error!(error = %e, "读取 audit 文件失败");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let filtered = filter_entries(entries, &filter);
    let (content_type, body) = match format {
        ReportFormat::Markdown => (
            "text/markdown; charset=utf-8",
            render_markdown(&filtered, &filter),
        ),
        ReportFormat::Csv => ("text/csv; charset=utf-8", render_csv(&filtered)),
    };
    Ok(([(axum::http::header::CONTENT_TYPE, content_type)], body).into_response())
}

/// `GET /admin/status` 单 collection 状态（BETA-78：desktop 瘦客户端轮询用，
/// 取代此前桌面本地 `status.rs`/`index_status.rs` 直接探测本地 backend 的做法）。
#[derive(Debug, Serialize)]
pub struct CollectionStatus {
    pub id: String,
    pub display_name: String,
    pub doc_count: u64,
    pub indexed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub reindex_in_flight: bool,
}

/// `GET /admin/status` 响应体。
#[derive(Debug, Serialize)]
pub struct StatusResp {
    pub collections: Vec<CollectionStatus>,
    /// 语义臂是否就绪（embedder 非 stub 且 `embed("ping")` 成功）。
    pub semantic_ready: bool,
    /// Windows 原生文件名索引（MFT/USN）是否可用——服务以 `LocalSystem` 常驻时
    /// 应为 `true`；非 Windows 或探测失败恒 `false`。
    pub native_file_index_available: bool,
    pub indexing_in_progress: bool,
}

/// `GET /admin/status`：鉴权即可读（不需要 admin 标志——desktop 瘦客户端用
/// 普通个人 token 轮询这个端点展示索引进度/后端可用性，团队部署下非 admin
/// token 只能看到自己被授权的 collection，不泄漏未授权集合信息）。
pub async fn admin_status(
    State(ctx): State<Arc<ServerCtx>>,
    Extension(principal): Extension<Arc<AuthedPrincipal>>,
) -> Json<StatusResp> {
    let collections = ctx
        .collections
        .values()
        .filter(|rt| principal.can_access(&rt.meta.id))
        .map(|rt| {
            let state = rt.state.read();
            CollectionStatus {
                id: rt.meta.id.clone(),
                display_name: rt.meta.display_name().to_string(),
                doc_count: state.doc_count,
                indexed_at: state.indexed_at,
                reindex_in_flight: state.reindex_in_flight,
            }
        })
        .collect();

    Json(StatusResp {
        collections,
        semantic_ready: ctx.embedder.embed("ping").is_ok(),
        native_file_index_available: native_file_index_available(),
        indexing_in_progress: ctx.indexing_in_progress(),
    })
}

#[cfg(windows)]
fn native_file_index_available() -> bool {
    scout_native_index::native_index_available()
}

#[cfg(not(windows))]
fn native_file_index_available() -> bool {
    false
}

/// `POST /admin/personal/roots` 请求体。
#[derive(Debug, Deserialize)]
pub struct UpdateRootsParams {
    pub roots: Vec<String>,
}

/// `POST /admin/personal/roots` 响应体。
#[derive(Debug, Serialize)]
pub struct UpdateRootsResp {
    pub status: &'static str,
    pub restart_required: bool,
}

/// `POST /admin/personal/roots`（BETA-78）：个人模式便捷端点——改
/// `default` collection 的索引根目录并持久化回 `config.toml`。**v1 简化**：
/// 只落盘，不做实时局部 reindex——`CollectionRuntime.meta.roots` 目前是
/// 构造期定值（无 `RwLock` 包裹），本进程内没有安全的运行时热更新路径；新
/// roots 在下次服务重启（Windows Service `AutoStart`，随系统或手动
/// `sc stop/start Scoutd` 生效）后随正常 reindex 流程生效。桌面客户端据
/// `restart_required` 提示用户。
///
/// 仅接受单 collection（`default`，个人模式约定）——多 collection（团队模式）
/// 场景改根目录属于运维改配置文件的范畴，不经这个便捷端点。
pub async fn admin_update_personal_roots(
    State(ctx): State<Arc<ServerCtx>>,
    Extension(principal): Extension<Arc<AuthedPrincipal>>,
    Json(params): Json<UpdateRootsParams>,
) -> Result<Json<UpdateRootsResp>, StatusCode> {
    require_admin(&ctx, &principal, "/admin/personal/roots")?;

    if ctx.collections.len() != 1 {
        tracing::warn!(
            collections = ctx.collections.len(),
            "/admin/personal/roots 仅支持单 collection 个人模式部署，本部署非个人模式"
        );
        return Err(StatusCode::CONFLICT);
    }
    let Some(rt) = ctx.collection(crate::collections::LEGACY_COLLECTION_ID) else {
        return Err(StatusCode::CONFLICT);
    };
    if rt.meta.read_only {
        return Err(StatusCode::CONFLICT);
    }

    let roots: Vec<std::path::PathBuf> = params.roots.into_iter().map(Into::into).collect();
    if roots.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let config_path = ctx.config.data_dir.join("config.toml");
    crate::collections::rewrite_personal_roots(&config_path, &roots).map_err(|e| {
        tracing::error!(error = %e, "重写个人模式配置 roots 失败");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(UpdateRootsResp {
        status: "saved",
        restart_required: true,
    }))
}
