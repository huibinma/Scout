//! BETA-78：desktop 连接后台 `scoutd` 的 HTTP client。
//!
//! 桌面自身不再碰索引/原生 MFT API——`search.local`（本地 FTS）、`search.semantic`
//! （语义召回）、`search.native_file_index`（Windows MFT/USN，读取需要
//! `LocalSystem` 权限）三个 [`scout_search_backend::SearchBackend`] 改由
//! [`RemoteSearchBackend`] 经 `POST /backend/search` 向 scoutd 借一次
//! `search_expanded()` 调用。桌面既有的 harness 管线（policy / refine / 同义词
//! 扩展 / 能力感知路由 / fan-out / tracer 事件，见 `search.rs`）**完全不动**——
//! 只是这三个 backend 的真正执行位置从桌面进程挪到了服务进程，产品体验零回归。
//!
//! **发现机制**：scoutd 每次成功 bind 后覆写
//! `<ProgramData>\Scout\scoutd\connection.json`（见
//! `apps/daemon/src/personal.rs::write_connection_file`，字段形状必须与本模块的
//! [`ConnectionFile`] 保持一致，改动需两边同步）——本模块只读这个文件，不需要
//! 端口猜测或额外发现协议。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use scout_search_backend::{
    BackendKind, BackendSearchFuture, CancellationToken, ExpandedSearchIntent,
    ImplementationStatus, SearchBackend, SearchError, SearchIntent, SearchResult,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// scoutd 个人模式数据目录：`%ProgramData%\Scout\scoutd`（镜像
/// `apps/daemon/src/personal.rs::default_data_dir` 的 Windows 分支——桌面不能
/// 反向依赖 `daemon` binary crate，常量在此重复一份，改动需两边同步）。
fn scoutd_data_dir() -> PathBuf {
    if cfg!(windows) {
        let program_data =
            std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".to_string());
        PathBuf::from(program_data).join("Scout").join("scoutd")
    } else {
        std::env::temp_dir().join("scout-personal")
    }
}

fn connection_file_path() -> PathBuf {
    scoutd_data_dir().join("connection.json")
}

/// `connection.json` 的形状（与 `apps/daemon/src/personal.rs::ConnectionFile` 对应）。
#[derive(Debug, Deserialize)]
struct ConnectionFile {
    bind: SocketAddr,
    token: String,
}

/// 到 scoutd 的连接：发现 + 健康态缓存 + `/backend/search` 代理。
/// `Arc` 常驻 Tauri managed state，三个 `RemoteSearchBackend` 共享同一份连接态。
#[derive(Debug)]
pub struct ServiceConnection {
    client: reqwest::Client,
    base_url: RwLock<Option<String>>,
    token: RwLock<Option<String>>,
    connected: AtomicBool,
}

impl ServiceConnection {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base_url: RwLock::new(None),
            token: RwLock::new(None),
            connected: AtomicBool::new(false),
        })
    }

    /// 读 `connection.json`；找到就更新 base_url/token（返回 `true`），
    /// 找不到/解析失败则保持上一次已知值不变（返回 `false`）——服务重启期间
    /// 短暂拿不到文件不应该把已经用过的凭据清空。
    fn discover(&self) -> bool {
        let path = connection_file_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "未找到 scoutd connection.json（服务可能未安装/未启动）");
                return false;
            }
        };
        match serde_json::from_str::<ConnectionFile>(&text) {
            Ok(cf) => {
                *self.base_url.write().unwrap_or_else(|e| e.into_inner()) =
                    Some(format!("http://{}", cf.bind));
                *self.token.write().unwrap_or_else(|e| e.into_inner()) = Some(cf.token);
                true
            }
            Err(e) => {
                warn!(error = %e, path = %path.display(), "connection.json 解析失败");
                false
            }
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// 健康探测：必要时先 discover，再 `GET /health`；更新 `connected` 缓存并返回。
    pub async fn health_check(&self) -> bool {
        let has_conn = self
            .base_url
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        if !has_conn {
            self.discover();
        }
        let base = self
            .base_url
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(base) = base else {
            self.connected.store(false, Ordering::Relaxed);
            return false;
        };
        let ok = self
            .client
            .get(format!("{base}/health"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success());
        self.connected.store(ok, Ordering::Relaxed);
        ok
    }

    /// 后台健康探测循环：每 `interval` 探测一次；服务刚装好/重启后自动重连，
    /// 用户不需要重启桌面。fire-and-forget，随进程退出而终止。
    pub fn spawn_health_loop(self: &Arc<Self>, interval: Duration) {
        let conn = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            loop {
                let was = conn.is_connected();
                let now = conn.health_check().await;
                if was != now {
                    info!(connected = now, "scoutd 连接状态变化");
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    async fn backend_search(
        &self,
        tool_id: &'static str,
        expanded: &ExpandedSearchIntent,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let base = self
            .base_url
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| SearchError::BackendUnavailable {
                reason: "scoutd 未连接（服务未安装或未启动）".to_string(),
            })?;
        let token = self
            .token
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default();

        #[derive(Serialize)]
        struct Req<'a> {
            tool_id: &'a str,
            expanded: &'a ExpandedSearchIntent,
        }
        #[derive(Deserialize)]
        struct Resp {
            results: Vec<SearchResult>,
        }

        let resp = self
            .client
            .post(format!("{base}/backend/search"))
            .bearer_auth(token)
            .json(&Req { tool_id, expanded })
            .send()
            .await
            .map_err(|e| {
                self.connected.store(false, Ordering::Relaxed);
                SearchError::BackendUnavailable {
                    reason: e.to_string(),
                }
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            return Err(SearchError::BackendUnavailable {
                reason: format!("scoutd 返回 {status}"),
            });
        }
        self.connected.store(true, Ordering::Relaxed);
        resp.json::<Resp>()
            .await
            .map(|r| r.results)
            .map_err(|e| SearchError::Io {
                detail: e.to_string(),
            })
    }
}

/// 实现 [`SearchBackend`]：桌面既有 harness 管线（policy/refine/同义词/路由/
/// fan-out/tracer，见 `search.rs`）完全不变，只是这一个 backend 的真正执行
/// 挪到 scoutd 进程。
///
/// `similarity_floor`：语义相似度下限过滤——原本在桌面自建的 `SemanticIndexBackend`
/// 内部执行（构造时注入 `floor_provider` 闭包）；backend 挪到服务端后，服务端并
/// 不知道桌面用户在设置页配的这个个性化下限（scoutd 是无 settings.json 的共享
/// 服务进程，`/backend/search` 也不接受这个参数），因此改为在这里、拿到结果之后
/// 本地 filter——语义仍是"低于下限的语义命中不进入后续融合/展示"，行为与旧版
/// 一致，只是过滤点从"服务端 backend 内部"挪到"客户端拿到结果之后"。只对
/// `BackendKind::SemanticIndex` 生效；`None` = 不过滤（`search.local`/
/// `search.native_file_index` 用）。
pub struct RemoteSearchBackend {
    tool_id: &'static str,
    kind: BackendKind,
    conn: Arc<ServiceConnection>,
    similarity_floor: Option<Arc<dyn Fn() -> f32 + Send + Sync>>,
}

impl std::fmt::Debug for RemoteSearchBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteSearchBackend")
            .field("tool_id", &self.tool_id)
            .field("kind", &self.kind)
            .field("has_similarity_floor", &self.similarity_floor.is_some())
            .finish_non_exhaustive()
    }
}

impl RemoteSearchBackend {
    #[must_use]
    pub fn new(tool_id: &'static str, kind: BackendKind, conn: Arc<ServiceConnection>) -> Self {
        Self {
            tool_id,
            kind,
            conn,
            similarity_floor: None,
        }
    }

    /// 注入相似度下限 provider（`search.semantic` 用，main.rs 传
    /// `settings::read_similarity_floor` 闭包，每次查询 live-read）。
    #[must_use]
    pub fn with_similarity_floor(mut self, provider: Arc<dyn Fn() -> f32 + Send + Sync>) -> Self {
        self.similarity_floor = Some(provider);
        self
    }

    fn filter_by_floor(&self, results: Vec<SearchResult>) -> Vec<SearchResult> {
        let Some(provider) = &self.similarity_floor else {
            return results;
        };
        let floor = provider();
        results
            .into_iter()
            .filter(|r| {
                r.source != BackendKind::SemanticIndex || r.score.unwrap_or(0.0) >= f64::from(floor)
            })
            .collect()
    }
}

/// 供 UI 展示"是否已连接到后台服务"（设置页 / 托盘状态用）。
#[tauri::command]
pub fn service_connection_status(conn: tauri::State<'_, Arc<ServiceConnection>>) -> bool {
    conn.is_connected()
}

impl SearchBackend for RemoteSearchBackend {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn implementation_status(&self) -> ImplementationStatus {
        ImplementationStatus::Real
    }

    fn is_available(&self) -> bool {
        self.conn.is_connected()
    }

    fn search<'a>(
        &'a self,
        intent: &'a SearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        let expanded = ExpandedSearchIntent::identity(intent.clone());
        Box::pin(async move {
            let results = self.conn.backend_search(self.tool_id, &expanded).await?;
            Ok(scout_search_backend::backend_stream_from_results(
                self.filter_by_floor(results),
                cancel,
            ))
        })
    }

    fn search_expanded<'a>(
        &'a self,
        expanded: &'a ExpandedSearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        Box::pin(async move {
            let results = self.conn.backend_search(self.tool_id, expanded).await?;
            Ok(scout_search_backend::backend_stream_from_results(
                self.filter_by_floor(results),
                cancel,
            ))
        })
    }
}
