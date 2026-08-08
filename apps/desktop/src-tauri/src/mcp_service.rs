//! BETA-53：桌面内嵌「本机 MCP 服务」。
//!
//! 复用桌面已加载的 embedder（[`crate::search::embedding_model::EmbeddingModelHandle`]，
//! 实现 `TextEmbedder`）+ **只读挂载**桌面自己的 `index.db`
//! （[`scout_server::ServerCtx::attach_readonly`]，零重索引、语义白送），起一个只绑
//! `127.0.0.1` 的 axum server，把 hybrid 检索（search / read_document / list_collections）
//! 经 MCP streamable-HTTP 暴露给本机 LLM 客户端（Claude Code / Codex）。
//!
//! 内嵌复用桌面检索栈，非起子进程 daemon。
//!
//! 安全红线（设计 §5，不可省）：
//! 1. **只绑 `127.0.0.1`**——绝不 `0.0.0.0` / 局域网（局域网暴露留 v2 + 强警告）。
//! 2. **随机 token 必填**——本机任何持 token 的进程才能搜 / 读；重置令牌即踢掉旧连接。
//! 3. **暴露面知情**——启用即「本机任何拿到 token 的进程可搜索并读取被索引文件内容」，UI 讲清。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use secrecy::SecretString;
use serde::Serialize;
use tauri::State;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex};
use tracing::{info, level_filters::LevelFilter, warn};

use scout_indexer::embed::TextEmbedder;
use scout_server::app::serve_bound;
use scout_server::collections::{DaemonConfigFile, LEGACY_COLLECTION_ID};
use scout_server::config::ServerConfig;
use scout_server::ServerCtx;

use crate::search::IndexStatus;
use crate::settings::AppSettings;

/// 固定监听端口（设计 §7 拍板 **8766**，避开 daemon 惯用 8765；端口自定义留 v2）。
pub const MCP_SERVICE_PORT: u16 = 8766;
/// token 随机字节数（→ 64 hex 字符，远超 server 侧 `MIN_TOKEN_LEN=32` 的下限）。
const TOKEN_RANDOM_BYTES: usize = 32;
/// 认可的最短既存 token 长度（低于此视为损坏 / 旧格式，重新生成）。
const MIN_ACCEPTED_TOKEN_LEN: usize = 32;
/// 端口占用（`AddrInUse`）时的最大重试次数——覆盖 app 重启时旧实例 socket 落停的短窗口。
const BIND_MAX_RETRIES: u32 = 6;
/// 每次重试前的等待（总窗口 ≈ `BIND_MAX_RETRIES` × 此值 ≈ 3s）。
const BIND_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// 绑定监听端口，端口占用（`AddrInUse` / Windows `os error 10048`）时有界重试。
///
/// **Why**：single-instance 插件的进程锁**先于** 8766 的 OS socket 释放——app 重启时新实例
/// 已过单例闸、但旧实例的 listener 还没落停，首绑就撞 `AddrInUse`。旧逻辑「一次 bind 失败
/// 即告警放弃」会让重启后 MCP 服务**静默死掉**（`/health` 拒连），直到用户手动开关或再重启；
/// 本机真机实锤（Codex 连不上退回手搓 grep）。有界重试等旧 listener 收尾即可自愈；仍失败
/// （真有另一活实例长期占用）则如实返回，不无限重试。其余错误立即上抛。
async fn bind_with_retry(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let mut attempt = 0u32;
    loop {
        match TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && attempt < BIND_MAX_RETRIES => {
                attempt += 1;
                warn!(%addr, attempt, "MCP 端口被占用，等待旧监听释放后重试");
                tokio::time::sleep(BIND_RETRY_DELAY).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// 运行中的服务句柄（内部）。
struct RunningService {
    /// 关停信号发送端（`send(())` → axum graceful shutdown）。
    shutdown_tx: oneshot::Sender<()>,
    /// server task 句柄（stop 时 `await` 确保 listener 释放、端口即时可复用）。
    join: tauri::async_runtime::JoinHandle<()>,
    /// 启动时快照的索引文档数（供 UI 显示「已挂载 N 篇」）。
    doc_count: u64,
    /// 启动时语义臂是否可用（false = FTS-only 降级，构建未开 semantic-recall 或模型缺失）。
    semantic: bool,
}

/// 桌面「本机 MCP 服务」managed 状态（以 `Arc` 交给 Tauri manage，便于自动启动任务共享）。
pub struct McpServiceState {
    /// 复用桌面已加载 embedder（构 `attach_readonly` ctx；避免二次加载 GGUF）。
    embedder: Arc<dyn TextEmbedder>,
    /// Scout 数据目录（`index.db` / `audit.jsonl` 所在；`attach_readonly` 的 `data_dir`）。
    data_dir: PathBuf,
    /// settings.json 路径（读写开关态 + token；读生效 roots / semantic_weight）。
    settings_path: Option<PathBuf>,
    /// 桌面 FTS 索引状态，供 MCP search 给出“结果可能不完整”提示。
    indexing_status: Arc<std::sync::Mutex<IndexStatus>>,
    /// 当前运行态（`None` = 已停）。
    running: Mutex<Option<RunningService>>,
}

/// 对外状态（前端 McpPane 渲染）。
#[derive(Debug, Clone, Serialize)]
pub struct McpServiceStatus {
    /// 服务是否正在监听。
    pub running: bool,
    /// 持久化的开关意图（下次启动 app 是否自动拉起）。
    pub enabled: bool,
    /// 监听地址（`127.0.0.1:8766`）。
    pub address: String,
    /// MCP endpoint URL（客户端 `mcpServers.url` 配置用）。
    pub url: String,
    /// 当前 bearer token（尚未生成时 `None`）。
    pub token: Option<String>,
    /// 运行中时索引文档数（`None` = 未运行）。
    pub doc_count: Option<u64>,
    /// 运行中时语义臂是否可用（`None` = 未运行；`Some(false)` = FTS-only）。
    pub semantic: Option<bool>,
}

/// 生成随机 bearer token（OS CSPRNG，32 字节 → 64 小写 hex 字符）。
fn generate_token() -> Result<String, String> {
    let mut buf = [0u8; TOKEN_RANDOM_BYTES];
    getrandom::getrandom(&mut buf)
        .map_err(|e| format!("生成随机 token 失败（OS 熵源不可用）: {e}"))?;
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(TOKEN_RANDOM_BYTES * 2);
    for b in buf {
        let _ = write!(hex, "{b:02x}");
    }
    Ok(hex)
}

/// 桌面当前生效的索引根（自定义 roots + 可选系统默认三夹），供 `list_collections` 如实展示。
fn effective_roots(settings: &AppSettings) -> Vec<PathBuf> {
    crate::settings::resolve_index_roots_tagged(
        &settings.index_roots,
        settings.include_system_defaults,
    )
    .into_iter()
    .map(|(p, _)| p)
    .collect()
}

impl McpServiceState {
    /// 构造 managed 状态。`embedder` 传桌面进程级单例句柄（`Arc<EmbeddingModelHandle>`
    /// 自动 coerce 为 `Arc<dyn TextEmbedder>`）；`data_dir` = [`crate::scout_data_dir`]。
    #[must_use]
    pub fn new(
        embedder: Arc<dyn TextEmbedder>,
        data_dir: PathBuf,
        settings_path: Option<PathBuf>,
        indexing_status: Arc<std::sync::Mutex<IndexStatus>>,
    ) -> Self {
        Self {
            embedder,
            data_dir,
            settings_path,
            indexing_status,
            running: Mutex::new(None),
        }
    }

    /// 把 `AppSettings` 落盘（持久化开关态 / token）。
    fn persist(&self, settings: &AppSettings) -> Result<(), String> {
        let path = self
            .settings_path
            .as_ref()
            .ok_or("settings.json 路径不可用，无法持久化本机 MCP 服务配置")?;
        crate::settings::write_settings(path, settings)
    }

    /// 组装对外状态（持锁调用，避免 running 态与返回值不一致）。
    fn status_locked(
        &self,
        running: &Option<RunningService>,
        settings: &AppSettings,
    ) -> McpServiceStatus {
        let address = format!("127.0.0.1:{MCP_SERVICE_PORT}");
        McpServiceStatus {
            running: running.is_some(),
            enabled: settings.mcp_service_enabled,
            url: format!("http://{address}/mcp"),
            address,
            token: settings.mcp_service_token.clone(),
            doc_count: running.as_ref().map(|s| s.doc_count),
            semantic: running.as_ref().map(|s| s.semantic),
        }
    }

    /// 启动服务（幂等：已运行则直接返回当前状态）。持久化 `enabled=true` + token。
    ///
    /// # Errors
    ///
    /// 挂载索引失败 / 端口被占用 / token 生成失败 / settings 持久化失败时返回 `Err`。
    pub async fn start(&self) -> Result<McpServiceStatus, String> {
        let mut guard = self.running.lock().await;
        if guard.is_some() {
            // 已在运行：幂等返回，不重复 bind。
            let settings = crate::settings::read_settings_or_default(&self.settings_path);
            return Ok(self.status_locked(&guard, &settings));
        }

        // ---- 取 / 生成 token + 置 enabled，一次性持久化 ----
        let mut settings = crate::settings::read_settings_or_default(&self.settings_path);
        let token = match &settings.mcp_service_token {
            Some(t) if t.len() >= MIN_ACCEPTED_TOKEN_LEN => t.clone(),
            _ => {
                let t = generate_token()?;
                settings.mcp_service_token = Some(t.clone());
                t
            }
        };
        settings.mcp_service_enabled = true;
        self.persist(&settings)?;

        // ---- 只读挂载桌面 index.db 构 ctx（放 spawn_blocking：attach 会跑 embed("ping")
        //      探针，语义构建下可能阻塞 load 模型）----
        let data_dir = self.data_dir.clone();
        let embedder = self.embedder.clone();
        let roots = effective_roots(&settings);
        let semantic_weight = crate::settings::resolve_semantic_weight(settings.semantic_weight);
        // 复用与桌面内搜索同一份「模型感知」阈值解析——MCP 服务这里挂的就是同一个 embedder
        // 句柄（`self.embedder`，构造时传入桌面进程级 `EmbeddingModelHandle`），按它真实解析出
        // 的 `model_id()` 查表，阈值口径必须一致，不能各算各的。
        let cosine_threshold =
            crate::search::embedding_model::cosine_threshold_for_model_id(embedder.model_id());
        let embed_images = settings.enable_image_semantics;
        let match_all_conditions = settings.search_match_all_conditions;
        let token_secret = SecretString::from(token.clone());

        let (mut ctx, semantic) =
            tauri::async_runtime::spawn_blocking(move || -> Result<(ServerCtx, bool), String> {
                // 语义臂探针：与 attach_readonly 内部同口径（stub / 模型缺失 → FTS-only）。
                let semantic = embedder.embed("ping").is_ok();
                let access = DaemonConfigFile::personal_local(roots, token_secret);
                let config = ServerConfig {
                    bind_addr: SocketAddr::from(([127, 0, 0, 1], MCP_SERVICE_PORT)),
                    data_dir,
                    // attach_readonly 不加载模型（embedder 已注入），model_path 不参与运行期。
                    model_path: PathBuf::new(),
                    log_level: LevelFilter::WARN,
                    semantic_weight,
                    cosine_threshold,
                    embed_images,
                    // 2026-07-20：与桌面 search_impl 同口径读同一份全局设置，MCP 检索的复合
                    // 条件匹配行为与桌面内搜索一致。
                    match_mode: if match_all_conditions {
                        scout_search_backend::MatchMode::All
                    } else {
                        scout_search_backend::MatchMode::Any
                    },
                    access,
                };
                let ctx = ServerCtx::attach_readonly(config, embedder)
                    .map_err(|e| format!("挂载本机索引失败: {e:#}"))?;
                Ok((ctx, semantic))
            })
            .await
            .map_err(|e| format!("构建本机 MCP 服务上下文任务失败: {e}"))??;

        let indexing_status = Arc::clone(&self.indexing_status);
        ctx.indexing_probe = Arc::new(move || {
            indexing_status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .indexing
        });

        let ctx = Arc::new(ctx);
        let doc_count = ctx
            .collection(LEGACY_COLLECTION_ID)
            .map_or(0, |c| c.state.read().doc_count);

        // ---- bind（同步拿端口占用错误反馈 UI）+ spawn server task ----
        // AddrInUse 有界重试：app 重启时旧实例 socket 未落停会撞 os 10048（见 bind_with_retry）。
        let addr = SocketAddr::from(([127, 0, 0, 1], MCP_SERVICE_PORT));
        let listener = bind_with_retry(addr)
            .await
            .map_err(|e| format!("绑定 {addr} 失败（端口可能被占用，或已有另一实例在跑）: {e}"))?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let join = tauri::async_runtime::spawn(async move {
            let shutdown = async move {
                let _ = shutdown_rx.await;
            };
            if let Err(e) = serve_bound(listener, ctx, shutdown).await {
                warn!(error = %format!("{e:#}"), "本机 MCP 服务异常退出");
            }
        });

        info!(%addr, doc_count, semantic, "本机 MCP 服务已启动（只读挂载桌面索引）");
        *guard = Some(RunningService {
            shutdown_tx,
            join,
            doc_count,
            semantic,
        });
        Ok(self.status_locked(&guard, &settings))
    }

    /// 停止服务（幂等）。持久化 `enabled=false`。
    ///
    /// # Errors
    ///
    /// settings 持久化失败时返回 `Err`（服务本身已停）。
    pub async fn stop(&self) -> Result<McpServiceStatus, String> {
        let mut guard = self.running.lock().await;
        if let Some(svc) = guard.take() {
            let _ = svc.shutdown_tx.send(());
            // 等 server task 收尾——listener drop 后端口即时可复用（避免快速 off→on 撞占用）。
            let _ = svc.join.await;
            info!("本机 MCP 服务已停止");
        }
        let mut settings = crate::settings::read_settings_or_default(&self.settings_path);
        settings.mcp_service_enabled = false;
        self.persist(&settings)?;
        Ok(self.status_locked(&guard, &settings))
    }

    /// 重置令牌：先停服务（踢掉持旧 token 的连接，设计 §5.2），换新 token 持久化，
    /// **若重置前服务在运行则自动以新 token 重启**——旧 token 再连即 401、新 token 立即 200，
    /// 免去用户手动重新启用。重置前已停则仅换 token、保持停止态（下次启用即用新 token）。
    ///
    /// # Errors
    ///
    /// token 生成 / settings 持久化 / 重启（挂载或绑定）失败时返回 `Err`。
    pub async fn reset_token(&self) -> Result<McpServiceStatus, String> {
        // 记录重置前的运行态——决定换 token 后是否自动重启。
        let was_running = self.running.lock().await.is_some();

        // 先停服务：drop listener + 结束 server task，踢掉所有持旧 token 的连接（设计 §5.2）。
        self.stop().await?;

        // 换新 token 并落盘（stop 已把 enabled 置 false；下面若重启会重新置 true）。
        let mut settings = crate::settings::read_settings_or_default(&self.settings_path);
        settings.mcp_service_token = Some(generate_token()?);
        self.persist(&settings)?;

        if was_running {
            // 原本在跑：自动重启——start() 复用刚落盘的新 token（len≥32 不再重生），新 token 即时生效。
            return self.start().await;
        }

        let guard = self.running.lock().await;
        Ok(self.status_locked(&guard, &settings))
    }

    /// 当前状态（只读，不改任何持久态）。
    pub async fn status(&self) -> McpServiceStatus {
        let guard = self.running.lock().await;
        let settings = crate::settings::read_settings_or_default(&self.settings_path);
        self.status_locked(&guard, &settings)
    }
}

// ---- Tauri 命令（薄封装；`State` 持 `Arc<McpServiceState>`）----

/// 启用并启动本机 MCP 服务。
#[tauri::command]
pub async fn start_mcp_service(
    state: State<'_, Arc<McpServiceState>>,
) -> Result<McpServiceStatus, String> {
    state.start().await
}

/// 停止本机 MCP 服务。
#[tauri::command]
pub async fn stop_mcp_service(
    state: State<'_, Arc<McpServiceState>>,
) -> Result<McpServiceStatus, String> {
    state.stop().await
}

/// 查询本机 MCP 服务状态。
#[tauri::command]
pub async fn mcp_service_status(
    state: State<'_, Arc<McpServiceState>>,
) -> Result<McpServiceStatus, String> {
    Ok(state.status().await)
}

/// 重置 bearer token（踢掉旧连接，需重新启用）。
#[tauri::command]
pub async fn reset_mcp_token(
    state: State<'_, Arc<McpServiceState>>,
) -> Result<McpServiceStatus, String> {
    state.reset_token().await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::field_reassign_with_default)]

    use super::*;

    /// token 为 64 小写 hex 字符、两次生成不相同（随机性 sanity）。
    #[test]
    fn generate_token_is_64_hex_and_unique() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_eq!(a.len(), TOKEN_RANDOM_BYTES * 2, "32 字节 → 64 hex 字符");
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert!(a.len() >= MIN_ACCEPTED_TOKEN_LEN);
        assert_ne!(a, b, "两次生成应不同（随机）");
    }

    /// `bind_with_retry` 自愈守卫：端口先被占、短暂后释放 → 重试窗口内应绑定成功，
    /// 而非旧逻辑的「首绑失败即放弃」。坐实 app 重启端口竞态可自愈。
    #[tokio::test]
    async fn bind_with_retry_recovers_after_port_frees() {
        // 高位端口避开 8766 真机服务 + 其他测试。
        let addr: SocketAddr = "127.0.0.1:18766".parse().unwrap();
        let holder = TcpListener::bind(addr).await.expect("测试端口应空闲");
        // 后台 ~700ms 后释放（落在 6×500ms 重试窗口内）。
        let releaser = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            drop(holder);
        });
        let bound = bind_with_retry(addr).await;
        releaser.await.unwrap();
        assert!(bound.is_ok(), "端口释放后重试应绑定成功: {bound:?}");
    }

    /// effective_roots 反映 index_roots + include_system_defaults（复用 settings 解析）。
    #[test]
    fn effective_roots_reflects_custom_roots() {
        let mut s = AppSettings::default();
        s.index_roots = vec!["/tmp/a".to_string(), "/tmp/b".to_string()];
        s.include_system_defaults = false;
        let roots = effective_roots(&s);
        assert_eq!(
            roots,
            vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]
        );
    }

    /// BETA-53 修复守卫：auto-start / start() 经 `persist()` 写入的 token 与 enabled，
    /// 必须能被 `status()` 从**同一** settings 文件读回——运行态（`status.token`）与持久态
    /// （磁盘 `settings.mcp_service_token`）不得分叉。
    #[test]
    fn status_reads_token_from_same_file_persist_writes() {
        let dir = std::env::temp_dir().join(format!("scout-mcpstatus-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let settings_path = dir.join("settings.json");
        let state = McpServiceState::new(
            Arc::new(crate::search::embedding_model::EmbeddingModelHandle::new(
                None,
                PathBuf::from("/tmp/x"),
            )),
            PathBuf::from("/tmp/data"),
            Some(settings_path.clone()),
            Arc::new(std::sync::Mutex::new(IndexStatus::default())),
        );

        // 模拟 auto-start / start() 的持久化：置 enabled + token 落盘（与 start 走同一 persist）。
        let mut settings = AppSettings::default();
        settings.mcp_service_enabled = true;
        settings.mcp_service_token = Some("b".repeat(64));
        state.persist(&settings).unwrap();

        // status() 从 settings_path 读——必须与 persist 写的是同一文件、token 一致。
        let st = tauri::async_runtime::block_on(state.status());
        assert!(st.enabled);
        assert_eq!(st.token.as_deref(), Some("b".repeat(64).as_str()));

        // 分叉守卫：磁盘 settings.mcp_service_token 与运行态 status.token 一致。
        let on_disk = crate::settings::read_settings_or_default(&Some(settings_path));
        assert_eq!(on_disk.mcp_service_token, st.token);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 未运行时 status_locked：running=false、doc_count/semantic 为 None、url 含 8766。
    #[test]
    fn status_locked_when_stopped() {
        let state = McpServiceState::new(
            Arc::new(crate::search::embedding_model::EmbeddingModelHandle::new(
                None,
                PathBuf::from("/tmp/x"),
            )),
            PathBuf::from("/tmp/data"),
            None,
            Arc::new(std::sync::Mutex::new(IndexStatus::default())),
        );
        let mut settings = AppSettings::default();
        settings.mcp_service_token = Some("t".repeat(64));
        let st = state.status_locked(&None, &settings);
        assert!(!st.running);
        assert!(!st.enabled);
        assert_eq!(st.doc_count, None);
        assert_eq!(st.semantic, None);
        assert_eq!(st.url, "http://127.0.0.1:8766/mcp");
        assert_eq!(st.token.as_deref(), Some("t".repeat(64).as_str()));
    }

    /// 停止态下重置：换出新 token（64 hex，与旧值不同）、落盘、保持 stopped/enabled=false。
    /// 覆盖 `reset_token` 的「原本已停 → 仅换 token」分支（不绑端口）。
    #[tokio::test]
    async fn reset_token_when_stopped_rotates_and_stays_stopped() {
        let dir = std::env::temp_dir().join(format!("scout-mcpreset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("settings.json");
        let old = "a".repeat(64);
        std::fs::write(&f, format!(r#"{{"mcp_service_token":"{old}"}}"#)).unwrap();

        let state = McpServiceState::new(
            Arc::new(crate::search::embedding_model::EmbeddingModelHandle::new(
                None,
                PathBuf::from("/tmp/x"),
            )),
            PathBuf::from("/tmp/data"),
            Some(f.clone()),
            Arc::new(std::sync::Mutex::new(IndexStatus::default())),
        );

        let st = state.reset_token().await.unwrap();
        assert!(!st.running, "重置后保持停止态");
        assert!(!st.enabled, "重置后 enabled=false");
        let new = st.token.expect("重置应生成新 token");
        assert_eq!(new.len(), 64, "新 token 为 64 hex 字符");
        assert!(new
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(new, old, "新旧 token 不同");

        // 落盘校验：磁盘上的 token = 返回的新 token。
        let persisted = crate::settings::read_settings_or_default(&Some(f));
        assert_eq!(persisted.mcp_service_token.as_deref(), Some(new.as_str()));

        std::fs::remove_dir_all(&dir).ok();
    }
}
