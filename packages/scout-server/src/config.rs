//! Server 配置 + 共享上下文。
//!
//! BETA-32 T4：`ServerConfig` 承载启动参数（由 CLI / TOML / env 合并得出），
//! `ServerCtx` 是运行时依赖容器（注入到所有 tools / handlers）。
//!
//! BETA-36 重构：单根单钥升级为 **collection 模型**——`ServerConfig.access` 承载
//! `[[collections]]`/`[[tokens]]`/`[audit]`（TOML 或 legacy 合成，见
//! [`crate::collections`]）；`ServerCtx.collections` 每个归档集合一份
//! [`CollectionRuntime`]（独立 index.db + 候选链缓存 + 运行时状态——物理信息墙）。

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::level_filters::LevelFilter;

use parking_lot::{Mutex, RwLock};

use scout_harness::SearchableTool;
use scout_indexer::embed::TextEmbedder;
use scout_indexer::{DocumentIndex, MusicIndex};

use crate::auth::AuthCtx;
use crate::collections::{CollectionConfig, DaemonConfigFile, LEGACY_COLLECTION_ID};

/// 索引活动探针：由 desktop / daemon 注入，默认 false，避免 server 感知上层状态实现。
pub type IndexingProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// 默认空闲探针，保持未注入场景的历史行为。
#[must_use]
pub fn idle_indexing_probe() -> IndexingProbe {
    Arc::new(|| false)
}

/// 启动参数 — 由 CLI / TOML / env 合并填充。
#[derive(Debug)]
pub struct ServerConfig {
    /// HTTP 监听地址（含端口）。
    pub bind_addr: SocketAddr,
    /// 索引 / 缓存 / audit 等 server 数据目录。
    pub data_dir: PathBuf,
    /// 模型文件路径（embedding gguf 等）。
    pub model_path: PathBuf,
    /// 日志级别过滤。
    pub log_level: LevelFilter,
    /// hybrid RRF 融合中语义臂的权重（daemon 无 settings.json，由 CLI
    /// `--semantic-weight` 注入；默认镜像桌面
    /// `scout_result_normalizer::DEFAULT_SEMANTIC_WEIGHT`）。企业评测
    /// （BETA-40 收尾）用它 A/B 不同权重下 FTS 精确命中 vs 语义召回的排位。
    pub semantic_weight: f64,
    /// 2026-07-28：hybrid RRF 融合语义臂 cosine 路由阈值——按当前生效 embedding 模型
    /// （`derive_model_id(model_path)`）查已校准阈值表
    /// （[`scout_result_normalizer::cosine_threshold_for_model`]）算出，daemon 无
    /// settings.json、由启动期一次性解析注入（未收录模型回落
    /// `DEFAULT_COSINE_ROUTING_THRESHOLD` 并告警）。阈值与模型版本结构化绑定，取代此前
    /// `run_fanout_merge_rrf` 内部写死的全局常量——换模型忘补校准记录时可观测，而非静默
    /// 沿用上一代模型的旧阈值。
    pub cosine_threshold: f64,
    /// OCR 图片文本是否入语义索引（BETA-39 双层质量门槛沿用）。**daemon 默认
    /// true**——企业冷归档场景检索者不熟悉语料、图片证据（凭证/截图/现场照片）
    /// 是三场景共同需求，且 2 字 CJK 词 FTS 结构性不可达、语义臂是图片内容唯一
    /// 兜底（评估见当时的评审记录 §5）。
    /// 桌面端维持 opt-in 默认关，两侧策略独立。CLI `--disable-image-semantics`
    /// 关闭；启动期 purge 按本开关镜像桌面语义（关 → 清全部图片向量）。
    pub embed_images: bool,
    /// 2026-07-20：多个复合检索条件（关键词组）之间的匹配模式，daemon 无 settings.json、
    /// 由 CLI `--match-any-condition` 一次性注入（默认 `MatchMode::All`，严格全部命中，
    /// 与桌面端默认口径一致；取代 BETA-57 旧版自动 OR 兜底）。四个检索后端统一读取。
    pub match_mode: scout_search_backend::MatchMode,
    /// 已校验的 collections / tokens / audit 配置（TOML 解析或 legacy 合成）。
    pub access: DaemonConfigFile,
}

/// 一个 collection 的运行时状态（BETA-36：原全局 `RuntimeState` 按 collection 拆分）。
#[derive(Debug, Default)]
pub struct CollectionState {
    /// 最近一次 indexing 完成的时间。
    pub indexed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 当前索引内文档总数。
    pub doc_count: u64,
    /// reindex 是否在跑（用作互斥锁，防并发触发）。
    pub reindex_in_flight: bool,
}

/// 一个 collection 的运行时容器：元信息 + 独立 index.db + 候选链缓存 + 状态。
///
/// `music_index` / `document_index` 用 `Mutex` 包：内部含裸 `rusqlite::Connection`
/// （`!Sync`），加 `parking_lot::Mutex` 是让容器 `Sync` 成立的最小代价（rusqlite
/// 单连接自带 `SQLite` mutex、并发 query 本就要排队，业务语义无损）。
pub struct CollectionRuntime {
    /// 配置元信息（显示名 / 归档主体 / roots / 只读态 / 审计标签）。
    pub meta: CollectionConfig,
    /// 本 collection 的 index.db 路径（[`collection_db_path`] 布局）。
    pub db_path: PathBuf,
    /// 音乐索引句柄。
    pub music_index: Arc<Mutex<MusicIndex>>,
    /// 文档索引句柄。
    pub document_index: Arc<Mutex<DocumentIndex>>,
    /// search 候选链缓存：启动时构造一次、每次 invoke `Arc::clone` 复用
    /// （BETA-32 T6 #6 节奏不变，粒度从全局改 per-collection）。
    pub search_candidates: Arc<Vec<Arc<dyn SearchableTool>>>,
    /// `运行时状态（indexed_at` / `doc_count` / reindex 互斥）。
    pub state: Arc<RwLock<CollectionState>>,
}

impl std::fmt::Debug for CollectionRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollectionRuntime")
            .field("meta", &self.meta)
            .field("db_path", &self.db_path)
            .field(
                "search_candidates",
                &format!("Arc<Vec<{}-tool(s)>>", self.search_candidates.len()),
            )
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// collection 的 index.db 布局：legacy `default` 沿用 `<data_dir>/index.db`
/// （现有部署零迁移），其余走 `<data_dir>/collections/<id>/index.db`。
///
/// id 字符集由 [`crate::collections::is_valid_collection_id`] 在配置校验时把守，
/// 拼路径无注入面。
#[must_use]
pub fn collection_db_path(data_dir: &Path, collection_id: &str) -> PathBuf {
    if collection_id == LEGACY_COLLECTION_ID {
        data_dir.join("index.db")
    } else {
        data_dir
            .join("collections")
            .join(collection_id)
            .join("index.db")
    }
}

/// 运行时依赖容器 — 注入到所有 tools / handlers。
///
/// 通过 `Arc<ServerCtx>` 在 axum State 中传递；含 `Arc<dyn TextEmbedder>` trait object，
/// 因此手动实现 `Debug`。
pub struct ServerCtx {
    /// 启动配置（含 access 权限模型）。
    pub config: ServerConfig,
    /// embedder（语义臂；stub 时 FTS-only 降级）。
    pub embedder: Arc<dyn TextEmbedder>,
    /// 全部 collection 运行时，key = collection id（`BTreeMap` 保证遍历顺序稳定）。
    pub collections: BTreeMap<String, CollectionRuntime>,
    /// 检索留痕 sink（`<data_dir>/audit.jsonl`，BETA-36 验收 ③④）。
    pub audit: Arc<crate::audit::AuditSink>,
    /// FTS 索引是否正在进行；未注入时恒为 false。
    pub indexing_probe: IndexingProbe,
}

impl std::fmt::Debug for ServerCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerCtx")
            .field("config", &self.config)
            .field("embedder_model_id", &self.embedder.model_id())
            .field("collections", &self.collections)
            .field("audit", &self.audit)
            .field("indexing_probe", &"<probe>")
            .field("indexing_in_progress", &self.indexing_in_progress())
            .finish()
    }
}

impl ServerCtx {
    /// 只读挂载已建索引构造 `ServerCtx`（桌面内嵌本机 MCP 服务用；设计见当时的评审记录 S1）。
    ///
    /// 与 daemon 的 `build_runtime_ctx` 的**唯一区别**：**不跑首次全量索引**——
    /// 索引由外部（桌面后台调度）维护，本 ctx 只读检索。逐 collection 打开现有
    /// index.db（`open` 幂等、只 ensure schema 不改数据）、按 embedder 探针装配候选
    /// 链、读当前 `doc_count`；embedder 由调用方传入（复用外部已加载模型，避免二次
    /// 加载 GGUF），`embed("ping")` 成功才挂语义臂、否则 FTS-only。
    ///
    /// `indexed_at` 置 `None`：本 ctx 未亲自索引，时间未知（索引新鲜度以外部为准）。
    ///
    /// # Errors
    ///
    /// 任一 collection 的 index.db 打开失败或 `count()` 失败时返回 `Err`。
    pub fn attach_readonly(
        config: ServerConfig,
        embedder: Arc<dyn TextEmbedder>,
    ) -> anyhow::Result<Self> {
        use anyhow::Context as _;

        // 语义臂随 embedder 探针装配（与 daemon 一致）：stub / 加载失败 → FTS-only。
        let semantic_ready = embedder.embed("ping").is_ok();

        let mut collections: BTreeMap<String, CollectionRuntime> = BTreeMap::new();
        for meta in config.access.collections.clone() {
            let db_path = collection_db_path(&config.data_dir, &meta.id);
            let music_index = MusicIndex::open(&db_path)
                .with_context(|| format!("打开 MusicIndex 失败：{}", db_path.display()))?;
            let document_index = DocumentIndex::open(&db_path)
                .with_context(|| format!("打开 DocumentIndex 失败：{}", db_path.display()))?;

            let music_count = music_index.count().context("MusicIndex.count() 失败")?;
            let document_count = document_index
                .count()
                .context("DocumentIndex.count() 失败")?;
            let doc_count = music_count.saturating_add(document_count);

            let search_candidates = Arc::new(crate::tools::search::build_local_search_candidates(
                db_path.clone(),
                semantic_ready.then(|| embedder.clone()),
            ));

            let rt = CollectionRuntime {
                meta,
                db_path,
                music_index: Arc::new(Mutex::new(music_index)),
                document_index: Arc::new(Mutex::new(document_index)),
                search_candidates,
                state: Arc::new(RwLock::new(CollectionState {
                    indexed_at: None,
                    doc_count,
                    reindex_in_flight: false,
                })),
            };
            collections.insert(rt.meta.id.clone(), rt);
        }

        let audit = Arc::new(crate::audit::AuditSink::new(
            &config.data_dir,
            config.access.audit.log_query,
        ));

        Ok(Self {
            config,
            embedder,
            collections,
            audit,
            indexing_probe: idle_indexing_probe(),
        })
    }

    /// 从 `ServerCtx` 派生出窄一点的 [`AuthCtx`]，
    /// 用于装配 bearer 中间件 layer（避免给 middleware 注入完整索引）。
    #[must_use]
    pub fn auth_ctx(&self) -> Arc<AuthCtx> {
        Arc::new(AuthCtx::from_config_file(&self.config.access))
    }

    /// 按 id 查 collection 运行时。
    #[must_use]
    pub fn collection(&self, id: &str) -> Option<&CollectionRuntime> {
        self.collections.get(id)
    }

    /// 当前 FTS 索引是否正在进行。
    #[must_use]
    pub fn indexing_in_progress(&self) -> bool {
        (self.indexing_probe)()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn db_path_legacy_default_stays_flat() {
        let p = collection_db_path(Path::new("/data"), LEGACY_COLLECTION_ID);
        assert_eq!(p, Path::new("/data").join("index.db"));
    }

    #[test]
    fn db_path_named_collection_nested() {
        let p = collection_db_path(Path::new("/data"), "case-a");
        assert_eq!(
            p,
            Path::new("/data")
                .join("collections")
                .join("case-a")
                .join("index.db")
        );
    }

    /// S1 只读挂载：外部先建好 index.db，`attach_readonly` 只开库不重索引、
    /// 读到既有 `doc_count`、装配候选链，`indexed_at` 为 None（未亲自索引）。
    #[test]
    fn attach_readonly_opens_existing_index_without_reindexing() {
        use secrecy::SecretString;

        use scout_indexer::{DocumentIndex, NoopProgress};

        use crate::collections::{DaemonConfigFile, LEGACY_COLLECTION_ID};
        use crate::test_support::StubEmbedder;

        let tmp = tempfile::tempdir().expect("tempdir 应建成功");
        let data_dir = tmp.path().join("data");
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("note.txt"), "quarterly budget planning notes").unwrap();

        // 模拟「桌面」先把索引建好，然后关闭写连接（drop）。
        let db_path = collection_db_path(&data_dir, LEGACY_COLLECTION_ID);
        {
            let doc = DocumentIndex::open(&db_path).expect("open doc index");
            let stats = doc
                .index_dirs_with_progress(std::slice::from_ref(&root), &NoopProgress)
                .expect("预建索引应能跑");
            assert!(stats.added >= 1, "预建索引应至少 1 篇");
        }

        let access = DaemonConfigFile::legacy_single_root(root, SecretString::from("x".repeat(32)));
        let config = ServerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            data_dir,
            model_path: PathBuf::from("unused-for-attach.gguf"),
            log_level: LevelFilter::WARN,
            semantic_weight: scout_result_normalizer::DEFAULT_SEMANTIC_WEIGHT,
            cosine_threshold: scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD,
            embed_images: true,
            match_mode: scout_search_backend::MatchMode::All,
            access,
        };

        let ctx = ServerCtx::attach_readonly(config, Arc::new(StubEmbedder::default()))
            .expect("attach_readonly 应成功");
        let rt = ctx
            .collection(LEGACY_COLLECTION_ID)
            .expect("default collection 应在");
        assert!(
            rt.state.read().doc_count >= 1,
            "只读挂载应读到已建索引的 doc_count"
        );
        assert!(
            rt.state.read().indexed_at.is_none(),
            "attach 未亲自索引、indexed_at 应为 None"
        );
        assert!(
            !rt.search_candidates.is_empty(),
            "应装配 search 候选链（stub embedder embed 成功 → 含语义臂）"
        );
    }
}
