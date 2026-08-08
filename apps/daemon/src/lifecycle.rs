//! daemon 生命周期：bind + serve + 信号处理。
//!
//! [`serve`] 拿到组装好的 [`axum::Router`] 后真 bind `bind_addr` 并启 axum
//! server；信号到来时走 `with_graceful_shutdown` 让 axum 收尾已 in-flight 请求
//! 再退出。
//!
//! 信号策略：
//! - SIGINT（Ctrl+C）—— 所有平台都支持，交互运行的 daemon 由此停。
//! - SIGTERM —— 仅 `unix` 注册（Windows 走 `ctrl_c` 即可），由容器 / `launchctl` /
//!   `systemd` 等 supervisor 发送。
//!
//! bind 端口冲突由 `TcpListener::bind` 的 `Result` 直接 surface 出去（exit
//! code 非零、stderr 含 `anyhow` chain），preflight 不重复 try（避免 TOCTOU）。

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use tokio::net::TcpListener;
use tokio::signal;
use tracing::{error, info, warn};

use scout_server::ServerCtx;

/// 启动 axum server 并阻塞直到关闭信号到达。
///
/// `ctx` 持有以便扩展（例如关闭前 flush 状态），当前仅做日志锚点。
pub async fn serve(ctx: Arc<ServerCtx>, router: Router) -> Result<()> {
    let bind_addr = ctx.config.bind_addr;
    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("绑定监听地址失败：{bind_addr}"))?;
    info!(%bind_addr, "scoutd 监听就绪");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum server 异常退出")?;
    info!("scoutd 已退出");
    Ok(())
}

/// 等待 SIGINT 或 SIGTERM（`unix`）/ `ctrl_c`（`windows`）。
///
/// 用 `tokio::select!` 同时挂两路 future、任一到来即返回，让上层
/// `with_graceful_shutdown` 触发 axum 收尾。
///
/// reviewer I-4：注册失败不再 `.expect()` panic（daemon 已 serve traffic、
/// panic abort runtime 会丢 in-flight requests）。退化为 `tracing::error!` +
/// 永久 `pending()`：那一路 future 永不 ready、另一路 signal 仍能触发关闭。
/// 极端情况两路都 fail 时 daemon 不会因关闭逻辑 panic 而崩、要靠 supervisor
/// kill -9——但比丢请求好。
async fn shutdown_signal() {
    let ctrl_c = async {
        match signal::ctrl_c().await {
            Ok(()) => {}
            Err(e) => {
                error!(error = %e, "注册 SIGINT 失败、退化为永久等待（仅依赖 SIGTERM / 强制 kill）");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                error!(error = %e, "注册 SIGTERM 失败、退化为仅响应 SIGINT");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => warn!("收到 SIGINT，准备退出"),
        () = terminate => warn!("收到 SIGTERM，准备退出"),
    }
}
