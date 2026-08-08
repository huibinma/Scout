//! `search` tool —— 自然语言查询 -> hybrid 检索 hit 列表。
//!
//! BETA-32 T6 接 `packages/harness::fallback_chain` 真实流程：
//! `intent_parser::parse` → `ExpandedSearchIntent::identity` → `LocalIndexBackend`
//! 包装为 `SearchableTool` → `run_fallback_chain` → `merge_results` → `ranker::rank`
//! → 截断 limit 后 jsonify。
//!
//! BETA-36 升级为 **collection 感知**：入参可选 `collections`（缺省 = token 授权的
//! 全部）；每个目标 collection 用自己的候选链（独立 index.db，物理信息墙）跑链，
//! 结果合并统一 rank；命中带 `collection` 字段。请求了未授权 / 不存在的 collection
//! → [`ToolError::Denied`]（消息只回显请求的 id，不泄漏该 id 是否存在——两种情况
//! 同文案）。
//!
//! 失败链路：parser clarify / refine / `file_action` 等非检索 intent → 返空 results；
//! 不可用 backend / 0 命中 → 同空 results。`degraded` = 所有目标集合的链都没有干净
//! 成功（db 不存在 / 无可用候选）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use scout_harness::{
    run_fallback_chain, run_fanout_merge_rrf, SearchTool as HarnessSearchTool, SearchableTool,
    SupportedIntent,
};
use scout_indexer::embed::TextEmbedder;
use scout_local_index_backend::LocalIndexBackend;
use scout_ranker::{rank, RankContext};
use scout_result_normalizer::{merge_results, MergedResult};
// 重导出给 scoutd main / evals 用（daemon `--semantic-weight` 的缺省值单一信源）。
pub use scout_result_normalizer::DEFAULT_SEMANTIC_WEIGHT;
use scout_search_backend::{
    BackendKind, CancellationToken, ExpandedSearchIntent, KeywordGroup, SearchIntent, SearchResult,
};
use scout_semantic_index::SemanticIndexBackend;

use super::{Tool, ToolError};
use crate::audit::{AuditAction, AuditRecord};
use crate::auth::AuthedPrincipal;
use crate::config::ServerCtx;

/// `limit` 上限 —— MCP 客户端给的值再大也截到这里、防滥用。
pub const HARD_LIMIT_CAP: usize = 50;
/// `limit` 缺省值。
pub const DEFAULT_LIMIT: usize = 20;
const INDEXING_IN_PROGRESS_NOTE: &str = "本地索引正在构建中，结果可能不完整，建议稍后重试。";
/// daemon 语义臂相似度下限（镜像 desktop `DEFAULT_SIMILARITY_FLOOR`；daemon 无
/// settings.json，用编译期常量。低于此 cosine 的候选在融合前过滤）。
pub const DAEMON_SIMILARITY_FLOOR: f32 = 0.30;

#[derive(Deserialize)]
struct SearchInput {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
    /// 目标 collection id 列表；缺省 = token 授权的全部。
    #[serde(default)]
    collections: Option<Vec<String>>,
}

#[derive(Serialize)]
struct SearchHit {
    path: String,
    name: String,
    /// 命中所属 collection id（BETA-36；BETA-43 出处三要素之一）。
    collection: String,
    /// 字节数；metadata 缺时 `None`、字段不出 JSON（与 desktop UI Option 语义一致）。
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    /// UNIX 秒；metadata 缺时 `None`，避免与 epoch 1970 撞车（reviewer Important #4）。
    #[serde(skip_serializing_if = "Option::is_none")]
    mtime: Option<i64>,
    score: f64,
    /// 出处片段（BETA-43 验收 ①）：正文中关键词命中的上下文窗口。索引无正文
    /// （音乐 / db 缺失）或语义命中无字面词 → 缺省不出 JSON。
    #[serde(skip_serializing_if = "Option::is_none")]
    snippet: Option<String>,
    /// 命中回页（BETA-43 验收 ①，复用 BETA-35 `document_passages` 来源映射）：
    /// 扫描件中含关键词的页号（起于 1）。非扫描件 → 空、不出 JSON。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pages: Vec<u32>,
}

#[derive(Serialize)]
struct SearchOutput {
    results: Vec<SearchHit>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    degraded: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    indexing_in_progress: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'static str>,
}

/// `search` tool —— 自然语言查询包装。
#[derive(Debug)]
pub struct SearchTool;

fn search_output(
    results: Vec<SearchHit>,
    degraded: bool,
    indexing_in_progress: bool,
) -> SearchOutput {
    SearchOutput {
        results,
        degraded,
        indexing_in_progress,
        note: indexing_in_progress.then_some(INDEXING_IN_PROGRESS_NOTE),
    }
}

/// 解析目标 collection：显式请求逐个校验授权与存在性（两种失败同文案，防探测）；
/// 缺省 = principal 授权且已声明的全部。
fn resolve_target_ids(
    ctx: &ServerCtx,
    principal: &AuthedPrincipal,
    requested: Option<&[String]>,
) -> Result<Vec<String>, ToolError> {
    match requested {
        Some(ids) => {
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if !principal.can_access(id) || !ctx.collections.contains_key(id) {
                    return Err(ToolError::Denied(format!("collection '{id}'")));
                }
                out.push(id.clone());
            }
            if out.is_empty() {
                return Err(ToolError::InvalidParams("collections 不能为空数组".into()));
            }
            Ok(out)
        }
        None => Ok(ctx
            .collections
            .keys()
            .filter(|id| principal.can_access(id))
            .cloned()
            .collect()),
    }
}

#[async_trait]
impl Tool for SearchTool {
    fn name(&self) -> &'static str {
        "search"
    }

    fn description(&self) -> &'static str {
        "Search indexed archive collections by natural language query, including file metadata, \
         Office/PDF/text/email body text, OCR text from images or scanned pages, and hybrid \
         semantic recall across Chinese and English. Supports concept queries for detected PII \
         types such as 身份证/身份证号/证件号/identity_card and 手机号/电话/phone; only type \
         keywords are indexed, not newly copied raw numbers. Returns hit file paths + metadata + \
         owning collection."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "自然语言查询"},
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": HARD_LIMIT_CAP,
                    "default": DEFAULT_LIMIT
                },
                "collections": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "目标 collection id 列表；缺省搜索当前 token 授权的全部集合"
                }
            },
            "required": ["query"]
        })
    }

    #[tracing::instrument(
        skip(self, args, ctx, principal),
        fields(
            subject = %principal.subject,
            query_len = tracing::field::Empty,
            limit = tracing::field::Empty,
            targets = tracing::field::Empty,
            results = tracing::field::Empty,
            degraded = tracing::field::Empty,
            elapsed_ms = tracing::field::Empty,
        )
    )]
    async fn invoke(
        &self,
        args: Value,
        ctx: Arc<ServerCtx>,
        principal: Arc<AuthedPrincipal>,
    ) -> Result<Value, ToolError> {
        // spec §6.2 隐私硬规则：不把 query 内容写进 ops log，仅记 query_len / limit / count。
        // （audit.jsonl 专用留痕另一套规则，见 crate::audit——cycle 4。）
        let started = std::time::Instant::now();
        let span = tracing::Span::current();

        let input: SearchInput =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        if input.query.trim().is_empty() {
            return Err(ToolError::InvalidParams("query 不能为空".into()));
        }
        let limit = input
            .limit
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, HARD_LIMIT_CAP);

        // 越权 / 未知 collection → Denied + audit denied 留痕（验收 ④）。
        let target_ids = match resolve_target_ids(&ctx, &principal, input.collections.as_deref()) {
            Ok(ids) => ids,
            Err(e) => {
                if let ToolError::Denied(reason) = &e {
                    ctx.audit.record(
                        &AuditRecord::now(
                            &principal.subject,
                            AuditAction::Denied,
                            input.collections.clone().unwrap_or_default(),
                        )
                        .with_query(&input.query, ctx.audit.log_query)
                        .with_denied_reason(reason),
                    );
                }
                return Err(e);
            }
        };

        span.record("query_len", input.query.len());
        span.record("limit", limit);
        span.record("targets", target_ids.len());

        // 1) NL → SearchIntent（rule parser；模型 fallback 不在 daemon 范围）
        let intent: SearchIntent = scout_intent_parser::parse(&input.query);

        // 2) Refine / FileAction / Clarify 在 daemon search 路径下不可执行 —— 返空。
        if !matches!(
            intent,
            SearchIntent::FileSearch(_) | SearchIntent::MediaSearch(_)
        ) {
            span.record("results", 0);
            span.record("degraded", false);
            tracing::info!(
                intent_variant = ?std::mem::discriminant(&intent),
                "search short-circuited: intent variant not supported in daemon"
            );
            let output = search_output(Vec::new(), false, ctx.indexing_in_progress());
            return serde_json::to_value(output).map_err(|e| ToolError::Internal(e.to_string()));
        }

        // 3) daemon FTS-only 关键词展开（multi-word phrase 拆 token，详函数注释）。
        // 2026-07-20：叠加启动期注入的全局复合条件匹配模式（daemon 无 settings.json，
        // 一次性配置，见 `ServerConfig.match_mode`）。
        let expanded = expand_intent_for_daemon(intent).with_match_mode(ctx.config.match_mode);

        // 4) 逐 collection 跑链（各自独立候选链 / 独立 db —— 物理信息墙），命中
        //    path → collection 建映射供 rank 后回标。
        let cancel = CancellationToken::new();
        let mut merged_all = Vec::new();
        let mut path_to_collection: HashMap<String, String> = HashMap::new();
        let mut served_any = false;
        let mut used_hybrid = false;
        for id in &target_ids {
            let Some(rt) = ctx.collection(id) else {
                continue; // resolve_target_ids 已校验存在性；防御性跳过
            };
            let candidates: Vec<Arc<dyn SearchableTool>> = (*rt.search_candidates).clone();
            // BETA-40 收尾：候选链含语义臂（embedder 可用时 build 期注入）→ 走桌面同款
            // 加权 RRF hybrid 融合；否则维持原 fallback chain（FTS-only，行为零变化）。
            let has_semantic = candidates
                .iter()
                .any(|t| t.capability().backend_kind == Some(BackendKind::SemanticIndex));
            let mut merged: Vec<MergedResult> = Vec::new();
            if has_semantic {
                used_hybrid = true;
                let outcome = {
                    let merged_ref = &mut merged;
                    let mut on_result = |m: MergedResult| merged_ref.push(m);
                    run_fanout_merge_rrf(
                        &candidates,
                        &expanded,
                        cancel.clone(),
                        &mut on_result,
                        ctx.config.semantic_weight,
                        ctx.config.cosine_threshold,
                        &input.query,
                    )
                    .await
                };
                // 至少一臂干净跑完 → 本集合 served；部分臂失败仅记警告（与桌面容忍语义一致）。
                if !outcome.sources_queried.is_empty() {
                    served_any = true;
                }
                for (tool_id, err) in &outcome.errors {
                    tracing::warn!(
                        collection = %id,
                        tool = %tool_id,
                        error = %err,
                        "hybrid 融合中单臂失败（其余臂结果保留）"
                    );
                }
            } else {
                let mut raw_results: Vec<SearchResult> = Vec::new();
                let outcome = {
                    let mut on_result = |r: SearchResult| {
                        raw_results.push(r);
                    };
                    let mut on_switch = |_sw: scout_harness::BackendSwitch| {};
                    run_fallback_chain(
                        &candidates,
                        &expanded,
                        cancel.clone(),
                        &mut on_result,
                        &mut on_switch,
                    )
                    .await
                };
                if outcome.served_by.is_some() {
                    served_any = true;
                } else if let Some(err) = outcome.last_error.as_deref() {
                    tracing::warn!(
                        collection = %id,
                        last_error = err,
                        "collection 检索链失败（fallback chain exhausted）"
                    );
                }
                merged = merge_results(raw_results);
            }
            for m in merged {
                path_to_collection.insert(m.result.path.display().to_string(), id.clone());
                merged_all.push(m);
            }
        }

        // 5) 跨集合统一 rank（同一套 FTS/rank 逻辑、score 同源可比）+ 截断 limit。
        let mut rank_ctx = RankContext::from_expanded(&expanded);
        if used_hybrid {
            rank_ctx = rank_ctx.preserve_existing_relevance();
        }
        let ranked = rank(merged_all, &rank_ctx);

        // BETA-43 验收 ①：出处定位词条（组 head/synonyms，回退 query token）。
        let terms = crate::provenance::query_terms(&input.query, &expanded.keyword_groups);

        let mut hits: Vec<SearchHit> = Vec::with_capacity(ranked.len().min(limit));
        for m in ranked.into_iter().take(limit) {
            let r = m.result;
            let path = r.path.display().to_string();
            let collection = path_to_collection.get(&path).cloned().unwrap_or_default();
            let (snippet, pages) = hit_provenance(&ctx, &collection, &path, &terms);
            hits.push(SearchHit {
                path,
                name: r.name,
                collection,
                size: r.metadata.size_bytes,
                mtime: r.metadata.modified_time.map(|t| t.timestamp()),
                score: r.score.unwrap_or(0.0),
                snippet,
                pages,
            });
        }

        // degraded：没有任何目标集合的链干净成功（db 不存在 / 无可用候选）。
        let degraded = !served_any;

        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        span.record("results", hits.len());
        span.record("degraded", degraded);
        span.record("elapsed_ms", elapsed_ms);
        if degraded {
            tracing::info!(
                targets = target_ids.len(),
                "search degraded, no candidate served"
            );
        } else {
            tracing::info!(results = hits.len(), "search ok");
        }

        // 检索留痕（验收 ③：subject + collections + query + 命中数）。
        let indexing_in_progress = ctx.indexing_in_progress();
        ctx.audit.record(
            &AuditRecord::now(&principal.subject, AuditAction::Search, target_ids.clone())
                .with_query(&input.query, ctx.audit.log_query)
                .with_results(hits.len())
                .with_indexing_in_progress(indexing_in_progress),
        );

        let output = search_output(hits, degraded, indexing_in_progress);
        serde_json::to_value(output).map_err(|e| ToolError::Internal(e.to_string()))
    }
}

/// 单个命中的出处定位（BETA-43 验收 ①）：从该 collection 的文档索引取正文与
/// `document_passages`，算关键词上下文窗口 + 命中回页。
///
/// 走 [`CollectionRuntime::document_index`] 常驻句柄（与 `LocalIndexBackend` 指同一
/// db 文件）、**只读索引不触磁盘原文件**。任何失败（音乐命中 / db 缺文档 / 查询错）
/// 都降级为无定位——出处的 collection + path 两要素始终在 `SearchHit` 顶层。
///
/// [`CollectionRuntime::document_index`]: crate::config::CollectionRuntime::document_index
fn hit_provenance(
    ctx: &ServerCtx,
    collection: &str,
    path: &str,
    terms: &[String],
) -> (Option<String>, Vec<u32>) {
    let Some(rt) = ctx.collection(collection) else {
        return (None, Vec::new());
    };
    let docs = rt.document_index.lock();
    // SearchResult path 是 canonicalize 后形态、documents.path 存原始扫描路径 →
    // 逐候选尝试（与 desktop preview 同款归一）。
    for cand in crate::provenance::lookup_candidates(path) {
        match docs.preview_for_path(&cand, None) {
            Ok(Some(p)) => {
                let snippet = crate::provenance::snippet_windows(
                    &p.body,
                    terms,
                    1,
                    crate::provenance::SEARCH_CONTEXT_CHARS,
                )
                .pop();
                let pages = match docs.passages_for_doc(&cand) {
                    Ok(ps) => {
                        crate::provenance::matching_pages(&ps, terms, crate::provenance::MAX_PAGES)
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, path, "出处回页取 passages 失败（降级为无页号）");
                        Vec::new()
                    }
                };
                return (snippet, pages);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(error = %e, path, "出处片段取正文失败（降级为无片段）");
                return (None, Vec::new());
            }
        }
    }
    (None, Vec::new())
}

/// 构造某个 collection 的 search 候选链——FTS 臂 = `LocalIndexBackend`（团队归档场景
/// 下 root 子树外的系统索引无意义）；`embedder` 可用（daemon 启动 ping probe 通过）时
/// 追加语义臂 `SemanticIndexBackend`（BETA-40 收尾——此前 daemon 只有 FTS 臂，
/// 「语义检索底座」在企业 collection 上名不副实）。daemon / `test_support` 启动时
/// per-collection 调一次、装入 [`CollectionRuntime::search_candidates`] 缓存、
/// `SearchTool::invoke` 每次 `Arc::clone` 复用。
///
/// **不需要 swap on reindex**：`LocalIndexBackend` 持 `db_path` 不持持久
/// `sqlite::Connection`、每次 search 内部开连接查完即关；`SemanticIndexBackend`
/// 的向量缓存按 db mtime + 行数签名自动失效。
#[must_use]
pub fn build_local_search_candidates(
    db_path: PathBuf,
    embedder: Option<Arc<dyn TextEmbedder>>,
) -> Vec<Arc<dyn SearchableTool>> {
    let backend = LocalIndexBackend::new(db_path.clone());
    let mut out: Vec<Arc<dyn SearchableTool>> = vec![Arc::new(HarnessSearchTool::new(
        "search.local",
        "本地索引",
        backend,
        vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
        "Scout 本地索引（daemon per-collection FTS 臂）",
    ))];
    if let Some(embedder) = embedder {
        let semantic = SemanticIndexBackend::new(
            db_path,
            Some(embedder),
            Arc::new(|| DAEMON_SIMILARITY_FLOOR),
        );
        out.push(Arc::new(HarnessSearchTool::new(
            "search.semantic",
            "语义召回",
            semantic,
            vec![SupportedIntent::FileSearch],
            "Scout 本地语义召回（embedding + cosine，按意思/跨语言；daemon per-collection 语义臂）",
        )));
    }
    out
}

/// daemon FTS 臂把 parser 抽出的 multi-word phrase keyword 拆成 token 级 singleton
/// group（BETA-40 收尾后 daemon 可带语义臂 hybrid，但 FTS 臂的 phrase 契约问题不变，
/// 拆分对语义臂无影响——语义臂走 base intent 不消费 `keyword_groups`）。
///
/// 背景（与 [`LocalIndexBackend`] / `intent_parser` 的契约 mismatch）：
/// `intent_parser::extract_en_residual_keywords` 把英文连续内容词合并成
/// `"BETA-32 daemon design"` 这种 **phrase keyword**（对桌面 hybrid 检索 OK：semantic
/// embedding 兜底分散匹配 + ranker 重排）。`ExpandedSearchIntent::identity` 把它包成单个
/// singleton group、`fts_match_from_groups` 进而包成 FTS5 双引号短语，要求文档内连续
/// 出现整个短语 —— daemon FTS-only 路径（无 semantic 兜底）必 0 命中、`degraded=true` 返空。
///
/// 修法：把 head 含 unicode whitespace 且无 synonyms 的 group 按空格再拆成多个 singleton
/// group（FTS5 表达式 `"BETA-32" AND "daemon" AND "design"`）。含 synonyms 的 group 不拆
/// （保护组内 OR 语义，留 daemon 接同义词扩展时的契约守门）。
pub(crate) fn expand_intent_for_daemon(intent: SearchIntent) -> ExpandedSearchIntent {
    let mut expanded = ExpandedSearchIntent::identity(intent);
    expanded.keyword_groups = expanded
        .keyword_groups
        .into_iter()
        .flat_map(|g| {
            if g.synonyms.is_empty() && g.head.split_whitespace().count() > 1 {
                g.head
                    .split_whitespace()
                    .map(KeywordGroup::singleton)
                    .collect::<Vec<_>>()
            } else {
                vec![g]
            }
        })
        .collect();
    expanded
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::test_support::{
        build_test_ctx_inmem, build_test_ctx_inmem_with_indexing_probe, build_test_ctx_multi_inmem,
        full_access_principal, restricted_principal,
    };
    use scout_search_backend::{FileSearch, SchemaVersion};

    fn file_search_intent(kws: Vec<&str>) -> SearchIntent {
        SearchIntent::FileSearch(FileSearch {
            schema_version: SchemaVersion::V1,
            language: None,
            keywords: Some(kws.into_iter().map(str::to_owned).collect()),
            extensions: None,
            file_type: None,
            location: None,
            modified_time: None,
            created_time: None,
            accessed_time: None,
            size: None,
            exclude_extensions: None,
            exclude_file_type: None,
            sort: None,
            limit: None,
        })
    }

    #[test]
    fn input_schema_has_query_limit_collections() {
        let s = SearchTool.input_schema();
        assert_eq!(s["properties"]["query"]["type"], "string");
        assert_eq!(s["properties"]["limit"]["maximum"], HARD_LIMIT_CAP);
        assert_eq!(s["properties"]["collections"]["type"], "array");
        assert_eq!(s["required"], json!(["query"]));
    }

    #[tokio::test]
    async fn empty_query_returns_invalid_params() {
        let ctx = build_test_ctx_inmem();
        let err = SearchTool
            .invoke(json!({"query": ""}), ctx, full_access_principal())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn limit_above_cap_does_not_panic() {
        let ctx = build_test_ctx_inmem();
        let v = SearchTool
            .invoke(
                json!({"query": "anything", "limit": 1000}),
                ctx,
                full_access_principal(),
            )
            .await
            .unwrap();
        let arr = v["results"].as_array().expect("results 应是数组");
        assert!(arr.len() <= HARD_LIMIT_CAP);
    }

    #[tokio::test]
    async fn empty_corpus_returns_empty_results_no_error() {
        let ctx = build_test_ctx_inmem();
        let v = SearchTool
            .invoke(
                json!({"query": "找昨天的 ppt"}),
                ctx,
                full_access_principal(),
            )
            .await
            .expect("空 corpus 下不应报错，应返空 results");
        let arr = v["results"].as_array().expect("results 应是数组");
        assert!(arr.is_empty(), "空 db 应返空 results");
        // db 不存在 → 无集合被 serve → degraded=true。
        assert_eq!(v["degraded"], json!(true));
        assert!(
            v.get("indexing_in_progress").is_none(),
            "default probe must not emit indexing_in_progress"
        );
        assert!(v.get("note").is_none(), "default probe must not emit note");
    }

    #[test]
    fn idle_indexing_output_keeps_legacy_json_bytes() {
        let ok_json = serde_json::to_string(&search_output(Vec::new(), false, false)).unwrap();
        assert_eq!(
            ok_json, r#"{"results":[]}"#,
            "idle non-degraded output must stay byte-identical"
        );
        let degraded_json = serde_json::to_string(&search_output(Vec::new(), true, false)).unwrap();
        assert_eq!(
            degraded_json, r#"{"results":[],"degraded":true}"#,
            "idle degraded output must stay byte-identical"
        );
    }

    #[tokio::test]
    async fn indexing_in_progress_adds_structured_hint_and_note() {
        let ctx = build_test_ctx_inmem_with_indexing_probe(Arc::new(|| true));
        let v = SearchTool
            .invoke(json!({"query": "anything"}), ctx, full_access_principal())
            .await
            .unwrap();
        assert_eq!(v["indexing_in_progress"], json!(true));
        assert_eq!(v["note"], json!(INDEXING_IN_PROGRESS_NOTE));
    }

    // ===== BETA-36：collection 授权 =====

    /// 请求未授权 collection → Denied（验收 ④ MCP 侧）。
    #[tokio::test]
    async fn unauthorized_collection_denied() {
        let ctx = build_test_ctx_multi_inmem();
        // restricted_principal 只授权 case-a。
        let err = SearchTool
            .invoke(
                json!({"query": "合同", "collections": ["case-b"]}),
                ctx,
                restricted_principal("zhang.san", &["case-a"]),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Denied(msg) => assert!(msg.contains("case-b"), "{msg}"),
            other => panic!("应返 Denied，实得：{other:?}"),
        }
    }

    /// 请求不存在的 collection → 同样 Denied 且同文案（不泄漏存在性）。
    #[tokio::test]
    async fn unknown_collection_denied_same_message_shape() {
        let ctx = build_test_ctx_multi_inmem();
        let err = SearchTool
            .invoke(
                json!({"query": "x", "collections": ["ghost"]}),
                ctx,
                full_access_principal(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Denied(msg) => {
                assert_eq!(msg, "collection 'ghost'", "未知与未授权应同文案：{msg}");
            }
            other => panic!("应返 Denied，实得：{other:?}"),
        }
    }

    /// 混合请求（一个授权 + 一个未授权）→ 整体 Denied，不部分执行。
    #[tokio::test]
    async fn mixed_authorization_denied_whole_request() {
        let ctx = build_test_ctx_multi_inmem();
        let err = SearchTool
            .invoke(
                json!({"query": "x", "collections": ["case-a", "case-b"]}),
                ctx,
                restricted_principal("zhang.san", &["case-a"]),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)));
    }

    /// 空数组 collections → InvalidParams（区别于缺省"全部授权"语义）。
    #[tokio::test]
    async fn empty_collections_array_invalid() {
        let ctx = build_test_ctx_multi_inmem();
        let err = SearchTool
            .invoke(
                json!({"query": "x", "collections": []}),
                ctx,
                full_access_principal(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    /// 缺省 collections = 授权范围内全部集合（restricted 只见 case-a、不炸）。
    #[tokio::test]
    async fn default_targets_are_authorized_subset() {
        let ctx = build_test_ctx_multi_inmem();
        let v = SearchTool
            .invoke(
                json!({"query": "anything"}),
                ctx,
                restricted_principal("zhang.san", &["case-a"]),
            )
            .await
            .expect("授权子集检索不应报错");
        assert!(v["results"].as_array().unwrap().is_empty());
    }

    // ===== 关键词展开（BETA-32 契约，不变）=====

    #[test]
    fn expand_intent_for_daemon_splits_multi_word_phrase_keyword() {
        let intent = file_search_intent(vec!["BETA-32 daemon design"]);
        let expanded = expand_intent_for_daemon(intent);
        let heads: Vec<&str> = expanded
            .keyword_groups
            .iter()
            .map(|g| g.head.as_str())
            .collect();
        assert_eq!(heads, vec!["BETA-32", "daemon", "design"]);
        assert!(expanded
            .keyword_groups
            .iter()
            .all(|g| g.synonyms.is_empty()));
    }

    #[test]
    fn expand_intent_for_daemon_preserves_singleton_word_keyword() {
        let intent = file_search_intent(vec!["quality"]);
        let expanded = expand_intent_for_daemon(intent);
        let heads: Vec<&str> = expanded
            .keyword_groups
            .iter()
            .map(|g| g.head.as_str())
            .collect();
        assert_eq!(heads, vec!["quality"]);
    }

    #[test]
    fn expand_intent_for_daemon_preserves_multi_singleton_keywords() {
        let intent = file_search_intent(vec!["budget", "annual"]);
        let expanded = expand_intent_for_daemon(intent);
        let heads: Vec<&str> = expanded
            .keyword_groups
            .iter()
            .map(|g| g.head.as_str())
            .collect();
        assert_eq!(heads, vec!["budget", "annual"]);
    }

    #[test]
    fn expand_intent_for_daemon_preserves_groups_with_synonyms() {
        let intent = file_search_intent(vec!["work report"]);
        let mut expanded = ExpandedSearchIntent::identity(intent);
        if let Some(g) = expanded.keyword_groups.first_mut() {
            g.synonyms.push("述职".into());
        }
        let kept = expanded
            .keyword_groups
            .into_iter()
            .flat_map(|g| {
                if g.synonyms.is_empty() && g.head.split_whitespace().count() > 1 {
                    g.head
                        .split_whitespace()
                        .map(KeywordGroup::singleton)
                        .collect::<Vec<_>>()
                } else {
                    vec![g]
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].head, "work report");
        assert_eq!(kept[0].synonyms, vec!["述职".to_string()]);
    }

    /// `build_local_search_candidates`：无 embedder → 仅 FTS 臂；有 embedder → 加语义臂。
    #[test]
    fn build_local_search_candidates_arms_follow_embedder() {
        let candidates = build_local_search_candidates(
            std::path::PathBuf::from("/tmp/nonexistent/index.db"),
            None,
        );
        assert_eq!(candidates.len(), 1, "无 embedder 应只有 FTS 臂");

        let embedder: Arc<dyn TextEmbedder> =
            Arc::new(crate::test_support::StubEmbedder::default());
        let candidates = build_local_search_candidates(
            std::path::PathBuf::from("/tmp/nonexistent/index.db"),
            Some(embedder),
        );
        assert_eq!(candidates.len(), 2, "有 embedder 应追加语义臂");
        assert!(
            candidates
                .iter()
                .any(|t| { t.capability().backend_kind == Some(BackendKind::SemanticIndex) }),
            "第二臂应为 SemanticIndex"
        );
    }
}
