//! search 模块的单元/集成测试（从 search.rs 拆出，逻辑零改动）。
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use futures::stream;
use scout_harness::context::TargetRefError;
use scout_harness::file_action_tool::FileActionError;
use scout_harness::{
    NoopExpander, PolicyEngine, SearchTool, SupportedIntent, ToolCallEvent, ToolErrorEvent,
    ToolRegistry, Tracer, TracingHook,
};
use scout_search_backend::{
    BackendKind, BackendSearchFuture, BackendStream, CancellationToken, FileActionKind,
    ImplementationStatus, MatchType, SearchBackend, SearchError, SearchIntent, SearchResult,
    SearchResultMetadata,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::ipc::{Channel, InvokeResponseBody};

/// 记录 trace 事件序列。
#[derive(Default)]
struct MockHook {
    calls: Arc<Mutex<Vec<String>>>,
}
impl MockHook {
    fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let me = Self {
            calls: Arc::clone(&calls),
        };
        (me, calls)
    }
}
impl TracingHook for MockHook {
    fn on_tool_call(&self, e: &ToolCallEvent) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("call:{}", e.tool_id));
    }
    fn on_tool_result(&self, e: &scout_harness::ToolResultEvent) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("result:{}:{}", e.tool_id, e.result_count));
    }
    fn on_error(&self, e: &ToolErrorEvent) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("error:{}:{}", e.tool_id, e.error_type));
    }
}

/// 捕获前端 channel 事件。
fn capture_channel() -> (Channel<SearchEvent>, Arc<Mutex<Vec<String>>>) {
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_clone = Arc::clone(&captured);
    let ch = Channel::new(move |body| {
        if let InvokeResponseBody::Json(s) = body {
            captured_clone.lock().unwrap().push(s);
        }
        Ok(())
    });
    (ch, captured)
}

/// 2026-07-28：本文件的 fake 结果路径统一带 `.pdf` 扩展名——`result_matches_intent`
/// （`fallback_chain.rs` post-filter，见该处 commit）现在会按 intent 的 `extensions`/
/// `file_type` 约束真的过滤结果；本文件绝大多数测试流以 `QUERY_FOR_FILE_SEARCH`
/// （"find pdf"，parse 为 `extensions:["pdf"]`）为种子查询，无扩展名的 fake 路径会被
/// 正确判定为不匹配、静默丢弃，让下游「打开/复制/移动/重命名第 N 个」等断言看到空结果。
/// 用真实扩展名让 fixture 符合它自己模拟的查询语义。
const FAKE_RESULT_EXT: &str = ".pdf";

/// 返回 N 条 fake SearchResult 的 backend。
#[derive(Debug)]
struct FakeOkBackend(usize);
impl SearchBackend for FakeOkBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Spotlight
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
        let n = self.0;
        Box::pin(async move {
            let items: Vec<Result<SearchResult, SearchError>> = (0..n)
                .map(|i| {
                    Ok(SearchResult {
                        id: format!("id-{i}"),
                        path: PathBuf::from(format!("/tmp/f{i}{FAKE_RESULT_EXT}")),
                        name: format!("f{i}"),
                        source: BackendKind::Spotlight,
                        match_type: MatchType::Filename,
                        score: None,
                        metadata: SearchResultMetadata::default(),
                    })
                })
                .collect();
            Ok(Box::pin(stream::iter(items)) as BackendStream)
        })
    }
}

/// 返回指定文件名结果的 backend（BETA-05 排序测试用）。
#[derive(Debug)]
struct FakeNamedBackend(Vec<&'static str>);
impl SearchBackend for FakeNamedBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Spotlight
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
        let names = self.0.clone();
        Box::pin(async move {
            let items: Vec<Result<SearchResult, SearchError>> = names
                .into_iter()
                .map(|name| {
                    Ok(SearchResult {
                        id: name.to_owned(),
                        path: PathBuf::from(format!("/x/{name}")),
                        name: name.to_owned(),
                        source: BackendKind::Spotlight,
                        match_type: MatchType::Filename,
                        score: None,
                        metadata: SearchResultMetadata::default(),
                    })
                })
                .collect();
            Ok(Box::pin(stream::iter(items)) as BackendStream)
        })
    }
}

/// 立刻 open err 的 backend。
#[derive(Debug)]
struct FakeOpenErrBackend;
impl SearchBackend for FakeOpenErrBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Spotlight
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
        Box::pin(async { Err(SearchError::Timeout { elapsed_ms: 42 }) })
    }
}

/// 先发 1 条结果再 mid-stream err 的 backend。
#[derive(Debug)]
struct FakeMidErrBackend;
impl SearchBackend for FakeMidErrBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Spotlight
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
        Box::pin(async {
            let items: Vec<Result<SearchResult, SearchError>> = vec![
                Ok(SearchResult {
                    id: "x".into(),
                    path: PathBuf::from(format!("/tmp/x{FAKE_RESULT_EXT}")),
                    name: "x".into(),
                    source: BackendKind::Spotlight,
                    match_type: MatchType::Filename,
                    score: None,
                    metadata: SearchResultMetadata::default(),
                }),
                Err(SearchError::Io {
                    detail: "boom".into(),
                }),
            ];
            Ok(Box::pin(stream::iter(items)) as BackendStream)
        })
    }
}

/// 捕获每次 search 收到的 effective intent，返回 N 条 fake 结果。
#[derive(Debug)]
struct FakeCapturingBackend {
    seen: Arc<Mutex<Vec<SearchIntent>>>,
    n: usize,
}
impl SearchBackend for FakeCapturingBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Spotlight
    }
    fn implementation_status(&self) -> ImplementationStatus {
        ImplementationStatus::Real
    }
    fn is_available(&self) -> bool {
        true
    }
    fn search<'a>(
        &'a self,
        intent: &'a SearchIntent,
        _cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        self.seen.lock().unwrap().push(intent.clone());
        let n = self.n;
        // 多轮 refine 会改变 intent 的 extensions 约束（如 pdf → png）；结果扩展名跟着
        // intent 走，否则 refine 后 result_matches_intent 会把「上一轮扩展名」的假结果
        // 全部过滤掉，跟这个测试想验证的「refine 后仍应正常拿到结果」矛盾。
        let ext = fake_result_ext_for_intent(intent);
        Box::pin(async move {
            let items: Vec<Result<SearchResult, SearchError>> = (0..n)
                .map(|i| {
                    Ok(SearchResult {
                        id: format!("id-{i}"),
                        path: PathBuf::from(format!("/tmp/f{i}{ext}")),
                        name: format!("f{i}{ext}"),
                        source: BackendKind::Spotlight,
                        match_type: MatchType::Filename,
                        score: None,
                        metadata: SearchResultMetadata::default(),
                    })
                })
                .collect();
            Ok(Box::pin(stream::iter(items)) as BackendStream)
        })
    }
}

/// 从 intent 的 `extensions`（取首个，回退默认 [`FAKE_RESULT_EXT`]）派生 fake 结果的
/// 扩展名，供 [`FakeCapturingBackend`] 在多轮 refine（约束跟着变）场景下用。
fn fake_result_ext_for_intent(intent: &SearchIntent) -> String {
    if let SearchIntent::FileSearch(fs) = intent {
        if let Some(ext) = fs.extensions.as_ref().and_then(|v| v.first()) {
            return format!(".{ext}");
        }
    }
    FAKE_RESULT_EXT.to_owned()
}

use scout_harness::file_action_tool::{FileActionExecutor, FileActionTool};
use std::io;

/// 记录每次文件操作调用("open:/path" / "locate:/path" 等)。
#[derive(Debug, Default)]
struct MockFileActionExecutor {
    calls: Arc<Mutex<Vec<String>>>,
}
impl MockFileActionExecutor {
    fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                calls: Arc::clone(&calls),
            },
            calls,
        )
    }
}
impl FileActionExecutor for MockFileActionExecutor {
    fn open(&self, path: &std::path::Path) -> io::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("open:{}", path.display()));
        Ok(())
    }
    fn locate(&self, path: &std::path::Path) -> io::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("locate:{}", path.display()));
        Ok(())
    }
    fn copy(&self, src: &std::path::Path, _dest: &std::path::Path) -> io::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("copy:{}", src.display()));
        Ok(())
    }
    fn move_to(&self, src: &std::path::Path, _dest: &std::path::Path) -> io::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("move:{}", src.display()));
        Ok(())
    }
    fn rename(&self, src: &std::path::Path, _new_name: &str) -> io::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("rename:{}", src.display()));
        Ok(())
    }
}

/// 用 MockExecutor 建一个默认 Policy 的 FileActionTool。
fn build_file_action_tool() -> (Arc<FileActionTool>, Arc<Mutex<Vec<String>>>) {
    let (exec, calls) = MockFileActionExecutor::new();
    let tool = FileActionTool::new(Arc::new(exec), PolicyEngine::new());
    (Arc::new(tool), calls)
}

/// 建一个含 N 条结果的 ContextMemory(intent 为 FileSearch{pdf})。
fn context_with_results(n: usize) -> Arc<Mutex<ContextMemory>> {
    use scout_search_backend::{BackendKind, MatchType, SearchResult, SearchResultMetadata};
    let results: Vec<SearchResult> = (0..n)
        .map(|i| SearchResult {
            id: format!("id-{i}"),
            path: PathBuf::from(format!("/tmp/f{i}")),
            name: format!("f{i}"),
            source: BackendKind::Spotlight,
            match_type: MatchType::Filename,
            score: None,
            metadata: SearchResultMetadata::default(),
        })
        .collect();
    let mut ctx = ContextMemory::new();
    ctx.record(mk_base_file_search_pdf(), results);
    Arc::new(Mutex::new(ctx))
}

/// 构造一个 FileAction intent。
fn mk_file_action(
    kind: scout_search_backend::FileActionKind,
    idx: u32,
) -> scout_search_backend::FileAction {
    use scout_search_backend::{FileAction, Language, SchemaVersion, TargetRef, TargetSelector};
    FileAction {
        schema_version: SchemaVersion::V1,
        language: Some(Language::Zh),
        action: kind,
        target_ref: TargetRef::LastResults {
            selector: TargetSelector::Index { value: idx },
        },
        destination: None,
        new_name: None,
        requires_confirmation: true,
    }
}

/// 用 capturing backend 建 registry，并返回捕获表句柄。
fn build_capturing_registry(n: usize) -> (Arc<ToolRegistry>, Arc<Mutex<Vec<SearchIntent>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let backend = FakeCapturingBackend {
        seen: Arc::clone(&seen),
        n,
    };
    let mut r = ToolRegistry::new();
    let tool = SearchTool::new(
        "search.fake",
        "Fake",
        backend,
        vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
        "capturing fake backend",
    );
    r.register_search(tool).unwrap();
    (Arc::new(r), seen)
}

fn build_test_registry(
    backend: impl SearchBackend + 'static,
    supported: Vec<SupportedIntent>,
) -> Arc<ToolRegistry> {
    let mut r = ToolRegistry::new();
    let tool = SearchTool::new(
        "search.fake",
        "Fake",
        backend,
        supported,
        "fake backend for test",
    );
    r.register_search(tool).unwrap();
    Arc::new(r)
}

fn build_tracer_with_mock() -> (Arc<Tracer>, Arc<Mutex<Vec<String>>>) {
    let (mock, calls) = MockHook::new();
    let tracer = Arc::new(Tracer::with_hooks(vec![Box::new(mock)]));
    (tracer, calls)
}

fn empty_context() -> Arc<Mutex<ContextMemory>> {
    Arc::new(Mutex::new(ContextMemory::new()))
}

/// 空 registry，供不依赖搜索后端的 file-action 测试构造 SearchDeps 用。
fn empty_registry_arc() -> Arc<ToolRegistry> {
    Arc::new(ToolRegistry::new())
}

#[test]
fn search_deps_new_exposes_registry() {
    let registry = build_test_registry(FakeOkBackend(0), vec![SupportedIntent::FileSearch]);
    let policy = Arc::new(PolicyEngine::new());
    let (tracer, _calls) = build_tracer_with_mock();
    let deps = SearchDeps::new(
        registry,
        policy,
        tracer,
        empty_context(),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    let summaries = scout_harness::CapabilityDiscovery::new(deps.registry()).backend_summary();
    assert!(
        summaries.iter().any(|s| s.id == "search.fake"),
        "registry() 应暴露已注册的 fake 后端, 实得 {summaries:?}"
    );
}

#[test]
fn search_deps_holds_synonym_expander() {
    let registry = build_test_registry(FakeOkBackend(0), vec![SupportedIntent::FileSearch]);
    let policy = Arc::new(PolicyEngine::new());
    let (tracer, _calls) = build_tracer_with_mock();
    let deps = SearchDeps::new(
        registry,
        policy,
        tracer,
        empty_context(),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    // 取得 expander 引用,验证可访问
    let _ = deps.synonym_expander();
}

/// BETA-15B-1 F3：with_embedding 注入的句柄经 embedding() 原样取回（desktop 接缝回路）。
/// 断言取回的 Arc 与注入的是同一实例（ptr_eq），守住 main.rs「与 registry 共享同一句柄」契约。
#[test]
fn search_deps_with_embedding_round_trips() {
    let registry = build_test_registry(FakeOkBackend(0), vec![SupportedIntent::FileSearch]);
    let policy = Arc::new(PolicyEngine::new());
    let (tracer, _calls) = build_tracer_with_mock();
    let deps = SearchDeps::new(
        registry,
        policy,
        tracer,
        empty_context(),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    let handle = Arc::new(embedding_model::EmbeddingModelHandle::new(
        None,
        PathBuf::from("/tmp/scout-embed-roundtrip"),
    ));
    let deps = deps.with_embedding(Arc::clone(&handle));
    assert!(
        Arc::ptr_eq(deps.embedding(), &handle),
        "embedding() 应原样取回 with_embedding 注入的同一 Arc"
    );
}

/// "find pdf" 稳定解析为 FileSearch（经 CLI 验证）。
const QUERY_FOR_FILE_SEARCH: &str = "find pdf";

/// "找最近的" 稳定解析为 Clarify(AmbiguousTime)，router 返回 ClarifyNotRoutable。
const QUERY_CLARIFY: &str = "找最近的";

#[tokio::test]
async fn search_impl_success_emits_call_then_result() {
    let registry = build_test_registry(
        FakeOkBackend(3),
        vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
    );
    let policy = Arc::new(PolicyEngine::new());
    let (tracer, calls) = build_tracer_with_mock();
    let (ch, captured) = capture_channel();

    let deps = SearchDeps::new(
        registry,
        policy,
        tracer,
        empty_context(),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    search_impl(QUERY_FOR_FILE_SEARCH.into(), None, ch, &deps)
        .await
        .unwrap();

    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "应 1 call + 1 result, 实得 {calls:?}");
    assert!(calls[0].starts_with("call:search.fake"), "{calls:?}");
    assert_eq!(calls[1], "result:search.fake:3", "{calls:?}");

    let events = captured.lock().unwrap();
    assert!(
        events.iter().any(|e| e.contains("\"started\"")),
        "captured: {events:?}"
    );
    assert!(
        events.iter().any(|e| e.contains("\"complete\"")),
        "captured: {events:?}"
    );
}

// chain 接入后语义变化：
// - open_err（pre-stream 失败，零结果）：chain 返回 total=0 → on_tool_result(0) + UI Complete(0)。
//   0 命中不是错误（见「找文件没命中/Everything 未启用不应报错」用户反馈）：无论零结果
//   是真无匹配还是某候选后端失败，UI 都应落空态而非错误态。last_error 仅落诊断日志。
//   旧：on_error("Timeout") + UI Error；新：on_tool_result(0) + UI Complete(0)。
// - mid_stream_err（1 条 ok 后 Io）：chain 保留已收到的部分结果（total=1）→
//   on_tool_result(1) + UI Complete(1)。这是 fallback chain 的设计新语义：
//   有部分结果时不因 stream 中途崩而丢弃，视为成功（等待 fallback 或直接 Complete）。
//   旧：on_error("Io") + UI Error；新：on_tool_result(1) + UI Complete。

#[tokio::test]
async fn search_impl_open_err_emits_call_then_result_zero_and_complete() {
    // open_err（pre-stream Timeout）：chain 零结果 → on_tool_result(0) + UI Complete(0)，非 Error
    let registry = build_test_registry(
        FakeOpenErrBackend,
        vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
    );
    let policy = Arc::new(PolicyEngine::new());
    let (tracer, calls) = build_tracer_with_mock();
    let (ch, captured) = capture_channel();

    let deps = SearchDeps::new(
        registry,
        policy,
        tracer,
        empty_context(),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    search_impl(QUERY_FOR_FILE_SEARCH.into(), None, ch, &deps)
        .await
        .unwrap();

    let calls = calls.lock().unwrap().clone();
    // chain 后：1 call + 1 result(0)（on_tool_result 用 total=0 代替旧 on_error）
    assert_eq!(calls.len(), 2, "应 1 call + 1 result(0), 实得 {calls:?}");
    assert!(calls[0].starts_with("call:search.fake"), "{calls:?}");
    assert_eq!(calls[1], "result:search.fake:0", "{calls:?}");

    let events = captured.lock().unwrap();
    assert!(
        events.iter().any(|e| e.contains("\"complete\"")),
        "captured channel 应包含 SearchEvent::Complete, 实得: {events:?}"
    );
    assert!(
        !events.iter().any(|e| e.contains("\"error\"")),
        "0 命中不应发 SearchEvent::Error, 实得: {events:?}"
    );
}

#[tokio::test]
async fn search_impl_mid_stream_err_keeps_partial_results_and_completes() {
    // mid_stream_err（1 条 ok 后 Io）：chain 保留 1 条部分结果 → Complete(1)，非 Error
    let registry = build_test_registry(
        FakeMidErrBackend,
        vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
    );
    let policy = Arc::new(PolicyEngine::new());
    let (tracer, calls) = build_tracer_with_mock();
    let (ch, captured) = capture_channel();

    let deps = SearchDeps::new(
        registry,
        policy,
        tracer,
        empty_context(),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    search_impl(QUERY_FOR_FILE_SEARCH.into(), None, ch, &deps)
        .await
        .unwrap();

    let calls = calls.lock().unwrap().clone();
    // chain 后：1 call + 1 result(1)（部分结果被保留，不发 on_error）
    assert_eq!(calls.len(), 2, "应 1 call + 1 result(1), 实得 {calls:?}");
    assert!(calls[0].starts_with("call:search.fake"), "{calls:?}");
    assert_eq!(calls[1], "result:search.fake:1", "{calls:?}");

    let events = captured.lock().unwrap();
    assert!(
        events.iter().any(|e| e.contains("\"complete\"")),
        "有部分结果时应 Complete 非 Error, 实得: {events:?}"
    );
    assert!(
        !events.iter().any(|e| e.contains("\"error\"")),
        "有部分结果时不应发 SearchEvent::Error, 实得: {events:?}"
    );
}

/// "find files containing budget" 稳定解析为 FileSearch + keyword（内容查询）。
const QUERY_CONTENT_KEYWORD: &str = "find files containing budget";

#[tokio::test]
async fn search_impl_content_query_fans_out_and_merges_two_backends() {
    // 两个 content-capable 后端（kind=Spotlight）→ 内容查询走 fan-out；两后端返回相同
    // 2 条路径 → Result Normalizer 去重合并为 2 条。验证 BETA-04 fan-out 分流 + 合并接通。
    let mut r = ToolRegistry::new();
    r.register_search(SearchTool::new(
        "search.a",
        "A",
        FakeOkBackend(2),
        vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
        "a",
    ))
    .unwrap();
    r.register_search(SearchTool::new(
        "search.b",
        "B",
        FakeOkBackend(2),
        vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
        "b",
    ))
    .unwrap();

    let policy = Arc::new(PolicyEngine::new());
    let (tracer, calls) = build_tracer_with_mock();
    let (ch, captured) = capture_channel();
    let deps = SearchDeps::new(
        Arc::new(r),
        policy,
        tracer,
        empty_context(),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    search_impl(QUERY_CONTENT_KEYWORD.into(), None, ch, &deps)
        .await
        .unwrap();

    // fan-out：on_tool_call(first=search.a) + on_tool_result(合并去重后 total=2)。
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "应 1 call + 1 result, 实得 {calls:?}");
    assert!(calls[0].starts_with("call:search.a"), "{calls:?}");
    assert_eq!(
        calls[1], "result:search.a:2",
        "合并去重后应 2 条, 实得 {calls:?}"
    );

    let events = captured.lock().unwrap();
    assert!(
        events.iter().any(|e| e.contains("\"complete\"")),
        "captured: {events:?}"
    );
    // 结果项应带 sources 字段（多源溯源）。
    assert!(
        events.iter().any(|e| e.contains("\"sources\"")),
        "Result 应含 sources 字段, 实得: {events:?}"
    );
}

/// "查找周华健的歌" 稳定解析为 MediaSearch(audio, artist=周华健) + sort=relevance_desc
/// → Ranker 走相关性分支（file_search 默认 modified_desc，media_search 默认 relevance_desc）。
const QUERY_MEDIA_ARTIST: &str = "查找周华健的歌";

#[tokio::test]
async fn search_impl_fanout_ranks_name_match_first() {
    // backend A 返回 [其他歌曲.mp3, 周华健-朋友.mp3]（命中 artist 的故意放后），backend B 空。
    // 媒体查询（relevance 默认）→ fan-out → Ranker 按 name-match 把命中 artist 的排到前面。
    let mut r = ToolRegistry::new();
    r.register_search(SearchTool::new(
        "search.a",
        "A",
        FakeNamedBackend(vec!["其他歌曲.mp3", "周华健-朋友.mp3"]),
        vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
        "a",
    ))
    .unwrap();
    r.register_search(SearchTool::new(
        "search.b",
        "B",
        FakeNamedBackend(vec![]),
        vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
        "b",
    ))
    .unwrap();

    let policy = Arc::new(PolicyEngine::new());
    let (tracer, _calls) = build_tracer_with_mock();
    let (ch, captured) = capture_channel();
    let deps = SearchDeps::new(
        Arc::new(r),
        policy,
        tracer,
        empty_context(),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    search_impl(QUERY_MEDIA_ARTIST.into(), None, ch, &deps)
        .await
        .unwrap();

    // 在 Result 事件流里，命中 artist 的「周华健-朋友.mp3」应先于「其他歌曲.mp3」（相关性排序生效）。
    let events = captured.lock().unwrap();
    let hit_pos = events.iter().position(|e| e.contains("周华健-朋友.mp3"));
    let other_pos = events.iter().position(|e| e.contains("其他歌曲.mp3"));
    assert!(
        hit_pos.is_some() && other_pos.is_some(),
        "两个结果都应出现: {events:?}"
    );
    assert!(hit_pos < other_pos, "命中 artist 的歌应排在前: {events:?}");
}

// ---- BETA-19 跨范畴多类型均衡 ----

/// 构造最小 `FileSearch`（仅 file_type / extensions 可变，其余默认）。
fn mk_fs(file_type: Option<Vec<FileType>>, extensions: Option<Vec<String>>) -> SearchIntent {
    SearchIntent::FileSearch(FileSearch {
        schema_version: SchemaVersion::V1,
        language: None,
        keywords: None,
        extensions,
        file_type,
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
fn multi_file_types_detects_cross_category() {
    let i = mk_fs(Some(vec![FileType::Video, FileType::Image]), None);
    assert_eq!(
        multi_file_types(&i),
        Some(vec![FileType::Video, FileType::Image])
    );
}

#[test]
fn multi_file_types_single_is_none() {
    assert_eq!(
        multi_file_types(&mk_fs(Some(vec![FileType::Image]), None)),
        None
    );
}

#[test]
fn multi_file_types_dedups_below_two() {
    // 重复类型去重后 <2 → 不触发（防御性）。
    assert_eq!(
        multi_file_types(&mk_fs(Some(vec![FileType::Image, FileType::Image]), None)),
        None
    );
}

#[test]
fn multi_file_types_no_filetype_is_none() {
    assert_eq!(multi_file_types(&mk_fs(None, None)), None);
}

#[test]
fn single_type_expanded_narrows_extensions_to_subset() {
    // 「ppt和pdf」并集 [ppt,pptx,pdf] + [Presentation,Document]：
    // 拆 Presentation → file_type=[Presentation]、extensions 交集 [ppt,pptx]；
    // 拆 Image（不在并集）→ 交集空 → extensions None（让后端按 file_type 派生）。
    let base = mk_fs(
        Some(vec![FileType::Presentation, FileType::Document]),
        Some(vec!["ppt".to_owned(), "pptx".to_owned(), "pdf".to_owned()]),
    );
    let expanded = scout_search_backend::ExpandedSearchIntent::identity(base);

    let pres = single_type_expanded(&expanded, FileType::Presentation);
    let SearchIntent::FileSearch(fs) = &pres.base else {
        panic!("应为 FileSearch");
    };
    assert_eq!(fs.file_type, Some(vec![FileType::Presentation]));
    assert_eq!(
        fs.extensions,
        Some(vec!["ppt".to_owned(), "pptx".to_owned()])
    );

    let img = single_type_expanded(&expanded, FileType::Image);
    let SearchIntent::FileSearch(fs) = &img.base else {
        panic!("应为 FileSearch");
    };
    assert_eq!(fs.file_type, Some(vec![FileType::Image]));
    assert!(fs.extensions.is_none(), "交集空应回 None");
}

#[test]
fn single_type_expanded_keeps_none_extensions() {
    // 「图片和视频」extensions 本就 None → 拆后仍 None、file_type 单值。
    let base = mk_fs(Some(vec![FileType::Video, FileType::Image]), None);
    let expanded = scout_search_backend::ExpandedSearchIntent::identity(base);
    let sub = single_type_expanded(&expanded, FileType::Video);
    let SearchIntent::FileSearch(fs) = &sub.base else {
        panic!("应为 FileSearch");
    };
    assert_eq!(fs.file_type, Some(vec![FileType::Video]));
    assert!(fs.extensions.is_none());
}

/// 按 `file_type` 返回不同结果的 backend：模拟图片多、视频少（少数派碾压场景）。
#[derive(Debug)]
struct FakeTypeAwareBackend;
impl SearchBackend for FakeTypeAwareBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::WindowsSearch
    }
    fn implementation_status(&self) -> ImplementationStatus {
        ImplementationStatus::Real
    }
    fn is_available(&self) -> bool {
        true
    }
    fn search<'a>(
        &'a self,
        intent: &'a SearchIntent,
        _cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        let names: Vec<&'static str> = match intent {
            SearchIntent::FileSearch(fs) => match fs.file_type.as_deref() {
                Some([FileType::Video]) => vec!["vid1.mp4"],
                Some([FileType::Image]) => vec!["img1.jpg", "img2.jpg", "img3.jpg"],
                _ => vec![],
            },
            _ => vec![],
        };
        Box::pin(async move {
            let items: Vec<Result<SearchResult, SearchError>> = names
                .into_iter()
                .map(|name| {
                    Ok(SearchResult {
                        id: name.to_owned(),
                        path: PathBuf::from(format!("/m/{name}")),
                        name: name.to_owned(),
                        source: BackendKind::WindowsSearch,
                        match_type: MatchType::Filename,
                        score: None,
                        metadata: SearchResultMetadata::default(),
                    })
                })
                .collect();
            Ok(Box::pin(stream::iter(items)) as BackendStream)
        })
    }
}

#[tokio::test]
async fn search_impl_balanced_multitype_surfaces_minority() {
    // 「图片和视频」→ file_type=[Image,Video]（BETA-13-G3：多 file_type 按 query 语序）：
    // 按类型分查 → 图片桶[img1,img2,img3] / 视频桶[vid1] → round-robin 交错
    // （img1, vid1, img2, img3）让少数派视频「前列可见、不被碾压到末尾」。
    let mut r = ToolRegistry::new();
    r.register_search(SearchTool::new(
        "search.win",
        "Win",
        FakeTypeAwareBackend,
        vec![SupportedIntent::FileSearch],
        "win",
    ))
    .unwrap();

    let policy = Arc::new(PolicyEngine::new());
    let (tracer, _calls) = build_tracer_with_mock();
    let (ch, captured) = capture_channel();
    let deps = SearchDeps::new(
        Arc::new(r),
        policy,
        tracer,
        empty_context(),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    search_impl("找图片和视频".into(), None, ch, &deps)
        .await
        .unwrap();

    let events = captured.lock().unwrap();
    let vid_pos = events.iter().position(|e| e.contains("vid1.mp4"));
    // 最后一张图片的位置：用于验证少数派视频未被碾压到所有图片之后。
    let last_img = events.iter().rposition(|e| e.contains(".jpg"));
    assert!(vid_pos.is_some(), "少数派视频应出现: {events:?}");
    assert!(last_img.is_some(), "图片应出现: {events:?}");
    // BETA-13-G3 后多 file_type 按 query 语序（「图片和视频」→[Image,Video]）、图片桶在前；
    // round-robin 交错（img1, vid1, img2, img3）保证少数派视频「前列可见、不被碾压到末尾」
    // ——即排在最后一张图片之前（设计意图见 fanout.rs::run_balanced_multitype_search）。
    assert!(
        vid_pos < last_img,
        "round-robin 应让少数派视频前列可见、不被碾压到末尾（排在最后一张图片之前）: {events:?}"
    );
}

#[tokio::test]
async fn search_impl_pre_tool_failure_emits_no_trace() {
    // "找最近的" → Clarify(AmbiguousTime) → ClarifyNotRoutable → pre-tool failure
    let registry = build_test_registry(
        FakeOkBackend(0),
        vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
    );
    let policy = Arc::new(PolicyEngine::new());
    let (tracer, calls) = build_tracer_with_mock();
    let (ch, captured) = capture_channel();

    let deps = SearchDeps::new(
        registry,
        policy,
        tracer,
        empty_context(),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    search_impl(QUERY_CLARIFY.into(), None, ch, &deps)
        .await
        .unwrap();

    let calls = calls.lock().unwrap().clone();
    assert!(
        calls.is_empty(),
        "pre-tool 失败不应触发 trace, 实得 {calls:?}"
    );

    let events = captured.lock().unwrap();
    assert!(
        events.iter().any(|e| e.contains("\"error\"")),
        "captured channel 应包含 SearchEvent::Error, 实得: {events:?}"
    );
}

// ---- Task 3: 多轮 refine 集成测试 ----

/// "只看 png" 稳定解析为 Refine(delta.extensions=[png])。
const QUERY_REFINE_PNG: &str = "只看 png";
/// "只看下载目录" 稳定解析为 Refine(delta.location.hint=下载)。
const QUERY_REFINE_DOWNLOADS: &str = "只看下载目录";

#[tokio::test]
async fn search_impl_record_then_refine_merges_base() {
    let (registry, seen) = build_capturing_registry(2);
    let policy = Arc::new(PolicyEngine::new());
    let (tracer, _calls) = build_tracer_with_mock();
    let ctx = empty_context();

    // 第一轮:基准 find pdf → 记录 FileSearch{pdf}
    let (ch1, _c1) = capture_channel();
    let deps1 = SearchDeps::new(
        Arc::clone(&registry),
        Arc::clone(&policy),
        Arc::clone(&tracer),
        Arc::clone(&ctx),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    search_impl(QUERY_FOR_FILE_SEARCH.into(), None, ch1, &deps1)
        .await
        .unwrap();

    // 第二轮:refine 只看 png → 合并上一轮 → FileSearch{png}
    let (ch2, events2) = capture_channel();
    let deps2 = SearchDeps::new(
        Arc::clone(&registry),
        Arc::clone(&policy),
        Arc::clone(&tracer),
        Arc::clone(&ctx),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    search_impl(QUERY_REFINE_PNG.into(), None, ch2, &deps2)
        .await
        .unwrap();

    // 第二轮应成功(complete 而非 error)
    let events2 = events2.lock().unwrap();
    assert!(
        events2.iter().any(|e| e.contains("\"complete\"")),
        "refine 第二轮应 complete, 实得: {events2:?}"
    );
    assert!(
        !events2.iter().any(|e| e.contains("\"error\"")),
        "refine 第二轮不应 error, 实得: {events2:?}"
    );

    // capturing backend 第二次收到的 effective intent 应是合并后的 FileSearch{png}
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "应两次进 backend, 实得 {}", seen.len());
    match &seen[1] {
        SearchIntent::FileSearch(fs) => {
            assert_eq!(
                fs.extensions,
                Some(vec!["png".to_owned()]),
                "合并后 extensions 应为 png"
            );
        }
        other => panic!("第二轮 effective 应是 FileSearch, 实得 {other:?}"),
    }
}

#[tokio::test]
async fn search_impl_refine_without_context_errors() {
    let (registry, seen) = build_capturing_registry(1);
    let policy = Arc::new(PolicyEngine::new());
    let (tracer, calls) = build_tracer_with_mock();
    let ctx = empty_context();

    let (ch, events) = capture_channel();
    let deps = SearchDeps::new(
        registry,
        Arc::clone(&policy),
        tracer,
        ctx,
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    search_impl(QUERY_REFINE_PNG.into(), None, ch, &deps)
        .await
        .unwrap();

    // 应 error 且文案含"上一轮"
    let events = events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.contains("\"error\"") && e.contains("上一轮")),
        "空 context refine 应 error 且文案含'上一轮', 实得: {events:?}"
    );
    // pre-tool 失败:不进 trace、不进 backend
    assert!(calls.lock().unwrap().is_empty(), "合并失败不应 trace");
    assert!(seen.lock().unwrap().is_empty(), "合并失败不应进 backend");
}

#[tokio::test]
async fn search_impl_chained_refine_accumulates() {
    let (registry, seen) = build_capturing_registry(1);
    let policy = Arc::new(PolicyEngine::new());
    let (tracer, _calls) = build_tracer_with_mock();
    let ctx = empty_context();

    // 基准 → refine1(下载目录) → refine2(png)
    for q in [
        QUERY_FOR_FILE_SEARCH,
        QUERY_REFINE_DOWNLOADS,
        QUERY_REFINE_PNG,
    ] {
        let (ch, _c) = capture_channel();
        let deps = SearchDeps::new(
            Arc::clone(&registry),
            Arc::clone(&policy),
            Arc::clone(&tracer),
            Arc::clone(&ctx),
            build_file_action_tool().0,
            empty_pending(),
            Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
        );
        search_impl(q.into(), None, ch, &deps).await.unwrap();
    }

    // 末轮 effective 应同时含 refine1(location 下载) + refine2(extensions png)
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3, "应三轮进 backend");
    match &seen[2] {
        SearchIntent::FileSearch(fs) => {
            assert_eq!(
                fs.extensions,
                Some(vec!["png".to_owned()]),
                "末轮应含 refine2 的 png"
            );
            let hint = fs.location.as_ref().and_then(|l| l.hint.as_deref());
            assert_eq!(hint, Some("下载"), "末轮应保留 refine1 的 location 下载");
        }
        other => panic!("末轮 effective 应是 FileSearch, 实得 {other:?}"),
    }
}

// ---- Task 1: apply_refine_if_needed 单元测试 ----

use scout_harness::context::{ContextMemory, RefineMergeError};
use scout_search_backend::{
    BaseRef, FileSearch, FileType, Language, Refine, RefineDelta, SchemaVersion,
};

fn mk_base_file_search_pdf() -> SearchIntent {
    SearchIntent::FileSearch(FileSearch {
        schema_version: SchemaVersion::V1,
        language: Some(Language::Zh),
        keywords: None,
        extensions: Some(vec!["pdf".to_owned()]),
        file_type: Some(vec![FileType::Document]),
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

fn mk_refine_extensions_png() -> SearchIntent {
    SearchIntent::Refine(Refine {
        schema_version: SchemaVersion::V1,
        language: Some(Language::Zh),
        base_ref: BaseRef::LastIntent,
        delta: RefineDelta {
            extensions: Some(vec!["png".to_owned()]),
            ..RefineDelta::default()
        },
        clear: None,
    })
}

#[test]
fn apply_refine_passthrough_non_refine() {
    let ctx = ContextMemory::new();
    let intent = mk_base_file_search_pdf();
    let out = apply_refine_if_needed(intent, &ctx).unwrap();
    match out {
        SearchIntent::FileSearch(fs) => {
            assert_eq!(fs.extensions, Some(vec!["pdf".to_owned()]));
        }
        other => panic!("应原样返回 FileSearch, 实得 {other:?}"),
    }
}

#[test]
fn apply_refine_merges_with_context() {
    let mut ctx = ContextMemory::new();
    ctx.record(mk_base_file_search_pdf(), vec![]);
    let out = apply_refine_if_needed(mk_refine_extensions_png(), &ctx).unwrap();
    match out {
        SearchIntent::FileSearch(fs) => {
            // delta 覆盖：extensions 应变成 png
            assert_eq!(fs.extensions, Some(vec!["png".to_owned()]));
            // file_type 不在 delta 中 → 应保留基准的 Document
            assert_eq!(fs.file_type, Some(vec![FileType::Document]));
        }
        other => panic!("合并后应是 FileSearch, 实得 {other:?}"),
    }
}

#[test]
fn apply_refine_without_context_errors() {
    let ctx = ContextMemory::new();
    let err = apply_refine_if_needed(mk_refine_extensions_png(), &ctx).unwrap_err();
    assert!(
        matches!(err, RefineMergeError::NoLastIntent),
        "空 context 应 NoLastIntent, 实得 {err:?}"
    );
}

// 保留原有 search_error_kind 测试
use scout_search_backend::SearchError as SE2;
#[test]
fn search_error_kind_maps_all_variants() {
    assert_eq!(
        search_error_kind(&SE2::BackendUnavailable { reason: "x".into() }),
        "BackendUnavailable"
    );
    assert_eq!(
        search_error_kind(&SE2::PermissionDenied {
            path: Some(PathBuf::from("/x"))
        }),
        "PermissionDenied"
    );
    assert_eq!(
        search_error_kind(&SE2::InvalidIntent { detail: "x".into() }),
        "InvalidIntent"
    );
    assert_eq!(
        search_error_kind(&SE2::UnsupportedIntent { detail: "x".into() }),
        "UnsupportedIntent"
    );
    assert_eq!(
        search_error_kind(&SE2::Timeout { elapsed_ms: 1000 }),
        "Timeout"
    );
    assert_eq!(search_error_kind(&SE2::Io { detail: "x".into() }), "Io");
}

#[test]
fn file_action_error_kind_maps_variants() {
    assert_eq!(
        file_action_error_kind(&FileActionError::TargetRef(TargetRefError::NoLastResults)),
        "NoLastResults"
    );
    assert_eq!(
        file_action_error_kind(&FileActionError::TargetRef(
            TargetRefError::IndexOutOfRange {
                requested: 9,
                available: 2
            }
        )),
        "IndexOutOfRange"
    );
    assert_eq!(
        file_action_error_kind(&FileActionError::EmptyTargets),
        "EmptyTargets"
    );
    assert_eq!(
        file_action_error_kind(&FileActionError::DeleteNotSupported),
        "DeleteNotSupported"
    );
    assert_eq!(
        file_action_error_kind(&FileActionError::PolicyDenied { reason: "x".into() }),
        "PolicyDenied"
    );
}

#[test]
fn friendly_message_for_common_errors() {
    let no_last = FileActionError::TargetRef(TargetRefError::NoLastResults);
    assert!(friendly_file_action_message(&no_last).contains("请先发起一次搜索"));

    let oob = FileActionError::TargetRef(TargetRefError::IndexOutOfRange {
        requested: 9,
        available: 2,
    });
    let msg = friendly_file_action_message(&oob);
    assert!(msg.contains('9') && msg.contains('2'), "实得: {msg}");
}

// ---- Task 2: handle_file_action 单元测试 ----

#[tokio::test]
async fn handle_file_action_open_executes() {
    let (tool, calls) = build_file_action_tool();
    let (tracer, trace_calls) = build_tracer_with_mock();
    let ctx = context_with_results(2);
    let (ch, events) = capture_channel();

    let deps = SearchDeps::new(
        empty_registry_arc(),
        Arc::new(PolicyEngine::new()),
        tracer,
        ctx,
        tool,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    handle_file_action(mk_file_action(FileActionKind::Open, 1), ch, &deps)
        .await
        .unwrap();

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "应 1 次 open, 实得 {calls:?}");
    assert_eq!(calls[0], "open:/tmp/f0", "应打开第 1 个(0-based f0)");
    let events = events.lock().unwrap();
    assert!(
        events.iter().any(|e| e.contains("\"action_done\"")),
        "实得 {events:?}"
    );
    assert!(
        events.iter().any(|e| e.contains("open")),
        "action_kind 应含 open"
    );
    assert!(
        events.iter().any(|e| e.contains("/tmp/f0")),
        "action_done 应含路径, 实得 {events:?}"
    );
    let tc = trace_calls.lock().unwrap();
    assert!(
        tc.iter().any(|c| c.starts_with("call:")),
        "应有 tool_call, 实得 {tc:?}"
    );
    assert!(
        tc.iter().any(|c| c.starts_with("result:")),
        "应有 tool_result, 实得 {tc:?}"
    );
    assert!(
        !tc.iter().any(|c| c.starts_with("error:")),
        "成功路径不应有 error, 实得 {tc:?}"
    );
}

#[tokio::test]
async fn handle_file_action_locate_executes() {
    let (tool, calls) = build_file_action_tool();
    let (tracer, trace_calls) = build_tracer_with_mock();
    let ctx = context_with_results(3);
    let (ch, events) = capture_channel();

    let deps = SearchDeps::new(
        empty_registry_arc(),
        Arc::new(PolicyEngine::new()),
        tracer,
        ctx,
        tool,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    handle_file_action(mk_file_action(FileActionKind::Locate, 2), ch, &deps)
        .await
        .unwrap();

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "应 1 次 locate, 实得 {calls:?}");
    assert_eq!(calls[0], "locate:/tmp/f1", "应 locate 第 2 个(f1)");
    let events = events.lock().unwrap();
    assert!(events
        .iter()
        .any(|e| e.contains("\"action_done\"") && e.contains("locate")));
    let tc = trace_calls.lock().unwrap();
    assert!(
        tc.iter().any(|c| c.starts_with("call:")),
        "应有 tool_call, 实得 {tc:?}"
    );
    assert!(
        tc.iter().any(|c| c.starts_with("result:")),
        "应有 tool_result, 实得 {tc:?}"
    );
    assert!(
        !tc.iter().any(|c| c.starts_with("error:")),
        "成功路径不应有 error, 实得 {tc:?}"
    );
}

#[tokio::test]
async fn handle_file_action_copy_routes_to_confirm() {
    use scout_search_backend::FileActionKind;
    let (tool, calls) = build_file_action_tool();
    let (tracer, trace_calls) = build_tracer_with_mock();
    let ctx = context_with_results(2);
    let pending = empty_pending();
    let (ch, events) = capture_channel();

    let deps = SearchDeps::new(
        empty_registry_arc(),
        Arc::new(PolicyEngine::new()),
        tracer,
        ctx,
        tool,
        Arc::clone(&pending),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    handle_file_action(
        mk_confirmable(FileActionKind::Copy, 1, Some("~/Desktop"), None),
        ch,
        &deps,
    )
    .await
    .unwrap();

    assert!(calls.lock().unwrap().is_empty(), "copy 首次下发不应执行");
    assert!(trace_calls.lock().unwrap().is_empty(), "首次下发不进 trace");
    assert!(pending.lock().unwrap().is_some(), "应存 pending");
    let events = events.lock().unwrap();
    assert!(
        events.iter().any(|e| e.contains("\"confirm_action\"")),
        "实得 {events:?}"
    );
}

#[tokio::test]
async fn handle_file_action_index_out_of_range_errors() {
    let (tool, _calls) = build_file_action_tool();
    let (tracer, trace_calls) = build_tracer_with_mock();
    let ctx = context_with_results(2);
    let (ch, events) = capture_channel();

    let deps = SearchDeps::new(
        empty_registry_arc(),
        Arc::new(PolicyEngine::new()),
        tracer,
        ctx,
        tool,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    handle_file_action(mk_file_action(FileActionKind::Open, 9), ch, &deps)
        .await
        .unwrap();

    let events = events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.contains("\"error\"") && e.contains('9') && e.contains('2')),
        "应越界友好错误, 实得 {events:?}"
    );
    let tc = trace_calls.lock().unwrap();
    assert!(tc.iter().any(|c| c.starts_with("call:")), "实得 {tc:?}");
    assert!(tc.iter().any(|c| c.starts_with("error:")), "实得 {tc:?}");
}

#[tokio::test]
async fn handle_file_action_no_context_errors() {
    let (tool, _calls) = build_file_action_tool();
    let (tracer, _t) = build_tracer_with_mock();
    let ctx = empty_context();
    let (ch, events) = capture_channel();

    let deps = SearchDeps::new(
        empty_registry_arc(),
        Arc::new(PolicyEngine::new()),
        tracer,
        ctx,
        tool,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    handle_file_action(mk_file_action(FileActionKind::Open, 1), ch, &deps)
        .await
        .unwrap();

    let events = events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.contains("\"error\"") && e.contains("请先发起一次搜索")),
        "实得 {events:?}"
    );
}

// ---- Task 3: search_impl FileAction 集成测试 ----

/// "打开第1个" 稳定解析为 FileAction(Open, LastResults{Index:1})。
const QUERY_OPEN_FIRST: &str = "打开第1个";
/// "打开第2个" → Index:2。
const QUERY_OPEN_SECOND: &str = "打开第2个";

#[tokio::test]
async fn search_then_open_first_executes_on_last_results() {
    let registry = build_test_registry(
        FakeOkBackend(2),
        vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
    );
    let policy = Arc::new(PolicyEngine::new());
    let (tracer, _t) = build_tracer_with_mock();
    let ctx = empty_context();
    let (tool, calls) = build_file_action_tool();

    let (ch1, _c1) = capture_channel();
    let deps1 = SearchDeps::new(
        Arc::clone(&registry),
        Arc::clone(&policy),
        Arc::clone(&tracer),
        Arc::clone(&ctx),
        Arc::clone(&tool),
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    search_impl(QUERY_FOR_FILE_SEARCH.into(), None, ch1, &deps1)
        .await
        .unwrap();

    let (ch2, _c2) = capture_channel();
    let deps2 = SearchDeps::new(
        Arc::clone(&registry),
        Arc::clone(&policy),
        Arc::clone(&tracer),
        Arc::clone(&ctx),
        Arc::clone(&tool),
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    search_impl(QUERY_OPEN_FIRST.into(), None, ch2, &deps2)
        .await
        .unwrap();

    let calls_snapshot = calls.lock().unwrap().clone();
    assert_eq!(
        calls_snapshot,
        vec!["open:/tmp/f0.pdf".to_owned()],
        "应打开上一轮第 1 个"
    );
}

#[test]
fn resolve_destination_dir_expands_tilde() {
    let home = home_dir().unwrap();
    assert_eq!(
        resolve_destination_dir("~/Desktop").unwrap(),
        home.join("Desktop")
    );
}

#[test]
fn expand_tilde_bare_returns_home() {
    assert_eq!(expand_tilde("~").unwrap(), home_dir().unwrap());
}

#[test]
fn resolve_destination_dir_absolute_passthrough() {
    assert_eq!(
        resolve_destination_dir("/Users/x/Downloads").unwrap(),
        std::path::PathBuf::from("/Users/x/Downloads")
    );
}

#[test]
fn friendly_message_path_conflict() {
    let err = FileActionError::PathConflict {
        dest: PathBuf::from("/Users/x/Desktop/a.pdf"),
    };
    let msg = friendly_file_action_message(&err);
    assert!(
        msg.contains("已存在") && msg.contains("a.pdf"),
        "实得: {msg}"
    );
}

#[tokio::test]
async fn action_does_not_clobber_context() {
    let registry = build_test_registry(
        FakeOkBackend(2),
        vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
    );
    let policy = Arc::new(PolicyEngine::new());
    let (tracer, _t) = build_tracer_with_mock();
    let ctx = empty_context();
    let (tool, calls) = build_file_action_tool();

    for q in [QUERY_FOR_FILE_SEARCH, QUERY_OPEN_FIRST, QUERY_OPEN_SECOND] {
        let (ch, _c) = capture_channel();
        let deps = SearchDeps::new(
            Arc::clone(&registry),
            Arc::clone(&policy),
            Arc::clone(&tracer),
            Arc::clone(&ctx),
            Arc::clone(&tool),
            empty_pending(),
            Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
        );
        search_impl(q.into(), None, ch, &deps).await.unwrap();
    }

    let calls_snapshot = calls.lock().unwrap().clone();
    assert_eq!(
        calls_snapshot,
        vec!["open:/tmp/f0.pdf".to_owned(), "open:/tmp/f1.pdf".to_owned()],
        "第二次 action 仍应命中同一搜索(context 未被 record/clear)"
    );
}

// ---- Task 2: handle_confirmable_action 单元测试 ----

fn empty_pending() -> Arc<Mutex<Option<scout_search_backend::FileAction>>> {
    Arc::new(Mutex::new(None))
}

/// 构造一个指定 destination/new_name 的 FileAction(target=Index)。
fn mk_confirmable(
    kind: scout_search_backend::FileActionKind,
    idx: u32,
    destination: Option<&str>,
    new_name: Option<&str>,
) -> scout_search_backend::FileAction {
    use scout_search_backend::{FileAction, Language, SchemaVersion, TargetRef, TargetSelector};
    FileAction {
        schema_version: SchemaVersion::V1,
        language: Some(Language::Zh),
        action: kind,
        target_ref: TargetRef::LastResults {
            selector: TargetSelector::Index { value: idx },
        },
        destination: destination.map(str::to_owned),
        new_name: new_name.map(str::to_owned),
        requires_confirmation: true,
    }
}

#[tokio::test]
async fn confirmable_copy_stores_pending_and_emits_confirm() {
    use scout_search_backend::{FileActionKind, TargetRef};
    let ctx = context_with_results(2);
    let pending = empty_pending();
    let (ch, events) = capture_channel();

    handle_confirmable_action(
        mk_confirmable(FileActionKind::Copy, 1, Some("~/Desktop"), None),
        ch,
        &pending,
        &ctx,
    )
    .await
    .unwrap();

    let home = home_dir().unwrap();
    let dir_str = home.join("Desktop").to_string_lossy().into_owned();
    // 事件是 JSON 序列化的：Windows 路径里的反斜杠会被转义成 `\\`，故按 JSON 转义形态
    // 比对（Unix 路径无反斜杠，replace 为空操作，保持跨平台）。
    let dir_json = dir_str.replace('\\', "\\\\");
    let events = events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.contains("\"confirm_action\"") && e.contains(&dir_json)),
        "实得 {events:?}"
    );
    drop(events);
    let p = pending.lock().unwrap();
    let pa = p.as_ref().unwrap();
    assert_eq!(pa.destination.as_deref(), Some(dir_str.as_str()));
    assert!(
        matches!(&pa.target_ref, TargetRef::Paths { values } if values == &vec!["/tmp/f0".to_owned()])
    );
    assert!(pa.requires_confirmation);
}

#[tokio::test]
async fn confirmable_rename_stores_pending() {
    use scout_search_backend::FileActionKind;
    let ctx = context_with_results(2);
    let pending = empty_pending();
    let (ch, events) = capture_channel();

    handle_confirmable_action(
        mk_confirmable(FileActionKind::Rename, 1, None, Some("final")),
        ch,
        &pending,
        &ctx,
    )
    .await
    .unwrap();

    let events = events.lock().unwrap();
    assert!(events
        .iter()
        .any(|e| e.contains("\"confirm_action\"") && e.contains("rename") && e.contains("final")));
    let p = pending.lock().unwrap();
    assert_eq!(p.as_ref().unwrap().new_name.as_deref(), Some("final"));
}

#[tokio::test]
async fn confirmable_copy_multi_target_stores_paths_pending() {
    use scout_search_backend::{
        FileAction, FileActionKind, Language, SchemaVersion, TargetRef, TargetSelector,
    };
    let ctx = context_with_results(3);
    let pending = empty_pending();
    let (ch, events) = capture_channel();

    let action = FileAction {
        schema_version: SchemaVersion::V1,
        language: Some(Language::Zh),
        action: FileActionKind::Copy,
        target_ref: TargetRef::LastResults {
            selector: TargetSelector::Indices { values: vec![1, 2] },
        },
        destination: Some("~/Desktop".to_owned()),
        new_name: None,
        requires_confirmation: true,
    };
    handle_confirmable_action(action, ch, &pending, &ctx)
        .await
        .unwrap();

    let evs = events.lock().unwrap();
    assert!(
        evs.iter()
            .any(|e| e.contains("\"confirm_action\"") && e.contains("copy")),
        "实得 {evs:?}"
    );
    drop(evs);

    let home = home_dir().unwrap();
    let dir_str = home.join("Desktop").to_string_lossy().into_owned();
    let p = pending.lock().unwrap();
    let pa = p.as_ref().unwrap();
    assert_eq!(pa.action, FileActionKind::Copy);
    assert_eq!(pa.destination.as_deref(), Some(dir_str.as_str()));
    match &pa.target_ref {
        TargetRef::Paths { values } => {
            assert_eq!(values, &vec!["/tmp/f0".to_owned(), "/tmp/f1".to_owned()]);
        }
        other => panic!("expected Paths, got {other:?}"),
    }
}

#[tokio::test]
async fn confirmable_rename_multi_target_errors_no_pending() {
    use scout_search_backend::{
        FileAction, FileActionKind, Language, SchemaVersion, TargetRef, TargetSelector,
    };
    let ctx = context_with_results(3);
    let pending = empty_pending();
    let (ch, events) = capture_channel();

    let action = FileAction {
        schema_version: SchemaVersion::V1,
        language: Some(Language::Zh),
        action: FileActionKind::Rename,
        target_ref: TargetRef::LastResults {
            selector: TargetSelector::Indices { values: vec![1, 2] },
        },
        destination: None,
        new_name: Some("final".to_owned()),
        requires_confirmation: true,
    };
    handle_confirmable_action(action, ch, &pending, &ctx)
        .await
        .unwrap();

    let evs = events.lock().unwrap();
    assert!(
        evs.iter()
            .any(|e| e.contains("\"error\"") && e.contains("一次只能重命名单个文件")),
        "实得 {evs:?}"
    );
    assert!(
        pending.lock().unwrap().is_none(),
        "rename 多目标不应存 pending"
    );
}

#[tokio::test]
async fn confirmable_out_of_range_errors() {
    use scout_search_backend::FileActionKind;
    let ctx = context_with_results(2);
    let pending = empty_pending();
    let (ch, events) = capture_channel();

    handle_confirmable_action(
        mk_confirmable(FileActionKind::Copy, 9, Some("~/Desktop"), None),
        ch,
        &pending,
        &ctx,
    )
    .await
    .unwrap();

    let events = events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.contains("\"error\"") && e.contains('9')),
        "越界友好错误, 实得 {events:?}"
    );
    assert!(pending.lock().unwrap().is_none());
}

#[tokio::test]
async fn confirmable_move_stores_pending() {
    use scout_search_backend::{FileActionKind, TargetRef};
    let ctx = context_with_results(2);
    let pending = empty_pending();
    let (ch, events) = capture_channel();

    handle_confirmable_action(
        mk_confirmable(FileActionKind::Move, 1, Some("~/Downloads"), None),
        ch,
        &pending,
        &ctx,
    )
    .await
    .unwrap();

    let events = events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.contains("\"confirm_action\"") && e.contains("move")),
        "实得 {events:?}"
    );
    let p = pending.lock().unwrap();
    let pa = p.as_ref().unwrap();
    assert_eq!(pa.action, FileActionKind::Move);
    assert!(
        matches!(&pa.target_ref, TargetRef::Paths { values } if values == &vec!["/tmp/f0".to_owned()])
    );
}

#[tokio::test]
async fn confirmable_copy_no_destination_errors() {
    use scout_search_backend::FileActionKind;
    let ctx = context_with_results(2);
    let pending = empty_pending();
    let (ch, events) = capture_channel();

    handle_confirmable_action(
        mk_confirmable(FileActionKind::Copy, 1, None, None),
        ch,
        &pending,
        &ctx,
    )
    .await
    .unwrap();

    let events = events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.contains("\"error\"") && e.contains("无法确定目标位置")),
        "实得 {events:?}"
    );
    assert!(
        pending.lock().unwrap().is_none(),
        "无 destination 不应存 pending"
    );
}

// ---- Task 3: confirm_action_impl / cancel_action 单元测试 ----

/// 预置一个 pending copy action(target=Path 完整源,destination 为目录)。
/// 方案 A：harness 在 dir.join(basename) 上做预检和写盘；仅配 MockExecutor 使用。
fn pending_with_copy(
    src: &str,
    dest: &str,
) -> Arc<Mutex<Option<scout_search_backend::FileAction>>> {
    use scout_search_backend::{FileAction, FileActionKind, Language, SchemaVersion, TargetRef};
    let a = FileAction {
        schema_version: SchemaVersion::V1,
        language: Some(Language::Zh),
        action: FileActionKind::Copy,
        target_ref: TargetRef::Path {
            value: src.to_owned(),
        },
        destination: Some(dest.to_owned()),
        new_name: None,
        requires_confirmation: true,
    };
    Arc::new(Mutex::new(Some(a)))
}

#[test]
fn confirm_action_no_pending_errs() {
    let pending = empty_pending();
    let (tool, _calls) = build_file_action_tool();
    let (tracer, _t) = build_tracer_with_mock();
    let ctx = empty_context();
    let deps = SearchDeps::new(
        empty_registry_arc(),
        Arc::new(PolicyEngine::new()),
        Arc::clone(&tracer),
        Arc::clone(&ctx),
        Arc::clone(&tool),
        Arc::clone(&pending),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    let err = confirm_action_impl(&deps).unwrap_err();
    assert!(err.contains("没有待确认的操作"), "实得 {err}");
}

#[test]
fn confirm_action_executes_and_clears_pending() {
    let pending = pending_with_copy("/tmp/f0", "/tmp/scout-confirm-test-dest-f0");
    let (tool, calls) = build_file_action_tool();
    let (tracer, trace_calls) = build_tracer_with_mock();
    let ctx = empty_context();
    let deps = SearchDeps::new(
        empty_registry_arc(),
        Arc::new(PolicyEngine::new()),
        Arc::clone(&tracer),
        Arc::clone(&ctx),
        Arc::clone(&tool),
        Arc::clone(&pending),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );

    let res = confirm_action_impl(&deps).unwrap();
    assert_eq!(res.action_kind, "copy");
    assert_eq!(res.paths, vec!["/tmp/f0".to_owned()]);

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0], "copy:/tmp/f0");

    assert!(
        pending.lock().unwrap().is_none(),
        "confirm 后 pending 应清空"
    );

    let tc = trace_calls.lock().unwrap();
    assert!(tc.iter().any(|c| c.starts_with("call:")));
    assert!(tc.iter().any(|c| c.starts_with("result:")));
}

#[test]
fn cancel_action_clears_pending() {
    let pending = pending_with_copy("/tmp/f0", "/tmp/whatever");
    assert!(pending.lock().unwrap().is_some());
    cancel_action_impl(&pending);
    assert!(
        pending.lock().unwrap().is_none(),
        "cancel 后 pending 应清空"
    );
}

#[test]
fn confirm_action_invoke_error_maps_to_err_and_traces() {
    // 方案 A：destination 是目录，harness 预检 dir.join(basename)。
    // 建临时目录 + 放与源 basename("f0")同名的文件 → 预检 PathConflict。
    let dir = std::env::temp_dir().join(format!("scout-confirm-err-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("f0"), b"x").unwrap();
    let pending = pending_with_copy("/tmp/f0", dir.to_str().unwrap());
    let (tool, calls) = build_file_action_tool();
    let (tracer, trace_calls) = build_tracer_with_mock();
    let ctx = empty_context();
    let deps = SearchDeps::new(
        empty_registry_arc(),
        Arc::new(PolicyEngine::new()),
        Arc::clone(&tracer),
        Arc::clone(&ctx),
        Arc::clone(&tool),
        Arc::clone(&pending),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );

    let err = confirm_action_impl(&deps).unwrap_err();
    assert!(
        err.contains("已存在"),
        "应返回 PathConflict 友好文案, 实得 {err}"
    );
    assert!(
        calls.lock().unwrap().is_empty(),
        "PathConflict 不应到达 executor"
    );
    let tc = trace_calls.lock().unwrap();
    assert!(tc.iter().any(|c| c.starts_with("call:")));
    assert!(tc.iter().any(|c| c.starts_with("error:")));
    assert!(
        !tc.iter().any(|c| c.starts_with("result:")),
        "失败不应有 result"
    );
    drop(tc);
    assert!(pending.lock().unwrap().is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- Task 5: move/rename/cancel/delete 集成测试 ----

/// "把第1个移动到下载" → FileAction(Move, Index:1, dest=~/Downloads)。
const QUERY_MOVE_FIRST_TO_DOWNLOADS: &str = "把第1个移动到下载";
/// "把第1个重命名为 final" → FileAction(Rename, Index:1, new_name=final)。
const QUERY_RENAME_FIRST: &str = "把第1个重命名为 final";

async fn run_search(
    query: &str,
    registry: &Arc<ToolRegistry>,
    policy: &Arc<PolicyEngine>,
    tracer: &Arc<scout_harness::Tracer>,
    ctx: &Arc<Mutex<ContextMemory>>,
    tool: &Arc<scout_harness::file_action_tool::FileActionTool>,
    pending: &Arc<Mutex<Option<scout_search_backend::FileAction>>>,
) -> Arc<Mutex<Vec<String>>> {
    let (ch, events) = capture_channel();
    let deps = SearchDeps::new(
        Arc::clone(registry),
        Arc::clone(policy),
        Arc::clone(tracer),
        Arc::clone(ctx),
        Arc::clone(tool),
        Arc::clone(pending),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    search_impl(query.into(), None, ch, &deps).await.unwrap();
    events
}

#[tokio::test]
async fn search_move_then_confirm_executes() {
    let registry = build_test_registry(
        FakeOkBackend(2),
        vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
    );
    let policy = Arc::new(PolicyEngine::new());
    let (tracer, _t) = build_tracer_with_mock();
    let ctx = empty_context();
    let (tool, calls) = build_file_action_tool();
    let pending = empty_pending();

    run_search(
        QUERY_FOR_FILE_SEARCH,
        &registry,
        &policy,
        &tracer,
        &ctx,
        &tool,
        &pending,
    )
    .await;
    let events = run_search(
        QUERY_MOVE_FIRST_TO_DOWNLOADS,
        &registry,
        &policy,
        &tracer,
        &ctx,
        &tool,
        &pending,
    )
    .await;
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.contains("\"confirm_action\"") && e.contains("move")),
        "应发 move confirm_action"
    );
    assert!(pending.lock().unwrap().is_some());

    let deps = SearchDeps::new(
        empty_registry_arc(),
        Arc::new(PolicyEngine::new()),
        Arc::clone(&tracer),
        Arc::clone(&ctx),
        Arc::clone(&tool),
        Arc::clone(&pending),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    let res = confirm_action_impl(&deps).unwrap();
    assert_eq!(res.action_kind, "move");
    assert_eq!(calls.lock().unwrap()[0], "move:/tmp/f0.pdf");
}

#[tokio::test]
async fn search_rename_then_confirm_executes() {
    let registry = build_test_registry(
        FakeOkBackend(2),
        vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
    );
    let policy = Arc::new(PolicyEngine::new());
    let (tracer, _t) = build_tracer_with_mock();
    let ctx = empty_context();
    let (tool, calls) = build_file_action_tool();
    let pending = empty_pending();

    run_search(
        QUERY_FOR_FILE_SEARCH,
        &registry,
        &policy,
        &tracer,
        &ctx,
        &tool,
        &pending,
    )
    .await;
    let events = run_search(
        QUERY_RENAME_FIRST,
        &registry,
        &policy,
        &tracer,
        &ctx,
        &tool,
        &pending,
    )
    .await;
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.contains("\"confirm_action\"") && e.contains("rename")),
        "应发 rename confirm_action"
    );

    let deps = SearchDeps::new(
        empty_registry_arc(),
        Arc::new(PolicyEngine::new()),
        Arc::clone(&tracer),
        Arc::clone(&ctx),
        Arc::clone(&tool),
        Arc::clone(&pending),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    let res = confirm_action_impl(&deps).unwrap();
    assert_eq!(res.action_kind, "rename");
    assert_eq!(calls.lock().unwrap()[0], "rename:/tmp/f0.pdf");
}

#[tokio::test]
async fn search_copy_then_cancel_clears_pending() {
    let registry = build_test_registry(
        FakeOkBackend(2),
        vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
    );
    let policy = Arc::new(PolicyEngine::new());
    let (tracer, _t) = build_tracer_with_mock();
    let ctx = empty_context();
    let (tool, calls) = build_file_action_tool();
    let pending = empty_pending();

    run_search(
        QUERY_FOR_FILE_SEARCH,
        &registry,
        &policy,
        &tracer,
        &ctx,
        &tool,
        &pending,
    )
    .await;
    run_search(
        "把第1个复制到桌面",
        &registry,
        &policy,
        &tracer,
        &ctx,
        &tool,
        &pending,
    )
    .await;
    assert!(pending.lock().unwrap().is_some(), "copy 后应有 pending");

    cancel_action_impl(&pending);
    assert!(
        pending.lock().unwrap().is_none(),
        "cancel 后 pending 应清空"
    );
    assert!(calls.lock().unwrap().is_empty(), "取消不应执行任何操作");
}

#[tokio::test]
async fn handle_file_action_delete_rejected() {
    use scout_search_backend::FileActionKind;
    let (tool, calls) = build_file_action_tool();
    let (tracer, trace_calls) = build_tracer_with_mock();
    let ctx = context_with_results(2);
    let (ch, events) = capture_channel();

    let deps = SearchDeps::new(
        empty_registry_arc(),
        Arc::new(PolicyEngine::new()),
        tracer,
        ctx,
        tool,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    handle_file_action(mk_file_action(FileActionKind::Delete, 1), ch, &deps)
        .await
        .unwrap();

    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.contains("\"error\"") && e.contains("删除操作不支持")),
        "实得 {:?}",
        events.lock().unwrap()
    );
    assert!(calls.lock().unwrap().is_empty(), "delete 不应执行");
    assert!(
        trace_calls.lock().unwrap().is_empty(),
        "delete 是 pre-tool, 不进 trace"
    );
}

// ---- Task 4: search_impl 复制确认集成测试 ----

/// "把第1个复制到桌面" 稳定解析为 FileAction(Copy, Index:1, dest=~/Desktop)。
const QUERY_COPY_FIRST_TO_DESKTOP: &str = "把第1个复制到桌面";

#[tokio::test]
async fn search_copy_then_confirm_executes() {
    let registry = build_test_registry(
        FakeOkBackend(2),
        vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
    );
    let policy = Arc::new(PolicyEngine::new());
    let (tracer, _t) = build_tracer_with_mock();
    let ctx = empty_context();
    let (tool, calls) = build_file_action_tool();
    let pending = empty_pending();

    let (ch1, _c1) = capture_channel();
    let deps1 = SearchDeps::new(
        Arc::clone(&registry),
        Arc::clone(&policy),
        Arc::clone(&tracer),
        Arc::clone(&ctx),
        Arc::clone(&tool),
        Arc::clone(&pending),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    search_impl(QUERY_FOR_FILE_SEARCH.into(), None, ch1, &deps1)
        .await
        .unwrap();

    let (ch2, events2) = capture_channel();
    let deps2 = SearchDeps::new(
        Arc::clone(&registry),
        Arc::clone(&policy),
        Arc::clone(&tracer),
        Arc::clone(&ctx),
        Arc::clone(&tool),
        Arc::clone(&pending),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    search_impl(QUERY_COPY_FIRST_TO_DESKTOP.into(), None, ch2, &deps2)
        .await
        .unwrap();

    assert!(
        events2
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.contains("\"confirm_action\"")),
        "应发 confirm_action"
    );
    assert!(calls.lock().unwrap().is_empty(), "确认前不应执行");
    assert!(pending.lock().unwrap().is_some(), "应存 pending");

    let deps = SearchDeps::new(
        empty_registry_arc(),
        Arc::new(PolicyEngine::new()),
        Arc::clone(&tracer),
        Arc::clone(&ctx),
        Arc::clone(&tool),
        Arc::clone(&pending),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    let res = confirm_action_impl(&deps).unwrap();
    assert_eq!(res.action_kind, "copy");
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0], "copy:/tmp/f0.pdf", "应复制上一轮第 1 个");
}

// ---- 第 34 阶段: confirm 多目标端到端集成测试 ----

/// 预置一个多目标 pending copy（target_ref=Paths，destination 为目录）。
fn pending_with_copy_paths(
    srcs: &[&str],
    dest_dir: &str,
) -> Arc<Mutex<Option<scout_search_backend::FileAction>>> {
    use scout_search_backend::{FileAction, FileActionKind, Language, SchemaVersion, TargetRef};
    let a = FileAction {
        schema_version: SchemaVersion::V1,
        language: Some(Language::Zh),
        action: FileActionKind::Copy,
        target_ref: TargetRef::Paths {
            values: srcs.iter().map(|s| (*s).to_owned()).collect(),
        },
        destination: Some(dest_dir.to_owned()),
        new_name: None,
        requires_confirmation: true,
    };
    Arc::new(Mutex::new(Some(a)))
}

#[test]
fn confirm_action_multi_target_executes_n_copies() {
    // 目录不存在 → 预检通过 → 逐个 copy
    let dir = std::env::temp_dir().join(format!("scout-multi-confirm-{}", std::process::id()));
    // 不创建 dir：MockExecutor 不写盘；预检只查 dir.join(basename).exists()=false
    let pending = pending_with_copy_paths(&["/tmp/f0", "/tmp/f1"], dir.to_str().unwrap());
    let (tool, calls) = build_file_action_tool();
    let (tracer, _t) = build_tracer_with_mock();
    let ctx = empty_context();
    let deps = SearchDeps::new(
        empty_registry_arc(),
        Arc::new(PolicyEngine::new()),
        Arc::clone(&tracer),
        Arc::clone(&ctx),
        Arc::clone(&tool),
        Arc::clone(&pending),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );

    let res = confirm_action_impl(&deps).unwrap();
    assert_eq!(res.action_kind, "copy");
    assert_eq!(res.paths, vec!["/tmp/f0".to_owned(), "/tmp/f1".to_owned()]);

    let calls = calls.lock().unwrap();
    assert_eq!(
        *calls,
        vec!["copy:/tmp/f0".to_owned(), "copy:/tmp/f1".to_owned()]
    );

    assert!(
        pending.lock().unwrap().is_none(),
        "confirm 后 pending 应清空"
    );
}

// ── Task 12: search_impl 接入 expander + SynonymExpandEvent 测试 ──

/// 捕获 SynonymExpandEvent 的 mock hook。
#[derive(Default)]
struct MockSynonymHook {
    events: Arc<Mutex<Vec<scout_harness::tracing::SynonymExpandEvent>>>,
}
impl TracingHook for MockSynonymHook {
    fn on_tool_call(&self, _: &ToolCallEvent) {}
    fn on_tool_result(&self, _: &scout_harness::ToolResultEvent) {}
    fn on_error(&self, _: &ToolErrorEvent) {}
    fn on_synonym_expand(&self, event: &scout_harness::tracing::SynonymExpandEvent) {
        self.events.lock().unwrap().push(event.clone());
    }
}

/// 构造含 EN YAML 词典的扩展器（"slides" -> slideshow/pptx）。
/// "find slides" 被 parser 解析为 FileSearch{keywords:["slides"]}（"slides" 不在 lexicon）。
/// 确保 expand 路径收到非 singleton 组以触发 SynonymExpandEvent。
fn build_en_expander() -> Arc<dyn SynonymExpander> {
    use scout_harness::YamlSynonymExpander;
    use std::path::PathBuf;
    let zh = r"
version: 1
language: zh
groups: []
";
    let en = r"
version: 1
language: en
groups:
  - head: cover letter
    aliases: [application]
";
    Arc::new(
        YamlSynonymExpander::from_str(zh, &PathBuf::from("zh.yaml"), en, &PathBuf::from("en.yaml"))
            .expect("测试 yaml 词典解析失败"),
    )
}

/// NoopExpander 下 search_expanded 路径与旧 search 路径行为一致：backend 被调用。
#[tokio::test]
async fn search_impl_expands_intent_before_backend_call() {
    // 使用 FakeCapturingBackend 捕获传入 backend 的 intent。
    // NoopExpander → singleton groups → search_expanded 走 default fallback → search(&base)
    // 验证 backend 确实被调用，行为与未引入 expand 路径前等价。
    let (registry, seen) = build_capturing_registry(1);
    let policy = Arc::new(PolicyEngine::new());
    let (tracer, _calls) = build_tracer_with_mock();
    let (fat, _) = build_file_action_tool();
    let pending = Arc::new(Mutex::new(None));

    let deps = SearchDeps::new(
        registry,
        policy,
        tracer,
        empty_context(),
        fat,
        pending,
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );

    let (ch, _events) = capture_channel();
    search_impl(QUERY_FOR_FILE_SEARCH.into(), None, ch, &deps)
        .await
        .unwrap();

    // FakeCapturingBackend::search() 被调用（search_expanded default fallback）
    let seen = seen.lock().unwrap();
    assert!(
        !seen.is_empty(),
        "NoopExpander 下 backend.search 仍应被调用"
    );
}

/// expander 命中词典的关键词时，search_impl 发出对应 SynonymExpandEvent。
#[tokio::test]
async fn search_impl_emits_synonym_expand_trace_event() {
    // "find my cover letter" 被 parser 解析为 FileSearch{keywords:["cover letter"]}。
    // 注：BETA-13-G3 后 "slides" 已归为 Presentation 类型词、不再产 keyword，故改用内容短语
    // "cover letter"。build_en_expander 在 en.yaml 中注册 cover letter -> [application]。
    let (registry, _) = build_capturing_registry(0);
    let policy = Arc::new(PolicyEngine::new());

    // 构造可捕获 synonym_expand 事件的 hook + tracer
    let mock_hook = MockSynonymHook::default();
    let events_ref = Arc::clone(&mock_hook.events);
    let tracer = Arc::new(Tracer::with_hooks(vec![Box::new(mock_hook)]));

    let (fat, _) = build_file_action_tool();
    let pending = Arc::new(Mutex::new(None));

    let deps = SearchDeps::new(
        registry,
        policy,
        tracer,
        empty_context(),
        fat,
        pending,
        // en expander: "cover letter" → [application]，必触发非 singleton 组
        build_en_expander(),
    );

    let (ch, _events) = capture_channel();
    search_impl("find my cover letter".into(), None, ch, &deps)
        .await
        .unwrap();

    let captured = events_ref.lock().unwrap();
    assert!(
        !captured.is_empty(),
        "应至少发出一条 SynonymExpandEvent，实际为空"
    );
    // 验证 head + group + source 字段正确
    let ev = &captured[0];
    assert_eq!(ev.head, "cover letter");
    assert!(
        ev.group.contains(&"application".to_string()),
        "group 应含同义词 'application'"
    );
    assert_eq!(ev.source, "en.yaml", "source 应识别为 en.yaml");
    assert!(!ev.truncated);
}

/// NoopExpander 不扩词，search_impl 不发 SynonymExpandEvent。
#[tokio::test]
async fn search_impl_noop_expander_emits_no_synonym_event() {
    let (registry, _) = build_capturing_registry(0);
    let policy = Arc::new(PolicyEngine::new());

    let mock_hook = MockSynonymHook::default();
    let events_ref = Arc::clone(&mock_hook.events);
    let tracer = Arc::new(Tracer::with_hooks(vec![Box::new(mock_hook)]));

    let (fat, _) = build_file_action_tool();
    let pending = Arc::new(Mutex::new(None));

    let deps = SearchDeps::new(
        registry,
        policy,
        tracer,
        empty_context(),
        fat,
        pending,
        // NoopExpander → 全 singleton 组，不发事件
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );

    let (ch, _events) = capture_channel();
    search_impl(QUERY_FOR_FILE_SEARCH.into(), None, ch, &deps)
        .await
        .unwrap();

    let captured = events_ref.lock().unwrap();
    assert!(
        captured.is_empty(),
        "NoopExpander 下不应发出 SynonymExpandEvent，实际有 {} 条",
        captured.len()
    );
}

// ---- run_path_action（UI 双击打开 / 右键定位）----

/// 用 MockFileActionExecutor 建一套只关心 file-action 的 SearchDeps。
fn deps_with_file_action() -> (SearchDeps, Arc<Mutex<Vec<String>>>) {
    let (fat, calls) = build_file_action_tool();
    let (tracer, _t) = build_tracer_with_mock();
    let deps = SearchDeps::new(
        empty_registry_arc(),
        Arc::new(PolicyEngine::new()),
        tracer,
        empty_context(),
        fat,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    (deps, calls)
}

#[test]
fn run_path_action_open_invokes_open() {
    let (deps, calls) = deps_with_file_action();
    let res = run_path_action(FileActionKind::Open, "/tmp/foo.pdf".into(), &deps);
    let res = res.expect("open 应成功");
    assert_eq!(res.action_kind, "open");
    assert_eq!(res.paths, vec!["/tmp/foo.pdf".to_owned()]);
    assert_eq!(*calls.lock().unwrap(), vec!["open:/tmp/foo.pdf".to_owned()]);
}

#[test]
fn run_path_action_locate_invokes_locate() {
    let (deps, calls) = deps_with_file_action();
    let res = run_path_action(FileActionKind::Locate, "/tmp/bar.txt".into(), &deps);
    let res = res.expect("locate 应成功");
    assert_eq!(res.action_kind, "locate");
    assert_eq!(
        *calls.lock().unwrap(),
        vec!["locate:/tmp/bar.txt".to_owned()]
    );
}

/// 写操作(copy/move/rename/delete)绝不从 UI 旁路执行 —— 直接 Err 且不碰 executor。
#[test]
fn run_path_action_rejects_write_kinds() {
    for kind in [
        FileActionKind::Copy,
        FileActionKind::Move,
        FileActionKind::Rename,
        FileActionKind::Delete,
    ] {
        let (deps, calls) = deps_with_file_action();
        let res = run_path_action(kind, "/tmp/x".into(), &deps);
        assert!(res.is_err(), "{kind:?} 应被拒绝");
        assert!(
            calls.lock().unwrap().is_empty(),
            "{kind:?} 不应触达 executor"
        );
    }
}

// ===== BETA-06 审计日志 =====

use scout_harness::{AuditLog, AuditOperation, AuditResult, InMemoryAuditLog};

fn mk_action_path(
    kind: FileActionKind,
    target: scout_search_backend::TargetRef,
    destination: Option<&str>,
    new_name: Option<&str>,
) -> scout_search_backend::FileAction {
    use scout_search_backend::{FileAction, SchemaVersion};
    FileAction {
        schema_version: SchemaVersion::V1,
        language: None,
        action: kind,
        target_ref: target,
        destination: destination.map(str::to_owned),
        new_name: new_name.map(str::to_owned),
        requires_confirmation: true,
    }
}

#[test]
fn record_audit_executed_records_affected() {
    use scout_harness::file_action_tool::FileActionOutcome;
    use scout_search_backend::TargetRef;
    let audit = InMemoryAuditLog::default();
    let action = mk_action_path(
        FileActionKind::Open,
        TargetRef::Path {
            value: "/a.mp3".into(),
        },
        None,
        None,
    );
    let outcome = Ok(FileActionOutcome::Executed {
        affected: vec![PathBuf::from("/a.mp3")],
    });
    record_audit(&audit, &action, &outcome);
    let all = audit.read_all();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].operation, AuditOperation::Open);
    assert_eq!(all[0].result, AuditResult::Executed);
    assert_eq!(all[0].source_paths, vec!["/a.mp3".to_string()]);
    assert!(all[0].error.is_none());
}

#[test]
fn record_audit_err_records_failed_with_kind() {
    use scout_search_backend::TargetRef;
    let audit = InMemoryAuditLog::default();
    let action = mk_action_path(
        FileActionKind::Copy,
        TargetRef::Paths {
            values: vec!["/a.txt".into()],
        },
        Some("/dest"),
        None,
    );
    let outcome = Err(FileActionError::PathConflict {
        dest: PathBuf::from("/dest/a.txt"),
    });
    record_audit(&audit, &action, &outcome);
    let all = audit.read_all();
    assert_eq!(all[0].operation, AuditOperation::Copy);
    assert_eq!(all[0].result, AuditResult::Failed);
    assert_eq!(all[0].error.as_deref(), Some("PathConflict"));
    assert_eq!(all[0].source_paths, vec!["/a.txt".to_string()]);
    assert_eq!(all[0].destination.as_deref(), Some("/dest"));
}

#[test]
fn record_audit_requires_confirmation_not_recorded() {
    use scout_harness::file_action_tool::FileActionOutcome;
    use scout_search_backend::TargetRef;
    let audit = InMemoryAuditLog::default();
    let action = mk_action_path(
        FileActionKind::Move,
        TargetRef::Paths {
            values: vec!["/a".into()],
        },
        Some("/d"),
        None,
    );
    let outcome = Ok(FileActionOutcome::RequiresConfirmation {
        paths: vec![PathBuf::from("/a")],
    });
    record_audit(&audit, &action, &outcome);
    assert!(audit.read_all().is_empty(), "未执行不记");
}

#[tokio::test]
async fn open_action_records_audit_end_to_end() {
    let audit: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::default());
    let deps = SearchDeps::new(
        empty_registry_arc(),
        Arc::new(PolicyEngine::new()),
        build_tracer_with_mock().0,
        context_with_results(2),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    )
    .with_audit(Arc::clone(&audit));

    let (ch, _captured) = capture_channel();
    // selector 1-based：index 1 → 第 1 个结果（0-based f0）。
    handle_file_action(mk_file_action(FileActionKind::Open, 1), ch, &deps)
        .await
        .unwrap();

    let entries = audit.read_all();
    assert_eq!(entries.len(), 1, "open 执行后应有 1 条审计");
    assert_eq!(entries[0].operation, AuditOperation::Open);
    assert_eq!(entries[0].result, AuditResult::Executed);
}

#[test]
fn get_audit_log_impl_newest_first_and_clear() {
    let audit: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::default());
    let deps = SearchDeps::new(
        empty_registry_arc(),
        Arc::new(PolicyEngine::new()),
        build_tracer_with_mock().0,
        empty_context(),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    )
    .with_audit(Arc::clone(&audit));

    // 直接记两条（Open 先、Locate 后）。
    let a1 = mk_action_path(
        FileActionKind::Open,
        scout_search_backend::TargetRef::Path { value: "/a".into() },
        None,
        None,
    );
    let a2 = mk_action_path(
        FileActionKind::Locate,
        scout_search_backend::TargetRef::Path { value: "/b".into() },
        None,
        None,
    );
    use scout_harness::file_action_tool::FileActionOutcome;
    record_audit(
        audit.as_ref(),
        &a1,
        &Ok(FileActionOutcome::Executed {
            affected: vec![PathBuf::from("/a")],
        }),
    );
    record_audit(
        audit.as_ref(),
        &a2,
        &Ok(FileActionOutcome::Executed {
            affected: vec![PathBuf::from("/b")],
        }),
    );

    let json = get_audit_log_impl(&deps);
    assert_eq!(json.len(), 2);
    assert_eq!(json[0].operation, "locate", "newest-first");
    assert_eq!(json[1].operation, "open");

    deps.audit().clear();
    assert!(get_audit_log_impl(&deps).is_empty());
}

// ===== BETA-07 索引状态 / 调度 =====

#[test]
fn perform_reindex_skips_when_already_indexing() {
    // 并发守卫：已在索引中 → Ok(None) 跳过（不跑真 reindex）。
    let status = Arc::new(Mutex::new(IndexStatus {
        indexing: true,
        ..Default::default()
    }));
    let result = perform_reindex(&status, PathBuf::from("/no/such/db.sqlite"), None);
    assert!(
        matches!(result, Ok(None)),
        "已在索引中应跳过, 实得 {result:?}"
    );
    assert!(
        status.lock().unwrap().indexing,
        "守卫提前返回，indexing 仍 true"
    );
}

#[test]
fn apply_reindex_result_success_updates_status() {
    use scout_indexer::IndexStats;
    let status = Arc::new(Mutex::new(IndexStatus {
        indexing: true,
        ..Default::default()
    }));
    let music = IndexStats {
        scanned: 10,
        added: 5,
        updated: 2,
        ..Default::default()
    };
    let doc = IndexStats {
        scanned: 4,
        added: 3,
        ..Default::default()
    };
    let image = IndexStats {
        scanned: 6,
        added: 4,
        updated: 1,
        ..Default::default()
    };
    // 摘要用总数（totals），不是本轮 delta：传 Some((总音频, 总文档, 总图片)）。
    let out = apply_reindex_result(&status, Ok((music, doc, image)), Some((100, 20, 8)))
        .unwrap()
        .unwrap();
    assert_eq!(out.music_added, 5, "ReindexStats 仍返回本轮 delta");
    assert_eq!(out.doc_added, 3);
    assert_eq!(out.image_added, 4);
    let s = status.lock().unwrap();
    assert!(!s.indexing, "完成后 indexing=false");
    assert!(s.last_indexed.is_some());
    assert_eq!(
        s.last_summary.as_deref(),
        Some("音频 100 / 文档 20 / 图片 8"),
        "摘要应为总数而非 delta"
    );
    // cycle 9：结构化全库总数与 last_summary 数字同源同步（供概貌口径比对）。
    assert_eq!(s.db_totals, Some((100, 20, 8)));
}

#[test]
fn apply_reindex_result_falls_back_to_delta_when_no_totals() {
    use scout_indexer::IndexStats;
    let status = Arc::new(Mutex::new(IndexStatus::default()));
    let music = IndexStats {
        added: 5,
        updated: 2,
        ..Default::default()
    };
    let doc = IndexStats {
        added: 3,
        ..Default::default()
    };
    let image = IndexStats {
        added: 4,
        updated: 1,
        ..Default::default()
    };
    // totals=None（如 count 失败）→ 退回本轮 delta（added+updated）。
    apply_reindex_result(&status, Ok((music, doc, image)), None).unwrap();
    let s = status.lock().unwrap();
    assert_eq!(s.last_summary.as_deref(), Some("音频 7 / 文档 3 / 图片 5"));
    // cycle 9：totals 拿不到时 db_totals 保留旧值（此处初始 None → 仍 None，不写 delta 冒充全库数）。
    assert_eq!(s.db_totals, None);
}

#[test]
fn apply_reindex_result_error_resets_indexing() {
    let status = Arc::new(Mutex::new(IndexStatus {
        indexing: true,
        ..Default::default()
    }));
    let result = apply_reindex_result(
        &status,
        Err(scout_search_backend::SearchError::Io {
            detail: "boom".into(),
        }),
        None,
    );
    assert!(result.is_err());
    assert!(!status.lock().unwrap().indexing, "失败也清 indexing");
}

#[test]
fn index_status_snapshot_defaults() {
    let deps = SearchDeps::new(
        empty_registry_arc(),
        Arc::new(PolicyEngine::new()),
        build_tracer_with_mock().0,
        empty_context(),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    let snap = index_status_snapshot(&deps);
    assert!(!snap.indexing);
    assert!(snap.last_indexed.is_none());
}

// ---- BETA-11D: inject_adhoc_group 单元测试 ----

#[test]
fn inject_adhoc_group_overrides_same_head() {
    use scout_search_backend::{
        ExpandedSearchIntent, FileSearch, KeywordGroup, SchemaVersion, SearchIntent,
    };
    let base = SearchIntent::FileSearch(FileSearch {
        schema_version: SchemaVersion::V1,
        language: None,
        keywords: Some(vec!["友商竞争分析".into()]),
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
    });
    let expanded = ExpandedSearchIntent {
        base,
        keyword_groups: vec![KeywordGroup::singleton("友商竞争分析")],
        match_mode: scout_search_backend::MatchMode::default(),
    };
    let out =
        super::inject_adhoc_group(expanded, "友商竞争分析", vec!["AWS".into(), "Azure".into()]);
    assert_eq!(out.keyword_groups.len(), 1);
    assert_eq!(out.keyword_groups[0].head, "友商竞争分析");
    assert_eq!(out.keyword_groups[0].synonyms, vec!["AWS", "Azure"]);
}

#[test]
fn inject_adhoc_group_inserts_new_head_at_front() {
    use scout_search_backend::{
        ExpandedSearchIntent, FileSearch, KeywordGroup, SchemaVersion, SearchIntent,
    };
    let base = SearchIntent::FileSearch(FileSearch {
        schema_version: SchemaVersion::V1,
        language: None,
        keywords: Some(vec!["竞品".into()]),
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
    });
    let expanded = ExpandedSearchIntent {
        base,
        keyword_groups: vec![KeywordGroup::singleton("竞品")],
        match_mode: scout_search_backend::MatchMode::default(),
    };
    // adhoc head 不在现有 groups 中，应插到最前
    let out = super::inject_adhoc_group(expanded, "对标产品", vec!["competitor".into()]);
    assert_eq!(out.keyword_groups.len(), 2);
    assert_eq!(out.keyword_groups[0].head, "对标产品");
    assert_eq!(out.keyword_groups[0].synonyms, vec!["competitor"]);
    assert_eq!(out.keyword_groups[1].head, "竞品");
}

#[test]
fn inject_adhoc_group_deduplicates_and_filters_aliases() {
    use scout_search_backend::{
        ExpandedSearchIntent, FileSearch, KeywordGroup, SchemaVersion, SearchIntent,
    };
    let base = SearchIntent::FileSearch(FileSearch {
        schema_version: SchemaVersion::V1,
        language: None,
        keywords: Some(vec!["报告".into()]),
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
    });
    let expanded = ExpandedSearchIntent {
        base,
        keyword_groups: vec![KeywordGroup::singleton("报告")],
        match_mode: scout_search_backend::MatchMode::default(),
    };
    // 空串、等于 head、重复项均应被过滤
    let out = super::inject_adhoc_group(
        expanded,
        "报告",
        vec![
            "  ".into(),   // 纯空白 → 过滤
            "报告".into(), // 等于 head → 过滤
            "report".into(),
            "report".into(), // 重复 → 过滤
        ],
    );
    assert_eq!(out.keyword_groups.len(), 1);
    assert_eq!(out.keyword_groups[0].synonyms, vec!["report"]);
}

// ---- BETA-15B-1 终审加固：无模型端到端降级守护 ----

/// 始终 `embed()` 报错的嵌入器：模拟「registry 注册了 SemanticIndexBackend，但运行时无
/// 可用模型」——即生产里 embedding 句柄存在（is_available()=true 进 fanout），但对 query
/// 嵌入时失败的 no-model 路径。注入它能真正走到 `embed()` 的 Err 分支。
#[derive(Debug)]
struct ErrEmbedder;
impl scout_indexer::embed::TextEmbedder for ErrEmbedder {
    fn embed(&self, _text: &str) -> Result<Vec<f32>, scout_indexer::IndexError> {
        Err(scout_indexer::IndexError::Io {
            path: "<no-model>".to_owned(),
            detail: "embedding model unavailable".to_owned(),
        })
    }
    fn model_id(&self) -> &str {
        "err-embedder"
    }
}

/// BETA-15B-1 终审加固：registry **含语义臂但无可用模型** 时，FileSearch 仍经 RRF fan-out
/// 返回正确的 FTS 结果——语义臂 `embed()` 报错被优雅吞掉（记 error、不中断），FTS 臂结果存活。
///
/// 用**真实** `SemanticIndexBackend`（非重实现），注入始终报错的嵌入器，并备真实 db + 候选向量
/// 让查询走到 `embed()` 的 Err 分支（db 不存在会在 embed 前就空返回，遮蔽错误路径）。
/// 经 `search_impl` 整链（route_search_fanout → run_fanout_merge_rrf → ranker → channel）驱动，
/// 与其它 fan-out 测试同一入口；query 带关键词（"find files containing budget"）确保语义臂被
/// 纳入 fan-out（≥2 后端）。断言：search_impl 成功返回（错误不外泄给用户）、Complete 而非 Error、
/// 且 FTS 后端的结果出现在输出里（召回未因语义臂报错而丢失）。
#[tokio::test]
async fn search_impl_semantic_no_model_degrades_to_fts_results() {
    use scout_indexer::DocumentIndex;

    // 真实 db + 一条候选向量：让语义臂查询走到 embed()（否则 db 不存在会在 embed 前空返回，
    // 不触发 Err 路径）。
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("doc.txt"), "关于预算的笔记").unwrap();
    let db = dir.path().join("index.db");
    let idx = DocumentIndex::open(&db).unwrap();
    idx.index_dirs(&[dir.path().to_path_buf()]).unwrap();
    let doc = dir.path().join("doc.txt").to_string_lossy().into_owned();
    idx.upsert_vector(&doc, &[1.0, 0.0], "err-embedder", "h1")
        .unwrap();

    // registry：FTS 内容臂（FakeOkBackend, kind=Spotlight, 返回 2 条）+ 真实语义臂（无可用模型）。
    let mut r = ToolRegistry::new();
    r.register_search(SearchTool::new(
        "search.fts",
        "FTS",
        FakeOkBackend(2),
        vec![SupportedIntent::FileSearch],
        "fts content backend",
    ))
    .unwrap();
    let semantic = scout_search_backend_semantic::SemanticIndexBackend::new(
        &db,
        Some(Arc::new(ErrEmbedder) as Arc<dyn scout_indexer::embed::TextEmbedder>),
        std::sync::Arc::new(|| 0.30_f32),
    );
    r.register_search(SearchTool::new(
        "search.semantic",
        "语义召回",
        semantic,
        vec![SupportedIntent::FileSearch],
        "semantic backend (no model → embed errors)",
    ))
    .unwrap();

    let policy = Arc::new(PolicyEngine::new());
    let (tracer, _calls) = build_tracer_with_mock();
    let (ch, captured) = capture_channel();
    let deps = SearchDeps::new(
        Arc::new(r),
        policy,
        tracer,
        empty_context(),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );

    // 带关键词的 FileSearch → route_search_fanout 纳入语义臂（≥2 后端）→ 走加权 RRF 融合分支。
    search_impl(QUERY_CONTENT_KEYWORD.into(), None, ch, &deps)
        .await
        .expect("语义臂报错不应把错误外泄给用户");

    let events = captured.lock().unwrap();
    // 语义臂 embed() 报错被优雅吞掉 → 整链仍以 FTS 结果 Complete，绝不 Error。
    assert!(
        events.iter().any(|e| e.contains("\"complete\"")),
        "无模型时应以 FTS 结果 Complete, 实得: {events:?}"
    );
    assert!(
        !events.iter().any(|e| e.contains("\"error\"")),
        "语义臂报错不应触发 SearchEvent::Error（召回应降级而非失败）, 实得: {events:?}"
    );
    // FTS 后端的结果（/tmp/f0、/tmp/f1）应出现在输出里——召回未因语义臂报错而丢失。
    assert!(
        events.iter().any(|e| e.contains("/tmp/f0")),
        "FTS 臂结果应存活于输出（语义臂报错不得清空结果集）, 实得: {events:?}"
    );
}

/// BETA-33 cycle 9：**FTS 臂零结果 + 语义臂查询期报错**（路由后模型加载竞态失败）时，
/// 全链应以 Complete(0) 空态收尾——而非把语义臂的「embedding 模型不可用」冒充全链错误
/// 甚至冒充为 UI Error（其余臂已正常查完、语义能力真实状态另有 EmbedStatus 呈现；
/// 0 命中本身也不是错误，见「找文件没命中不应报错」用户反馈）。
/// 注：路由期探测（`TextEmbedder::is_ready()`=false → 语义臂整体退出 fan-out）由
/// embedding_model / semantic-index 单测覆盖；此处 ErrEmbedder 未覆写 is_ready（恒 true），
/// 专门驱动「路由进了臂、embed 才失败」的竞态残余路径。
#[tokio::test]
async fn search_impl_semantic_error_with_zero_fts_reports_empty_complete() {
    use scout_indexer::DocumentIndex;

    // 真实 db + 一条候选向量：让语义臂查询走到 embed() 的 Err 分支（同上一测）。
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("doc.txt"), "关于预算的笔记").unwrap();
    let db = dir.path().join("index.db");
    let idx = DocumentIndex::open(&db).unwrap();
    idx.index_dirs(&[dir.path().to_path_buf()]).unwrap();
    let doc = dir.path().join("doc.txt").to_string_lossy().into_owned();
    idx.upsert_vector(&doc, &[1.0, 0.0], "err-embedder", "h1")
        .unwrap();

    // registry：FTS 内容臂**零结果** + 真实语义臂（embed 必 Err）。
    let mut r = ToolRegistry::new();
    r.register_search(SearchTool::new(
        "search.fts",
        "FTS",
        FakeOkBackend(0),
        vec![SupportedIntent::FileSearch],
        "fts content backend (zero hits)",
    ))
    .unwrap();
    let semantic = scout_search_backend_semantic::SemanticIndexBackend::new(
        &db,
        Some(Arc::new(ErrEmbedder) as Arc<dyn scout_indexer::embed::TextEmbedder>),
        std::sync::Arc::new(|| 0.30_f32),
    );
    r.register_search(SearchTool::new(
        "search.semantic",
        "语义召回",
        semantic,
        vec![SupportedIntent::FileSearch],
        "semantic backend (no model → embed errors)",
    ))
    .unwrap();

    let policy = Arc::new(PolicyEngine::new());
    let (tracer, _calls) = build_tracer_with_mock();
    let (ch, captured) = capture_channel();
    let deps = SearchDeps::new(
        Arc::new(r),
        policy,
        tracer,
        empty_context(),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );

    search_impl(QUERY_CONTENT_KEYWORD.into(), None, ch, &deps)
        .await
        .expect("语义臂报错不应把错误外泄为 invoke 失败");

    let events = captured.lock().unwrap();
    // 零结果空态：应以 Complete(0) 收尾，绝不把语义臂 embed 错误冒充全链错误，
    // 也绝不把 0 命中本身报成 SearchEvent::Error。
    assert!(
        events.iter().any(|e| e.contains("\"complete\"")),
        "FTS 零结果 + 语义臂报错应以 Complete(0) 空态收尾, 实得: {events:?}"
    );
    assert!(
        !events.iter().any(|e| e.contains("\"error\"")),
        "0 命中不应发 SearchEvent::Error, 实得: {events:?}"
    );
    assert!(
        !events.iter().any(|e| e.contains("embedding")),
        "语义臂 embed 错误不得冒充全链错误信息, 实得: {events:?}"
    );
}

/// BETA-15B-5 步4：未索引路径 → explain 返回空 payload（前端无高亮，逐字节等价现状）。
#[test]
fn explain_semantic_hit_unindexed_path_is_empty() {
    let registry = build_test_registry(FakeOkBackend(0), vec![SupportedIntent::FileSearch]);
    let policy = Arc::new(PolicyEngine::new());
    let (tracer, _calls) = build_tracer_with_mock();
    let deps = SearchDeps::new(
        registry,
        policy,
        tracer,
        empty_context(),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    )
    .with_embedding(Arc::new(embedding_model::EmbeddingModelHandle::new(
        None,
        PathBuf::from("/tmp/scout-explain-test"),
    )));
    let payload = explain_semantic_hit_impl("/no/such/file.txt", "猫", &deps);
    assert!(payload.passages.is_empty());
}

// ============================================================
// BETA-29 意图草稿重跑（search_with_intent_impl）
// ============================================================

/// 便捷构造：默认 deps（fake 后端 + mock tracer + 捕获 channel）。
fn draft_test_deps(n_results: usize) -> (SearchDeps, Arc<Mutex<Vec<String>>>) {
    let registry = build_test_registry(
        FakeOkBackend(n_results),
        vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
    );
    let (tracer, calls) = build_tracer_with_mock();
    let deps = SearchDeps::new(
        registry,
        Arc::new(PolicyEngine::new()),
        tracer,
        empty_context(),
        build_file_action_tool().0,
        empty_pending(),
        Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
    );
    (deps, calls)
}

/// 合法 file_search 草稿：跳过 parser 直接执行，started 事件回带 intent_json、complete 收尾。
#[tokio::test]
async fn search_with_intent_valid_draft_executes() {
    let (deps, calls) = draft_test_deps(2);
    let (ch, captured) = capture_channel();

    let draft = serde_json::json!({
        "schema_version": "1.0",
        "intent": "file_search",
        "extensions": ["pdf"],
        "sort": "modified_desc"
    });
    search_with_intent_impl(draft, "找 pdf".into(), ch, &deps)
        .await
        .unwrap();

    let calls = calls.lock().unwrap().clone();
    assert!(
        calls.iter().any(|c| c.starts_with("call:search.fake")),
        "草稿应真正驱动后端, 实得 {calls:?}"
    );
    let events = captured.lock().unwrap();
    assert!(
        events.iter().any(|e| e.contains("\"started\"")),
        "captured: {events:?}"
    );
    // started 回带完整 intent JSON（前端草稿 UI 的数据源；round-trip 契约）。
    assert!(
        events
            .iter()
            .any(|e| e.contains("\"intent_json\"") && e.contains("\"file_search\"")),
        "started 应含 intent_json, 实得: {events:?}"
    );
    assert!(
        events.iter().any(|e| e.contains("\"complete\"")),
        "captured: {events:?}"
    );
}

/// 不合法草稿（未知字段，deny_unknown_fields 拒绝）→ Error 事件、不触碰后端。
#[tokio::test]
async fn search_with_intent_malformed_draft_errors_without_tool_call() {
    let (deps, calls) = draft_test_deps(2);
    let (ch, captured) = capture_channel();

    let draft = serde_json::json!({
        "schema_version": "1.0",
        "intent": "file_search",
        "no_such_field": true
    });
    search_with_intent_impl(draft, "q".into(), ch, &deps)
        .await
        .unwrap();

    assert!(
        calls.lock().unwrap().is_empty(),
        "不合法草稿不得触发任何 tool call"
    );
    let events = captured.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.contains("\"error\"") && e.contains("意图草稿不合法")),
        "应经 Error 事件报不合法, 实得: {events:?}"
    );
}

/// 非搜索类 intent（file_action）→ 拒绝（草稿 UI 只对搜索意图开放，防止绕确认流做文件操作）。
#[tokio::test]
async fn search_with_intent_rejects_file_action_draft() {
    let (deps, calls) = draft_test_deps(2);
    let (ch, captured) = capture_channel();

    let draft = serde_json::json!({
        "schema_version": "1.0",
        "intent": "file_action",
        "action": "open",
        "target_ref": { "source": "last_results", "selector": { "type": "index", "value": 1 } },
        "requires_confirmation": false
    });
    search_with_intent_impl(draft, "打开第一个".into(), ch, &deps)
        .await
        .unwrap();

    assert!(
        calls.lock().unwrap().is_empty(),
        "file_action 草稿不得执行任何动作"
    );
    let events = captured.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.contains("\"error\"") && e.contains("仅支持 file_search / media_search")),
        "应拒绝非搜索类草稿, 实得: {events:?}"
    );
}

/// 普通 search_impl 路径的 started 也回带 intent_json（草稿 UI 首次搜索即有数据源）。
#[tokio::test]
async fn search_impl_started_carries_intent_json() {
    let (deps, _calls) = draft_test_deps(1);
    let (ch, captured) = capture_channel();
    search_impl(QUERY_FOR_FILE_SEARCH.into(), None, ch, &deps)
        .await
        .unwrap();
    let events = captured.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.contains("\"intent_json\"") && e.contains("\"schema_version\"")),
        "started 应含完整 intent_json, 实得: {events:?}"
    );
}

/// 草稿重跑成功后 record 上下文：后续 Refine 以草稿 intent 为合并基准
/// （与普通搜索同语义，保证「修正草稿 → 再细化」链路成立）。
#[tokio::test]
async fn search_with_intent_records_context_for_refine() {
    let (deps, _calls) = draft_test_deps(2);
    let (ch, _captured) = capture_channel();

    let draft = serde_json::json!({
        "schema_version": "1.0",
        "intent": "file_search",
        "extensions": ["pdf"]
    });
    search_with_intent_impl(draft, "找 pdf".into(), ch, &deps)
        .await
        .unwrap();

    let guard = deps.context.lock().unwrap();
    let last = guard.last_turn().expect("草稿成功后应 record 上下文");
    match &last.intent {
        SearchIntent::FileSearch(fs) => {
            assert_eq!(fs.extensions.as_deref(), Some(&["pdf".to_owned()][..]));
        }
        other => panic!("record 的基准应为草稿 FileSearch, 实得 {other:?}"),
    }
}

/// BETA-29 v2：搜索前预览——只解析不执行（零 tool call）、回带 wire 格式 intent_json；
/// 空查询报错；Refine 无上一轮基准时与搜索同款文案。
#[test]
fn preview_intent_parses_without_executing() {
    let (deps, calls) = draft_test_deps(2);

    let p = preview_intent_impl("找上周改过的 pdf", &deps).unwrap();
    assert!(p.supported, "file_search 应可编辑");
    assert_eq!(p.intent_json["intent"], "file_search");
    assert_eq!(p.intent_json["schema_version"], "1.0");
    assert!(!p.intent_summary.is_empty());
    assert!(
        calls.lock().unwrap().is_empty(),
        "预览不得触发任何 tool call"
    );

    assert!(preview_intent_impl("   ", &deps).is_err(), "空查询应报错");

    let err = preview_intent_impl("只看 pdf", &deps).unwrap_err();
    assert!(
        err.contains("没有可细化的上一轮搜索"),
        "Refine 无基准应同款文案, 实得: {err}"
    );
}
