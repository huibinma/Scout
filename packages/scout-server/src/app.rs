//! `axum::Router` 工厂 —— 把 MCP transport 与 admin REST 装配成一个 Router。
//!
//! BETA-32 T8：daemon binary（T9 / T10）与未来嵌入式都通过 [`build_app`] 拿到同一份
//! axum app；本 mod 只负责拼装路由 + 套 bearer middleware：
//!
//! - `GET /health` —— 无鉴权（spec §5.1）
//! - `POST /admin/reindex` —— bearer 鉴权
//! - `POST /mcp` —— bearer 鉴权（rmcp 1.8 streamable-HTTP transport，`nest_service` 进来）
//!
//! **streamable-HTTP 装配**：rmcp 1.8 用 `StreamableHttpService::new(factory, session_mgr, config)`
//! 返一个实现了 `tower::Service` 的服务（不是 `axum::Router`），通过
//! `axum::Router::new().nest_service("/mcp", service)` 挂载。`factory` 必须返一个
//! 实现了 `ServerHandler` 的实例，每个会话调用一次；BETA-32 daemon 是有状态长生命
//! 周期 ctx，把 [`ScoutMcpHandler`] 包 `Arc` 后 clone 给 factory 闭包，工厂每次
//! 返一份 `Arc<ScoutMcpHandler>`（rmcp 1.8 为 `Arc<T: ServerHandler>` 提供了
//! blanket `Service` 适配，见 `handler/server.rs::impl_server_handler_for_wrapper!`）。

use std::future::Future;
use std::sync::Arc;

use anyhow::Context as _;
use axum::{
    middleware::from_fn_with_state,
    routing::{get, post},
    Router,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use tokio::net::TcpListener;

use crate::admin::{admin_audit, admin_audit_report, admin_reindex, health};
use crate::auth::require_bearer;
use crate::config::ServerCtx;
use crate::mcp::ScoutMcpHandler;

/// 组装 axum `Router`。
///
/// 公共子树：`/health` 不鉴权；
/// 保护子树：`/admin/*` 与 `/mcp` 套 bearer middleware。
pub fn build_app(ctx: Arc<ServerCtx>) -> Router {
    let auth_ctx = ctx.auth_ctx();

    // ---- /mcp：rmcp 1.8 streamable-HTTP transport ----
    let mcp_handler = Arc::new(ScoutMcpHandler::new(ctx.clone()));
    let session_mgr = Arc::new(LocalSessionManager::default());
    // rmcp 1.8 streamable-HTTP 配置（参考 `~/.cargo/registry/.../rmcp-1.8.*/src/transport/
    // streamable_http_server/tower.rs:1055-1280`）：
    // - `stateful_mode=false`：BETA-32 follow-up ⑤ 切 stateless（2026-06-29）。原 stateful
    //   保 session state 节省 client 重复 initialize / tools/list；但 stateless 模式下
    //   响应走纯 JSON framing 简化 e2e client、且 LLM client（Claude Code / Codex / Cline）
    //   普遍能 cache initialize / tools/list 跨 request，stateless 实际客户体验损失小。
    //   trade-off：每个连接 client 需自带 protocol-version header；session-id header 不再发。
    // - `json_response=true`：stateless 模式下生效（tower.rs：stateless 分支用 JSON framing、
    //   stateful 分支强制 SSE）。`Content-Type: application/json` 响应、单 request 单 body、
    //   无 SSE event stream framing。
    // - `sse_keep_alive=None`：显式禁用 SSE keep-alive ping（stateless 模式纯 JSON 也无 SSE）。
    let mcp_config = StreamableHttpServerConfig::default()
        .with_stateful_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None);
    let mcp_service: StreamableHttpService<Arc<ScoutMcpHandler>, LocalSessionManager> =
        StreamableHttpService::new(
            {
                let handler = mcp_handler.clone();
                move || Ok(handler.clone())
            },
            session_mgr,
            mcp_config,
        );

    let mcp_router: Router<Arc<ServerCtx>> = Router::new()
        .nest_service("/mcp", mcp_service)
        .route_layer(from_fn_with_state(auth_ctx.clone(), require_bearer));

    // ---- /admin/*：bearer 保护（handler 内另有 admin 标志门 → 403）----
    let admin_router: Router<Arc<ServerCtx>> = Router::new()
        .route("/admin/reindex", post(admin_reindex))
        .route("/admin/audit", get(admin_audit))
        .route("/admin/audit/report", get(admin_audit_report))
        .route_layer(from_fn_with_state(auth_ctx, require_bearer));

    // ---- /health：无鉴权 ----
    let public: Router<Arc<ServerCtx>> = Router::new().route("/health", get(health));

    Router::new()
        .merge(public)
        .merge(admin_router)
        .merge(mcp_router)
        .with_state(ctx)
}

/// 在**已绑定**的 listener 上跑 server 直到 `shutdown` future 就绪（BETA-53 桌面内嵌用）。
///
/// 与 daemon 的 `lifecycle::serve` 分工：daemon 走信号（SIGINT/SIGTERM）关停、且自己 bind；
/// 桌面内嵌由调用方先 `TcpListener::bind`（**同步拿到端口占用错误**反馈给 UI）、再把 listener
/// 交给本函数，关停由外部 `shutdown` future 驱动（如 `oneshot` 接收端）。axum 收尾已 in-flight
/// 请求后返回，listener 随之 drop、端口即时可复用。
///
/// # Errors
///
/// axum server 异常退出时返回 `Err`（正常 graceful shutdown 返回 `Ok`）。
pub async fn serve_bound(
    listener: TcpListener,
    ctx: Arc<ServerCtx>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let router = build_app(ctx);
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .context("axum server 异常退出")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::test_support::build_test_ctx_inmem;
    use axum::body::Body;
    use axum::http::{header::AUTHORIZATION, Request, StatusCode};
    use tower::ServiceExt;

    /// `/health` 不需要 Authorization header（spec §5.1 探活端点）。
    #[tokio::test]
    async fn health_no_auth_required() {
        let ctx = build_test_ctx_inmem();
        let app = build_app(ctx);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// `/admin/reindex` 无 Authorization → 401。
    #[tokio::test]
    async fn admin_reindex_requires_auth() {
        let ctx = build_test_ctx_inmem();
        let app = build_app(ctx);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/reindex")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// `/admin/reindex` 带正确 token → 200（stub `run_full_reindex` 返 `doc_count=0`）。
    #[tokio::test]
    async fn admin_reindex_with_correct_token_succeeds() {
        // build_test_ctx_inmem 用 `SecretString::from("test-token")`，对应 Bearer test-token。
        let ctx = build_test_ctx_inmem();
        let app = build_app(ctx);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/reindex")
                    .header(AUTHORIZATION, "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// `/mcp` 无 Authorization → 401。spec §6.2：MCP endpoint 全程鉴权；
    /// regression test 锁住 `route_layer` 透过 `nest_service` 套用的行为
    /// （reviewer Important #2）。
    #[tokio::test]
    async fn mcp_endpoint_requires_auth() {
        let ctx = build_test_ctx_inmem();
        let app = build_app(ctx);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// 极简 HTTP/1.1 客户端（`Connection: close` → server 应答后关连接，`read_to_string`
    /// 读到 EOF 返回）。走裸 `std::net::TcpStream`，避免为一个测试引 reqwest/hyper dev-dep。
    fn http_request(addr: std::net::SocketAddr, raw: &str) -> String {
        use std::io::{Read as _, Write as _};
        let mut stream = std::net::TcpStream::connect(addr).expect("连接 server 失败");
        stream.write_all(raw.as_bytes()).expect("写请求失败");
        stream.flush().ok();
        let mut buf = String::new();
        stream.read_to_string(&mut buf).expect("读响应失败");
        buf
    }

    /// BETA-53 桌面内嵌路径端到端：`serve_bound` 真 bind ephemeral 端口后——
    /// ① `/health` 无鉴权返 200；② `/mcp` 无 token 返 401（鉴权确实透过 bound serve 生效）；
    /// ③ 触发 shutdown 后 server task 及时返回 Ok（优雅关停不 hang）。
    /// 锁住桌面开关依赖的「起服务 → 真应答 → 关停」这条链路。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serve_bound_answers_then_shuts_down_gracefully() {
        let ctx = build_test_ctx_inmem();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(serve_bound(listener, ctx, async move {
            let _ = rx.await;
        }));

        // ① /health 无鉴权 → 200
        let health = tokio::task::spawn_blocking(move || {
            http_request(
                addr,
                "GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
            )
        })
        .await
        .unwrap();
        assert!(
            health.starts_with("HTTP/1.1 200"),
            "/health 应无鉴权返 200，实得响应首行：{}",
            health.lines().next().unwrap_or("")
        );

        // ② /mcp 无 Authorization → 401
        let mcp = tokio::task::spawn_blocking(move || {
            http_request(
                addr,
                "POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
                 Content-Length: 2\r\nConnection: close\r\n\r\n{}",
            )
        })
        .await
        .unwrap();
        assert!(
            mcp.contains(" 401 "),
            "/mcp 无 token 应返 401，实得响应首行：{}",
            mcp.lines().next().unwrap_or("")
        );

        // ③ shutdown → server task 5s 内正常返回 Ok（证明 graceful shutdown 生效、不 hang）
        tx.send(()).unwrap();
        let joined = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("shutdown 应在 5s 内完成、不 hang");
        joined.unwrap().expect("serve_bound 应正常返回 Ok");
    }
}
