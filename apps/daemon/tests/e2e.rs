//! BETA-32 T11：scoutd 集成测试（5 条端到端用例）。
//!
//! 走真 axum server + 真 HTTP 端口 + 真 reqwest 客户端，覆盖：
//!
//! 1. `/health` 无鉴权 → 200 `{status: "ok"}`
//! 2. `/admin/reindex` 错 token → 401
//! 3. `/mcp` `tools/call list_roots` → 命中 ctx.root
//! 4. `/mcp` `tools/call search` → 命中 corpus 文件
//! 5. `/admin/reindex` 并发竞态 → 409
//!
//! **rmcp 1.8 streamable-HTTP 协议处理**：stateful_mode=true 会强制 SSE framing
//! 即使 json_response=true 也无效（见 rmcp 1.8
//! `tests/test_streamable_http_json_response.rs::json_response_ignored_in_stateful_mode`）。
//! 本文件 [`extract_sse_data`] 把 SSE event 拆开取 `data:` 行的 JSON 负载。
//!
//! **测试 5 的竞态构造**：小 corpus 下真增量 reindex 近乎瞬时完成、并发两
//! 个 reqwest 请求很难稳定撞上 InFlight 标志。改用「pre-set ctx.state +
//! 单个请求」手法（与 `reindex::tests::concurrent_reindex_second_returns_in_flight`
//! 同款），确定性触发 409。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown
)]

mod common;

use common::DaemonHandle;
use reqwest::Client;
use serde_json::{json, Value};

// ---- 1. /health 无鉴权 ----

#[tokio::test]
async fn e2e_health_no_auth_required() {
    let d = DaemonHandle::spawn_with_fixtures(&[("a.txt", "hello world")]).await;
    let resp = Client::new()
        .get(d.url("/health"))
        .send()
        .await
        .expect("/health GET 应当能成功");
    assert_eq!(resp.status(), 200, "/health 应当返 200");
    let body: Value = resp.json().await.expect("/health 响应应当是 JSON");
    assert_eq!(body["status"], "ok", "/health JSON 应当含 status=ok");
    assert!(
        body["version"].is_string(),
        "/health JSON 应当含 version 字段"
    );
}

// ---- 2. /admin/reindex 错 token → 401 ----

#[tokio::test]
async fn e2e_auth_401_wrong_token() {
    let d = DaemonHandle::spawn_with_fixtures(&[("a.txt", "x")]).await;
    let resp = Client::new()
        .post(d.url("/admin/reindex"))
        .bearer_auth("wrong-token-32-chars-minimum-length")
        .send()
        .await
        .expect("/admin/reindex POST 应当能发出");
    assert_eq!(
        resp.status(),
        401,
        "错 bearer token 应当返 401，实得 {}",
        resp.status()
    );
}

// ---- 3. /mcp tools/call list_collections ----

// BETA-36 cycle 5：helper 已按 rmcp stateless JSON framing 重写（stateless 分支
// 无需 initialize / session-id，带 `Mcp-Protocol-Version` header 的 tools/call 直接
// 得纯 JSON 响应）——原 stateful/SSE 版 #[ignore] 解除。
#[tokio::test]
async fn e2e_list_collections_after_indexing() {
    let d = DaemonHandle::spawn_with_fixtures(&[("doc.txt", "competitive analysis content")]).await;
    let client = Client::new();

    let result = mcp_call_tool(&client, &d, &d.token, "list_collections", json!({})).await;

    // tool result 在 content[0].text，是序列化后的 JSON 字符串。
    let payload_str = result["content"][0]["text"]
        .as_str()
        .expect("tools/call result.content[0].text 应当是字符串");
    let payload: Value = serde_json::from_str(payload_str).expect("tool result 应当是合法 JSON");

    // BETA-36：legacy 形态合成单 default collection。
    let cols = payload["collections"]
        .as_array()
        .expect("list_collections 应返 collections 数组");
    assert_eq!(cols.len(), 1);
    assert_eq!(cols[0]["id"], "default");
    let expected_root = d.ctx.collections["default"].meta.roots[0]
        .display()
        .to_string();
    assert_eq!(cols[0]["roots"][0], Value::String(expected_root));
    // build_test_ctx_indexed 真索引后 doc_count > 0（"doc.txt" 是 DOC_EXTS 命中项）。
    assert!(
        cols[0]["doc_count"].as_u64().unwrap_or(0) >= 1,
        "list_collections 应当返 doc_count ≥ 1（corpus 含 1 txt）实得 {}",
        cols[0]["doc_count"]
    );
}

// ---- 4. /mcp tools/call search ----

#[tokio::test]
async fn e2e_search_returns_results() {
    let d = DaemonHandle::spawn_with_fixtures(&[
        (
            "notes-2024-Q1.txt",
            "competitive analysis versus CompetitorX market positioning",
        ),
        ("readme.md", "unrelated stuff about installation"),
    ])
    .await;
    let client = Client::new();

    // 用 body 内出现的词 "competitive" 走 parser → FileSearch → DocumentQuery →
    // documents_fts MATCH（trigram tokenizer）命中 notes-2024-Q1.txt。
    // 注：trigram FTS 索引 title/author/body 三列；txt extract 只填 body、不填
    // title，因此查询词必须在 body 内容里，纯文件名词（如 "notes"）匹不上。
    let result = mcp_call_tool(
        &client,
        &d,
        &d.token,
        "search",
        json!({"query": "competitive"}),
    )
    .await;
    let payload_str = result["content"][0]["text"]
        .as_str()
        .expect("search result.content[0].text 应当是字符串");
    let payload: Value = serde_json::from_str(payload_str).expect("search result 应当是合法 JSON");

    let results = payload["results"]
        .as_array()
        .expect("search 应当返 results 数组");
    assert!(
        !results.is_empty(),
        "search 'competitive' 应当命中 corpus（notes-2024-Q1.txt body 内含），\
         实得空数组：{payload}"
    );

    // 命中文件 path 应当在 root 子树内（端到端 wiring 正确性）。
    // macOS 上 indexer 会 canonicalize 路径（`/var/folders/...` → `/private/var/...`），
    // 因此用 canonicalize 后的 root 作比对锚点。
    let root_canon = std::fs::canonicalize(&d.ctx.collections["default"].meta.roots[0])
        .expect("root 在 e2e 期间应可 canonicalize")
        .display()
        .to_string();
    let first_path = results[0]["path"]
        .as_str()
        .expect("results[0].path 应当是字符串");
    assert!(
        first_path.starts_with(&root_canon),
        "命中文件 path 应当在 root（canonical={root_canon}）子树内，实得 {first_path}"
    );
}

// ---- 5. /admin/reindex 并发竞态 → 409 ----

#[tokio::test]
async fn e2e_reindex_409_when_in_flight() {
    let d = DaemonHandle::spawn_with_fixtures(&[("a.txt", "x")]).await;

    // 通过 ctx 直接 pre-set in-flight 标志 —— 比真造并发请求更确定性（stub
    // reindex 是瞬时返回的，自然撞不上 race window）。与
    // `reindex::tests::concurrent_reindex_second_returns_in_flight` 同款手法。
    d.ctx.collections["default"].state.write().reindex_in_flight = true;

    let resp = Client::new()
        .post(d.url("/admin/reindex"))
        .bearer_auth(&d.token)
        .send()
        .await
        .expect("/admin/reindex POST 应当能发出");

    assert_eq!(
        resp.status(),
        409,
        "已有 reindex 在跑时应当返 409 Conflict，实得 {}",
        resp.status()
    );
}

// ---- MCP 协议 helper（BETA-36 cycle 5：按 rmcp stateless JSON framing 重写）----

/// 调用 MCP `tools/call`，返回 JSON-RPC `result` 对象（含 `content` / `isError` 等）。
///
/// rmcp 1.8 stateless 模式（`app.rs` `stateful_mode=false` + `json_response=true`）：
/// 无需 initialize / session-id，每个请求独立处理；非 initialize 请求必须带
/// `Mcp-Protocol-Version` header；响应是纯 `application/json` 单 body。
/// `token` 参数化：BETA-36 双 token e2e 用不同身份调用。
async fn mcp_call_tool(
    client: &Client,
    d: &DaemonHandle,
    token: &str,
    tool_name: &str,
    arguments: Value,
) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 42,
        "method": "tools/call",
        "params": {
            "name": tool_name,
            "arguments": arguments
        }
    });
    let resp = client
        .post(d.url("/mcp"))
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Protocol-Version", "2025-06-18")
        .body(body.to_string())
        .send()
        .await
        .expect("MCP tools/call POST 应当能成功");
    assert_eq!(
        resp.status(),
        200,
        "MCP tools/call 应当返 200，实得 {}",
        resp.status()
    );

    let text = resp.text().await.expect("tools/call 响应应当能读 body");
    let json_rpc: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("stateless 响应应是纯 JSON（{e}），原始：{text}"));
    assert_eq!(
        json_rpc["id"], 42,
        "JSON-RPC 响应 id 应当回显 42，实得 {}",
        json_rpc["id"]
    );
    // JSON-RPC 协议级错误（method_not_found / invalid_params / parse error）
    // 走 `error` 字段、`result` 字段缺席（ultra-review C-3 语义沿用）。
    if let Some(err_obj) = json_rpc.get("error") {
        let code = err_obj.get("code").and_then(Value::as_i64).unwrap_or(0);
        let msg = err_obj
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("<no message>");
        panic!("MCP JSON-RPC error (code={code}): {msg}；原始 body={text}");
    }
    json_rpc["result"].clone()
}

/// 从 tools/call result 抽 tool 输出 JSON（`content[0].text` 反序列化）。
fn tool_payload(result: &Value) -> Value {
    let payload_str = result["content"][0]["text"]
        .as_str()
        .expect("tools/call result.content[0].text 应当是字符串");
    serde_json::from_str(payload_str).expect("tool result 应当是合法 JSON")
}

// ---- BETA-36：双 collection 双 token（验收 ②③④ e2e）----

use common::{TOKEN_OPS, TOKEN_ZHANG};

/// 信息墙：受限 token 缺省检索只见授权集合；显式请求未授权集合 → tool error + audit。
#[tokio::test]
async fn e2e_infowall_search_scoped_and_denied() {
    let d = DaemonHandle::spawn_two_collections().await;
    let client = Client::new();

    // ① zhang 缺省搜 case-a 语料词 → 命中且 collection=case-a。
    let result = mcp_call_tool(
        &client,
        &d,
        TOKEN_ZHANG,
        "search",
        json!({"query": "blueharbor"}),
    )
    .await;
    let payload = tool_payload(&result);
    let hits = payload["results"].as_array().expect("results 应是数组");
    assert!(!hits.is_empty(), "zhang 搜授权集合语料应命中：{payload}");
    assert!(
        hits.iter().all(|h| h["collection"] == "case-a"),
        "受限 token 命中只应来自 case-a：{payload}"
    );

    // ② zhang 缺省搜 case-b 语料词 → 0 命中（物理信息墙：未授权集合的 db 根本不被查）。
    let result = mcp_call_tool(
        &client,
        &d,
        TOKEN_ZHANG,
        "search",
        json!({"query": "payroll"}),
    )
    .await;
    let payload = tool_payload(&result);
    assert!(
        payload["results"]
            .as_array()
            .expect("results 应是数组")
            .is_empty(),
        "未授权集合的内容不应可达：{payload}"
    );

    // ③ zhang 显式请求 case-b → tool-level error `access denied`。
    let result = mcp_call_tool(
        &client,
        &d,
        TOKEN_ZHANG,
        "search",
        json!({"query": "payroll", "collections": ["case-b"]}),
    )
    .await;
    assert_eq!(
        result["isError"],
        json!(true),
        "越权应返 tool error：{result}"
    );
    let msg = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        msg.contains("access denied") && msg.contains("case-b"),
        "错误文案应含 access denied + 请求的 collection id：{msg}"
    );

    // ④ list_collections：zhang 只见 case-a（case-b 存在性不泄漏）；ops 见两个。
    let result = mcp_call_tool(&client, &d, TOKEN_ZHANG, "list_collections", json!({})).await;
    let payload = tool_payload(&result);
    let cols = payload["collections"].as_array().unwrap();
    assert_eq!(cols.len(), 1, "{payload}");
    assert_eq!(cols[0]["id"], "case-a");
    let result = mcp_call_tool(&client, &d, TOKEN_OPS, "list_collections", json!({})).await;
    let payload = tool_payload(&result);
    assert_eq!(payload["collections"].as_array().unwrap().len(), 2);
}

/// 越权 REST：非 admin token 调 /admin/* → 403；audit 留痕含 denied + search 记录
/// （验收 ③④）。
#[tokio::test]
async fn e2e_admin_403_and_audit_trail() {
    let d = DaemonHandle::spawn_two_collections().await;
    let client = Client::new();

    // 先制造两条 audit：zhang 一次成功 search + 一次越权 denied。
    let _ = mcp_call_tool(
        &client,
        &d,
        TOKEN_ZHANG,
        "search",
        json!({"query": "blueharbor"}),
    )
    .await;
    let _ = mcp_call_tool(
        &client,
        &d,
        TOKEN_ZHANG,
        "search",
        json!({"query": "payroll", "collections": ["case-b"]}),
    )
    .await;

    // 非 admin token 调 /admin/reindex → 403（验收 ④ REST 侧）。
    let resp = client
        .post(d.url("/admin/reindex"))
        .bearer_auth(TOKEN_ZHANG)
        .send()
        .await
        .expect("POST 应能发出");
    assert_eq!(resp.status(), 403, "非 admin token 应 403");

    // 非 admin token 也读不了 audit。
    let resp = client
        .get(d.url("/admin/audit"))
        .bearer_auth(TOKEN_ZHANG)
        .send()
        .await
        .expect("GET 应能发出");
    assert_eq!(resp.status(), 403);

    // admin token 读 audit：应含 zhang 的 search（记 query 明文与命中数）与 denied 记录。
    let resp = client
        .get(d.url("/admin/audit?tail=50"))
        .bearer_auth(TOKEN_OPS)
        .send()
        .await
        .expect("GET 应能发出");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("audit 响应应是 JSON");
    let entries = body["entries"].as_array().expect("entries 应是数组");
    assert!(
        entries.iter().any(|e| e["subject"] == "zhang.san"
            && e["action"] == "search"
            && e["query"] == "blueharbor"),
        "audit 应含 zhang 的 search 记录（query 明文）：{body}"
    );
    assert!(
        entries.iter().any(|e| e["subject"] == "zhang.san"
            && e["action"] == "denied"
            && e["denied_reason"]
                .as_str()
                .is_some_and(|r| r.contains("case-b"))),
        "audit 应含 zhang 的越权 denied 记录：{body}"
    );
    assert!(
        entries.iter().any(|e| e["subject"] == "zhang.san"
            && e["action"] == "denied"
            && e["denied_reason"]
                .as_str()
                .is_some_and(|r| r.contains("/admin/reindex"))),
        "audit 应含 zhang 的 admin 403 denied 记录：{body}"
    );
}

/// 只读集合（冷冻归档）指名 reindex → 409；省略时只重建非只读集合。
#[tokio::test]
async fn e2e_read_only_collection_reindex_409() {
    let d = DaemonHandle::spawn_two_collections().await;
    let client = Client::new();

    let resp = client
        .post(d.url("/admin/reindex?collection=case-a"))
        .bearer_auth(TOKEN_OPS)
        .send()
        .await
        .expect("POST 应能发出");
    assert_eq!(resp.status(), 409, "只读集合指名 reindex 应 409");

    let resp = client
        .post(d.url("/admin/reindex"))
        .bearer_auth(TOKEN_OPS)
        .send()
        .await
        .expect("POST 应能发出");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("reindex 响应应是 JSON");
    assert_eq!(
        body["collections"],
        json!(["case-b"]),
        "省略 collection 时只读集合应被跳过：{body}"
    );
    // reindex 真实化：doc_count 反映 case-b 的真实索引数（corpus 含 1 txt）。
    assert!(
        body["doc_count"].as_u64().unwrap_or(0) >= 1,
        "真实 reindex 应返 case-b 实际文档数：{body}"
    );
}

/// reindex 真实化端到端：运行中往 collection root 加新文件 → POST reindex →
/// 新文件可被检索命中（daemon 不再需要重启才能看到新归档材料）。
#[tokio::test]
async fn e2e_reindex_picks_up_new_file() {
    let d = DaemonHandle::spawn_two_collections().await;
    let client = Client::new();

    // 运行中写入新文件到 case-b root。
    let root_b = d.ctx.collections["case-b"].meta.roots[0].clone();
    std::fs::write(
        root_b.join("late-arrival.txt"),
        "quarterly severance settlement addendum",
    )
    .expect("写入新文件应成功");

    // reindex 前：搜不到。
    let result = mcp_call_tool(
        &client,
        &d,
        TOKEN_OPS,
        "search",
        json!({"query": "severance"}),
    )
    .await;
    assert!(
        tool_payload(&result)["results"]
            .as_array()
            .unwrap()
            .is_empty(),
        "reindex 前新文件不应命中"
    );

    // 指名 reindex case-b。
    let resp = client
        .post(d.url("/admin/reindex?collection=case-b"))
        .bearer_auth(TOKEN_OPS)
        .send()
        .await
        .expect("POST 应能发出");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("reindex 响应应是 JSON");
    assert!(
        body["doc_count"].as_u64().unwrap_or(0) >= 2,
        "reindex 后 case-b 应含原 corpus + 新文件：{body}"
    );

    // reindex 后：命中新文件，且归属 case-b。
    let result = mcp_call_tool(
        &client,
        &d,
        TOKEN_OPS,
        "search",
        json!({"query": "severance"}),
    )
    .await;
    let payload = tool_payload(&result);
    let hits = payload["results"].as_array().unwrap();
    assert!(!hits.is_empty(), "reindex 后新文件应可命中：{payload}");
    assert_eq!(hits[0]["collection"], "case-b");
    assert!(
        hits[0]["path"]
            .as_str()
            .unwrap_or_default()
            .contains("late-arrival"),
        "命中应是新写入的文件：{payload}"
    );

    // list_collections 的 doc_count / indexed_at 同步刷新。
    let result = mcp_call_tool(&client, &d, TOKEN_OPS, "list_collections", json!({})).await;
    let payload = tool_payload(&result);
    let case_b = payload["collections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "case-b")
        .expect("应含 case-b");
    assert!(case_b["doc_count"].as_u64().unwrap_or(0) >= 2, "{payload}");
}

// ---- BETA-43：出处 / 权限闸门 / 审计导出（验收 ①②③④ e2e）----

/// 验收 ①：search 命中强制带出处——collection + path 顶层字段 + snippet 命中片段。
#[tokio::test]
async fn e2e_search_hits_carry_provenance_snippet() {
    let d = DaemonHandle::spawn_two_collections().await;
    let client = Client::new();

    let result = mcp_call_tool(
        &client,
        &d,
        TOKEN_ZHANG,
        "search",
        json!({"query": "blueharbor"}),
    )
    .await;
    let payload = tool_payload(&result);
    let hits = payload["results"].as_array().expect("results 应是数组");
    assert!(!hits.is_empty(), "应命中 case-a 语料：{payload}");
    let hit = &hits[0];
    assert_eq!(
        hit["collection"], "case-a",
        "出处应含 collection：{payload}"
    );
    assert!(
        hit["path"].as_str().is_some_and(|p| !p.is_empty()),
        "出处应含 path：{payload}"
    );
    assert!(
        hit["snippet"]
            .as_str()
            .is_some_and(|s| s.to_lowercase().contains("blueharbor")),
        "出处应含关键词命中片段（snippet）：{payload}"
    );
}

/// 验收 ②：read_document 片段模式——禁全文集合只返回命中窗口、绝不吐全文；
/// audit 留 read 记录（snippets 模式 + path）。
#[tokio::test]
async fn e2e_read_document_snippet_mode() {
    let d = DaemonHandle::spawn_two_collections().await;
    let client = Client::new();

    // 从 search 拿 case-a 命中的真实索引 path（canonicalize 后形态）。
    let result = mcp_call_tool(
        &client,
        &d,
        TOKEN_ZHANG,
        "search",
        json!({"query": "blueharbor"}),
    )
    .await;
    let payload = tool_payload(&result);
    let path_a = payload["results"][0]["path"]
        .as_str()
        .expect("case-a 命中应有 path")
        .to_string();

    // case-a 片段模式：返回命中窗口、绝不带 body 全文字段。
    let result = mcp_call_tool(
        &client,
        &d,
        TOKEN_ZHANG,
        "read_document",
        json!({"path": path_a, "collection": "case-a", "query": "blueharbor"}),
    )
    .await;
    assert_ne!(result["isError"], json!(true), "片段模式应成功：{result}");
    let payload = tool_payload(&result);
    assert_eq!(payload["mode"], "snippets", "{payload}");
    assert!(
        payload.get("body").is_none(),
        "片段模式不得吐全文：{payload}"
    );
    let snippets = payload["snippets"].as_array().expect("应有 snippets 数组");
    assert!(
        snippets
            .iter()
            .any(|s| s.as_str().unwrap_or_default().contains("blueharbor")),
        "片段应含命中词：{payload}"
    );

    // audit 留痕：read（snippets 模式 + path）。
    let resp = client
        .get(d.url("/admin/audit?tail=50"))
        .bearer_auth(TOKEN_OPS)
        .send()
        .await
        .expect("GET 应能发出");
    let body: Value = resp.json().await.expect("audit 响应应是 JSON");
    let entries = body["entries"].as_array().expect("entries 应是数组");
    assert!(
        entries.iter().any(|e| e["subject"] == "zhang.san"
            && e["action"] == "read"
            && e["read_mode"] == "snippets"
            && e["path"].as_str().is_some_and(|p| !p.is_empty())),
        "audit 应含 zhang 的片段读取记录：{body}"
    );
}

/// 验收 ②④：read_document 全文闸门——禁全文集合 full=true 被拒（audit denied 留痕）；
/// 允许全文集合 full=true 返回正文；越权集合同 search 拒绝语义。
#[tokio::test]
async fn e2e_read_document_full_gate() {
    let d = DaemonHandle::spawn_two_collections().await;
    let client = Client::new();

    // ① case-a 禁全文：full=true → access denied（不泄漏正文）。
    // path 用任意值即可——闸门判定先于文档存在性判定。
    let result = mcp_call_tool(
        &client,
        &d,
        TOKEN_ZHANG,
        "read_document",
        json!({"path": "whatever.txt", "collection": "case-a", "full": true}),
    )
    .await;
    assert_eq!(result["isError"], json!(true), "禁全文应拒：{result}");
    let msg = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        msg.contains("access denied") && msg.contains("full read disabled"),
        "错误文案应说明全文被策略禁用：{msg}"
    );

    // ② case-b 允许全文：ops full=true → 返回正文。
    let result = mcp_call_tool(
        &client,
        &d,
        TOKEN_OPS,
        "search",
        json!({"query": "payroll"}),
    )
    .await;
    let path_b = tool_payload(&result)["results"][0]["path"]
        .as_str()
        .expect("case-b 命中应有 path")
        .to_string();
    let result = mcp_call_tool(
        &client,
        &d,
        TOKEN_OPS,
        "read_document",
        json!({"path": path_b, "collection": "case-b", "full": true}),
    )
    .await;
    assert_ne!(result["isError"], json!(true), "允许全文应成功：{result}");
    let payload = tool_payload(&result);
    assert_eq!(payload["mode"], "full", "{payload}");
    assert!(
        payload["body"]
            .as_str()
            .is_some_and(|b| b.contains("payroll ledger")),
        "全文模式应返回索引内正文：{payload}"
    );

    // ③ zhang 读未授权 case-b → 与 search 同款 denied（存在性不泄漏）。
    let result = mcp_call_tool(
        &client,
        &d,
        TOKEN_ZHANG,
        "read_document",
        json!({"path": path_b, "collection": "case-b", "query": "payroll"}),
    )
    .await;
    assert_eq!(result["isError"], json!(true), "越权读应拒：{result}");

    // ④ audit 留痕：full-read denied 记录在。
    let resp = client
        .get(d.url("/admin/audit?tail=50"))
        .bearer_auth(TOKEN_OPS)
        .send()
        .await
        .expect("GET 应能发出");
    let body: Value = resp.json().await.expect("audit 响应应是 JSON");
    let entries = body["entries"].as_array().expect("entries 应是数组");
    assert!(
        entries.iter().any(|e| e["subject"] == "zhang.san"
            && e["action"] == "denied"
            && e["denied_reason"]
                .as_str()
                .is_some_and(|r| r.contains("full read disabled"))),
        "audit 应含禁全文 denied 记录：{body}"
    );
}

/// 验收 ③：/admin/audit/report 导出人读合规报告（md / csv + subject 过滤）；
/// 非 admin 403、坏 format 400。
#[tokio::test]
async fn e2e_admin_audit_report_export() {
    let d = DaemonHandle::spawn_two_collections().await;
    let client = Client::new();

    // 制造记录：zhang 一次 search + 一次越权 denied；ops 一次 search。
    let _ = mcp_call_tool(
        &client,
        &d,
        TOKEN_ZHANG,
        "search",
        json!({"query": "blueharbor"}),
    )
    .await;
    let _ = mcp_call_tool(
        &client,
        &d,
        TOKEN_ZHANG,
        "search",
        json!({"query": "payroll", "collections": ["case-b"]}),
    )
    .await;
    let _ = mcp_call_tool(
        &client,
        &d,
        TOKEN_OPS,
        "search",
        json!({"query": "payroll"}),
    )
    .await;

    // Markdown（缺省 format）：含标题 / 统计 / zhang 的明细与 denied 原因。
    let resp = client
        .get(d.url("/admin/audit/report?subject=zhang.san"))
        .bearer_auth(TOKEN_OPS)
        .send()
        .await
        .expect("GET 应能发出");
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers()["content-type"]
            .to_str()
            .unwrap_or_default()
            .starts_with("text/markdown"),
        "md 报告应带 text/markdown content-type"
    );
    let md = resp.text().await.expect("应能读 body");
    assert!(md.contains("# Scout 检索审计报告"), "{md}");
    assert!(md.contains("zhang.san"), "{md}");
    assert!(
        !md.contains("| ops |"),
        "subject 过滤后不应含 ops 的记录：{md}"
    );
    assert!(md.contains("case-b"), "denied 记录应在报告明细中：{md}");

    // CSV：表头 + zhang 行。
    let resp = client
        .get(d.url("/admin/audit/report?format=csv&subject=zhang.san"))
        .bearer_auth(TOKEN_OPS)
        .send()
        .await
        .expect("GET 应能发出");
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers()["content-type"]
            .to_str()
            .unwrap_or_default()
            .starts_with("text/csv"),
        "csv 报告应带 text/csv content-type"
    );
    let csv = resp.text().await.expect("应能读 body");
    assert!(
        csv.starts_with("ts,subject,action,collections,query,results,path,denied_reason"),
        "{csv}"
    );
    assert!(csv.contains("zhang.san"), "{csv}");

    // 非 admin → 403；坏 format → 400。
    let resp = client
        .get(d.url("/admin/audit/report"))
        .bearer_auth(TOKEN_ZHANG)
        .send()
        .await
        .expect("GET 应能发出");
    assert_eq!(resp.status(), 403, "非 admin token 应 403");
    let resp = client
        .get(d.url("/admin/audit/report?format=pdf"))
        .bearer_auth(TOKEN_OPS)
        .send()
        .await
        .expect("GET 应能发出");
    assert_eq!(resp.status(), 400, "未知 format 应 400");
}
