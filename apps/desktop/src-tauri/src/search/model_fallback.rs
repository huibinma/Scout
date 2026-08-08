//! BETA-23：模型 fallback 编排——懒加载状态机 + 同步等待超时 + 失败回落 parser。
//!
//! 硬约束：任何路径下搜索不因模型而失败。模型只可能把 parser 的 intent 变得更全，
//! 不可能让查询出错（失败/超时/未加载/占用 → 一律回落 parser intent 继续）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use scout_intent_parser::fallback::{
    parse_with_signals, should_invoke_model, FallbackDecision, IntentSource, ModelFallback,
    ResolvedIntent,
};
use tauri::ipc::Channel;

use super::SearchEvent;

/// 推理同步等待上限。超时回落 parser（被弃线程自然结束，busy 标志挡后续并发）。
const INFER_TIMEOUT: Duration = Duration::from_secs(3);

/// 默认模型文件名（BETA-17 胜出者）。
const DEFAULT_MODEL_FILE: &str = "qwen3-0.6b-q4_k_m.gguf";

/// 模型生命周期状态。
enum ModelState {
    /// 初始；首次触发时探测文件并转 Loading（文件缺失则保持 NotLoaded 下次再探测）。
    NotLoaded,
    /// 后台 load_blocking 进行中。
    Loading,
    /// 常驻就绪。
    Ready(Arc<ModelFallback>),
    /// 加载失败（文件损坏等），不再重试（设置页可见原因）。
    Failed(String),
    /// 模型不可用（feature 关 / 显式禁用），不参与触发。
    Unavailable(String),
}

/// 设置页用的状态快照。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelStatusJson {
    /// "ready" | "loading" | "failed" | "unavailable" | "not_found" | "not_loaded"
    pub state: String,
    /// 人话详情（路径 / 失败原因 / 放置提示）。
    pub detail: String,
}

/// busy 单飞守卫：Drop 时释放——正常返回 / Err / panic 三条路径归一释放。
struct BusyGuard(Arc<ModelFallbackHandle>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
    }
}

/// 编排句柄：挂 SearchDeps，进程级单例。
pub struct ModelFallbackHandle {
    state: Mutex<ModelState>,
    /// 推理单飞守卫：true = 有 in-flight 推理（含已超时被弃但仍在跑的）。
    pub(crate) busy: AtomicBool,
    /// settings.json 路径（每次触发读 enable 开关；None = 默认开启）。
    settings_path: Option<PathBuf>,
    /// 默认模型路径（app 数据目录 models/）。
    default_model_path: PathBuf,
    /// 推理超时（测试可缩短）。
    infer_timeout: Duration,
}

impl std::fmt::Debug for ModelFallbackHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelFallbackHandle")
            .finish_non_exhaustive()
    }
}

impl ModelFallbackHandle {
    /// 真句柄（main.rs setup 注入）。`data_dir` 即 Scout 数据目录（与 index.db 同级）。
    ///
    /// feature 关时构造期即 Unavailable——设置页从启动起就如实显示「本构建不含模型支持」，
    /// 而非误导性的「未找到模型文件」（放了也没用）。
    #[must_use]
    pub fn new(settings_path: Option<PathBuf>, data_dir: PathBuf) -> Self {
        let initial = if cfg!(feature = "model-fallback") {
            ModelState::NotLoaded
        } else {
            ModelState::Unavailable(
                "本构建不含模型支持（feature model-fallback 未开启）".to_owned(),
            )
        };
        Self {
            state: Mutex::new(initial),
            busy: AtomicBool::new(false),
            settings_path,
            default_model_path: data_dir.join("models").join(DEFAULT_MODEL_FILE),
            infer_timeout: INFER_TIMEOUT,
        }
    }

    /// 固定不可用句柄（SearchDeps::new 默认 / 测试）。
    #[must_use]
    pub fn disabled(reason: &str) -> Self {
        Self {
            state: Mutex::new(ModelState::Unavailable(reason.to_owned())),
            busy: AtomicBool::new(false),
            settings_path: None,
            default_model_path: PathBuf::new(),
            infer_timeout: INFER_TIMEOUT,
        }
    }

    /// 测试用：直接 Ready + 自定义超时。
    #[cfg(test)]
    pub(crate) fn with_ready_for_test(fb: ModelFallback, infer_timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ModelState::Ready(Arc::new(fb))),
            busy: AtomicBool::new(false),
            settings_path: None,
            default_model_path: PathBuf::new(),
            infer_timeout,
        })
    }

    /// 测试用：直接 Loading 状态。
    #[cfg(test)]
    pub(crate) fn with_loading_for_test() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ModelState::Loading),
            busy: AtomicBool::new(false),
            settings_path: None,
            default_model_path: PathBuf::new(),
            infer_timeout: INFER_TIMEOUT,
        })
    }

    /// settings.json 的 enable_model_fallback（缺文件/损坏 → 默认 true，与 AppSettings::default 一致）。
    ///
    /// 同步读盘有意为之：settings.json 为 μs 级小文件、仅在 InvokeModel 决策路径（低频）触发。
    fn enabled_in_settings(&self) -> bool {
        let Some(path) = &self.settings_path else {
            return true;
        };
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str::<crate::settings::AppSettings>(&s)
                .map(|v| v.enable_model_fallback)
                .unwrap_or(true),
            Err(_) => true,
        }
    }

    /// 当前生效的模型路径：settings.model_path 覆盖 → 默认 app 数据目录 models/。
    fn resolved_model_path(&self) -> PathBuf {
        if let Some(path) = &self.settings_path {
            if let Ok(s) = std::fs::read_to_string(path) {
                if let Ok(v) = serde_json::from_str::<crate::settings::AppSettings>(&s) {
                    if let Some(custom) = v.model_path.filter(|p| !p.trim().is_empty()) {
                        return PathBuf::from(custom);
                    }
                }
            }
        }
        self.default_model_path.clone()
    }

    /// BETA-12 卸载清理：主动卸载常驻模型（`Ready`/`Failed` → `NotLoaded`，释放 GGUF
    /// 文件句柄——Windows 上 mmap 中的模型文件删不掉）。`Loading` 不打断（加载线程稍后
    /// 写回 Ready；调用方以 `busy` / 下载守卫避开）。`Unavailable` 维持不变。
    /// 与 [`EmbeddingModelHandle::unload`](crate::search::embedding_model::EmbeddingModelHandle::unload) 同约定。
    pub(crate) fn unload(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(&*st, ModelState::Ready(_) | ModelState::Failed(_)) {
            *st = ModelState::NotLoaded;
        }
    }

    /// 设置页用的状态快照（NotLoaded 时 probe 一次文件存在性，μs 级 IO，轮询可接受）。
    ///
    /// 锁内只取判别式（owned），锁外再做文件 IO——不与搜索路径的 ready_or_kick_load 争锁。
    pub(crate) fn status_snapshot(&self) -> ModelStatusJson {
        enum Snap {
            Ready,
            Loading,
            Failed(String),
            Unavailable(String),
            NotLoaded,
        }
        let snap = {
            let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match &*st {
                ModelState::Ready(_) => Snap::Ready,
                ModelState::Loading => Snap::Loading,
                ModelState::Failed(e) => Snap::Failed(e.clone()),
                ModelState::Unavailable(r) => Snap::Unavailable(r.clone()),
                ModelState::NotLoaded => Snap::NotLoaded,
            }
        };
        let (state, detail) = match snap {
            Snap::Ready => (
                "ready",
                format!("已就绪：{}", self.resolved_model_path().display()),
            ),
            Snap::Loading => ("loading", "模型加载中…".to_owned()),
            Snap::Failed(e) => ("failed", format!("加载失败：{e}")),
            Snap::Unavailable(r) => ("unavailable", r),
            Snap::NotLoaded => {
                let p = self.resolved_model_path();
                if p.exists() {
                    ("not_loaded", "首次触发时自动加载".to_owned())
                } else {
                    (
                        "not_found",
                        format!("未找到模型文件，请放置到：{}", p.display()),
                    )
                }
            }
        };
        ModelStatusJson {
            state: state.to_owned(),
            detail,
        }
    }

    /// 就绪则给 fallback；NotLoaded 且文件存在则踢一次后台加载（本次返回 None）。
    fn ready_or_kick_load(self: &Arc<Self>) -> Option<Arc<ModelFallback>> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &*st {
            ModelState::Ready(fb) => return Some(Arc::clone(fb)),
            ModelState::Loading | ModelState::Failed(_) | ModelState::Unavailable(_) => {
                return None;
            }
            ModelState::NotLoaded => {}
        }
        // 双保险：正常情况下构造期已 latch 为 Unavailable，此处仅防御性兜底。
        if !cfg!(feature = "model-fallback") {
            *st = ModelState::Unavailable(
                "本构建不含模型支持（feature model-fallback 未开启）".to_owned(),
            );
            return None;
        }
        let model_path = self.resolved_model_path();
        if !model_path.exists() {
            // 不锁死：用户放好文件后下次触发即可加载
            return None;
        }
        *st = ModelState::Loading;
        drop(st);
        let this = Arc::clone(self);
        tauri::async_runtime::spawn_blocking(move || {
            let params = scout_model_runtime::ModelLoadParams {
                gpu_layers: 99,
                context_size: 4096,
            };
            let loaded = scout_model_runtime::ModelDaemon::load_blocking(&model_path, params);
            let mut st = this.state.lock().unwrap_or_else(|e| e.into_inner());
            *st = match loaded {
                Ok(daemon) => {
                    // BETA-23：预热 hybrid prompt 前缀的 KV（首次 prefill ≈ 2.9s，贴着 3s 超时；
                    // 在加载线程付掉这笔成本，用户首条触发查询直接走 warm 路径 ~350ms）。
                    // best-effort：预热失败不阻塞 Ready（推理时会再付 prefill，行为同未预热）。
                    let warmup = scout_model_runtime::GenerateParams {
                        max_tokens: 1,
                        temperature: 0.0,
                        top_p: 1.0,
                        stop_sequences: Vec::new(),
                        seed: 42,
                        grammar: None,
                        stop_at_json: false,
                    };
                    if let Err(err) = daemon.generate_cached_prefix(
                        scout_intent_parser::hybrid::hybrid_prompt_prefix(),
                        "",
                        &warmup,
                    ) {
                        eprintln!("model fallback: 前缀预热失败（不影响就绪）: {err}");
                    }
                    ModelState::Ready(Arc::new(
                        ModelFallback::new(Arc::new(daemon)).with_hybrid_mode(),
                    ))
                }
                Err(err) => {
                    eprintln!("model fallback: 模型加载失败: {err}");
                    ModelState::Failed(err.to_string())
                }
            };
        });
        None
    }
}

/// 触发但不可用/失败/超时 → 回落 parser intent（source 标记 ParserNoFallback）。
fn parser_fallback(
    parsed: &scout_intent_parser::fallback::ParseResult,
    decision: &FallbackDecision,
    reason: &str,
) -> ResolvedIntent {
    if !reason.is_empty() {
        eprintln!("model fallback: {reason}，回落 parser");
    }
    ResolvedIntent {
        intent: parsed.intent.clone(),
        source: IntentSource::ParserNoFallback,
        decision: decision.clone(),
        signals: parsed.signals,
    }
}

/// search_impl 的 intent 解析入口：parser 优先，必要且可行时模型 hybrid 补全。**永不失败**。
pub(crate) async fn resolve_with_model(
    query: &str,
    handle: &Arc<ModelFallbackHandle>,
    on_event: &Channel<SearchEvent>,
) -> ResolvedIntent {
    let parsed = parse_with_signals(query);
    let decision = should_invoke_model(&parsed);

    if matches!(decision, FallbackDecision::UseParser) {
        return ResolvedIntent {
            intent: parsed.intent,
            source: IntentSource::Parser,
            decision,
            signals: parsed.signals,
        };
    }

    if !handle.enabled_in_settings() {
        return parser_fallback(&parsed, &decision, "");
    }
    let Some(fb) = handle.ready_or_kick_load() else {
        return parser_fallback(&parsed, &decision, "");
    };
    if handle
        .busy
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return parser_fallback(&parsed, &decision, "推理占用中");
    }

    let _ = on_event.send(SearchEvent::ModelThinking);
    let q = query.to_owned();
    let h = Arc::clone(handle);
    let join = tauri::async_runtime::spawn_blocking(move || {
        let _busy = BusyGuard(h);
        fb.invoke(&q)
    });
    match tokio::time::timeout(handle.infer_timeout, join).await {
        Ok(Ok(Ok(intent))) => ResolvedIntent {
            intent,
            source: IntentSource::Model,
            decision,
            signals: parsed.signals,
        },
        Ok(Ok(Err(err))) => parser_fallback(&parsed, &decision, &format!("推理失败: {err}")),
        Ok(Err(join_err)) => {
            parser_fallback(&parsed, &decision, &format!("推理任务异常: {join_err}"))
        }
        Err(_) => {
            let t = handle.infer_timeout;
            parser_fallback(&parsed, &decision, &format!("推理超时({t:?})"))
        }
    }
}

// ============================================================
// 单元测试（TDD：先红后绿）
// ============================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::search::SearchEvent;
    use scout_intent_parser::fallback::IntentSource;
    use scout_model_runtime::{GenerateParams, LlamaModelRuntime, ModelDaemon, ModelError};
    use scout_search_backend::SearchIntent;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tauri::ipc::Channel;

    /// 固定输出 + 可注入延迟的假运行时（Task 5 `from_runtime` 注入口）。
    struct FakeRuntime {
        output: String,
        delay: Duration,
    }
    impl LlamaModelRuntime for FakeRuntime {
        fn generate(&self, _p: &str, _params: &GenerateParams) -> Result<String, ModelError> {
            std::thread::sleep(self.delay);
            Ok(self.output.clone())
        }
    }

    fn ready_handle(output: &str, delay: Duration, timeout: Duration) -> Arc<ModelFallbackHandle> {
        let daemon = Arc::new(ModelDaemon::from_runtime(Box::new(FakeRuntime {
            output: output.to_owned(),
            delay,
        })));
        let fb = scout_intent_parser::fallback::ModelFallback::new(daemon).with_hybrid_mode();
        ModelFallbackHandle::with_ready_for_test(fb, timeout)
    }

    /// 丢弃事件的前端 channel。
    fn noop_channel() -> Channel<SearchEvent> {
        Channel::new(|_| Ok(()))
    }

    /// 取 keywords（断言用）。
    fn keywords_of(intent: &SearchIntent) -> Vec<String> {
        match intent {
            SearchIntent::FileSearch(fs) => fs.keywords.clone().unwrap_or_default(),
            SearchIntent::MediaSearch(ms) => ms.keywords.clone().unwrap_or_default(),
            other => panic!("期望 FileSearch / MediaSearch，得到 {other:?}"),
        }
    }

    // 触发查询：BETA-13-G13 起 fill-empty-only —— FileSearch 的内容词 parser 已能抽，
    // 故 keyword 补全的合法触发收敛到媒体（audio）臂的主题/情境词（parser 留空 keywords）。
    // 本查询经 `media_search_keywords_omission_fires_on_true_omission` 证实触发。
    const TRIGGER_QUERY: &str = "play some indie tracks about rainy nights";

    #[tokio::test]
    async fn use_parser_query_never_touches_model() {
        let handle = Arc::new(ModelFallbackHandle::disabled("测试"));
        let resolved = resolve_with_model("上周的pdf", &handle, &noop_channel()).await;
        assert_eq!(resolved.source, IntentSource::Parser);
    }

    #[tokio::test]
    async fn disabled_handle_falls_back_to_parser() {
        let handle = Arc::new(ModelFallbackHandle::disabled("测试"));
        let resolved = resolve_with_model(TRIGGER_QUERY, &handle, &noop_channel()).await;
        assert_eq!(resolved.source, IntentSource::ParserNoFallback);
        // parser 对该媒体查询未抽 keywords（留空待模型补），禁用 fallback 时维持原样空
        assert!(keywords_of(&resolved.intent).is_empty());
    }

    #[tokio::test]
    async fn ready_model_patches_keywords() {
        let handle = ready_handle(
            r#"{"keywords":["rainy nights"]}"#,
            Duration::ZERO,
            Duration::from_secs(3),
        );
        let resolved = resolve_with_model(TRIGGER_QUERY, &handle, &noop_channel()).await;
        assert_eq!(resolved.source, IntentSource::Model);
        // fill-empty-only：parser 留空 keywords，模型补全并入
        assert_eq!(
            keywords_of(&resolved.intent),
            vec!["rainy nights".to_owned()]
        );
    }

    /// BETA-12：unload 释放 Ready 常驻句柄（→ NotLoaded、文件缺失时快照如实报 not_found）；
    /// Unavailable 不受影响（feature 关的构建 unload 后文案不变）。
    #[test]
    fn unload_drops_ready_and_keeps_unavailable() {
        let handle = ready_handle("{}", Duration::ZERO, Duration::from_secs(3));
        assert_eq!(handle.status_snapshot().state, "ready");
        handle.unload();
        assert_eq!(
            handle.status_snapshot().state,
            "not_found",
            "Ready → NotLoaded 后（默认路径为空）应报 not_found"
        );

        let disabled = ModelFallbackHandle::disabled("测试");
        disabled.unload();
        assert_eq!(disabled.status_snapshot().state, "unavailable");
    }

    #[tokio::test]
    async fn garbage_model_output_falls_back_to_parser() {
        let handle = ready_handle("not json at all", Duration::ZERO, Duration::from_secs(3));
        let resolved = resolve_with_model(TRIGGER_QUERY, &handle, &noop_channel()).await;
        assert_eq!(resolved.source, IntentSource::ParserNoFallback);
        // 模型输出垃圾 → 回落 parser，keywords 维持 parser 原样（空）
        assert!(keywords_of(&resolved.intent).is_empty());
    }

    #[tokio::test]
    async fn slow_model_times_out_to_parser() {
        let handle = ready_handle(
            r#"{"keywords":["会议纪要"]}"#,
            Duration::from_secs(2),
            Duration::from_millis(50),
        );
        let resolved = resolve_with_model(TRIGGER_QUERY, &handle, &noop_channel()).await;
        assert_eq!(resolved.source, IntentSource::ParserNoFallback);
        // 单飞验证：被弃线程仍持有 busy，紧接着的第二次调用也回落 parser
        let resolved2 = resolve_with_model(TRIGGER_QUERY, &handle, &noop_channel()).await;
        assert_eq!(resolved2.source, IntentSource::ParserNoFallback);
    }

    #[tokio::test]
    async fn busy_guard_skips_model() {
        let handle = ready_handle(
            r#"{"keywords":["会议纪要"]}"#,
            Duration::ZERO,
            Duration::from_secs(3),
        );
        handle.busy.store(true, Ordering::Release);
        let resolved = resolve_with_model(TRIGGER_QUERY, &handle, &noop_channel()).await;
        assert_eq!(resolved.source, IntentSource::ParserNoFallback);
    }

    #[test]
    fn model_path_override_from_settings() {
        let dir = std::env::temp_dir().join(format!("scout-mf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let settings_file = dir.join("settings.json");
        std::fs::write(
            &settings_file,
            r#"{"global_shortcut":"x","search_scope":["~"],"enable_model_fallback":true,"enable_tracing":false,"model_path":"/tmp/custom.gguf"}"#,
        )
        .unwrap();
        let handle = ModelFallbackHandle::new(Some(settings_file), dir.clone());
        assert_eq!(
            handle.resolved_model_path(),
            std::path::PathBuf::from("/tmp/custom.gguf")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn status_snapshot_reports_state() {
        let handle = ModelFallbackHandle::disabled("测试禁用");
        let s = handle.status_snapshot();
        assert_eq!(s.state, "unavailable");
        assert!(s.detail.contains("测试禁用"));
    }

    /// NotLoaded + 文件不存在 → not_found，detail 含路径片段（feature 开形态下）。
    #[test]
    fn status_snapshot_not_found_contains_path() {
        if !cfg!(feature = "model-fallback") {
            return; // feature 关时 new() 已 latch 为 Unavailable，本测试仅适用 feature 开形态
        }
        let h = ModelFallbackHandle::new(None, PathBuf::from("/tmp/nonexistent-scout-xyz"));
        let s = h.status_snapshot();
        assert_eq!(s.state, "not_found");
        assert!(s.detail.contains("nonexistent-scout-xyz"));
    }

    /// feature 关时 new() 构造期即 Unavailable——设置页从启动起如实显示，不误导用户放文件。
    #[test]
    fn feature_off_new_is_unavailable_from_start() {
        // 本测试在默认（feature 关）形态下运行：构造期即 Unavailable
        if cfg!(feature = "model-fallback") {
            return; // feature 开形态下跳过
        }
        let h = ModelFallbackHandle::new(None, std::path::PathBuf::from("/tmp/x"));
        assert_eq!(h.status_snapshot().state, "unavailable");
    }

    #[tokio::test]
    async fn loading_state_falls_back_to_parser() {
        let handle = ModelFallbackHandle::with_loading_for_test();
        let r = resolve_with_model(TRIGGER_QUERY, &handle, &noop_channel()).await;
        assert!(matches!(r.source, IntentSource::ParserNoFallback));
    }
}
