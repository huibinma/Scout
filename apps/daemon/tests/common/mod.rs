//! T11 集成测试公共支撑：[`DaemonHandle`] 起真 axum server + 真 HTTP 监听端口，
//! 让 5 条 e2e 用 [`reqwest`] 走端到端 HTTP（vs 单测里走 `tower::ServiceExt`
//! `oneshot`）。
//!
//! 复用 `scout_server::test_support`：
//! - [`build_test_ctx_indexed`](scout_server::test_support::build_test_ctx_indexed)
//!   真打 `<data_dir>/index.db` + 真跑 `index_dirs_with_progress`、让 `SearchTool`
//!   端到端能命中 fixture 文件。
//! - [`StubEmbedder`](scout_server::test_support::StubEmbedder) 让 embed 路径
//!   不拉真模型 / 不依赖 GGUF。
//!
//! **生命周期**：`TempDir` 句柄保存在 `DaemonHandle._root` / `_data`，drop 时清
//! 临时目录；`_task` 是 `tokio::spawn` 的 axum serve future 句柄，handle drop
//! 后 task abort（reqwest client 也不再持连接）。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::doc_markdown,
    clippy::used_underscore_binding,
    dead_code
)]
// dead_code 允许：每个 e2e 测试只用 DaemonHandle 的部分字段，编译期会按 case 报
// 部分 unused（addr 必用、token 部分用、ctx 仅 reindex 用、_root/_data/_task 仅
// 持有不读）。doc_markdown 允许：test helper 文档里 `DaemonHandle._root` /
// `TempDir` 等是路径占位 / 类型名片段、加 ``` 反而难读。
// `used_underscore_binding` 允许：`_task` 同时表达「仅持有不读」（drop 自动
// abort 由 Drop impl 接管）+ 在 Drop 里读一下显式 abort 防 JoinHandle drop
// 默认不 abort，惯例不要改名（与 axum 0.8 / tokio 多 e2e 模板一致）。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use secrecy::SecretString;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::level_filters::LevelFilter;

use scout_server::app::build_app;
use scout_server::collections::{
    AuditConfig, CollectionConfig, DaemonConfigFile, SubjectKind, TokenConfig,
};
use scout_server::test_support::build_test_ctx_indexed;
use scout_server::{ServerConfig, ServerCtx};

/// 用于 e2e 的固定 bearer token（≥ 32 char，与 `preflight::check_token` 同款下限）。
pub const TEST_TOKEN: &str = "test-token-32-chars-minimum-length";
/// BETA-36 双 collection e2e：仅授权 case-a 的受限 token（subject=zhang.san）。
pub const TOKEN_ZHANG: &str = "zhang-token-32-chars-minimum-len";
/// BETA-36 双 collection e2e：全权 admin token（subject=ops）。
pub const TOKEN_OPS: &str = "ops-token-32-chars-minimum-lengt";

/// 真起的 daemon 句柄 —— 含监听地址 + token + tempdir + serve task。
pub struct DaemonHandle {
    /// axum 真 bind 的地址（127.0.0.1:<动态端口>）。
    pub addr: SocketAddr,
    /// 鉴权 bearer token（明文，仅 test）。
    pub token: String,
    /// 共享给 ctx 的 server 上下文 —— e2e_reindex_409 直接调 `state.write()`
    /// 设 `reindex_in_flight=true` 模拟并发竞态（与
    /// `reindex::tests::concurrent_reindex_second_returns_in_flight` 同款手法）。
    pub ctx: Arc<ServerCtx>,
    /// fixture 根目录 TempDir 句柄，drop 时清。
    pub _root: TempDir,
    /// 索引数据目录 TempDir 句柄，drop 时清。
    pub _data: TempDir,
    /// axum serve 的 spawn 句柄；drop handle 时 task abort、端口释放。
    pub _task: JoinHandle<()>,
}

impl DaemonHandle {
    /// 起一份带 fixture 的 daemon：写 corpus → 真索引 → bind 动态端口 → spawn
    /// axum serve → 等 ~10ms 让 listener 就绪。
    ///
    /// 模型路径用 `/dev/null` 占位：preflight 不在 build_app 路径上跑（preflight
    /// 是 main binary 启动期的检查、本 helper 跳过 daemon main 直接组 Router），
    /// 且 stub embedder 不读 model_path、对 ctx 字段无影响。
    pub async fn spawn_with_fixtures(corpus: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().expect("tempdir for root 应当能创建");
        let data = tempfile::tempdir().expect("tempdir for data 应当能创建");

        // BETA-36：legacy 形态合成 default collection + 全权 admin token（e2e 的
        // 单集合场景与 daemon --root/--token 启动等价）。
        let access = DaemonConfigFile::legacy_single_root(
            root.path().to_path_buf(),
            SecretString::from(TEST_TOKEN.to_owned()),
        );
        let config = ServerConfig {
            bind_addr: "127.0.0.1:0".parse().expect("常量 SocketAddr 应当能解析"),
            data_dir: data.path().to_path_buf(),
            model_path: std::path::PathBuf::from("/dev/null"),
            log_level: LevelFilter::OFF,
            semantic_weight: scout_server::tools::search::DEFAULT_SEMANTIC_WEIGHT,
            cosine_threshold: scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD,
            embed_images: true,
            match_mode: scout_search_backend::MatchMode::All,
            access,
        };

        // corpus 映射到 default collection。
        let corpus_tagged: Vec<(&str, &str, &str)> = corpus
            .iter()
            .map(|(name, body)| ("default", *name, *body))
            .collect();
        let ctx = build_test_ctx_indexed(config, &corpus_tagged);
        Self::serve_ctx(ctx, TEST_TOKEN, root, data).await
    }

    /// 构造完整 `http://<addr><path>` URL，省 e2e 重复拼。
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// BETA-36 e2e：起双 collection daemon——`case-a`（**只读**，含
    /// `contract-a.txt`："blueharbor" 语料）+ `case-b`（读写，含
    /// `notes-b.txt`："payroll" 语料）；token 两枚：zhang.san 仅 case-a、
    /// ops 全权 admin。`self.token` = ops。
    pub async fn spawn_two_collections() -> Self {
        let root = tempfile::tempdir().expect("tempdir for root 应当能创建");
        let data = tempfile::tempdir().expect("tempdir for data 应当能创建");

        // BETA-43：case-a 禁全文（read_document 仅片段模式）、case-b 允许全文。
        let mk_col =
            |id: &str, sub: SubjectKind, read_only: bool, allow_full_read: bool, dir: &str| {
                CollectionConfig {
                    id: id.to_string(),
                    display_name: None,
                    subject_kind: sub,
                    roots: vec![root.path().join(dir)],
                    read_only,
                    audit_tags: Vec::new(),
                    allow_full_read,
                }
            };
        let access = DaemonConfigFile {
            collections: vec![
                mk_col("case-a", SubjectKind::Case, true, false, "a"),
                mk_col("case-b", SubjectKind::Employee, false, true, "b"),
            ],
            tokens: vec![
                TokenConfig {
                    token: SecretString::from(TOKEN_ZHANG.to_owned()),
                    subject: "zhang.san".to_string(),
                    collections: vec!["case-a".to_string()],
                    admin: false,
                },
                TokenConfig {
                    token: SecretString::from(TOKEN_OPS.to_owned()),
                    subject: "ops".to_string(),
                    collections: vec!["*".to_string()],
                    admin: true,
                },
            ],
            audit: AuditConfig::default(),
        };
        let config = ServerConfig {
            bind_addr: "127.0.0.1:0".parse().expect("常量 SocketAddr 应当能解析"),
            data_dir: data.path().to_path_buf(),
            model_path: std::path::PathBuf::from("/dev/null"),
            log_level: LevelFilter::OFF,
            semantic_weight: scout_server::tools::search::DEFAULT_SEMANTIC_WEIGHT,
            cosine_threshold: scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD,
            embed_images: true,
            match_mode: scout_search_backend::MatchMode::All,
            access,
        };
        let corpus: Vec<(&str, &str, &str)> = vec![
            (
                "case-a",
                "contract-a.txt",
                "blueharbor supply contract dispute evidence",
            ),
            (
                "case-b",
                "notes-b.txt",
                "payroll ledger offboarding handover notes",
            ),
        ];
        let ctx = build_test_ctx_indexed(config, &corpus);
        Self::serve_ctx(ctx, TOKEN_OPS, root, data).await
    }

    /// 共用装配尾段：bind 动态端口 + spawn axum serve。
    async fn serve_ctx(ctx: Arc<ServerCtx>, token: &str, root: TempDir, data: TempDir) -> Self {
        let app = build_app(ctx.clone());
        let listener = TcpListener::bind(ctx.config.bind_addr)
            .await
            .expect("bind 127.0.0.1:0 应当能成功");
        let addr = listener
            .local_addr()
            .expect("已 bind 的 listener 必有 addr");
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        Self {
            addr,
            token: token.to_owned(),
            ctx,
            _root: root,
            _data: data,
            _task: task,
        }
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        // 显式 abort serve task；JoinHandle drop 不自动 abort（tokio doc 明示），
        // 不主动 abort 会让 listener 端口在 TempDir 已清后仍占着、并发 e2e 端口
        // 资源耗尽。
        self._task.abort();
    }
}
