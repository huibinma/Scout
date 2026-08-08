//! 测试支持 —— stub embedder + 内存 ctx builder + principal helper。
//!
//! BETA-32 T6：让 [`SearchTool`] / [`ListCollectionsTool`] 等 ctx-aware 测试不必拉真
//! 模型 / 真索引 db；T11 集成测试 daemon HTTP 端到端复用。BETA-36 起 ctx 按
//! collection 组织，另提供 [`build_test_ctx_multi_inmem`]（双集合：case-a 只读 /
//! case-b 读写）与 [`full_access_principal`] / [`restricted_principal`]。
//!
//! 公开暴露而非 feature-gated：dev-dependencies 已含 `tempfile`，prod build 不
//! 引用本模块即零代码生成。
//!
//! 本模块统一放行 `unwrap_used` / `expect_used`：test infrastructure 异常无意义
//! 的错误传播只会污染调用方签名。
//!
//! [`SearchTool`]: crate::tools::search::SearchTool
//! [`ListCollectionsTool`]: crate::tools::list_collections::ListCollectionsTool

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use secrecy::SecretString;
use tracing::level_filters::LevelFilter;

use scout_indexer::embed::TextEmbedder;
use scout_indexer::{DocumentIndex, IndexError, MusicIndex, NoopProgress};

use crate::auth::AuthedPrincipal;
use crate::collections::{
    CollectionConfig, CollectionGrant, DaemonConfigFile, SubjectKind, TokenConfig,
};
use crate::config::{
    collection_db_path, idle_indexing_probe, CollectionRuntime, CollectionState, IndexingProbe,
    ServerConfig, ServerCtx,
};
use crate::tools::search::build_local_search_candidates;

/// 测试用 stub embedder —— 返固定 dim、确定性向量。
///
/// 同 query 同 vec、不同 query 不同 vec（FNV-style hash 派生）。不调任何模型 /
/// llama.cpp，可在无 GGUF 文件的 CI 环境直接 build 与跑测。
#[derive(Debug)]
pub struct StubEmbedder {
    /// 向量维度（与 `qwen3-embedding` 等真实模型对齐 = 768）。
    pub dim: usize,
    /// `model_id()` 返回值。
    pub model_id: String,
}

impl Default for StubEmbedder {
    fn default() -> Self {
        Self {
            dim: 768,
            model_id: "stub-embedder".to_owned(),
        }
    }
}

impl TextEmbedder for StubEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>, IndexError> {
        // FNV-1a 32bit hash 派生 → 确定性、零依赖。同 query 同向量。
        let mut h: u32 = 0x811c_9dc5;
        for b in text.as_bytes() {
            h ^= u32::from(*b);
            h = h.wrapping_mul(0x0100_0193);
        }
        let mut v = vec![0.0_f32; self.dim];
        for (i, slot) in v.iter_mut().enumerate() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
            let raw = (h.wrapping_add(i as u32) % 100) as f32 / 100.0;
            *slot = raw;
        }
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        Ok(v)
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }
}

/// 全权 admin principal（等价 legacy 单 token 语义）。
#[must_use]
pub fn full_access_principal() -> Arc<AuthedPrincipal> {
    Arc::new(AuthedPrincipal {
        subject: "default".to_string(),
        grant: CollectionGrant::All,
        admin: true,
    })
}

/// 限定授权 principal（非 admin）。
#[must_use]
pub fn restricted_principal(subject: &str, collections: &[&str]) -> Arc<AuthedPrincipal> {
    Arc::new(AuthedPrincipal {
        subject: subject.to_string(),
        grant: CollectionGrant::Listed(collections.iter().map(|s| (*s).to_string()).collect()),
        admin: false,
    })
}

/// 派生进程内唯一临时路径（不真 mkdir）。
fn unique_temp_paths() -> (PathBuf, PathBuf, PathBuf) {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let stem = format!("scout-test-{pid}-{nanos}");
    (
        std::env::temp_dir().join(format!("{stem}-data")),
        std::env::temp_dir().join(format!("{stem}-root")),
        std::env::temp_dir().join(format!("{stem}-model.gguf")),
    )
}

/// 用内存索引装配一个 [`CollectionRuntime`]（db 文件不落盘；`LocalIndexBackend`
/// 见 `db_path` 不存在即返空结果、非错误）。
fn inmem_collection_runtime(data_dir: &Path, meta: CollectionConfig) -> CollectionRuntime {
    let db_path = collection_db_path(data_dir, &meta.id);
    let music = MusicIndex::open_in_memory().expect("test 内存 MusicIndex 应当能开");
    let document = DocumentIndex::open_in_memory().expect("test 内存 DocumentIndex 应当能开");
    CollectionRuntime {
        meta,
        db_path: db_path.clone(),
        music_index: Arc::new(Mutex::new(music)),
        document_index: Arc::new(Mutex::new(document)),
        // 测试链维持 FTS-only（None）：语义臂行为由 search.rs 单测与 daemon 真机 smoke 覆盖。
        search_candidates: Arc::new(build_local_search_candidates(db_path, None)),
        state: Arc::new(RwLock::new(CollectionState::default())),
    }
}

fn simple_collection(id: &str, root: PathBuf, read_only: bool) -> CollectionConfig {
    CollectionConfig {
        id: id.to_string(),
        display_name: None,
        subject_kind: if read_only {
            SubjectKind::Case
        } else {
            SubjectKind::Other
        },
        roots: vec![root],
        read_only,
        audit_tags: Vec::new(),
        // 镜像 TOML 缺省（禁全文）：read_document 闸门单测依赖此姿态。
        allow_full_read: false,
    }
}

fn test_server_config(
    data_dir: PathBuf,
    model_path: PathBuf,
    access: DaemonConfigFile,
) -> ServerConfig {
    ServerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("常量 SocketAddr 解析应成功"),
        data_dir,
        model_path,
        log_level: LevelFilter::WARN,
        semantic_weight: scout_result_normalizer::DEFAULT_SEMANTIC_WEIGHT,
        cosine_threshold: scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD,
        embed_images: true,
        match_mode: scout_search_backend::MatchMode::All,
        access,
    }
}

/// 构造内存 ctx —— 单 `default` collection（legacy 形态）+ stub embedder +
/// dummy token `test-token`。
///
/// - 索引用 `open_in_memory()`，schema 完整、不落盘。
/// - `data_dir` / `root` 用临时路径（不创建真目录）；search 路径里
///   `LocalIndexBackend` 见 `db_path` 不存在即返空结果（非错误）。
#[must_use]
pub fn build_test_ctx_inmem() -> Arc<ServerCtx> {
    build_test_ctx_inmem_with_indexing_probe(idle_indexing_probe())
}

/// 构造单 collection 内存 ctx，并允许测试注入索引活动探针。
#[must_use]
pub fn build_test_ctx_inmem_with_indexing_probe(indexing_probe: IndexingProbe) -> Arc<ServerCtx> {
    let (data_dir, root, model_path) = unique_temp_paths();
    let access =
        DaemonConfigFile::legacy_single_root(root, SecretString::from("test-token".to_owned()));
    let meta = access.collections[0].clone();
    let rt = inmem_collection_runtime(&data_dir, meta);
    let mut collections = BTreeMap::new();
    collections.insert(rt.meta.id.clone(), rt);
    let audit = Arc::new(crate::audit::AuditSink::new(
        &data_dir,
        access.audit.log_query,
    ));
    Arc::new(ServerCtx {
        config: test_server_config(data_dir, model_path, access),
        embedder: Arc::new(StubEmbedder::default()),
        collections,
        audit,
        indexing_probe,
    })
}

/// 构造双 collection 内存 ctx（BETA-36 权限 / reindex 单测用）：
/// `case-a` **只读**、`case-b` 读写；token 表含 `test-token` 全权 admin。
#[must_use]
pub fn build_test_ctx_multi_inmem() -> Arc<ServerCtx> {
    let (data_dir, root, model_path) = unique_temp_paths();
    let ca = simple_collection("case-a", root.join("a"), true);
    let cb = simple_collection("case-b", root.join("b"), false);
    let access = DaemonConfigFile {
        collections: vec![ca.clone(), cb.clone()],
        tokens: vec![TokenConfig {
            token: SecretString::from("test-token".to_owned()),
            subject: "default".to_string(),
            collections: vec!["*".to_string()],
            admin: true,
        }],
        audit: crate::collections::AuditConfig::default(),
    };
    let mut collections = BTreeMap::new();
    for meta in [ca, cb] {
        let rt = inmem_collection_runtime(&data_dir, meta);
        collections.insert(rt.meta.id.clone(), rt);
    }
    let audit = Arc::new(crate::audit::AuditSink::new(
        &data_dir,
        access.audit.log_query,
    ));
    Arc::new(ServerCtx {
        config: test_server_config(data_dir, model_path, access),
        embedder: Arc::new(StubEmbedder::default()),
        collections,
        audit,
        indexing_probe: idle_indexing_probe(),
    })
}

/// 构造 **真 on-disk** ctx —— 把 `corpus` 写到各 collection 的首个 root、真跑
/// `index_dirs_with_progress` 灌进各自的 `index.db`（同 daemon 生产路径）。
///
/// **caller 责任**：`config.data_dir` 与各 collection 的 roots 必须指向已存在的
/// 目录（典型为 `tempfile::TempDir::path()` 下的子目录）；caller 持 `TempDir`
/// 句柄、负责生命周期。
///
/// `corpus` 形如 `&[("case-a", "notes.txt", "competitive analysis"), ...]`：
/// 每条写入对应 collection 的首个 root。
///
/// # Panics
///
/// 路径 / IO / sqlite open / index 失败均 panic（test infra 约定）。
#[must_use]
pub fn build_test_ctx_indexed(
    config: ServerConfig,
    corpus: &[(&str, &str, &str)],
) -> Arc<ServerCtx> {
    let mut collections = BTreeMap::new();
    for meta in config.access.collections.clone() {
        let root = meta.roots.first().expect("collection 应至少有一个 root");
        std::fs::create_dir_all(root).expect("创建 collection root 应成功");
        for (cid, name, body) in corpus {
            if *cid == meta.id {
                let path = root.join(name);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).expect("创建 corpus 父目录应成功");
                }
                std::fs::write(&path, body).expect("写入 corpus 文件应成功");
            }
        }

        let db_path = collection_db_path(&config.data_dir, &meta.id);
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).expect("创建 collection db 目录应成功");
        }
        let music = MusicIndex::open(&db_path).expect("test 真盘 MusicIndex 应当能开");
        let document = DocumentIndex::open(&db_path).expect("test 真盘 DocumentIndex 应当能开");

        let roots = meta.roots.clone();
        let _ = music
            .index_dirs_with_progress(&roots, &NoopProgress)
            .expect("MusicIndex.index_dirs_with_progress 应当能跑");
        let doc_stats = document
            .index_dirs_with_progress(&roots, &NoopProgress)
            .expect("DocumentIndex.index_dirs_with_progress 应当能跑");

        let doc_count = u64::try_from(doc_stats.added.saturating_add(doc_stats.updated))
            .expect("文档计数应当能转 u64");
        let state = CollectionState {
            indexed_at: Some(chrono::Utc::now()),
            doc_count,
            reindex_in_flight: false,
        };
        let rt = CollectionRuntime {
            meta,
            db_path: db_path.clone(),
            music_index: Arc::new(Mutex::new(music)),
            document_index: Arc::new(Mutex::new(document)),
            // 测试链维持 FTS-only（None），同 inmem_collection_runtime。
            search_candidates: Arc::new(build_local_search_candidates(db_path, None)),
            state: Arc::new(RwLock::new(state)),
        };
        collections.insert(rt.meta.id.clone(), rt);
    }
    let audit = Arc::new(crate::audit::AuditSink::new(
        &config.data_dir,
        config.access.audit.log_query,
    ));
    Arc::new(ServerCtx {
        config,
        embedder: Arc::new(StubEmbedder::default()),
        collections,
        audit,
        indexing_probe: idle_indexing_probe(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_embedder_deterministic() {
        let e = StubEmbedder::default();
        let v1 = e.embed("hello world").unwrap();
        let v2 = e.embed("hello world").unwrap();
        assert_eq!(v1, v2, "同 query 应得同向量");
        assert_eq!(v1.len(), 768);
    }

    #[test]
    fn stub_embedder_distinguishes_queries() {
        let e = StubEmbedder::default();
        let a = e.embed("alpha").unwrap();
        let b = e.embed("beta").unwrap();
        assert_ne!(a, b, "不同 query 应得不同向量");
    }

    #[test]
    fn build_test_ctx_inmem_has_unique_paths_and_default_collection() {
        let c1 = build_test_ctx_inmem();
        let c2 = build_test_ctx_inmem();
        assert_ne!(
            c1.config.data_dir, c2.config.data_dir,
            "每次 build 应得到唯一 data_dir（防并发测试碰撞）"
        );
        assert!(c1.collections.contains_key("default"));
    }

    #[test]
    fn multi_ctx_has_readonly_case_a_and_writable_case_b() {
        let ctx = build_test_ctx_multi_inmem();
        assert!(ctx.collections["case-a"].meta.read_only);
        assert!(!ctx.collections["case-b"].meta.read_only);
        assert_eq!(ctx.collections.len(), 2);
    }
}
