//! BETA-32 T12：MCP client helper。
//!
//! 用于 daemon-mode evals harness 走 scoutd 暴露的 streamable HTTP MCP
//! endpoint 查询 daemon。
//!
//! **协议要点**：
//!
//! - daemon 当前以 stateless MCP 方式处理请求，daemon-mode evals 不再执行
//!   `initialize` / `notifications/initialized` 握手。
//! - 非 initialize 请求必须带 `Mcp-Protocol-Version` header。
//! - `tools/call` 可能返回普通 JSON，也兼容 SSE framing；[`mcp_call_tool`]
//!   统一用 [`extract_jsonrpc_from_sse`] 抽 JSON-RPC payload。

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde_json::{json, Value};

/// rmcp 1.8 协议版本号（与 daemon e2e 测试一致）。
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// 跨多个 tool 调用复用的 MCP session 句柄。
#[derive(Debug, Clone)]
pub struct McpSession {
    /// daemon 监听地址。
    pub addr: SocketAddr,
    /// bearer token（明文）。
    pub token: String,
    /// stateless 模式下为空；保留字段以维持 evals helper 的调用形态稳定。
    pub session_id: String,
}

impl McpSession {
    /// 拼 `http://<addr><path>` URL。
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

/// 构造 stateless MCP session 句柄。
pub async fn mcp_initialize(client: &Client, addr: SocketAddr, token: &str) -> Result<McpSession> {
    let _ = client;
    Ok(McpSession {
        addr,
        token: token.to_owned(),
        session_id: String::new(),
    })
}

/// 调用 MCP `tools/call`，返回 JSON-RPC `result` 对象。
///
/// SSE body 形如：
///
/// ```text
/// id: 0/0
/// retry: 3000
/// data: {priming json}
///
/// id: 1/0
/// data: {"jsonrpc":"2.0","id":42,"result":{...}}
/// ```
///
/// 取**第一个**含 `"jsonrpc"` 的 `data:` 行的 JSON、抽 `result` 字段。
/// rmcp 1.8 `stateful_mode` 每个 request 只 emit 一个 JSON-RPC 帧（priming
/// `data:` 是 placeholder、不含 `jsonrpc` 字段），first == 唯一一条；用
/// "first" 比 "last" 更准确反映实现。
pub async fn mcp_call_tool(
    client: &Client,
    session: &McpSession,
    tool_name: &str,
    arguments: Value,
) -> Result<Value> {
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
        .post(session.url("/mcp"))
        .bearer_auth(&session.token)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Protocol-Version", MCP_PROTOCOL_VERSION)
        .body(body.to_string())
        .send()
        .await
        .context("MCP tools/call POST 失败")?;
    let status = resp.status();
    if status != 200 {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "MCP tools/call 应当返 200，实得 {status}；body={body}"
        ));
    }

    let text = resp.text().await.context("tools/call 响应 body 读失败")?;
    let json_rpc = extract_jsonrpc_from_sse(&text)
        .ok_or_else(|| anyhow!("从 SSE 体抽不到 JSON-RPC 响应，原始 body={text}"))?;
    if json_rpc["id"] != json!(42) {
        return Err(anyhow!(
            "JSON-RPC 响应 id 应当回显 42，实得 {}",
            json_rpc["id"]
        ));
    }
    // ultra-review C-3：JSON-RPC 协议级错误（method_not_found / invalid_params /
    // parse error 等）走 `error` 字段、`result` 字段缺席。原实现直接返
    // `result.clone()` → `Value::Null`，下游误诊为"JSON shape wrong"、真实错误
    // 丢失。先查 `error`、显式向上抛。
    if let Some(err_obj) = json_rpc.get("error") {
        let code = err_obj.get("code").and_then(Value::as_i64).unwrap_or(0);
        let msg = err_obj
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("<no message>");
        anyhow::bail!("MCP JSON-RPC error (code={code}): {msg}");
    }
    Ok(json_rpc["result"].clone())
}

/// 从 SSE-framed 响应体里抽出 JSON-RPC 响应。
///
/// 找含 `"jsonrpc"` 字符串的 `data:` 行 → 反序列化 → 返回。priming event 的
/// 占位 payload 不含 `jsonrpc` 字段、自然被跳过。
fn extract_jsonrpc_from_sse(body: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(body.trim()) {
        return Some(v);
    }

    for event in body.split("\n\n") {
        for line in event.lines() {
            let Some(payload) = line
                .strip_prefix("data: ")
                .or_else(|| line.strip_prefix("data:"))
            else {
                continue;
            };
            let payload = payload.trim();
            if !payload.contains("\"jsonrpc\"") {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(payload) {
                return Some(v);
            }
        }
    }
    None
}

/// 轮询 `/health` 直到 200 或超时。daemon spawn 后 ctx 构造（首次全量索引）耗时
/// 不可预测；上层 CLI 默认 60s（见 `evals.rs --health-timeout-secs`），本函数
/// 不再 hardcode 默认、由调用方传 timeout。
pub async fn wait_for_health(client: &Client, addr: SocketAddr, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    let url = format!("http://{addr}/health");
    while std::time::Instant::now() < deadline {
        if let Ok(r) = client.get(&url).send().await {
            if r.status().is_success() {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(anyhow!("daemon /health 在 {timeout:?} 内未返回 200"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn extract_jsonrpc_skips_priming_event() {
        // priming event（首条 data: 不是 jsonrpc）+ 真正 result event。
        let body = "id: 0/0\nretry: 3000\ndata: keepalive\n\n\
                    id: 1/0\ndata: {\"jsonrpc\":\"2.0\",\"id\":42,\"result\":{\"ok\":true}}\n\n";
        let v = extract_jsonrpc_from_sse(body).expect("应当能抽到 JSON-RPC");
        assert_eq!(v["id"], 42);
        assert_eq!(v["result"]["ok"], true);
    }

    #[test]
    fn extract_jsonrpc_returns_none_when_no_data_line() {
        assert!(extract_jsonrpc_from_sse("nope\n\nnope2").is_none());
    }

    #[test]
    fn extract_jsonrpc_handles_data_without_space() {
        // SSE 规范允许 `data:payload` 无空格、本实现兼容。
        let body = "data:{\"jsonrpc\":\"2.0\",\"id\":42,\"result\":1}\n\n";
        let v = extract_jsonrpc_from_sse(body).expect("应当能抽");
        assert_eq!(v["result"], 1);
    }
}
