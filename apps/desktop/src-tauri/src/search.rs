//! Tauri search command —— UI 与 Scout 核心管道之间的薄桥。
//!
//! MVP-19+ Slice B：走 [`ToolRegistry`] + [`IntentRouter`] + [`PolicyEngine`]，
//! 不再直调 backend。
//!
//! 协议（`SearchEvent`）：
//! - `Started`：先发，UI 立刻刷 intent / signals / fallback / tool_id 摘要
//! - `Result`：每条结果发一次
//! - `Complete`：结束，附 total / elapsed_ms
//! - `Error`：失败时发，前端切到错误态

use std::sync::{Arc, Mutex};
use std::time::Instant;

use scout_harness::context::{ContextMemory, RefineMergeError};
use scout_harness::tracing::SynonymExpandEvent;
use scout_harness::{
    run_fallback_chain, IntentRouter, PolicyAction, PolicyDecision, PolicyEngine, SynonymExpander,
    ToolRegistry,
};
use scout_intent_parser::fallback::IntentSource;
use scout_search_backend::{CancellationToken, SearchIntent};
use serde::Serialize;
use tauri::ipc::Channel;

// BETA-15B-1：embedding 模型句柄（索引/查询期共用 TextEmbedder）。
pub(crate) mod embedding_model;
mod fanout;
mod file_actions;
mod index_status;
// BETA-23：不进通配重导出（CLEAN-1 教训），跨模块以 `model_fallback::` 路径引用。
pub(crate) mod model_fallback;
mod preview;
pub(crate) use fanout::*;
pub(crate) use file_actions::*;
pub(crate) use index_status::*;
pub(crate) use preview::*;

/// 序列化给前端的搜索结果。
#[derive(Debug, Clone, Serialize)]
pub struct SearchResultJson {
    pub id: String,
    pub path: String,
    pub name: String,
    pub source: String,
    /// 命中此结果的全部来源（BETA-04 多源融合：同一文件被多后端命中时列出）。
    /// 单源时与 `source` 一致；fan-out 合并时可含多项（如 spotlight + native_index）。
    pub sources: Vec<String>,
    pub match_type: String,
    pub score: Option<f64>,
    /// BETA-33 cycle 3 v3（v0.9.3）：语义原始 cosine（0-1），仅语义命中有值。
    /// 与 `score` 区分：`score` 融合后是 RRF 累积分（用户看起来像 0.16 且拥挤）；
    /// `semantic_cosine` 是给用户看的真相似度、可评估 `semantic_similarity_floor`
    /// 与按相似度排序。非语义命中或后端未提供时为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_cosine: Option<f64>,
    pub modified_time: Option<String>,
    pub size_bytes: Option<u64>,
}

/// 增量流式事件。前端用 `Channel<SearchEvent>` 接收。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SearchEvent {
    /// 查询开始 — UI 立刻刷 intent / signals / tool_id。
    Started {
        intent_summary: String,
        fallback_used: bool,
        signals: Vec<&'static str>,
        /// 被路由到的工具 id（如 `"search.spotlight"`）。
        tool_id: String,
        /// BETA-29：本轮生效的 Search Intent 完整 JSON（schema wire 格式）。
        /// 前端「意图草稿」据此展示关键字段；用户修正后原样送回 `search_with_intent` 重跑。
        intent_json: serde_json::Value,
    },
    /// 单条结果。
    Result { item: SearchResultJson },
    /// 查询结束 — UI 切到 ready 态。
    Complete { total: usize, elapsed_ms: u64 },
    /// 查询失败 — UI 切到 error 态。
    Error { message: String },
    /// BETA-23：已触发模型 fallback，正在等待模型补全（约 1s）。UI 显示轻量提示。
    ModelThinking,
    /// 主后端失败，已切到下一候选后端。
    BackendSwitched {
        from: String,
        to: String,
        reason: String,
    },
    /// 文件操作(open/locate)执行完成。
    ActionDone {
        /// 动作类型:"open" | "locate"。
        action_kind: String,
        /// 实际涉及的绝对路径。
        paths: Vec<String>,
    },
    /// 写操作待用户确认(copy/move/rename)。UI 弹确认对话框。
    ConfirmAction {
        /// "copy" | "move" | "rename"。
        action_kind: String,
        /// 待操作的源路径(本阶段单个)。
        paths: Vec<String>,
        /// copy/move 的完整目标路径(已解析);rename 为 None。
        destination: Option<String>,
        /// rename 的新名;copy/move 为 None。
        new_name: Option<String>,
    },
}

/// `confirm_action` command 的成功返回。UI 用它切到 action_done 态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActionDoneData {
    /// "copy" | "move" | "rename"。
    pub action_kind: String,
    /// 实际涉及的绝对路径。
    pub paths: Vec<String>,
}

/// 收拢 search/confirm/cancel 命令共享的 7 个进程级依赖，作为单一 `tauri::manage`
/// 状态注入。新增共享依赖只需改本结构体 + `new()` + 用到它的大函数体，
/// 不再触动命令签名与 `main.rs` 的 manage 列表。
pub struct SearchDeps {
    registry: Arc<ToolRegistry>,
    policy: Arc<PolicyEngine>,
    tracer: Arc<scout_harness::Tracer>,
    context: Arc<Mutex<ContextMemory>>,
    file_action_tool: Arc<scout_harness::file_action_tool::FileActionTool>,
    pending: Arc<Mutex<Option<scout_search_backend::FileAction>>>,
    /// 同义词扩展器，在 search_impl 内扩展关键词。
    synonym_expander: Arc<dyn SynonymExpander>,
    /// BETA-06 审计日志（文件操作执行点记录）。`new()` 默认内存日志，main.rs 经
    /// [`with_audit`](Self::with_audit) 换成持久 JSONL；构造不变避免 37 个 new() 调用点改动。
    audit: Arc<dyn scout_harness::AuditLog>,
    /// BETA-07 索引状态（后台/手动 reindex 共享，含并发守卫）。`new()` 默认。
    index_status: Arc<Mutex<IndexStatus>>,
    /// BETA-23 模型 fallback 编排句柄。`new()` 默认 disabled（测试零行为变化），
    /// main.rs 经 [`with_model`](Self::with_model) 注入真句柄。
    model: Arc<model_fallback::ModelFallbackHandle>,
    /// BETA-15B-1 embedding 句柄（索引期文档嵌入 + 查询期语义召回 + 状态命令共用）。
    /// `new()` 默认 `(None, ".")`——feature 关时 Unavailable、开时 NotFound，对测试无副作用。
    /// main.rs 经 [`with_embedding`](Self::with_embedding) 注入与 registry 共享的真句柄。
    embedding: Arc<embedding_model::EmbeddingModelHandle>,
    /// BETA-15B-3 A-2 融合层语义臂权重 provider（live-read settings.json）。
    /// `new()` 默认返 `DEFAULT_SEMANTIC_WEIGHT`；main.rs 经 [`with_weight_provider`] 注入
    /// `settings::read_semantic_weight` 闭包，每次查询读最新值。
    weight_provider: std::sync::Arc<dyn Fn() -> f64 + Send + Sync>,
    /// 2026-07-28：fan-out 融合语义臂 cosine 路由阈值 provider——按当前生效 embedding 模型
    /// 查已校准阈值表（[`embedding_model::cosine_threshold`]），不再是孤立全局常量（详见该
    /// 函数文档：阈值与模型版本结构化绑定，换模型忘补校准记录时告警而非静默沿用旧值）。
    /// `new()` 默认返 `DEFAULT_COSINE_ROUTING_THRESHOLD`；main.rs 经
    /// [`with_threshold_provider`] 注入真闭包。
    threshold_provider: std::sync::Arc<dyn Fn() -> f64 + Send + Sync>,
    /// BETA-39 图片语义索引 opt-in provider（live-read settings.json）。
    /// `new()` 默认返 false（现状一刀切）；main.rs 经 [`with_image_semantics_provider`]
    /// 注入 `settings::read_enable_image_semantics` 闭包，段落级 explain 每次调读最新值。
    image_semantics_provider: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
    /// 2026-07-20：全局复合条件匹配模式 provider（live-read settings.json）。`new()` 默认
    /// 返 `MatchMode::All`（严格全部命中，与 `AppSettings::default()` 口径一致）；main.rs
    /// 经 [`with_match_mode_provider`] 注入 `settings::read_search_match_all_conditions`
    /// 闭包，`search_impl`/预览高亮每次调读最新值。
    match_mode_provider: std::sync::Arc<dyn Fn() -> scout_search_backend::MatchMode + Send + Sync>,
}

impl SearchDeps {
    pub fn new(
        registry: Arc<ToolRegistry>,
        policy: Arc<PolicyEngine>,
        tracer: Arc<scout_harness::Tracer>,
        context: Arc<Mutex<ContextMemory>>,
        file_action_tool: Arc<scout_harness::file_action_tool::FileActionTool>,
        pending: Arc<Mutex<Option<scout_search_backend::FileAction>>>,
        synonym_expander: Arc<dyn SynonymExpander>,
    ) -> Self {
        Self {
            registry,
            policy,
            tracer,
            context,
            file_action_tool,
            pending,
            synonym_expander,
            audit: Arc::new(scout_harness::InMemoryAuditLog::default()),
            index_status: Arc::new(Mutex::new(IndexStatus::default())),
            model: Arc::new(model_fallback::ModelFallbackHandle::disabled("未初始化")),
            embedding: Arc::new(embedding_model::EmbeddingModelHandle::new(
                None,
                std::path::PathBuf::from("."),
            )),
            weight_provider: std::sync::Arc::new(|| {
                scout_result_normalizer::DEFAULT_SEMANTIC_WEIGHT
            }),
            threshold_provider: std::sync::Arc::new(|| {
                scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD
            }),
            image_semantics_provider: std::sync::Arc::new(|| false),
            match_mode_provider: std::sync::Arc::new(|| scout_search_backend::MatchMode::All),
        }
    }

    /// 注入模型 fallback 句柄（main.rs 用；测试可注入 stub 后断言）。
    #[must_use]
    pub fn with_model(mut self, model: Arc<model_fallback::ModelFallbackHandle>) -> Self {
        self.model = model;
        self
    }

    /// 只读访问模型 fallback 句柄。
    pub(crate) fn model(&self) -> &Arc<model_fallback::ModelFallbackHandle> {
        &self.model
    }

    /// 注入 embedding 句柄（main.rs 用，与 registry 内 SemanticIndexBackend 共享同一 Arc）。
    #[must_use]
    pub fn with_embedding(mut self, embedding: Arc<embedding_model::EmbeddingModelHandle>) -> Self {
        self.embedding = embedding;
        self
    }

    /// 只读访问 embedding 句柄（reindex 文档嵌入 + F5 状态命令用）。
    pub(crate) fn embedding(&self) -> &Arc<embedding_model::EmbeddingModelHandle> {
        &self.embedding
    }

    /// 注入 weight provider 闭包（main.rs 用，每次查询 live-read settings.json）。
    #[must_use]
    pub fn with_weight_provider(
        mut self,
        provider: std::sync::Arc<dyn Fn() -> f64 + Send + Sync>,
    ) -> Self {
        self.weight_provider = provider;
        self
    }

    /// 只读：取当前 semantic weight（调闭包 → live-read 设置文件）。
    /// 旁路守护：非有限值（NaN/±∞）或 <=0 一律回落默认，闭包返坏值不会污染 `fuse_rrf`。
    /// 生产路径 `read_semantic_weight` 内已 clamp[0.5, 50.0]，本守护是契约边界兜底（测试 stub / 未来 caller）。
    pub(crate) fn semantic_weight(&self) -> f64 {
        let v = (self.weight_provider)();
        if v.is_finite() && v > 0.0 {
            v
        } else {
            scout_result_normalizer::DEFAULT_SEMANTIC_WEIGHT
        }
    }

    /// 注入 cosine 路由阈值 provider 闭包（main.rs 用，模型感知——见字段文档）。
    #[must_use]
    pub fn with_threshold_provider(
        mut self,
        provider: std::sync::Arc<dyn Fn() -> f64 + Send + Sync>,
    ) -> Self {
        self.threshold_provider = provider;
        self
    }

    /// 只读：取当前 cosine 路由阈值（调闭包 → 按生效模型查校准表）。
    /// 旁路守护同 [`Self::semantic_weight`]：非法值（NaN/±∞ 或越出 `[0.0, 1.0]` cosine 值域）
    /// 一律回落默认，闭包返坏值不会污染 `fuse_rrf_lists_with_fts_routing` 的路由判定。
    pub(crate) fn cosine_threshold(&self) -> f64 {
        let v = (self.threshold_provider)();
        if v.is_finite() && (0.0..=1.0).contains(&v) {
            v
        } else {
            scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD
        }
    }

    /// 注入图片语义 opt-in provider 闭包（BETA-39；main.rs 用，段落级 explain live-read）。
    #[must_use]
    pub fn with_image_semantics_provider(
        mut self,
        provider: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        self.image_semantics_provider = provider;
        self
    }

    /// 只读：图片语义索引 opt-in 当前是否开启（调闭包 → live-read 设置文件）。
    pub(crate) fn image_semantics_enabled(&self) -> bool {
        (self.image_semantics_provider)()
    }

    /// 注入复合条件匹配模式 provider（main.rs 用，每次查询 live-read settings.json）。
    #[must_use]
    pub fn with_match_mode_provider(
        mut self,
        provider: std::sync::Arc<dyn Fn() -> scout_search_backend::MatchMode + Send + Sync>,
    ) -> Self {
        self.match_mode_provider = provider;
        self
    }

    /// 只读：当前全局复合条件匹配模式（调闭包 → live-read 设置文件）。
    pub(crate) fn match_mode(&self) -> scout_search_backend::MatchMode {
        (self.match_mode_provider)()
    }

    /// 注入持久审计日志（main.rs 用 `JsonlAuditLog`；测试可注入 `InMemoryAuditLog` 后断言）。
    #[must_use]
    pub fn with_audit(mut self, audit: Arc<dyn scout_harness::AuditLog>) -> Self {
        self.audit = audit;
        self
    }

    /// 只读访问 registry，供同 crate 的 `status::get_backend_status` 使用。
    pub(crate) fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// 只读访问 synonym_expander，供搜索管道扩展关键词用。
    pub(crate) fn synonym_expander(&self) -> &Arc<dyn SynonymExpander> {
        &self.synonym_expander
    }

    /// 只读访问审计日志。
    pub(crate) fn audit(&self) -> &Arc<dyn scout_harness::AuditLog> {
        &self.audit
    }

    /// 克隆索引状态 Arc（后台启动任务 + 命令共享）。
    pub(crate) fn index_status_arc(&self) -> Arc<Mutex<IndexStatus>> {
        Arc::clone(&self.index_status)
    }
}

/// 主搜索 command:thin wrapper,解 State 后委托 [`search_impl`]。
#[tauri::command]
pub async fn search(
    query: String,
    on_event: Channel<SearchEvent>,
    deps: tauri::State<'_, SearchDeps>,
) -> Result<(), String> {
    search_impl(query, None, on_event, deps.inner()).await
}

/// BETA-29：意图草稿重跑——thin wrapper，解 State 后委托 [`search_with_intent_impl`]。
#[tauri::command]
pub async fn search_with_intent(
    intent: serde_json::Value,
    query: String,
    on_event: Channel<SearchEvent>,
    deps: tauri::State<'_, SearchDeps>,
) -> Result<(), String> {
    search_with_intent_impl(intent, query, on_event, deps.inner()).await
}

/// BETA-11D：零命中教学——用 adhoc 同义词立即重查（不写用户词典）。
#[tauri::command]
pub async fn search_with_adhoc_synonyms(
    query: String,
    head: String,
    aliases: Vec<String>,
    on_event: Channel<SearchEvent>,
    deps: tauri::State<'_, SearchDeps>,
) -> Result<(), String> {
    search_impl(query, Some((head, aliases)), on_event, deps.inner()).await
}

/// BETA-11D：把一次性 adhoc 同义词组注入 ExpandedSearchIntent（不落盘）。覆盖同 head 组，
/// 否则插到最前。用于零命中教学的「立即重查但不沉淀」。
pub(crate) fn inject_adhoc_group(
    mut expanded: scout_search_backend::ExpandedSearchIntent,
    head: &str,
    aliases: Vec<String>,
) -> scout_search_backend::ExpandedSearchIntent {
    use scout_search_backend::KeywordGroup;
    let mut synonyms = Vec::new();
    for a in aliases {
        let a = a.trim().to_owned();
        if !a.is_empty() && a != head && !synonyms.contains(&a) {
            synonyms.push(a);
        }
    }
    let group = KeywordGroup {
        head: head.to_owned(),
        synonyms,
    };
    if let Some(slot) = expanded.keyword_groups.iter_mut().find(|g| g.head == head) {
        *slot = group;
    } else {
        expanded.keyword_groups.insert(0, group);
    }
    expanded
}

/// search command 主体。不依赖 [`tauri::State`],可被单测注入 mock 调用。
///
/// `adhoc`：BETA-11D 零命中教学用的一次性同义词组（head, aliases），不落盘。
/// 传 `None` 走普通搜索路径（行为不变）。
///
/// 返回 `Result<(), String>` 仅表示"任务派发是否成功"；查询结果与失败均通过
/// `on_event` 流式投递(包括 `SearchEvent::Error`)。
pub(crate) async fn search_impl(
    query: String,
    adhoc: Option<(String, Vec<String>)>,
    on_event: Channel<SearchEvent>,
    deps: &SearchDeps,
) -> Result<(), String> {
    let start = Instant::now();

    // BETA-31-v3 cycle 2：诊断打点（query + adhoc 标志）。query 完整记日志便于复现诊断；
    // 日志文件仅在用户本机、不外传——上下文用户已知。
    tracing::info!(
        query = %query,
        query_len = query.chars().count(),
        adhoc = adhoc.is_some(),
        "search 入口"
    );

    // 1) NL → intent（BETA-23：parser 优先；结构性遗漏且模型可用时 hybrid 补全；永不失败）
    let resolved = model_fallback::resolve_with_model(&query, deps.model(), &on_event).await;
    let scout_intent_parser::fallback::ResolvedIntent {
        intent,
        source,
        signals,
        ..
    } = resolved;

    // 2) Refine 合并:Refine → 合并上一轮基准；其余原样。pre-tool 失败不进 trace。
    let effective = {
        let guard = deps.context.lock().unwrap_or_else(|e| e.into_inner());
        apply_refine_if_needed(intent, &guard)
    };
    let effective = match effective {
        Ok(i) => i,
        Err(err) => {
            let message = match err {
                RefineMergeError::NoLastIntent => {
                    "没有可细化的上一轮搜索，请先发起一次搜索".to_owned()
                }
                other => other.to_string(),
            };
            eprintln!("search: refine 合并失败: {message}");
            let _ = on_event.send(SearchEvent::Error { message });
            return Ok(());
        }
    };

    run_resolved_search(
        effective, source, signals, query, adhoc, on_event, deps, start,
    )
    .await
}

/// BETA-29：意图草稿重跑 command 主体（便于单测注入 deps）。
///
/// 前端把用户修正后的 Search Intent JSON 原样送回：serde 强校验（`deny_unknown_fields` +
/// 类型化枚举）反序列化，**不绕过 schema**；仅接受 `file_search` / `media_search`
/// （草稿 UI 只对搜索意图开放编辑，action/refine/clarify 不走此口）。校验通过后跳过
/// parser / 模型 / refine 合并，直接进共同执行管线（policy / 同义词扩展 / 路由照旧）。
/// 校验失败经 `SearchEvent::Error` 投递（与 `search_impl` 的失败口径一致）。
pub(crate) async fn search_with_intent_impl(
    intent: serde_json::Value,
    query: String,
    on_event: Channel<SearchEvent>,
    deps: &SearchDeps,
) -> Result<(), String> {
    let start = Instant::now();
    let parsed: SearchIntent = match serde_json::from_value(intent) {
        Ok(i) => i,
        Err(e) => {
            let _ = on_event.send(SearchEvent::Error {
                message: format!("意图草稿不合法: {e}"),
            });
            return Ok(());
        }
    };
    if !matches!(
        parsed,
        SearchIntent::FileSearch(_) | SearchIntent::MediaSearch(_)
    ) {
        let _ = on_event.send(SearchEvent::Error {
            message: "意图草稿仅支持 file_search / media_search".to_owned(),
        });
        return Ok(());
    }
    tracing::info!(
        query = %query,
        intent = %describe_intent(&parsed),
        "search_with_intent 入口（BETA-29 意图草稿重跑）"
    );
    run_resolved_search(
        parsed,
        IntentSource::Parser,
        scout_intent_parser::signals::CandidateSignals::default(),
        query,
        None,
        on_event,
        deps,
        start,
    )
    .await
}

/// BETA-29 v2：搜索前意图预览的返回载荷。
#[derive(Debug, Clone, Serialize)]
pub struct IntentPreview {
    /// 是否可在草稿面板编辑（仅 file_search / media_search）。
    pub supported: bool,
    /// 人读摘要（与 `Started.intent_summary` 同口径）。
    pub intent_summary: String,
    /// schema wire 格式 intent JSON（`supported=false` 时仍回带，供前端提示）。
    pub intent_json: serde_json::Value,
}

/// BETA-29 v2：搜索前预览意图草稿——只解析不执行。thin wrapper。
#[tauri::command]
pub fn preview_intent(
    query: String,
    deps: tauri::State<'_, SearchDeps>,
) -> Result<IntentPreview, String> {
    preview_intent_impl(&query, deps.inner())
}

/// BETA-29 v2：搜索前预览主体（便于单测注入 deps）。
///
/// 走 parser + Refine 合并（与 [`search_impl`] 步骤 1-2 同构），**不含模型 fallback**
/// （预览要求同步快速返回；模型补全仍只在真正搜索时发生——草稿面板本就是修正入口，
/// parser 视角即预览语义）。不执行搜索、不 record、不进 ContextMemory。
/// Refine 无上一轮基准时与搜索同款文案报错。
pub(crate) fn preview_intent_impl(query: &str, deps: &SearchDeps) -> Result<IntentPreview, String> {
    let q = query.trim();
    if q.is_empty() {
        return Err("查询不能为空".to_owned());
    }
    let intent = scout_intent_parser::parse(q);
    let effective = {
        let guard = deps.context.lock().unwrap_or_else(|e| e.into_inner());
        apply_refine_if_needed(intent, &guard)
    }
    .map_err(|err| match err {
        RefineMergeError::NoLastIntent => "没有可细化的上一轮搜索，请先发起一次搜索".to_owned(),
        other => other.to_string(),
    })?;
    let supported = matches!(
        effective,
        SearchIntent::FileSearch(_) | SearchIntent::MediaSearch(_)
    );
    Ok(IntentPreview {
        supported,
        intent_summary: describe_intent(&effective),
        intent_json: intent_to_json(&effective),
    })
}

/// intent 已定（parser / 模型 hybrid / BETA-29 用户草稿）后的共同执行管线：
/// FileAction 分支 → policy gate → 同义词扩展 → 路由（balanced / fanout / chain）→
/// 流式投递 + ContextMemory record。从 [`search_impl`] 原样抽出，行为不变。
#[allow(clippy::too_many_arguments)]
async fn run_resolved_search(
    effective: SearchIntent,
    source: IntentSource,
    signals: scout_intent_parser::signals::CandidateSignals,
    query: String,
    adhoc: Option<(String, Vec<String>)>,
    on_event: Channel<SearchEvent>,
    deps: &SearchDeps,
    start: Instant,
) -> Result<(), String> {
    // 2.5) FileAction 分支:交给 handle_file_action(含路由 copy/move/rename 确认往返)。
    //      其余 intent 落到下方现有 search 路径。
    if let SearchIntent::FileAction(ref fa) = effective {
        let action = fa.clone();
        return handle_file_action(action, on_event, deps).await;
    }

    // 3) Policy gate(跑在合并后的 effective 上)
    let action = PolicyAction::from(&effective);
    match deps.policy.evaluate(&action) {
        PolicyDecision::Allow => {}
        PolicyDecision::Deny { reason } => {
            eprintln!("search: policy 拒绝: {reason}");
            let _ = on_event.send(SearchEvent::Error {
                message: format!("policy denied: {reason}"),
            });
            return Ok(());
        }
        PolicyDecision::RequireConfirmation => {
            eprintln!("search: 不应触发 RequireConfirmation(intent 路由 bug)");
            let _ = on_event.send(SearchEvent::Error {
                message: "search 不应触发 RequireConfirmation".to_owned(),
            });
            return Ok(());
        }
    }

    // 4) 同义词扩展（必须先于路由）：把 effective intent 扩为 ExpandedSearchIntent。
    //    先扩展，能力感知路由才能据扩展后的内容关键词组选后端——包括 parser 未抽出 keyword、
    //    由 gazetteer（BETA-15E）兜底注入的内容词；否则无 keyword 的 base 会错误落到只索引
    //    文件名的 Everything（命中 match-all 垃圾结果）。
    //    effective.clone() 保留原 intent 供后续 ContextMemory.record 使用（走 expanded.base）。
    //    BETA-11D：adhoc 注入在 expand 之后、路由之前，保证 adhoc OR 组影响能力感知路由。
    let expanded = {
        let e = deps.synonym_expander().expand(effective.clone(), &query);
        let e = match &adhoc {
            Some((head, aliases)) => inject_adhoc_group(e, head, aliases.clone()),
            None => e,
        };
        // 2026-07-20：全局复合条件匹配模式（live-read settings.json），四个检索后端统一读取。
        e.with_match_mode(deps.match_mode())
    };

    let router = IntentRouter::new(&deps.registry);

    // BETA-19 跨范畴均衡：FileSearch 含 ≥2 个不同 file_type（「图片和视频」「ppt和pdf」）→
    //   按类型分别查询 + round-robin 交错，避免少数派类型被单后端 limit + modified_desc 截断后不可见。
    //   单类型 / 非 FileSearch → 跳过，沿用下方原路径（零行为变化）。
    if let Some(types) = multi_file_types(&expanded.base) {
        return run_balanced_multitype_search(
            types,
            ResolvedQuery {
                effective,
                expanded,
                source,
                signals: &signals,
                raw_query: &query,
            },
            on_event,
            deps,
            start,
        )
        .await;
    }

    // BETA-04 fan-out 分流：内容/媒体查询且 ≥2 个可一起查的后端（系统搜索 + 本地索引）
    //   → 多源 fan-out 合并；纯文件名 / 单后端 → 沿用下方 fallback 链（行为不变）。
    if let Some(backends) = router
        .route_search_fanout(&expanded)
        .ok()
        .filter(|b| b.len() >= 2)
    {
        return run_fanout_search(
            backends,
            ResolvedQuery {
                effective,
                expanded,
                source,
                signals: &signals,
                raw_query: &query,
            },
            on_event,
            deps,
            start,
        )
        .await;
    }

    // 5) Intent → 有序候选列表（能力感知路由：扩展后含内容关键词组 → 内容型后端优先）
    let candidates = match router.route_search_chain(&expanded) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("search: 无可用 tool: {err}");
            let _ = on_event.send(SearchEvent::Error {
                message: err.to_string(),
            });
            return Ok(());
        }
    };

    let first_id = candidates[0].id().to_owned();
    let tool_start = Instant::now();

    // Trace A: tool 即将被调用（用首候选 id）
    deps.tracer.on_tool_call(&scout_harness::ToolCallEvent {
        tool_id: first_id.clone(),
        tool_kind: scout_harness::ToolKind::Search,
        intent_variant: scout_harness::SupportedIntent::from_intent(&effective),
    });

    // 6) Started 事件
    let _ = on_event.send(SearchEvent::Started {
        intent_summary: describe_intent(&effective),
        fallback_used: matches!(source, IntentSource::Model),
        signals: signals_to_labels(&signals),
        tool_id: first_id.clone(),
        intent_json: intent_to_json(&effective),
    });

    // 7) 对每个非 singleton 组发 SynonymExpandEvent（singleton 表示未命中词典，不发事件）。
    emit_synonym_events(&expanded, &deps.tracer);

    // 6c) chain 驱动：逐候选 fallback，跨候选去重，流式投递结果。
    // run_fallback_chain 泛型化后（R: FnMut + Send, S: FnMut + Send），
    // future 满足 Tauri command 的 Send 要求。
    let cancel = CancellationToken::new();
    // record 需要原始 SearchResult；边发 UI 事件边累积。
    let mut recorded: Vec<scout_search_backend::SearchResult> = Vec::new();
    let mut send_failed = false;

    let outcome = {
        let on_event_ref = &on_event;
        let recorded_ref = &mut recorded;
        let send_failed_ref = &mut send_failed;
        let tracer_ref = &deps.tracer;

        let mut on_result = move |result: scout_search_backend::SearchResult| {
            // fallback 链单源：sources 即该结果自身来源。
            let sources = vec![format!("{:?}", result.source).to_lowercase()];
            // fallback 单源无融合：SemanticIndex 时 result.score 就是 raw cosine，直传给前端。
            let sem_cos = if result.source == scout_search_backend::BackendKind::SemanticIndex {
                result.score
            } else {
                None
            };
            let json = result_to_json(&result, sources, sem_cos);
            recorded_ref.push(result);
            if !*send_failed_ref
                && on_event_ref
                    .send(SearchEvent::Result { item: json })
                    .is_err()
            {
                *send_failed_ref = true;
            }
        };
        let mut on_switch = move |sw: scout_harness::BackendSwitch| {
            tracer_ref.on_error(&scout_harness::ToolErrorEvent {
                tool_id: sw.from.clone(),
                duration: tool_start.elapsed(),
                error_type: format!("fallback_switch:{}", sw.reason.as_str()),
            });
            let _ = on_event_ref.send(SearchEvent::BackendSwitched {
                from: sw.from,
                to: sw.to,
                reason: sw.reason.as_str().to_owned(),
            });
        };

        run_fallback_chain(
            &candidates,
            &expanded,
            cancel,
            &mut on_result,
            &mut on_switch,
        )
        .await
        // on_result/on_switch drop，借用 recorded_ref/send_failed_ref 释放
    };
    let _ = send_failed; // UI 通道断开仅停止后续投递，不改终止语义

    // on_tool_result 归属实际服务后端（I-1）：served_by 成功停链时为实际服务者 id，
    // 全失败/无干净成功时为 None，此时回落 first_id（查询以首候选开始，语义一致）。
    let result_tool_id = outcome.served_by.as_deref().unwrap_or(&first_id).to_owned();
    deps.tracer.on_tool_result(&scout_harness::ToolResultEvent {
        tool_id: result_tool_id,
        duration: tool_start.elapsed(),
        result_count: outcome.total,
    });

    if outcome.total == 0 {
        // 0 命中不是错误：可能是真无匹配，也可能是链上某个可选后端（如 Everything
        // 未启动）不可用而被跳过——两者对用户都应呈现为「没有命中结果」的空态
        // （前端按 total==0 走既有空态 UI），而非错误态。last_error 仅落诊断日志。
        if let Some(err) = &outcome.last_error {
            tracing::debug!(last_error = %err, "fallback chain 零命中（含可选后端不可用）");
        }
        let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let _ = on_event.send(SearchEvent::Complete {
            total: 0,
            elapsed_ms,
        });
        return Ok(());
    }

    // 8) 成功完成 → 记录本轮为新的 last turn(渐进收窄链的基准)
    //    用 expanded.base（等同于扩展前的 effective），保留语义一致性。
    {
        let mut guard = deps.context.lock().unwrap_or_else(|e| e.into_inner());
        guard.record(expanded.base, recorded);
    }

    let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::info!(
        path = "fallback-chain",
        total = outcome.total,
        elapsed_ms,
        served_by = ?outcome.served_by,
        "search 出口"
    );
    let _ = on_event.send(SearchEvent::Complete {
        total: outcome.total,
        elapsed_ms,
    });
    Ok(())
}

/// 把 `SearchResult` + 来源列表映射为前端 JSON。两条路径（fallback / fan-out）共用。
///
/// BETA-33 cycle 3 v3：`semantic_cosine` 是可选传参，fan-out 走融合后从 `MergedResult` 拿、
/// fallback 走单源无融合时对 SemanticIndex 直接从 `result.score` 拿（此时 score 未被 RRF 覆盖）。
fn result_to_json(
    result: &scout_search_backend::SearchResult,
    sources: Vec<String>,
    semantic_cosine: Option<f64>,
) -> SearchResultJson {
    SearchResultJson {
        id: result.id.clone(),
        path: result.path.to_string_lossy().into_owned(),
        name: result.name.clone(),
        source: format!("{:?}", result.source).to_lowercase(),
        sources,
        match_type: format!("{:?}", result.match_type).to_lowercase(),
        score: result.score,
        semantic_cosine,
        modified_time: result.metadata.modified_time.map(|t| t.to_rfc3339()),
        size_bytes: result.metadata.size_bytes,
    }
}

/// 对每个非 singleton 同义词组发 [`SynonymExpandEvent`]（singleton = 未命中词典，不发）。
fn emit_synonym_events(
    expanded: &scout_search_backend::ExpandedSearchIntent,
    tracer: &scout_harness::Tracer,
) {
    if expanded.is_identity() {
        return;
    }
    for group in &expanded.keyword_groups {
        if !group.is_singleton() {
            tracer.on_synonym_expand(&SynonymExpandEvent {
                head: group.head.clone(),
                group: group.all().into_iter().map(String::from).collect(),
                source: guess_source(&group.head),
                // TODO(BETA-11+): cap_keyword_groups 的 _warn_truncated 当前被吞，
                // 需让 ExpandedSearchIntent 携带 truncated 标志才能精确上报。
                truncated: false,
            });
        }
    }
}

/// fan-out 分支的「已解析查询」上下文（避免 `run_fanout_search` 参数过多）。
pub(crate) struct ResolvedQuery<'a> {
    effective: SearchIntent,
    expanded: scout_search_backend::ExpandedSearchIntent,
    source: IntentSource,
    signals: &'a scout_intent_parser::signals::CandidateSignals,
    /// BETA-15B-3 A-4：原始 query 字符串（用户输入框文本）供 `run_fanout_merge_rrf` 入口
    /// `detect_lang` 判 query 语种；balanced multitype 分支不使用（不入 RRF wrapper）。
    raw_query: &'a str,
}

/// 确认并执行待确认的 file action。
#[tauri::command]
pub async fn confirm_action(deps: tauri::State<'_, SearchDeps>) -> Result<ActionDoneData, String> {
    confirm_action_impl(deps.inner())
}

/// 取消待确认的 file action(清空 pending 槽)。
#[tauri::command]
pub async fn cancel_action(deps: tauri::State<'_, SearchDeps>) -> Result<(), String> {
    cancel_action_impl(&deps.pending);
    Ok(())
}

/// BETA-06：序列化给前端的一条审计记录。
#[derive(Debug, Clone, Serialize)]
pub struct AuditEntryJson {
    pub timestamp: String,
    pub operation: String,
    pub source_paths: Vec<String>,
    pub destination: Option<String>,
    pub new_name: Option<String>,
    pub result: String,
    pub error: Option<String>,
}

fn audit_entry_to_json(e: &scout_harness::AuditEntry) -> AuditEntryJson {
    AuditEntryJson {
        timestamp: e.timestamp.to_rfc3339(),
        operation: format!("{:?}", e.operation).to_lowercase(),
        source_paths: e.source_paths.clone(),
        destination: e.destination.clone(),
        new_name: e.new_name.clone(),
        result: format!("{:?}", e.result).to_lowercase(),
        error: e.error.clone(),
    }
}

/// 读全部审计记录，newest-first。不依赖 [`tauri::State`]，可单测。
fn get_audit_log_impl(deps: &SearchDeps) -> Vec<AuditEntryJson> {
    let mut entries = deps.audit().read_all();
    entries.reverse(); // newest-first
    entries.iter().map(audit_entry_to_json).collect()
}

/// 查看操作记录（审计日志），newest-first。
#[tauri::command]
pub async fn get_audit_log(
    deps: tauri::State<'_, SearchDeps>,
) -> Result<Vec<AuditEntryJson>, String> {
    Ok(get_audit_log_impl(deps.inner()))
}

/// 清除全部操作记录。
#[tauri::command]
pub async fn clear_audit_log(deps: tauri::State<'_, SearchDeps>) -> Result<(), String> {
    deps.audit().clear();
    Ok(())
}

/// BETA-07：查当前索引状态（正在索引 / 上次索引时间 + 摘要）。
#[tauri::command]
pub async fn get_index_status(deps: tauri::State<'_, SearchDeps>) -> Result<IndexStatus, String> {
    Ok(index_status_snapshot(deps.inner()))
}

/// BETA-23：设置页查询模型状态。
#[tauri::command]
pub fn get_model_status(deps: tauri::State<'_, SearchDeps>) -> model_fallback::ModelStatusJson {
    deps.model().status_snapshot()
}

/// BETA-15B-1（F5）：设置页 / 隐私面板查询 embedding 模型状态
/// （ready / loading / not_found / failed / unavailable）。
#[tauri::command]
pub async fn embedding_model_status(
    deps: tauri::State<'_, SearchDeps>,
) -> Result<embedding_model::EmbedStatus, String> {
    Ok(deps.embedding().status())
}

/// 设置页「检测」按钮的返回：给定模型文件路径是否可用。
///
/// 纯文件系统检查（不加载模型、不触发推理）：存在性 + 后缀 + 体积下限。
/// 为把语义 / 生成模型指向更强的本地模型或局域网可信模型路径提供"落地前"的
/// 可用性反馈——用户填好自定义路径即可先检测、再应用生效。
#[derive(Debug, serde::Serialize)]
pub struct ModelProbe {
    /// 实际检测的路径（空输入回显为空串，由前端提示"使用默认位置"）。
    pub path: String,
    /// 路径存在且是文件。
    pub exists: bool,
    /// 文件字节数（不存在为 0）。
    pub size_bytes: u64,
    /// 后缀是 gguf（大小写不敏感）。
    pub is_gguf: bool,
    /// 综合判定：存在 + gguf 后缀 + 体积达下限。
    pub usable: bool,
    /// 人话结论（前端直接展示）。
    pub message: String,
}

/// 体积下限：挡住空文件 / 占位文件（真实 gguf 均在数十 MB 以上）。
const PROBE_MIN_BYTES: u64 = 1024 * 1024;

/// 纯文件系统检查结果（存在性 + 后缀 + 体积），[`probe_model_file`] 两条分支
/// （用户填的覆盖路径 / 默认槽路径）共用，避免同一套判定逻辑写两遍。
struct FileProbe {
    exists: bool,
    size_bytes: u64,
    is_gguf: bool,
    usable: bool,
}

fn probe_path(p: &std::path::Path) -> FileProbe {
    let is_gguf = p
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gguf"));
    let meta = std::fs::metadata(p).ok();
    let exists = meta.as_ref().is_some_and(std::fs::Metadata::is_file);
    let size_bytes = meta.as_ref().map_or(0, std::fs::Metadata::len);
    let usable = exists && is_gguf && size_bytes >= PROBE_MIN_BYTES;
    FileProbe {
        exists,
        size_bytes,
        is_gguf,
        usable,
    }
}

/// 整数算 MB（保留一位小数），避开 f64 转换的精度损失 lint。
fn size_label_mb(size_bytes: u64) -> String {
    let tenths = size_bytes * 10 / PROBE_MIN_BYTES;
    format!("{}.{} MB", tenths / 10, tenths % 10)
}

/// 检测指定路径的模型文件是否可用（设置页「检测」按钮，2026-07-07）。
///
/// `kind`（`"embedding"` / `"generation"`）决定路径留空时探测哪个模型的默认槽——
/// 2026-08 起不再回一句静态的"将使用默认模型位置"，而是像非空路径那样真的去
/// 探测默认槽文件、报告"已使用默认模型 · 文件名 · 体积"或"默认模型尚未下载"，
/// 默认路径复用 [`crate::model_download::resolve_target_paths`]（与实际加载路径
/// 同一信源，见该函数文档）。
#[tauri::command]
pub fn probe_model_file(path: String, kind: String) -> ModelProbe {
    let trimmed = path.trim();

    if trimmed.is_empty() {
        let model_kind = match crate::model_download::parse_kind(&kind) {
            Ok(k) => k,
            Err(message) => {
                return ModelProbe {
                    path: String::new(),
                    exists: false,
                    size_bytes: 0,
                    is_gguf: false,
                    usable: false,
                    message,
                };
            }
        };
        let (_dir, default_path, _partial) =
            crate::model_download::resolve_target_paths(model_kind);
        let filename = default_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let probe = probe_path(&default_path);
        let message = if probe.usable {
            format!(
                "已使用默认模型 · {filename} · {}",
                size_label_mb(probe.size_bytes)
            )
        } else if probe.exists {
            format!(
                "默认模型路径下的文件疑似损坏或不完整（{filename}）——建议在上方「模型管理」重新下载"
            )
        } else {
            format!(
                "默认模型尚未下载（{filename}）——可在上方「模型管理」下载，或在此指定自定义路径"
            )
        };
        return ModelProbe {
            path: default_path.display().to_string(),
            exists: probe.exists,
            size_bytes: probe.size_bytes,
            is_gguf: probe.is_gguf,
            usable: probe.usable,
            message,
        };
    }

    let p = std::path::Path::new(trimmed);
    let probe = probe_path(p);
    let message = if !probe.exists {
        "文件不存在或不可访问".to_owned()
    } else if !probe.is_gguf {
        "文件存在，但后缀不是 gguf（可能不是模型文件）".to_owned()
    } else if probe.size_bytes < PROBE_MIN_BYTES {
        format!("文件过小（{} 字节），可能不是完整模型", probe.size_bytes)
    } else {
        format!("可用 · {}", size_label_mb(probe.size_bytes))
    };
    ModelProbe {
        path: trimmed.to_owned(),
        exists: probe.exists,
        size_bytes: probe.size_bytes,
        is_gguf: probe.is_gguf,
        usable: probe.usable,
        message,
    }
}

/// 打开指定路径的文件(UI 双击行)。
#[tauri::command]
pub async fn open_path(
    path: String,
    deps: tauri::State<'_, SearchDeps>,
) -> Result<ActionDoneData, String> {
    run_path_action(
        scout_search_backend::FileActionKind::Open,
        path,
        deps.inner(),
    )
}

/// 在系统文件管理器中显示指定路径(UI 右键「在文件夹中显示」)。
#[tauri::command]
pub async fn locate_path(
    path: String,
    deps: tauri::State<'_, SearchDeps>,
) -> Result<ActionDoneData, String> {
    run_path_action(
        scout_search_backend::FileActionKind::Locate,
        path,
        deps.inner(),
    )
}

/// BETA-20：取选中结果的预览数据（音频元数据 / 文档·OCR 正文 + 命中高亮）。
/// `query` 为当前搜索原文（用于命中片段高亮，可空）。**只读本地索引，不读原文件、不进 trace**。
#[tauri::command]
pub async fn get_preview(
    path: String,
    query: Option<String>,
    deps: tauri::State<'_, SearchDeps>,
) -> Result<PreviewPayload, String> {
    Ok(get_preview_impl(&path, query.as_deref(), deps.inner()))
}

/// BETA-15B-5：取选中语义结果的「命中段落」高亮区间（字符偏移 + 真 cosine）。
/// **只读本地索引、不读原文件、不进 trace**。无模型 / feature 关 → 空。
#[tauri::command]
pub async fn explain_semantic_hit(
    path: String,
    query: String,
    deps: tauri::State<'_, SearchDeps>,
) -> Result<ExplainPayload, String> {
    Ok(explain_semantic_hit_impl(&path, &query, deps.inner()))
}

/// BETA-29：intent → schema wire 格式 JSON（`Started.intent_json` 用）。
/// SearchIntent 的 Serialize 按构造不失败；防御性兜底 Null（前端判空隐藏草稿入口）。
pub(crate) fn intent_to_json(intent: &SearchIntent) -> serde_json::Value {
    serde_json::to_value(intent).unwrap_or(serde_json::Value::Null)
}

fn describe_intent(intent: &SearchIntent) -> String {
    match intent {
        SearchIntent::FileSearch(_) => "file_search".to_owned(),
        SearchIntent::MediaSearch(_) => "media_search".to_owned(),
        SearchIntent::FileAction(_) => "file_action".to_owned(),
        SearchIntent::Refine(_) => "refine".to_owned(),
        SearchIntent::Clarify(c) => format!("clarify: {}", c.question),
    }
}

fn signals_to_labels(
    signals: &scout_intent_parser::signals::CandidateSignals,
) -> Vec<&'static str> {
    let mut out = Vec::new();
    if signals.time {
        out.push("time");
    }
    if signals.size {
        out.push("size");
    }
    if signals.sort {
        out.push("sort");
    }
    if signals.location {
        out.push("location");
    }
    if signals.action {
        out.push("action");
    }
    if signals.media {
        out.push("media");
    }
    out
}

/// 返回 SearchError variant 名,不含 detail(避免泄路径)。供 trace 用。
/// chain 接管流错误处理后主路径不再调用，但单测仍覆盖此函数。
#[cfg_attr(not(test), allow(dead_code))]
fn search_error_kind(err: &scout_search_backend::SearchError) -> &'static str {
    use scout_search_backend::SearchError;
    match err {
        SearchError::BackendUnavailable { .. } => "BackendUnavailable",
        SearchError::PermissionDenied { .. } => "PermissionDenied",
        SearchError::InvalidIntent { .. } => "InvalidIntent",
        SearchError::UnsupportedIntent { .. } => "UnsupportedIntent",
        SearchError::Timeout { .. } => "Timeout",
        SearchError::Io { .. } => "Io",
    }
}

/// 根据 keyword 的字符集猜测词典来源（仅用于 trace event 的 source 字段）。
/// 覆盖 CJK 基本区 + Extension A + 兼容汉字 + 日文假名;其余视为英文。
/// 注:这是启发式,不影响实际查找路径(查找直接走 zh/en 双 index)。
fn guess_source(keyword: &str) -> String {
    let is_cjk = |c: char| -> bool {
        matches!(
            c as u32,
            0x3040..=0x30FF      // 日文平假名 + 片假名
            | 0x3400..=0x4DBF    // CJK Extension A
            | 0x4E00..=0x9FFF    // CJK Unified Ideographs
            | 0xF900..=0xFAFF    // CJK Compatibility Ideographs
        )
    };
    if keyword.chars().any(is_cjk) {
        "zh.yaml".to_string()
    } else {
        "en.yaml".to_string()
    }
}

#[cfg(test)]
mod tests;
