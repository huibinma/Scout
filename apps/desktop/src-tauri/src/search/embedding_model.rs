//! BETA-15B-1：embedding 模型懒加载句柄（镜像 model_fallback 的约定目录 + feature 门控）。
//! 实现 indexer 的 `TextEmbedder`，供索引期与查询期共用。
//!
//! BETA-15B-7-v2 (2026-06-26)：默认模型从 qwen3-embedding-0.6b 切到 bge-m3、
//! 落实 BETA-15B-8 v4-fixup 真水位 OVERALL=0.869 ⭐（vs qwen3 0.856 +0.013、
//! 评测层 baseline 仍守 qwen3-0.6b 不动、follow-up cycle 视真机反馈再切换）；
//! cosine 路由阈值 0.70 / 相似度下限 0.30 / 语义臂权重 10.0 保 qwen3 调优值不动、
//! bge-m3 sweep best 在 T*=0.0/0.30/0.45 是 follow-up cycle 工作；
//! crosslang 相对 qwen3-0.6b -0.055 是头号卖点 trade-off（OVERALL 净增 +0.008 cover）。
//!
//! BETA-15B-11-v2 (2026-06-27)：默认模型从 bge-m3 切到 embeddinggemma-300m、
//! 落实 BETA-15B-11 v6 真水位 no-prefix mode T=0.70 OVERALL=0.874 / crosslang=0.716 ⭐⭐
//! 双过 spec 字面 0.864 + 0.700（vs v5 bge-m3 baseline +0.010 / +0.030、**无 trade-off
//! 全方面提升** + content-not-name +0.026 + exact-name 守 1.000）；评测层 baseline.json
//! 仍守 v5 bge-m3 不动、follow-up cycle 视真机反馈再切换；cosine 路由阈值 0.70 /
//! 相似度下限 0.30 / 语义臂权重 10.0 保 v5 调优值不动；embeddinggemma sweep best 在
//! no-prefix T*=0.60 OVERALL=0.882 是 follow-up cycle 工作；prefix mode +0.013~+0.026
//! 加分项留 BETA-15B-11-v3 follow-up（不是 GO 必要条件）；模型分发 313 MB 比 bge-m3
//! 605 MB 净降 292 MB。

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use scout_indexer::embed::TextEmbedder;
use scout_indexer::IndexError;

/// 默认 embedding 模型文件名（BETA-15B-11 v6 EmbeddingGemma-300M no-prefix mode T=0.70 真水位
/// OVERALL=0.874 + crosslang=0.716 ⭐⭐ 双过 spec 字面 0.864 + 0.700、BETA-15B-11-v2 bake 切换；
/// vs v5 bge-m3 baseline +0.010 / +0.030 / +0.026 全方面提升无 trade-off、详 mod doc 顶部）。
pub(crate) const DEFAULT_EMBED_MODEL_FILE: &str = "embeddinggemma-300m-q8_0.gguf";
/// 2026-07-28：模型标识（写入 `document_vectors.embed_model`，换模型→旧向量陈旧）不再是
/// 固定常量，改成从**实际生效路径**（[`EmbeddingModelHandle::resolved_model_path`]，覆盖
/// 优先）派生——`scout_model_runtime::canonical_model_id` 剥掉 GGUF 量化后缀（`-q8_0` 等），
/// 与 daemon 端 `derive_model_id` 用同一份逻辑，两端不再各算各的（此前桌面报
/// `"embeddinggemma-300m"`、daemon 报 `"embeddinggemma-300m-q8_0"`，同一个模型两种 id）。
///
/// 对当前默认量化档位文件，派生结果与此前的硬编码常量逐字节相同——切换零迁移成本，见
/// `canonical_model_id` 文档。用户经 `embedding_model_path` 覆盖成另一个模型时，现在也能
/// 正确识别（此前的硬编码常量做不到这点，是已知局限，这次顺带修掉）。
///
/// 无热重载：一旦首次解析出结果即缓存（`OnceLock`），不随后续 settings.json 变化而变——
/// 与「已加载的模型不会中途热切换」这一既有行为一致（`ready()` 命中 `Ready` 态时直接复用
/// 已加载 daemon、不重新读路径判断是否要换模型）；若做成每次都重新解析，`model_id()` 会跟
/// 实际正在用的模型对不上。
fn resolve_model_id(path: &std::path::Path) -> String {
    scout_model_runtime::canonical_model_id(path)
}

/// 按 `model_id` 查已校准 cosine 路由阈值，未收录则回落默认值并告警。抽成独立函数（而非
/// `EmbeddingModelHandle` 方法）供 [`crate::mcp_service`] 复用——那边持有的是
/// `Arc<dyn TextEmbedder>`（可能是本句柄，也可能未来是别的实现），只需要 `model_id` 字符串，
/// 不需要整个句柄。
#[must_use]
pub(crate) fn cosine_threshold_for_model_id(model_id: &str) -> f64 {
    scout_result_normalizer::cosine_threshold_for_model(model_id).unwrap_or_else(|| {
        tracing::warn!(
            model_id,
            fallback = scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD,
            "当前 embedding 模型未收录 cosine 路由阈值校准记录，回落默认值——语义/FTS 路由\
             可能非最优，需要给该模型补一条 CALIBRATED_COSINE_THRESHOLDS 记录"
        );
        scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD
    })
}

/// embedding 模型生命周期状态。
enum EmbedState {
    /// 初始；首次 embed 时尝试一次阻塞加载。
    NotLoaded,
    /// 已被某调用者认领加载（load_blocking 进行中）；并发调用者读到此态返回 None 自然降级。
    Loading,
    /// 常驻就绪。
    Ready(Arc<scout_model_runtime::ModelDaemon>),
    /// 加载失败（文件损坏等），不再重试（设置页可见原因）。
    Failed(String),
    /// 模型不可用（feature 关），不参与召回。
    Unavailable(String),
}

/// 对外状态（设置页 / 隐私面板用）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EmbedStatus {
    /// 已加载就绪（含当前生效的模型文件路径，供设置页显示"用的是哪个模型"）。
    Ready {
        /// 当前生效模型文件的绝对路径（settings 覆盖优先，否则默认 models/ 路径）。
        active_path: String,
    },
    /// 加载中（某调用者正在阻塞 load）。
    Loading,
    /// 未找到模型文件（含期望放置路径）。
    NotFound {
        /// 期望放置模型文件的路径。
        expected_path: String,
    },
    /// 加载失败（含原因）。
    Failed {
        /// 失败原因。
        reason: String,
    },
    /// 本构建不支持语义召回（含原因）。
    Unavailable {
        /// 不可用原因。
        reason: String,
    },
}

/// embedding 模型句柄：进程级单例，索引期与查询期共用。
pub struct EmbeddingModelHandle {
    state: Mutex<EmbedState>,
    /// settings.json 路径（读 `embedding_model_path` 覆盖；None = 仅用默认路径）。
    settings_path: Option<PathBuf>,
    /// 默认模型路径（app 数据目录 models/）。
    default_model_path: PathBuf,
    /// 首次解析出的 model_id 缓存（见 [`resolve_model_id`] 文档：无热重载，只算一次）。
    resolved_model_id: OnceLock<String>,
}

impl std::fmt::Debug for EmbeddingModelHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddingModelHandle").finish()
    }
}

impl EmbeddingModelHandle {
    /// 真句柄（main.rs setup 注入）。`data_dir` 即 Scout 数据目录（与 index.db 同级）。
    ///
    /// feature 关时构造期即 Unavailable——设置页从启动起如实显示「本构建不含语义召回」。
    #[must_use]
    pub fn new(settings_path: Option<PathBuf>, data_dir: PathBuf) -> Self {
        let initial = if cfg!(feature = "semantic-recall") {
            EmbedState::NotLoaded
        } else {
            EmbedState::Unavailable(
                "本构建不含语义召回（feature semantic-recall 未开启）".to_owned(),
            )
        };
        Self {
            state: Mutex::new(initial),
            settings_path,
            default_model_path: data_dir.join("models").join(DEFAULT_EMBED_MODEL_FILE),
            resolved_model_id: OnceLock::new(),
        }
    }

    /// 当前生效模型的已校准 cosine 路由阈值。委托 [`cosine_threshold_for_model_id`]，
    /// 用本句柄真实解析出的 `model_id()`（而非固定常量）。
    #[must_use]
    pub(crate) fn cosine_threshold(&self) -> f64 {
        cosine_threshold_for_model_id(self.model_id())
    }

    /// 当前生效的 embedding 模型路径：settings.embedding_model_path 覆盖 → 默认 models/。
    fn resolved_model_path(&self) -> PathBuf {
        if let Some(path) = &self.settings_path {
            if let Ok(s) = std::fs::read_to_string(path) {
                if let Ok(v) = serde_json::from_str::<crate::settings::AppSettings>(&s) {
                    if let Some(custom) = v.embedding_model_path.filter(|p| !p.trim().is_empty()) {
                        return PathBuf::from(custom);
                    }
                }
            }
        }
        self.default_model_path.clone()
    }

    /// 同步取就绪 daemon。首次调用（NotLoaded）认领加载：置 Loading → **释放锁** → 阻塞 load
    /// → 重新加锁存 Ready/Failed。加载期间其他调用者读到 Loading 返回 None（查询侧自然降级 FTS-only；
    /// 认领者——通常是索引 pass——阻塞等待并拿到 daemon）。锁绝不跨 load_blocking 持有。
    fn ready(&self) -> Option<Arc<scout_model_runtime::ModelDaemon>> {
        let path = self.resolved_model_path();
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match &*st {
                EmbedState::Ready(d) => return Some(Arc::clone(d)),
                EmbedState::Failed(_) | EmbedState::Unavailable(_) | EmbedState::Loading => {
                    return None;
                }
                EmbedState::NotLoaded => {}
            }
            if !path.exists() {
                // 保持 NotLoaded（下次再探测）；status() 显示 NotFound。
                tracing::warn!(
                    expected_path = %path.display(),
                    "embedding 模型路径不存在、保持 NotLoaded（下次 ready() 重探）"
                );
                return None;
            }
            *st = EmbedState::Loading;
        } // 释放锁

        let file_size = std::fs::metadata(&path).ok().map(|m| m.len());
        tracing::info!(
            path = %path.display(),
            file_size_bytes = ?file_size,
            "embedding 模型 NotLoaded → Loading、开始阻塞 load"
        );
        let load_start = std::time::Instant::now();
        let params = scout_model_runtime::ModelLoadParams {
            gpu_layers: 99,
            context_size: 2048,
        };
        let loaded = scout_model_runtime::ModelDaemon::load_blocking(&path, params);
        let load_elapsed_ms = u64::try_from(load_start.elapsed().as_millis()).unwrap_or(u64::MAX);

        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match loaded {
            Ok(d) => {
                tracing::info!(
                    path = %path.display(),
                    load_elapsed_ms,
                    "embedding 模型 Loading → Ready"
                );
                let arc = Arc::new(d);
                *st = EmbedState::Ready(Arc::clone(&arc));
                Some(arc)
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    load_elapsed_ms,
                    error = %e,
                    "embedding 模型 Loading → Failed（不再重试）"
                );
                *st = EmbedState::Failed(e.to_string());
                None
            }
        }
    }

    /// 廉价探测：本构建是否开启语义召回 feature **且**模型文件就位（不触发加载）。
    /// reindex 用它门控文档嵌入 pass——无模型时跳过，避免白白重走一遍文档树 + 全数嵌入失败。
    #[must_use]
    pub fn is_active(&self) -> bool {
        cfg!(feature = "semantic-recall") && self.resolved_model_path().exists()
    }

    /// 后台暖机：在当前线程阻塞 load 模型（付掉冷启动成本），使后续查询直接走 warm 路径。
    /// 幂等——已 `Ready` 直接返 true；`Loading`/`Failed`/`Unavailable`/`NotFound` 返 false（不重试）。
    /// 应在后台 `spawn_blocking` 线程调，**绝不**在 UI / 查询线程调（会阻塞 16.8s 量级）。
    /// BETA-15B-2：由 `spawn_semantic_index` 后台 worker 调用。
    #[must_use]
    pub fn prewarm(&self) -> bool {
        self.ready().is_some()
    }

    /// BETA-12 卸载清理：主动卸载常驻模型（`Ready`/`Failed` → `NotLoaded`）。
    ///
    /// `Ready` 持有的 daemon Arc 被 drop 后释放 GGUF 文件句柄——Windows 上 mmap 中的模型
    /// 文件删不掉，删 `models/` 前必须先调本方法。`Failed` 一并复位（无句柄占用，但文件
    /// 即将被删、陈旧失败原因不再成立）。`Loading` 不打断（加载线程稍后会写回 Ready、
    /// 句柄仍占用；调用方以索引 / 语义嵌入守卫避开该窗口）。`Unavailable` 维持不变。
    /// 清理后下次 `ready()` 重探：文件已删 → 保持 NotLoaded、状态如实显示 NotFound。
    pub(crate) fn unload(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(&*st, EmbedState::Ready(_) | EmbedState::Failed(_)) {
            *st = EmbedState::NotLoaded;
        }
    }

    /// 设置页 / 隐私面板状态。
    ///
    /// **cycle 6 v3（v0.8.9）**：`NotLoaded` 分支加 `path.exists()` 检查、与
    /// `is_active()` / `StatusIndicator` 顶栏绿点判定对齐——文件就位 → 报 Ready
    /// （等同顶栏判可用）；文件不在 → 报 NotFound（提示用户下载/放置）。
    ///
    /// 旧实现（v0.8.8 及以前）`NotLoaded` 一刀切报 NotFound、不查文件、与顶栏
    /// 绿点判定不一致——表现为顶栏「语义召回」绿点亮（is_active() 走 path.exists()）
    /// 但设置页文案「模型未找到」+ 下载按钮误显示，引起用户困惑（明明能搜出语义命中）。
    ///
    /// 语义对齐：`NotLoaded` 是「未触发首次加载」的内部 lazy state，对用户呈现层
    /// 而言「文件就位 = 可用」（首次搜索时由 `ready()` 自动 1.7s 暖机加载）。
    #[must_use]
    pub fn status(&self) -> EmbedStatus {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &*st {
            EmbedState::Ready(_) => EmbedStatus::Ready {
                active_path: self.resolved_model_path().to_string_lossy().into_owned(),
            },
            EmbedState::Loading => EmbedStatus::Loading,
            EmbedState::Failed(r) => EmbedStatus::Failed { reason: r.clone() },
            EmbedState::Unavailable(r) => EmbedStatus::Unavailable { reason: r.clone() },
            EmbedState::NotLoaded => {
                let path = self.resolved_model_path();
                if path.exists() {
                    EmbedStatus::Ready {
                        active_path: path.to_string_lossy().into_owned(),
                    }
                } else {
                    EmbedStatus::NotFound {
                        expected_path: path.to_string_lossy().into_owned(),
                    }
                }
            }
        }
    }
}

impl TextEmbedder for EmbeddingModelHandle {
    fn embed(&self, text: &str) -> Result<Vec<f32>, IndexError> {
        let daemon = self.ready().ok_or_else(|| IndexError::Tag {
            path: String::new(),
            detail: "embedding 模型不可用".to_owned(),
        })?;
        daemon.embed(text).map_err(|e| IndexError::Tag {
            path: String::new(),
            detail: e.to_string(),
        })
    }

    fn model_id(&self) -> &str {
        self.resolved_model_id
            .get_or_init(|| resolve_model_id(&self.resolved_model_path()))
            .as_str()
    }

    /// 廉价探测（不触发加载）：`SemanticIndexBackend::is_available()` 每查询 live 调用，
    /// 决定语义臂是否参与 fan-out（BETA-33 cycle 9）。
    ///
    /// - `Ready` → true；
    /// - `Loading`（后台暖机 / 其他调用者认领加载中）→ false——加载期查询本就拿不到
    ///   daemon（`ready()` 返 None → embed 必 Err），退出臂让整链走干净的 FTS 路径，
    ///   暖机完成后语义臂自动回归；
    /// - `Failed` / `Unavailable`（加载失败 / feature 关）→ false——修复「必败语义臂
    ///   每查询报 embedding 模型不可用」（构造期注入句柄恒 Some、旧 `is_available()`
    ///   只查 `embedder.is_some()` 探不到这两态）；
    /// - `NotLoaded` → 模型文件是否就位（与 `is_active()` / 顶栏绿点判定同口径；
    ///   就位则首查询照旧认领阻塞加载，行为不变）。
    fn is_ready(&self) -> bool {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &*st {
            EmbedState::Ready(_) => true,
            EmbedState::Loading | EmbedState::Failed(_) | EmbedState::Unavailable(_) => false,
            EmbedState::NotLoaded => self.resolved_model_path().exists(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// feature 关时构造期即 Unavailable——设置页从启动起如实显示，不误导用户放文件。
    #[test]
    fn feature_off_new_is_unavailable_from_start() {
        if cfg!(feature = "semantic-recall") {
            return; // feature 开形态下跳过
        }
        let h = EmbeddingModelHandle::new(None, PathBuf::from("/tmp/x"));
        assert!(matches!(h.status(), EmbedStatus::Unavailable { .. }));
    }

    /// feature 关时 embed 返回 Err（不可用），永不 panic。
    #[test]
    fn feature_off_embed_errs() {
        if cfg!(feature = "semantic-recall") {
            return;
        }
        let h = EmbeddingModelHandle::new(None, PathBuf::from("/tmp/x"));
        assert!(h.embed("hello").is_err());
    }

    /// BETA-33 cycle 9：feature 关 → `is_ready()`=false——语义臂路由期即退出 fan-out，
    /// 不再让必败臂每查询报「embedding 模型不可用」。
    #[test]
    fn feature_off_is_ready_false() {
        if cfg!(feature = "semantic-recall") {
            return;
        }
        let h = EmbeddingModelHandle::new(None, PathBuf::from("/tmp/x"));
        assert!(!h.is_ready(), "feature 关（Unavailable）→ is_ready()=false");
    }

    /// BETA-33 cycle 9：feature 开但模型文件缺失（NotLoaded + 路径不存在）→
    /// `is_ready()`=false（与 `is_active()` / 顶栏绿点判定同口径）。
    #[test]
    fn model_file_missing_is_ready_false() {
        if !cfg!(feature = "semantic-recall") {
            return; // feature 关形态由上一测覆盖
        }
        let h = EmbeddingModelHandle::new(None, PathBuf::from("/nonexistent/scout-test"));
        assert!(!h.is_ready(), "模型文件缺失 → is_ready()=false");
    }

    /// model_id 稳定（写入向量表的 embed_model 列）、且对默认量化档位文件名派生结果与
    /// 此前的硬编码常量逐字节相同（切换零迁移成本，见 `resolve_model_id` 文档）。
    #[test]
    fn model_id_is_stable() {
        let h = EmbeddingModelHandle::new(None, PathBuf::from("/tmp/x"));
        assert_eq!(h.model_id(), "embeddinggemma-300m");
        // OnceLock 缓存：重复调用不重算、结果不变。
        assert_eq!(h.model_id(), h.model_id());
    }

    /// settings.embedding_model_path 覆盖默认路径（feature 开形态下生效）。
    #[test]
    fn embedding_model_path_override_from_settings() {
        if !cfg!(feature = "semantic-recall") {
            return; // feature 关时 new() 已 latch 为 Unavailable，resolved 路径不参与决策
        }
        let dir = std::env::temp_dir().join(format!("scout-embed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let settings_file = dir.join("settings.json");
        std::fs::write(
            &settings_file,
            r#"{"global_shortcut":"x","search_scope":["~"],"enable_model_fallback":true,"enable_tracing":false,"embedding_model_path":"/tmp/custom-embed.gguf"}"#,
        )
        .unwrap();
        let h = EmbeddingModelHandle::new(Some(settings_file), dir.clone());
        assert_eq!(
            h.resolved_model_path(),
            PathBuf::from("/tmp/custom-embed.gguf")
        );
        // 2026-07-28 修复前：model_id() 是固定常量，覆盖成别的模型文件也纹丝不动，
        // 感知不到实际加载的是哪个模型。现在应按覆盖路径派生。
        assert_eq!(h.model_id(), "custom-embed");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// feature 关时 prewarm 返 false（不可用）、不 panic、幂等。
    #[test]
    fn prewarm_feature_off_is_false_and_idempotent() {
        if cfg!(feature = "semantic-recall") {
            return; // feature 开形态下跳过（需真模型，留真机手测）
        }
        let h = EmbeddingModelHandle::new(None, PathBuf::from("/tmp/x"));
        assert!(!h.prewarm());
        assert!(!h.prewarm()); // 二次调用仍 false，不 panic
    }
}
