//! BETA-04/18/19 fan-out 多源搜索与跨范畴均衡分支（从 search.rs 拆出，逻辑零改动）。

use std::sync::Arc;
use std::time::Instant;

use scout_harness::context::{ContextMemory, RefineMergeError};
use scout_harness::{
    fuse_fanout_merge, fuse_fanout_rrf, run_fanout_merge_with_fallback, FanoutOutcome,
    IntentRouter, MergedResult, SearchableTool,
};
use scout_intent_parser::fallback::IntentSource;
use scout_search_backend::{
    BackendKind, CancellationToken, ExpandedSearchIntent, SearchIntent, SearchResult,
};
use tauri::ipc::Channel;

use super::{
    describe_intent, emit_synonym_events, result_to_json, signals_to_labels, ResolvedQuery,
    SearchDeps, SearchEvent,
};

/// fanout backends 是否含语义召回臂（决定走加权 RRF 还是原 fallback 合并）。
fn fanout_has_semantic(backends: &[std::sync::Arc<dyn SearchableTool>]) -> bool {
    backends.iter().any(|t| {
        t.capability().backend_kind == Some(scout_search_backend::BackendKind::SemanticIndex)
    })
}

/// Track A 并发收集：单后端查询产物（回收时**保持后端在 `backends` 中的原序**）。
struct BackendCollected {
    /// 该后端 `capability().backend_kind`（用于拆 fts/vec 臂 + 填 `sources_queried`）。
    backend_kind: Option<BackendKind>,
    /// pre-stream 是否成功拿到流；**仅成功者计入 `sources_queried`**（与串行版逐字节对齐）。
    queried: bool,
    /// 收集到的结果（保持后端到达顺序＝rank）。
    results: Vec<SearchResult>,
    /// 该后端错误 `(tool_id, message)`；至多一条（pre-stream Err 或首个流内 Err）。
    errors: Vec<(String, String)>,
}

/// Track A 搜索并发化核心：把每个后端的 `search_expanded` 查询丢到 tokio 阻塞线程池
/// （`spawn_blocking`），线程内用 **runtime 无关**的 `futures_executor::block_on` 驱动其
/// stream 到收齐，再按后端原序 `.await` 等齐——总耗时≈最慢的一个后端，而非各后端求和。
///
/// 为何在此层（desktop）编排：harness 是 runtime 无关的、后端多是「同步阻塞体塞进 async
/// 块、无 `spawn_blocking`」，单纯 `join_all` 在单线程 executor 上不会真并行阻塞体。真并行
/// 必须把每个后端查询丢到阻塞线程上，而 Tauri 跑在多线程 tokio 上，故并发编排放这里。
///
/// 并发安全：语义 embed 内部走 Mutex 串行化、SQLite 每后端独立连接、子进程互相独立，
/// 故并发查询安全；各后端 backend 内部为同步阻塞体（无 tokio reactor 需求），单线程
/// executor 足以驱动、且不与 tauri 全局 tokio runtime 耦合。
///
/// 取消/错误语义与串行 [`run_fanout_merge`](scout_harness::run_fanout_merge) 对齐：
/// pre-stream Err 记 `errors`、不计 `sources_queried`；流内首个 Err 记 `errors` 后停该后端；
/// 每个 `spawn_blocking` 闭包须 `Send + 'static`（`Arc`/`expanded`/`cancel` clone 后 move 进闭包）。
async fn concurrent_collect(
    backends: &[Arc<dyn SearchableTool>],
    expanded: &ExpandedSearchIntent,
    cancel: &CancellationToken,
) -> Vec<BackendCollected> {
    // 取消已触发 → 不发起任何查询（对齐串行版 for-loop 首行 break）。
    if cancel.is_cancelled() {
        return Vec::new();
    }

    let mut handles = Vec::with_capacity(backends.len());
    for tool in backends {
        let tool = Arc::clone(tool);
        let expanded = expanded.clone();
        let cancel = cancel.clone();
        let handle = tauri::async_runtime::spawn_blocking(move || {
            let tool_id = tool.id().to_owned();
            let backend_kind = tool.capability().backend_kind;
            let mut results: Vec<SearchResult> = Vec::new();
            let mut errors: Vec<(String, String)> = Vec::new();
            let mut queried = false;
            futures_executor::block_on(async {
                match tool.search_expanded(&expanded, cancel.clone()).await {
                    Err(err) => errors.push((tool_id.clone(), err.to_string())),
                    Ok(mut stream) => {
                        queried = true;
                        use futures_util::StreamExt;
                        while let Some(item) = stream.next().await {
                            if cancel.is_cancelled() {
                                break;
                            }
                            match item {
                                Ok(result) => results.push(result),
                                Err(err) => {
                                    errors.push((tool_id.clone(), err.to_string()));
                                    break;
                                }
                            }
                        }
                    }
                }
            });
            let results = scout_harness::filter_results_for_intent(results, &expanded.base);
            BackendCollected {
                backend_kind,
                queried,
                results,
                errors,
            }
        });
        handles.push(handle);
    }

    // 按后端原序回收（排序 tie-break 依赖它）。spawn_blocking join 失败（线程 panic）
    // → 记空产物占位跳过，不污染 sources_queried/errors、不中断其它后端。
    let mut collected = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.await {
            Ok(bc) => collected.push(bc),
            Err(_) => collected.push(BackendCollected {
                backend_kind: None,
                queried: false,
                results: Vec::new(),
                errors: Vec::new(),
            }),
        }
    }
    collected
}

/// Track A：把 [`concurrent_collect`] 收回的 per-backend 产物按后端原序归约为
/// `(sources_queried, errors)` 两条累加列表。`split` 回调对每个后端的 `results`
/// 做归口（RRF 路径拆 fts/vec 臂；merge 路径直接 extend 到单列表）——保证与串行版
/// 逐字节等价：`sources_queried` 只收 pre-stream 成功者、`errors` 按后端序拼接。
fn reduce_collected(
    collected: Vec<BackendCollected>,
    sources_queried: &mut Vec<BackendKind>,
    errors: &mut Vec<(String, String)>,
    mut split: impl FnMut(Option<BackendKind>, Vec<SearchResult>),
) {
    for bc in collected {
        if bc.queried {
            if let Some(kind) = bc.backend_kind {
                if !sources_queried.contains(&kind) {
                    sources_queried.push(kind);
                }
            }
        }
        errors.extend(bc.errors);
        split(bc.backend_kind, bc.results);
    }
}

/// BETA-04 fan-out 多源搜索分支：同时查 `backends`（系统搜索 + 本地索引），结果经
/// Result Normalizer 合并去重后流式投递。与 fallback 链路并列——纯文件名 / 单后端仍走 fallback。
/// `SearchResultJson.sources` 携带各结果的全部命中来源（供 UI 显示「via …」）。
pub(crate) async fn run_fanout_search(
    backends: Vec<Arc<dyn SearchableTool>>,
    query: ResolvedQuery<'_>,
    on_event: Channel<SearchEvent>,
    deps: &SearchDeps,
    start: Instant,
) -> Result<(), String> {
    let ResolvedQuery {
        effective,
        expanded,
        source,
        signals,
        raw_query,
    } = query;
    // 多源 fan-out：tracer 仍按首个后端记录（ToolCallEvent/ToolResultEvent 的 tool_id 是单 id
    // 语义）；但前端 Started 事件展示**全部并列源**，避免「via search.local」误导——实际并列查了
    // 如 local + windows + everything 多个后端。
    let backend_ids: Vec<String> = backends.iter().map(|b| b.id().to_owned()).collect();
    let first_id = backend_ids[0].clone();
    let sources_label = backend_ids.join(" + ");
    let tool_start = Instant::now();

    deps.tracer.on_tool_call(&scout_harness::ToolCallEvent {
        tool_id: first_id.clone(),
        tool_kind: scout_harness::ToolKind::Search,
        intent_variant: scout_harness::SupportedIntent::from_intent(&effective),
    });
    let _ = on_event.send(SearchEvent::Started {
        intent_summary: describe_intent(&effective),
        fallback_used: matches!(source, IntentSource::Model),
        signals: signals_to_labels(signals),
        tool_id: sources_label.clone(),
        intent_json: crate::search::intent_to_json(&effective),
    });
    emit_synonym_events(&expanded, &deps.tracer);

    let cancel = CancellationToken::new();
    // BETA-15B-1：含语义臂时走加权 RRF 融合（语义臂 + FTS 内容臂跨列表按 path 累加加权倒数排名），
    // 不接文件名兜底（语义/FTS 内容后端覆盖语义召回，无需 Everything 文件名补召回）。
    // 不含语义臂 → 沿用原 fan-out + 文件名兜底路径（字节级零行为变化）。
    let has_semantic = fanout_has_semantic(&backends);

    // Track A 搜索并发化：先并发查询所有后端（总耗时≈最慢的一个而非求和），再在 harness
    // 抽出的纯 fuse 函数上做与串行版逐字节等价的融合。融合语义（RRF 路由 / merge 去重 /
    // 文件名兜底触发条件）完全不变，仅查询由串行改并发。
    let collected = concurrent_collect(&backends, &expanded, &cancel).await;
    let mut sources_queried: Vec<BackendKind> = Vec::new();
    let mut errors: Vec<(String, String)> = Vec::new();

    // 收齐合并结果（fan-out 本就先收齐各源再合并，无流式损失）→ Ranker 排序 → 按序发出。
    let merged: Vec<MergedResult>;
    let mut outcome = if has_semantic {
        // 按 backend_kind 拆 fts 臂（非 SemanticIndex）/ vec 臂（SemanticIndex），喂纯 fuse_fanout_rrf。
        let mut fts_lists: Vec<Vec<SearchResult>> = Vec::new();
        let mut vec_lists: Vec<Vec<SearchResult>> = Vec::new();
        reduce_collected(
            collected,
            &mut sources_queried,
            &mut errors,
            |kind, results| {
                if matches!(kind, Some(BackendKind::SemanticIndex)) {
                    vec_lists.push(results);
                } else {
                    fts_lists.push(results);
                }
            },
        );
        let semantic_weight = deps.semantic_weight();
        let cosine_threshold = deps.cosine_threshold();
        let (fused, verdict) = fuse_fanout_rrf(
            fts_lists,
            vec_lists,
            semantic_weight,
            cosine_threshold,
            raw_query,
        );
        merged = fused;
        FanoutOutcome {
            total: merged.len(),
            sources_queried,
            errors,
            route_verdict: Some(verdict),
        }
    } else {
        // 非语义：各后端结果按序拼接 → fuse_fanout_merge 去重合并。
        let mut content: Vec<SearchResult> = Vec::new();
        reduce_collected(
            collected,
            &mut sources_queried,
            &mut errors,
            |_kind, results| content.extend(results),
        );
        let mut fused = fuse_fanout_merge(content);

        // 文件名兜底候选（Everything）：**仅当内容轮干净零结果**时按文件名补召回（兜底罕见触发，
        // 保持串行），闭合「文件在系统索引/本地索引未覆盖的位置、但文件名含关键词」的盲区。
        // macOS 无纯文件名后端 → 空 → 兜底永不触发（零行为变化）；取消时不兜底。
        if fused.is_empty() && !cancel.is_cancelled() {
            let filename_fallback =
                IntentRouter::new(deps.registry()).route_filename_fallback(&expanded);
            if !filename_fallback.is_empty() {
                // 内容源全空 → 切到 Everything 文件名兜底：发 BackendSwitched 供 UI 提示 + trace 记一条。
                if let Some(to) = filename_fallback.first().map(|t| t.id().to_owned()) {
                    deps.tracer.on_error(&scout_harness::ToolErrorEvent {
                        tool_id: backends[0].id().to_owned(),
                        duration: tool_start.elapsed(),
                        error_type: "fanout_filename_fallback".to_owned(),
                    });
                    let _ = on_event.send(SearchEvent::BackendSwitched {
                        from: backends[0].id().to_owned(),
                        to,
                        reason: "empty".to_owned(),
                    });
                }
                // 兜底轮同样并发收集（后端数极少、仅空结果时触发）→ merge。sources/errors 累加两轮。
                let fb_collected = concurrent_collect(&filename_fallback, &expanded, &cancel).await;
                let mut fb_content: Vec<SearchResult> = Vec::new();
                reduce_collected(
                    fb_collected,
                    &mut sources_queried,
                    &mut errors,
                    |_kind, results| fb_content.extend(results),
                );
                fused = fuse_fanout_merge(fb_content);
            }
        }
        merged = fused;
        FanoutOutcome {
            total: merged.len(),
            sources_queried,
            errors,
            route_verdict: None,
        }
    };

    deps.tracer.on_tool_result(&scout_harness::ToolResultEvent {
        tool_id: first_id,
        duration: tool_start.elapsed(),
        result_count: outcome.total,
    });

    if outcome.total == 0 {
        // BETA-33 cycle 9：语义臂错误不冒充全链错误——报错优先取非语义臂错误；仅语义臂
        // 出错（如路由后模型加载竞态失败）时按「未找到结果」空态呈现：其余臂已正常查完、
        // 语义能力的真实状态在顶栏 / 设置页 EmbedStatus 另有如实展示。
        let semantic_ids: Vec<&str> = backends
            .iter()
            .filter(|t| {
                t.capability().backend_kind
                    == Some(scout_search_backend::BackendKind::SemanticIndex)
            })
            .map(|t| t.id())
            .collect();
        let message = outcome
            .errors
            .iter()
            .find(|(id, _)| !semantic_ids.contains(&id.as_str()))
            .map(|(_, m)| m.clone())
            .unwrap_or_else(|| "未找到结果".to_owned());
        let _ = on_event.send(SearchEvent::Error { message });
        return Ok(());
    }

    // Hybrid 已有跨来源可比的 RRF 分数：保留为默认相关性主排序，Ranker 仅作 tie-break；
    // 非 Hybrid 仍沿用原启发式。用户显式 sort 始终覆盖相关性顺序。
    let mut rank_ctx = scout_ranker::RankContext::from_expanded(&expanded);
    if has_semantic {
        rank_ctx = rank_ctx.preserve_existing_relevance();
    }
    let mut ranked = scout_ranker::rank(merged, &rank_ctx);
    if let Some(limit) = intent_limit(&expanded.base) {
        ranked.truncate(limit);
    }
    outcome.total = ranked.len();

    let mut recorded: Vec<scout_search_backend::SearchResult> = Vec::with_capacity(ranked.len());
    let mut send_failed = false;
    for m in ranked {
        let sources: Vec<String> = m
            .sources
            .iter()
            .map(|s| format!("{s:?}").to_lowercase())
            .collect();
        let sem_cos = m.semantic_cosine;
        let json = result_to_json(&m.result, sources, sem_cos);
        recorded.push(m.result);
        if !send_failed && on_event.send(SearchEvent::Result { item: json }).is_err() {
            send_failed = true;
        }
    }
    let _ = send_failed;

    {
        let mut guard = deps.context.lock().unwrap_or_else(|e| e.into_inner());
        guard.record(expanded.base, recorded);
    }
    let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::info!(
        path = "fanout",
        total = outcome.total,
        elapsed_ms,
        "search 出口"
    );
    let _ = on_event.send(SearchEvent::Complete {
        total: outcome.total,
        elapsed_ms,
    });
    Ok(())
}

/// Intent 的全局结果上限。各 backend 自己的 limit 只是候选预算；多源合并后仍须再次截断，
/// 否则三个 backend 各返 N 条会让最终结果膨胀到 3N。
fn intent_limit(intent: &SearchIntent) -> Option<usize> {
    match intent {
        SearchIntent::FileSearch(search) => search.limit.map(|v| v as usize),
        SearchIntent::MediaSearch(search) => search.limit.map(|v| v as usize),
        SearchIntent::Refine(_) | SearchIntent::FileAction(_) | SearchIntent::Clarify(_) => None,
    }
}

/// BETA-19：`FileSearch` 是否含 ≥2 个**不同** `file_type`（跨范畴多类型 → 需均衡展示）。
/// 返回去重保序的类型列表；单类型 / 非 `FileSearch` / 无 file_type → `None`（不触发，走原路径）。
pub(crate) fn multi_file_types(
    intent: &SearchIntent,
) -> Option<Vec<scout_search_backend::FileType>> {
    let SearchIntent::FileSearch(fs) = intent else {
        return None;
    };
    let types = fs.file_type.as_ref()?;
    let mut uniq: Vec<scout_search_backend::FileType> = Vec::new();
    for t in types {
        if !uniq.contains(t) {
            uniq.push(*t);
        }
    }
    (uniq.len() >= 2).then_some(uniq)
}

/// BETA-19：把多类型 expanded 收窄为**单类型**子查询——`file_type` 置为单值，`extensions`
/// 并集按该类型切回子集（保留用户显式收窄，如「png 和 mp4」只查 png/mp4）；交集空 → `None`
/// 让后端按 `file_type` 派生。`keyword_groups` 原样保留（同义词与类型无关）。
pub(crate) fn single_type_expanded(
    expanded: &scout_search_backend::ExpandedSearchIntent,
    file_type: scout_search_backend::FileType,
) -> scout_search_backend::ExpandedSearchIntent {
    let mut sub = expanded.clone();
    if let SearchIntent::FileSearch(fs) = &mut sub.base {
        fs.file_type = Some(vec![file_type]);
        if let Some(exts) = &fs.extensions {
            let allowed = scout_search_backend::extensions_for_file_type(file_type);
            let narrowed: Vec<String> = exts
                .iter()
                .filter(|e| {
                    let lo = e.to_lowercase();
                    allowed.iter().any(|a| *a == lo)
                })
                .cloned()
                .collect();
            fs.extensions = if narrowed.is_empty() {
                None
            } else {
                Some(narrowed)
            };
        }
    }
    sub
}

/// BETA-19 均衡分支的逐类型执行计划：(单类型子查询, content 后端, 文件名兜底后端)。
pub(crate) type TypePlan = (
    scout_search_backend::ExpandedSearchIntent,
    Vec<Arc<dyn SearchableTool>>,
    Vec<Arc<dyn SearchableTool>>,
);

/// BETA-19 跨范畴均衡分支：对每个 `file_type` 各跑一遍自己的路由+执行（各得一份配额），
/// 桶内按 [`scout_ranker::rank`] 排序，再 round-robin 交错——保证少数派类型在结果前列可见。
/// 复用现有 fan-out 机制（每个单类型子查询用它自己的 `route_search_fanout` + `route_filename_fallback`）。
pub(crate) async fn run_balanced_multitype_search(
    types: Vec<scout_search_backend::FileType>,
    query: ResolvedQuery<'_>,
    on_event: Channel<SearchEvent>,
    deps: &SearchDeps,
    start: Instant,
) -> Result<(), String> {
    let ResolvedQuery {
        effective,
        expanded,
        source,
        signals,
        raw_query: _,
    } = query;

    // 1) 逐类型路由：单类型子查询 → (sub, content 后端, 文件名兜底)。无后端的类型跳过。
    let router = IntentRouter::new(deps.registry());
    let mut plans: Vec<TypePlan> = Vec::with_capacity(types.len());
    for t in &types {
        let sub = single_type_expanded(&expanded, *t);
        if let Ok(backends) = router.route_search_fanout(&sub) {
            let fallback = router.route_filename_fallback(&sub);
            plans.push((sub, backends, fallback));
        }
    }
    if plans.is_empty() {
        let _ = on_event.send(SearchEvent::Error {
            message: "未找到结果".to_owned(),
        });
        return Ok(());
    }

    // 跨范畴均衡同样并列多后端：tracer 记首个；前端展示全部并列源（去重，按 id 序）。
    let backend_ids: Vec<String> = {
        use std::collections::BTreeSet;
        plans
            .iter()
            .flat_map(|p| p.1.iter().map(|t| t.id().to_owned()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    let first_id = plans[0].1[0].id().to_owned();
    let sources_label = backend_ids.join(" + ");
    let tool_start = Instant::now();
    deps.tracer.on_tool_call(&scout_harness::ToolCallEvent {
        tool_id: first_id.clone(),
        tool_kind: scout_harness::ToolKind::Search,
        intent_variant: scout_harness::SupportedIntent::from_intent(&effective),
    });
    let _ = on_event.send(SearchEvent::Started {
        intent_summary: describe_intent(&effective),
        fallback_used: matches!(source, IntentSource::Model),
        signals: signals_to_labels(signals),
        tool_id: sources_label.clone(),
        intent_json: crate::search::intent_to_json(&effective),
    });
    emit_synonym_events(&expanded, &deps.tracer);

    // 2) 逐类型收桶 + 桶内排序。各类型独立 cancel 派生自同一根。
    let cancel = CancellationToken::new();
    let mut buckets: Vec<Vec<MergedResult>> = Vec::with_capacity(plans.len());
    for (sub, backends, fallback) in &plans {
        let mut bucket: Vec<MergedResult> = Vec::new();
        {
            let mut on_result = |m: MergedResult| bucket.push(m);
            // 均衡分支不对每类型单独发 BackendSwitched（噪声大）；兜底仍按需触发补召回。
            let mut on_fallback = || {};
            let _ = run_fanout_merge_with_fallback(
                backends,
                fallback,
                sub,
                cancel.clone(),
                &mut on_result,
                &mut on_fallback,
            )
            .await;
        }
        let ranked = scout_ranker::rank(bucket, &scout_ranker::RankContext::from_expanded(sub));
        buckets.push(ranked);
    }

    // 3) round-robin 交错 + 显式 limit 截断（总数 ≤ L 语义；默认不截断）。
    let mut results = scout_ranker::interleave(buckets);
    if let SearchIntent::FileSearch(fs) = &expanded.base {
        if let Some(limit) = fs.limit {
            results.truncate(limit as usize);
        }
    }
    let total = results.len();

    deps.tracer.on_tool_result(&scout_harness::ToolResultEvent {
        tool_id: first_id,
        duration: tool_start.elapsed(),
        result_count: total,
    });

    if total == 0 {
        let _ = on_event.send(SearchEvent::Error {
            message: "未找到结果".to_owned(),
        });
        return Ok(());
    }

    // 4) 流式投递 + 记上下文（记原多类型 intent 供 refine）。
    let mut recorded: Vec<scout_search_backend::SearchResult> = Vec::with_capacity(total);
    let mut send_failed = false;
    for m in results {
        let sources: Vec<String> = m
            .sources
            .iter()
            .map(|s| format!("{s:?}").to_lowercase())
            .collect();
        let sem_cos = m.semantic_cosine;
        let json = result_to_json(&m.result, sources, sem_cos);
        recorded.push(m.result);
        if !send_failed && on_event.send(SearchEvent::Result { item: json }).is_err() {
            send_failed = true;
        }
    }
    let _ = send_failed;

    {
        let mut guard = deps.context.lock().unwrap_or_else(|e| e.into_inner());
        guard.record(expanded.base, recorded);
    }
    let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::info!(
        path = "balanced-multitype",
        total,
        elapsed_ms,
        "search 出口"
    );
    let _ = on_event.send(SearchEvent::Complete { total, elapsed_ms });
    Ok(())
}

/// 若 intent 是 `Refine`，按 [`ContextMemory`] 合并上一轮基准；否则原样返回。
///
/// `conflicts`（clear 与 delta 同名字段冲突，已按 clear 为准合并）走 `eprintln`
/// 记录，不阻断。合并失败（无上一轮 / 基准非法 / 字段不适用）返回
/// [`RefineMergeError`]，由调用方转 [`SearchEvent::Error`]。
pub(crate) fn apply_refine_if_needed(
    intent: SearchIntent,
    ctx: &ContextMemory,
) -> Result<SearchIntent, RefineMergeError> {
    match intent {
        SearchIntent::Refine(refine) => {
            let outcome = ctx.apply_refine(&refine)?;
            if !outcome.conflicts.is_empty() {
                eprintln!(
                    "search: refine 字段冲突(以 clear 为准): {:?}",
                    outcome.conflicts
                );
            }
            Ok(outcome.intent)
        }
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use scout_harness::{SearchTool, SupportedIntent};
    use scout_search_backend::{
        BackendKind, BackendSearchFuture, BackendStream, ImplementationStatus, SearchBackend,
        SearchError, SearchResult,
    };

    /// 仅用于路由判定：`kind()` 可设的空结果后端（capability().backend_kind 取自它）。
    #[derive(Debug)]
    struct KindBackend(BackendKind);
    impl SearchBackend for KindBackend {
        fn kind(&self) -> BackendKind {
            self.0
        }
        fn implementation_status(&self) -> ImplementationStatus {
            ImplementationStatus::Real
        }
        fn is_available(&self) -> bool {
            true
        }
        fn search<'a>(
            &'a self,
            _intent: &'a SearchIntent,
            _cancel: CancellationToken,
        ) -> BackendSearchFuture<'a> {
            Box::pin(async move {
                let items: Vec<Result<SearchResult, SearchError>> = Vec::new();
                Ok(Box::pin(futures::stream::iter(items)) as BackendStream)
            })
        }
    }

    fn tool(kind: BackendKind) -> Arc<dyn SearchableTool> {
        Arc::new(SearchTool::new(
            "search.fake",
            "Fake",
            KindBackend(kind),
            vec![SupportedIntent::FileSearch],
            "kind-only fake backend",
        ))
    }

    /// 仅本地索引臂 → false；含语义臂 → true（决定加权 RRF vs 原 fallback 合并的路由谓词）。
    #[test]
    fn fanout_has_semantic_detects_semantic_arm() {
        let local_only = vec![tool(BackendKind::NativeIndex)];
        assert!(
            !fanout_has_semantic(&local_only),
            "仅 NativeIndex 本地索引臂应判为无语义"
        );

        let with_semantic = vec![
            tool(BackendKind::NativeIndex),
            tool(BackendKind::SemanticIndex),
        ];
        assert!(
            fanout_has_semantic(&with_semantic),
            "含 SemanticIndex 臂应判为有语义"
        );

        assert!(
            !fanout_has_semantic(&[]),
            "空 backends 应判为无语义（兜底安全）"
        );
    }
}
