//! MCP tool 实现集（`search` / `list_collections` 等）。
//!
//! BETA-32 T5 定义本地 [`Tool`] trait + [`ToolError`] + [`default_tools`] 注册器；
//! BETA-36 给 `invoke` 加 [`AuthedPrincipal`] 参数（HTTP 鉴权层注入、rmcp
//! extensions 穿透，见 [`crate::mcp`]）——tool 内部据此做 collection 级授权，
//! 越权返 [`ToolError::Denied`]。

pub mod list_collections;
pub mod read_document;
pub mod search;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::auth::AuthedPrincipal;
use crate::config::ServerCtx;

/// 本地 MCP tool 契约 —— 与 rmcp 的 `ServerHandler` trait 解耦，
/// 让单测可以脱离 transport 直接覆盖 schema / invoke 行为。
#[async_trait]
pub trait Tool: Send + Sync {
    /// MCP tool 名（用于 JSON-RPC `tools/call` 的 `name` 字段）。
    fn name(&self) -> &'static str;
    /// 给模型看的简短英文描述（MCP `tools/list` 暴露）。
    fn description(&self) -> &'static str;
    /// JSON Schema：`tools/list` 暴露给客户端做参数校验。
    fn input_schema(&self) -> Value;
    /// 真实执行入口；调用方负责 bearer 鉴权并传入命中的 principal。
    async fn invoke(
        &self,
        args: Value,
        ctx: Arc<ServerCtx>,
        principal: Arc<AuthedPrincipal>,
    ) -> Result<Value, ToolError>;
}

/// Tool 调用错误 —— 由 MCP adapter 翻译为 tool-level error / JSON-RPC error。
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// 参数解析 / 校验失败（对应 JSON-RPC `-32602 Invalid params`）。
    #[error("invalid params: {0}")]
    InvalidParams(String),
    /// collection 级越权（BETA-36 验收 ④ 的 MCP 侧路径）。message 可回显 client
    /// （只含请求里的 collection id，不泄漏服务端信息）。
    #[error("access denied: {0}")]
    Denied(String),
    /// 内部错误（对应 JSON-RPC `-32603 Internal error`）。
    #[error("internal error: {0}")]
    Internal(String),
}

/// 默认 tool 注册器 —— `search` + `list_collections` + `read_document`（BETA-43）。
///
/// daemon 启动时调用本函数拿到全集、灌进 MCP adapter（T8）。
#[must_use]
pub fn default_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(search::SearchTool),
        Arc::new(list_collections::ListCollectionsTool),
        Arc::new(read_document::ReadDocumentTool),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tools_lists_search_and_list_collections() {
        let tools = default_tools();
        let names: Vec<&'static str> = tools.iter().map(|t| t.name()).collect();
        assert!(
            names.contains(&"search"),
            "default_tools 应包含 search：{names:?}"
        );
        assert!(
            names.contains(&"list_collections"),
            "default_tools 应包含 list_collections：{names:?}"
        );
        assert!(
            names.contains(&"read_document"),
            "default_tools 应包含 read_document（BETA-43）：{names:?}"
        );
    }
}
