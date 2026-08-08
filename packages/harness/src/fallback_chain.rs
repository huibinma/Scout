//! 搜索后端 fallback chain：选中后端失败（起不来/中途崩/零结果）时按有序候选
//! 逐个回退，跨候选按结果的 `path` 字段去重合并（调用方应保证 backend 返回规范化绝对路径）。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use scout_search_backend::{CancellationToken, ExpandedSearchIntent, SearchError, SearchResult};

use crate::SearchableTool;

/// 切换原因，对应三类失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchReason {
    /// 后端整体不可用（pre-stream `BackendUnavailable`）。
    Unavailable,
    /// 其它错误（pre-stream 非 unavailable，或流中途 Err）。
    Error,
    /// 干净跑完但本候选贡献 0 条新结果。
    Empty,
}

impl SwitchReason {
    /// 前端/trace 用的稳定字符串。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Error => "error",
            Self::Empty => "empty",
        }
    }
}

/// 一次后端切换的描述。
#[derive(Debug, Clone)]
pub struct BackendSwitch {
    pub from: String,
    pub to: String,
    pub reason: SwitchReason,
}

/// 链执行结果。
#[derive(Debug, Clone, Default)]
pub struct ChainOutcome {
    /// 累积去重结果数。
    pub total: usize,
    /// `total == 0` 时供调用方生成 Error 文案。
    pub last_error: Option<String>,
    /// 干净跑完且贡献 ≥1 新结果的候选 id（实际服务者）；全失败或无候选干净成功则 None。
    pub served_by: Option<String>,
}

/// 驱动有序候选链。逐个 `search_expanded`，按结果的 `path` 字段去重累积（调用方应保证
/// backend 返回规范化绝对路径）；任一候选失败（pre-stream Err / 流中途 Err / 干净零新结果）
/// 即经 `on_switch` 通报并切下一候选；一旦某候选**干净跑完且贡献 ≥1 条新结果**即成功停链。
/// `on_result` 仅对去重后的新结果调用。
///
/// `candidates` 为空时返回 `ChainOutcome{total:0, last_error:None}`；
/// 调用方（`IntentRouter::route_search_chain`）已保证传入非空切片。
///
/// 回调使用泛型参数（`R: FnMut(SearchResult) + Send`，`S: FnMut(BackendSwitch) + Send`），
/// 使返回的 future 满足 `Send`，兼容多线程 async runtime（如 Tauri command executor）。
/// 所有既有调用方（`&mut closure`）无需修改，编译器自动推断具体类型。
#[must_use = "ChainOutcome 须被检查；total==0 时需向用户报告错误"]
pub async fn run_fallback_chain<R, S>(
    candidates: &[Arc<dyn SearchableTool>],
    expanded: &ExpandedSearchIntent,
    cancel: CancellationToken,
    on_result: &mut R,
    on_switch: &mut S,
) -> ChainOutcome
where
    R: FnMut(SearchResult) + Send,
    S: FnMut(BackendSwitch) + Send,
{
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut total = 0usize;
    let mut last_error: Option<String> = None;

    let mut iter = candidates.iter().peekable();
    while let Some(tool) = iter.next() {
        if cancel.is_cancelled() {
            break;
        }
        let from_id = tool.id().to_owned();

        let reason: SwitchReason = match tool.search_expanded(expanded, cancel.clone()).await {
            Err(err) => {
                let reason = match err {
                    SearchError::BackendUnavailable { .. } => SwitchReason::Unavailable,
                    _ => SwitchReason::Error,
                };
                last_error = Some(err.to_string());
                reason
            }
            Ok(mut stream) => {
                let mut this_yield = 0usize;
                let mut stream_err = false;
                while let Some(item) = stream.next().await {
                    if cancel.is_cancelled() {
                        break;
                    }
                    match item {
                        Ok(result) => {
                            // 结构化约束校验（文件类型/扩展名/路径/时间/大小等）：内容型后端
                            // （本地索引 FTS/语义）的 search_expanded 只消费关键词，不校验这些
                            // 字段——fan-out 路径靠 filter_results_for_intent 在合并前补一次
                            // （见 fanout_merge.rs），但单后端 fallback chain（<2 个可 fan-out
                            // 候选时的唯一路径）此前完全跳过该校验，导致文件名/内容关键词命中
                            // 但文件类型等条件不符的结果直接透传。此处补齐同款校验，与 fan-out
                            // 路径口径一致；先判约束再 dedup，避免约束不符的候选抢占 `seen` 名额
                            // 挡住后续候选里同路径但真正合规的结果（当前无此类场景，但语义更稳）。
                            if crate::fanout_merge::result_matches_intent(&result, &expanded.base)
                                && seen.insert(result.path.clone())
                            {
                                total += 1;
                                this_yield += 1;
                                on_result(result);
                            }
                        }
                        Err(err) => {
                            last_error = Some(err.to_string());
                            stream_err = true;
                            break;
                        }
                    }
                }
                if stream_err {
                    SwitchReason::Error
                } else if cancel.is_cancelled() {
                    // 取消导致流中断，不算候选失败，直接退出链（不发 on_switch）。
                    break;
                } else if this_yield == 0 {
                    SwitchReason::Empty
                } else {
                    return ChainOutcome {
                        total,
                        last_error: None,
                        served_by: Some(from_id.clone()),
                    };
                }
            }
        };

        if let Some(next) = iter.peek() {
            on_switch(BackendSwitch {
                from: from_id,
                to: next.id().to_owned(),
                reason,
            });
        }
    }

    ChainOutcome {
        total,
        last_error,
        served_by: None,
    }
}

// ============================================================
// 单元测试
// ============================================================

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::missing_docs_in_private_items
    )]

    use super::*;
    use crate::{SearchTool, SupportedIntent};
    use futures_util::FutureExt;
    use scout_search_backend::{
        backend_stream_from_results, BackendKind, BackendSearchFuture, CancellationToken,
        ExpandedSearchIntent, FileSearch, FileType, ImplementationStatus, MatchType, SchemaVersion,
        SearchBackend, SearchError, SearchIntent, SearchResult, SearchResultMetadata,
    };
    use std::path::PathBuf;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };

    use futures_executor::block_on;

    /// 最小 `FileSearch` intent。
    fn minimal_intent() -> SearchIntent {
        SearchIntent::FileSearch(FileSearch {
            schema_version: SchemaVersion::V1,
            language: None,
            keywords: None,
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

    /// 构造一条测试用 `SearchResult`。
    fn result_at(path: &str) -> SearchResult {
        SearchResult {
            id: path.to_owned(),
            path: PathBuf::from(path),
            name: path.to_owned(),
            source: BackendKind::Spotlight,
            match_type: MatchType::Filename,
            score: None,
            metadata: SearchResultMetadata::default(),
        }
    }

    // ----- Mock 后端脚本 -----

    enum Script {
        /// pre-stream `BackendUnavailable`
        Unavailable,
        /// 正常 N 条结果
        Results(Vec<SearchResult>),
        /// N 条结果后流中途 Err(Io)
        ResultsThenError(Vec<SearchResult>),
        /// 流首轮 poll 时取消 token，然后立即返回空流（零结果）。
        /// 用于真正覆盖 I-1 修复路径：候选已进入、search 已调用、流已开始，
        /// 但首轮 poll 前 token 被取消，此时应 break 退链且不发 `on_switch`。
        CancelThenEmpty,
    }

    struct MockBackend {
        kind: BackendKind,
        script: Mutex<Script>,
        /// 记录 search 是否被调用（用于断言"未被调用"）
        pub called: Arc<AtomicBool>,
    }

    impl std::fmt::Debug for MockBackend {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MockBackend")
                .field("kind", &self.kind)
                .field("called", &self.called)
                .finish_non_exhaustive()
        }
    }

    impl MockBackend {
        fn new(script: Script) -> Self {
            Self {
                kind: BackendKind::Spotlight,
                script: Mutex::new(script),
                called: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    impl SearchBackend for MockBackend {
        fn kind(&self) -> BackendKind {
            self.kind
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
            cancel: CancellationToken,
        ) -> BackendSearchFuture<'a> {
            self.called.store(true, Ordering::SeqCst);
            let guard = self.script.lock().unwrap();
            match &*guard {
                Script::Unavailable => {
                    // 恢复为 Unavailable 以便多次调用（但测试里只调一次）
                    let fut = async move {
                        Err(SearchError::BackendUnavailable {
                            reason: "mock unavailable".to_owned(),
                        })
                    };
                    fut.boxed()
                }
                Script::Results(results) => {
                    let results = results.clone();
                    async move { Ok(backend_stream_from_results(results, cancel)) }.boxed()
                }
                Script::ResultsThenError(results) => {
                    let results = results.clone();
                    async move {
                        let items: Vec<Result<SearchResult, SearchError>> = results
                            .into_iter()
                            .map(Ok)
                            .chain(std::iter::once(Err(SearchError::Io {
                                detail: "mock stream error".to_owned(),
                            })))
                            .collect();
                        Ok(Box::pin(futures_util::stream::iter(items))
                            as scout_search_backend::BackendStream)
                    }
                    .boxed()
                }
                Script::CancelThenEmpty => {
                    // 构造一个自定义流：首轮 poll 时取消 token，然后立即返回空流。
                    // 这样外层循环的 cancel 检查（L75）还没触发（token 此时未取消），
                    // 候选 a 正常被进入并调用 search，流开始后首轮 poll 才取消，
                    // 走到 L113 的 `else if cancel.is_cancelled() { break; }` 路径。
                    let token = cancel.clone();
                    let stream = futures_util::stream::poll_fn(
                        move |_cx| -> std::task::Poll<Option<Result<SearchResult, SearchError>>> {
                            token.cancel();
                            std::task::Poll::Ready(None)
                        },
                    );
                    async move { Ok(Box::pin(stream) as scout_search_backend::BackendStream) }
                        .boxed()
                }
            }
        }
    }

    // 对 Script::Unavailable 需要 guard 释放再用，用 replace 保持 Mutex 正确
    // 实际上上面的 match 已经 ok，但我们需要注意 guard drop 时机。
    // 上面实现里 guard 在 match body 结束时 drop，返回 Future 时已经释放，OK。

    /// 把 `MockBackend` 包成 `Arc<dyn SearchableTool>`，并返回 `called` 标志供断言。
    fn make_tool(id: &str, script: Script) -> (Arc<dyn SearchableTool>, Arc<AtomicBool>) {
        let backend = MockBackend::new(script);
        let called = Arc::clone(&backend.called);
        let tool: Arc<dyn SearchableTool> = Arc::new(SearchTool::new(
            id,
            id,
            backend,
            vec![SupportedIntent::FileSearch],
            id,
        ));
        (tool, called)
    }

    /// 驱动一次 `run_fallback_chain`，收集结果、switches、outcome。
    struct RunResult {
        results: Vec<SearchResult>,
        switches: Vec<BackendSwitch>,
        outcome: ChainOutcome,
    }

    fn run(candidates: &[Arc<dyn SearchableTool>], cancel: CancellationToken) -> RunResult {
        run_with_intent(candidates, minimal_intent(), cancel)
    }

    fn run_with_intent(
        candidates: &[Arc<dyn SearchableTool>],
        intent: SearchIntent,
        cancel: CancellationToken,
    ) -> RunResult {
        let expanded = ExpandedSearchIntent::identity(intent);
        let mut results = Vec::new();
        let mut switches = Vec::new();
        let outcome = block_on(run_fallback_chain(
            candidates,
            &expanded,
            cancel,
            &mut |r| results.push(r),
            &mut |s| switches.push(s),
        ));
        RunResult {
            results,
            switches,
            outcome,
        }
    }

    // ===== 测试 1: 单候选成功，无 switch =====
    #[test]
    fn single_candidate_success_no_switch() {
        let (tool, _) = make_tool("a", Script::Results(vec![result_at("/x"), result_at("/y")]));
        let rr = run(&[tool], CancellationToken::new());
        let paths: Vec<_> = rr
            .results
            .iter()
            .map(|r| r.path.to_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["/x", "/y"]);
        assert!(rr.switches.is_empty());
        assert_eq!(rr.outcome.total, 2);
        assert!(rr.outcome.last_error.is_none());
        assert_eq!(rr.outcome.served_by, Some("a".to_owned()));
    }

    // ===== 回归测试：单候选（无 fan-out）的结果须按 file_type 等结构化约束过滤 =====
    // 复现用户反馈的 bug：内容型后端（本地索引/语义）的 search_expanded 只按关键词
    // 命中，不校验 file_type；此前只有 fan-out 合并路径调用 filter_results_for_intent
    // 补这层校验，单后端 fallback chain 完全跳过，导致「关键词命中但文件类型不符」的
    // 结果直接透传给用户——且与「多条件匹配方式（All/Any）」设置无关，切换该设置
    // 不会改变这个透传行为，表现为「设置好像不起作用」。
    #[test]
    fn file_type_mismatch_filtered_out_in_single_backend_chain() {
        let intent = SearchIntent::FileSearch(FileSearch {
            file_type: Some(vec![FileType::Image]),
            ..match minimal_intent() {
                SearchIntent::FileSearch(fs) => fs,
                _ => unreachable!(),
            }
        });
        // /report.pdf 按关键词命中该唯一后端，但 file_type 要求仅图片——不应透传。
        let (tool, _) = make_tool(
            "a",
            Script::Results(vec![result_at("/report.pdf"), result_at("/photo.png")]),
        );
        let rr = run_with_intent(&[tool], intent, CancellationToken::new());
        let paths: Vec<_> = rr
            .results
            .iter()
            .map(|r| r.path.to_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["/photo.png"]);
        assert_eq!(rr.outcome.total, 1);
    }

    // ===== 测试 2: a=Unavailable，切到 b=[/x] =====
    #[test]
    fn unavailable_switches_to_next() {
        let (tool_a, _) = make_tool("a", Script::Unavailable);
        let (tool_b, _) = make_tool("b", Script::Results(vec![result_at("/x")]));
        let rr = run(&[tool_a, tool_b], CancellationToken::new());
        let paths: Vec<_> = rr
            .results
            .iter()
            .map(|r| r.path.to_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["/x"]);
        assert_eq!(rr.switches.len(), 1);
        assert_eq!(rr.switches[0].reason, SwitchReason::Unavailable);
        assert_eq!(rr.switches[0].from, "a");
        assert_eq!(rr.switches[0].to, "b");
        assert_eq!(rr.outcome.total, 1);
        assert_eq!(rr.outcome.served_by, Some("b".to_owned()));
    }

    // ===== 测试 3: a=空(干净), b=[/x] =====
    #[test]
    fn empty_switches_to_next() {
        let (tool_a, _) = make_tool("a", Script::Results(vec![]));
        let (tool_b, _) = make_tool("b", Script::Results(vec![result_at("/x")]));
        let rr = run(&[tool_a, tool_b], CancellationToken::new());
        let paths: Vec<_> = rr
            .results
            .iter()
            .map(|r| r.path.to_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["/x"]);
        assert_eq!(rr.switches.len(), 1);
        assert_eq!(rr.switches[0].reason, SwitchReason::Empty);
        assert_eq!(rr.outcome.total, 1);
    }

    // ===== 测试 4: a=[/x1,/x2,/x3]后崩，b=[/y1,/y2] → total=5 =====
    #[test]
    fn midstream_error_keeps_partials_and_switches() {
        let (tool_a, _) = make_tool(
            "a",
            Script::ResultsThenError(vec![result_at("/x1"), result_at("/x2"), result_at("/x3")]),
        );
        let (tool_b, _) = make_tool(
            "b",
            Script::Results(vec![result_at("/y1"), result_at("/y2")]),
        );
        let rr = run(&[tool_a, tool_b], CancellationToken::new());
        let paths: Vec<_> = rr
            .results
            .iter()
            .map(|r| r.path.to_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["/x1", "/x2", "/x3", "/y1", "/y2"]);
        assert_eq!(rr.switches.len(), 1);
        assert_eq!(rr.switches[0].reason, SwitchReason::Error);
        assert_eq!(rr.outcome.total, 5);
    }

    // ===== 测试 5: 跨候选去重 =====
    #[test]
    fn dedup_across_candidates() {
        let (tool_a, _) = make_tool("a", Script::ResultsThenError(vec![result_at("/x")]));
        let (tool_b, _) = make_tool("b", Script::Results(vec![result_at("/x"), result_at("/y")]));
        let rr = run(&[tool_a, tool_b], CancellationToken::new());
        let paths: Vec<_> = rr
            .results
            .iter()
            .map(|r| r.path.to_str().unwrap())
            .collect();
        // /x 只出现一次（B 的 /x 去重）
        assert_eq!(paths, vec!["/x", "/y"]);
        assert_eq!(rr.outcome.total, 2);
    }

    // ===== 测试 6: 全部失败 → last_error.is_some, total=0 =====
    #[test]
    fn all_fail_yields_error_outcome() {
        let (tool_a, _) = make_tool("a", Script::Unavailable);
        let (tool_b, _) = make_tool("b", Script::Results(vec![]));
        let rr = run(&[tool_a, tool_b], CancellationToken::new());
        assert!(rr.results.is_empty());
        assert_eq!(rr.outcome.total, 0);
        // a 不可用时 last_error 有值，b 空结果时 last_error 不会被覆盖
        // 实际上 a 的 BackendUnavailable 设置了 last_error；b 空但不报错，不覆盖 last_error
        // 因此 last_error 来自 a
        assert!(rr.outcome.last_error.is_some());
        assert!(rr.outcome.served_by.is_none());
    }

    // ===== 测试 7: a 成功，b 不应被调用 =====
    #[test]
    fn success_stops_chain() {
        let (tool_a, called_a) = make_tool("a", Script::Results(vec![result_at("/x")]));
        let (tool_b, called_b) = make_tool("b", Script::Results(vec![result_at("/y")]));
        let rr = run(&[tool_a, tool_b], CancellationToken::new());
        let paths: Vec<_> = rr
            .results
            .iter()
            .map(|r| r.path.to_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["/x"]);
        assert!(rr.switches.is_empty());
        assert!(called_a.load(Ordering::SeqCst));
        assert!(!called_b.load(Ordering::SeqCst), "b 不应被调用");
        assert_eq!(rr.outcome.served_by, Some("a".to_owned()));
    }

    // ===== 测试 8: 预先 cancel，b 不应被调用 =====
    #[test]
    fn cancel_stops_chain() {
        let (tool_a, _called_a) = make_tool("a", Script::Unavailable);
        let (tool_b, called_b) = make_tool("b", Script::Results(vec![result_at("/y")]));
        let cancel = CancellationToken::new();
        cancel.cancel(); // 预先取消
        let rr = run(&[tool_a, tool_b], cancel);
        assert!(rr.results.is_empty());
        assert!(!called_b.load(Ordering::SeqCst), "取消后 b 不应被调用");
        assert_eq!(rr.outcome.total, 0);
    }

    // ===== 测试 9 (M-3 / I-1 真回归): cancel 发生在流首轮 poll 时 → 不发 on_switch，不调次候选 =====
    //
    // 构造方式：token 不预取消；Script::CancelThenEmpty 的 mock 流在首轮 poll 时调用
    // token.cancel() 后立即返回 Poll::Ready(None)，使 this_yield==0 且 cancel.is_cancelled()。
    //
    // 外层循环开头（L75）检查时 token 还未取消 → 候选 a 被正常进入（called_a == true），
    // 流开始后首轮 poll 触发取消，走到 L113 的 `else if cancel.is_cancelled() { break; }`，
    // 正确退链且不发 on_switch。
    //
    // 修复前（注释掉 L113）该路径会走到 `else if this_yield == 0` 分支，
    // 误判为 SwitchReason::Empty 并发 on_switch → switches 非空 → 本测试 FAIL（反向验证通过）。
    #[test]
    fn cancel_mid_stream_no_switch_no_second_candidate() {
        let cancel = CancellationToken::new(); // 不预取消！由流首轮 poll 触发
        let (tool_a, called_a) = make_tool("a", Script::CancelThenEmpty);
        let (tool_b, called_b) = make_tool("b", Script::Results(vec![result_at("/y")]));
        let rr = run(&[tool_a, tool_b], cancel);
        assert!(rr.results.is_empty(), "取消后不应有结果");
        assert!(
            rr.switches.is_empty(),
            "流中途取消不应触发 on_switch（I-1 真回归）"
        );
        assert_eq!(rr.outcome.total, 0);
        // called_a == true 证明候选 a 真的被进入，走到了流层（而非外层预取消 break）
        assert!(
            called_a.load(Ordering::SeqCst),
            "a 应已被进入并调用（证明不是外层预取消 break）"
        );
        assert!(
            !called_b.load(Ordering::SeqCst),
            "取消后次候选 b 不应被调用"
        );
    }
}
