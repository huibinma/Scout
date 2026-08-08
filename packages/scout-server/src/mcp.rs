//! MCP server 装配（rmcp 1.8 `ServerHandler` + Streamable HTTP transport）。
//!
//! BETA-32 T8：把本 crate 的 [`Tool`] trait（`packages/scout-server/src/tools`）
//! 桥接到 rmcp 1.8 `ServerHandler`，让 [`crate::app::build_app`] 可以把 MCP 端点
//! 挂到 `axum::Router::nest_service("/mcp", ...)`。
//!
//! **rmcp 1.8 API 注意**：`ServerHandler` trait 用 RPIT-style，不用 `#[async_trait]`：
//!
//! ```ignore
//! fn call_tool(
//!     &self,
//!     request: CallToolRequestParams,
//!     context: RequestContext<RoleServer>,
//! ) -> impl Future<Output = Result<CallToolResult, McpError>> + MaybeSendFuture + '_;
//! ```
//!
//! 即在 trait method 上直接 `-> impl Future<...> + ...`、签名 sync、body 写
//! `async move { ... }` 包成 future。`async-trait` dep 不适用于 `ServerHandler` impl；
//! 但本 crate 内部 [`Tool`] trait 仍走 `#[async_trait]`（dyn-safe）。
//!
//! 错误映射：
//! - [`ToolError::InvalidParams`] → `Ok(CallToolResult::error(...))`：MCP 鼓励
//!   "工具跑了但失败" 走 tool-level error、把 message 还给客户端看（rmcp 1.8
//!   `CallToolResult::error` doc 明确这是 "几乎所有 tool 失败路径" 的正确选择）。
//! - [`ToolError::Internal`] → `Ok(CallToolResult::error(...))` + `is_error=true`
//!   同理；不走 `Err(ErrorData)` 因为后者在客户端会被渲成 opaque "internal error"
//!   提示，看不到我们的错误内容。
//! - "tool 不存在" → `Err(ErrorData::method_not_found::<CallToolRequestMethod>())`：
//!   这是真正的 protocol error、客户端按 -32601 处理。
//!
//! [`Tool`]: crate::tools::Tool
//! [`ToolError::InvalidParams`]: crate::tools::ToolError::InvalidParams
//! [`ToolError::Internal`]: crate::tools::ToolError::Internal

use std::future::Future;
use std::sync::Arc;

use rmcp::{
    model::{
        CallToolRequestMethod, CallToolRequestParams, CallToolResult, Content, JsonObject,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool as McpTool,
    },
    service::RequestContext,
    ErrorData as McpError, RoleServer, ServerHandler,
};
use serde_json::Value;

use crate::auth::AuthedPrincipal;
use crate::config::ServerCtx;
use crate::tools::{default_tools, Tool, ToolError};

/// 把本地 [`Tool`] trait 集合包成一个 rmcp 1.8 `ServerHandler`。
///
/// daemon 启动时构造一次、走 `Arc` 共享给 `StreamableHttpService::new`
/// 的 service factory，让每个 session clone 一份 handler ref。
pub struct ScoutMcpHandler {
    pub ctx: Arc<ServerCtx>,
    pub tools: Vec<Arc<dyn Tool>>,
}

impl std::fmt::Debug for ScoutMcpHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Arc<dyn Tool>` 不要求 Debug；这里仅列 tool name，避免给 Tool trait 强加 bound。
        let names: Vec<&'static str> = self.tools.iter().map(|t| t.name()).collect();
        f.debug_struct("ScoutMcpHandler")
            .field("ctx", &self.ctx)
            .field("tools", &names)
            .finish()
    }
}

impl ScoutMcpHandler {
    /// 默认装配 —— 用 [`default_tools`] 注册 BETA-32 范围内的 `search` + `list_roots`。
    #[must_use]
    pub fn new(ctx: Arc<ServerCtx>) -> Self {
        Self {
            ctx,
            tools: default_tools(),
        }
    }

    /// 自定义 tools —— 单测可注入桩 tool 验证 dispatch / 错误映射。
    #[must_use]
    pub fn with_tools(ctx: Arc<ServerCtx>, tools: Vec<Arc<dyn Tool>>) -> Self {
        Self { ctx, tools }
    }

    /// 把本地 [`Tool`] schema（`Value`）转 rmcp 1.8 `model::Tool`。
    ///
    /// 本地 `input_schema()` 返 `serde_json::Value`、rmcp 1.8 要的是
    /// `Arc<JsonObject>`（= `serde_json::Map<String, Value>`）。`Value` 不是
    /// object 时退化成空 schema —— 实际不会触发（两个内置 tool 的 schema
    /// 都是 `{ "type": "object", ... }`）但避免 unwrap 红线。
    fn to_mcp_tool(t: &dyn Tool) -> McpTool {
        let schema_val = t.input_schema();
        let schema_obj: JsonObject = match schema_val {
            Value::Object(map) => map,
            _ => JsonObject::new(),
        };
        // `McpTool` 是 `#[non_exhaustive]`、外部不能直接 struct literal；走 `Tool::new`。
        McpTool::new(t.name(), t.description(), Arc::new(schema_obj))
    }

    /// 从 rmcp `RequestContext` 提取 HTTP 鉴权层注入的 principal。
    ///
    /// BETA-36 spec §4「HTTP→MCP 穿透」：rmcp 1.8 streamable-HTTP 把
    /// `http::request::Parts`（含 axum middleware 塞的 extensions）注入 MCP
    /// request extensions（rmcp `tower.rs:1089/1158/1246`）。`/mcp` 全程套
    /// bearer middleware，正常路径必有 principal；缺失 = 装配错误（防御性返 None）。
    fn principal_from_context(
        context: &RequestContext<RoleServer>,
    ) -> Option<Arc<AuthedPrincipal>> {
        context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Arc<AuthedPrincipal>>())
            .cloned()
    }

    /// `call_tool` 的纯逻辑分支 —— 不依赖 `RequestContext`，让单测可独立覆盖
    /// 错误映射（`Ok`→`success` / `InvalidParams`/`Denied`/`Internal`→`error` /
    /// 未知 tool→`method_not_found`）。
    async fn dispatch(
        &self,
        name: &str,
        arguments: Option<JsonObject>,
        principal: Arc<AuthedPrincipal>,
    ) -> Result<CallToolResult, McpError> {
        let Some(tool) = self.tools.iter().find(|t| t.name() == name).cloned() else {
            return Err(McpError::method_not_found::<CallToolRequestMethod>());
        };
        let args_val: Value = match arguments {
            Some(map) => Value::Object(map),
            None => Value::Object(JsonObject::new()),
        };
        match tool
            .invoke(args_val, self.ctx.clone(), principal.clone())
            .await
        {
            Ok(v) => {
                let text = serde_json::to_string(&v)
                    .unwrap_or_else(|e| format!("{{\"error\":\"serialize failed: {e}\"}}"));
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            // InvalidParams 来自 caller 的入参错误、不含本机敏感信息、可回显。
            // `msg` 已是 tool 内部组装的描述（如 "missing query"），不再加 `invalid params:`
            // 前缀——`ToolError::InvalidParams` Display 自带、外面再拼会双前缀（reviewer Minor #9）。
            Err(ToolError::InvalidParams(msg)) => {
                Ok(CallToolResult::error(vec![Content::text(msg)]))
            }
            // Denied（BETA-36 验收 ④ MCP 侧）：越权 collection 访问。message 只含请求
            // 里的 collection id、可回显；tracing 记 subject 供 ops 侧观察（audit 专用
            // 留痕在 tool 内部/audit sink——cycle 4）。
            Err(ToolError::Denied(msg)) => {
                tracing::warn!(
                    subject = %principal.subject,
                    tool = name,
                    denied = %msg,
                    "collection 越权访问被拒"
                );
                Ok(CallToolResult::error(vec![Content::text(format!(
                    "access denied: {msg}"
                ))]))
            }
            // Internal 错误（rusqlite / io::Error 等）可能含本机绝对路径，按 spec §6.1/§6.2
            // 隐私硬规则不允许暴露给 client；只在 tracing log 留 full message 给 ops 排障，
            // client 仅看泛化 "internal error" 字串（reviewer Important #1）。
            Err(ToolError::Internal(msg)) => {
                tracing::error!(error = %msg, tool = name, "tool dispatch failed (internal)");
                Ok(CallToolResult::error(vec![Content::text(
                    "internal error".to_string(),
                )]))
            }
        }
    }
}

impl ServerHandler for ScoutMcpHandler {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` (= `InitializeResult`) 与 `Implementation` 都是 `#[non_exhaustive]`，
        // 必须走 `::new` + 链式 `with_*` 构造；不能直接 struct literal。
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Scout 团队归档检索 MCP server，按归档集合（collection：案件 / 离职员工 / \
                 审计项目）组织文档、文件、媒体索引；每个 token 只能检索其被授权的集合。\
                 用 `search` 工具按自然语言搜索（支持 OCR 正文、中文、英文、跨语言模糊查询，\
                 也支持按「身份证/手机号」等已识别 PII 类型概念词检索；例如\
                 「去年汇报过的友商竞争分析」；可选 `collections` 参数限定集合），\
                 返回命中文件的 path / name / collection / size / mtime / score 以及出处\
                 定位（snippet 命中片段；扫描件另带 pages 命中页号）。\
                 如需阅读命中文档内容，用 `read_document` 工具（传入命中的 path 与 \
                 collection）：缺省片段模式只返回 query 命中片段 + 有限上下文；\
                 `full=true` 请求全文，仅在该集合策略 allow_full_read 开启时可用，\
                 否则被拒并留审计记录。\
                 用 `list_collections` 工具查询当前 token 可检索的归档集合与索引状态。",
            )
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        // `list_tools` 不分页 —— BETA-32 范围内 tool 数极少（2 个），全量返。
        let tools: Vec<McpTool> = self.tools.iter().map(|t| Self::to_mcp_tool(&**t)).collect();
        async move { Ok(ListToolsResult::with_all_items(tools)) }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResult, McpError>> + Send + '_ {
        // `arguments` 借 owned 进 async block；name 提前 String 化避免借出 self.tools 生命周期。
        let name = request.name.into_owned();
        let arguments = request.arguments;
        let principal = Self::principal_from_context(&context);
        async move {
            // `/mcp` 全程套 bearer middleware，principal 缺失 = 装配错误（如测试
            // 环境绕过了 middleware）——按内部错误处理、不猜权限。
            let Some(principal) = principal else {
                tracing::error!(tool = %name, "request extensions 缺 AuthedPrincipal（装配错误？）");
                return Ok(CallToolResult::error(vec![Content::text(
                    "internal error".to_string(),
                )]));
            };
            self.dispatch(&name, arguments, principal).await
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::test_support::{build_test_ctx_inmem, full_access_principal};
    use async_trait::async_trait;
    use serde_json::json;

    /// 桩 tool：可控错误路径、不依赖 harness / 索引。
    struct OkTool;
    #[async_trait]
    impl Tool for OkTool {
        fn name(&self) -> &'static str {
            "ok"
        }
        fn description(&self) -> &'static str {
            "always returns success"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn invoke(
            &self,
            _args: Value,
            _ctx: Arc<ServerCtx>,
            _principal: Arc<AuthedPrincipal>,
        ) -> Result<Value, ToolError> {
            Ok(json!({"hello": "world"}))
        }
    }

    struct InvalidTool;
    #[async_trait]
    impl Tool for InvalidTool {
        fn name(&self) -> &'static str {
            "bad_params"
        }
        fn description(&self) -> &'static str {
            "always invalid params"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn invoke(
            &self,
            _args: Value,
            _ctx: Arc<ServerCtx>,
            _principal: Arc<AuthedPrincipal>,
        ) -> Result<Value, ToolError> {
            Err(ToolError::InvalidParams("missing query".into()))
        }
    }

    struct InternalTool;
    #[async_trait]
    impl Tool for InternalTool {
        fn name(&self) -> &'static str {
            "boom"
        }
        fn description(&self) -> &'static str {
            "always internal error"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn invoke(
            &self,
            _args: Value,
            _ctx: Arc<ServerCtx>,
            _principal: Arc<AuthedPrincipal>,
        ) -> Result<Value, ToolError> {
            Err(ToolError::Internal("db locked".into()))
        }
    }

    struct DeniedTool;
    #[async_trait]
    impl Tool for DeniedTool {
        fn name(&self) -> &'static str {
            "walled"
        }
        fn description(&self) -> &'static str {
            "always denied"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn invoke(
            &self,
            _args: Value,
            _ctx: Arc<ServerCtx>,
            _principal: Arc<AuthedPrincipal>,
        ) -> Result<Value, ToolError> {
            Err(ToolError::Denied("collection 'case-b'".into()))
        }
    }

    #[test]
    fn to_mcp_tool_preserves_name_description_schema() {
        let t: Arc<dyn Tool> = Arc::new(OkTool);
        let mcp = ScoutMcpHandler::to_mcp_tool(&*t);
        assert_eq!(mcp.name.as_ref(), "ok");
        assert_eq!(mcp.description.as_deref(), Some("always returns success"));
        // schema 是 object —— 进 JsonObject，type 字段还在。
        let schema_val = serde_json::to_value(&*mcp.input_schema).unwrap();
        assert_eq!(schema_val["type"], json!("object"));
    }

    #[tokio::test]
    async fn dispatch_ok_tool_returns_success_with_serialized_payload() {
        let ctx = build_test_ctx_inmem();
        let handler = ScoutMcpHandler::with_tools(ctx, vec![Arc::new(OkTool)]);
        let res = handler
            .dispatch("ok", None, full_access_principal())
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(false));
        // Content::text 单块、内容应是 OkTool 返的 JSON。
        assert_eq!(res.content.len(), 1);
        let text = res.content[0]
            .as_text()
            .expect("dispatch success 应返 Content::text")
            .text
            .clone();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, json!({"hello": "world"}));
    }

    #[tokio::test]
    async fn dispatch_invalid_params_returns_tool_error_without_double_prefix() {
        // reviewer Minor #9：dispatch 不再加 `invalid params:` 前缀
        // （`ToolError::InvalidParams` Display 自带，前缀会双前缀）。
        // 当前契约：直接透传 tool 内部组装的 message 给 client。
        let ctx = build_test_ctx_inmem();
        let handler = ScoutMcpHandler::with_tools(ctx, vec![Arc::new(InvalidTool)]);
        let res = handler
            .dispatch("bad_params", None, full_access_principal())
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        let text = res.content[0].as_text().unwrap().text.clone();
        assert_eq!(
            text, "missing query",
            "InvalidParams 应直接透传 tool message、不再加 `invalid params:` 前缀"
        );
    }

    #[tokio::test]
    async fn dispatch_internal_error_returns_sanitized_message() {
        // reviewer Important #1：Internal 错误（rusqlite / io::Error 等含本机路径）
        // 必须 sanitize、不暴露给 client；full message 走 tracing log（此处不验证 log）。
        let ctx = build_test_ctx_inmem();
        let handler = ScoutMcpHandler::with_tools(ctx, vec![Arc::new(InternalTool)]);
        let res = handler
            .dispatch("boom", None, full_access_principal())
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        let text = res.content[0].as_text().unwrap().text.clone();
        assert_eq!(
            text, "internal error",
            "Internal 错误 client 端应仅看到泛化字串、不回显 `db locked` 等原始 msg"
        );
        assert!(
            !text.contains("db locked"),
            "Internal 原始 msg 不允许出现在 client response"
        );
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_returns_method_not_found() {
        let ctx = build_test_ctx_inmem();
        let handler = ScoutMcpHandler::with_tools(ctx, vec![Arc::new(OkTool)]);
        let err = handler
            .dispatch("nonexistent", None, full_access_principal())
            .await
            .unwrap_err();
        // method_not_found 的 code 是 -32601。
        assert_eq!(err.code.0, -32601, "未知 tool 应回 -32601 method_not_found");
    }

    /// BETA-36 验收 ④ MCP 侧：Denied → tool-level error、message 带 `access denied:` 前缀。
    #[tokio::test]
    async fn dispatch_denied_returns_tool_error_with_prefix() {
        let ctx = build_test_ctx_inmem();
        let handler = ScoutMcpHandler::with_tools(ctx, vec![Arc::new(DeniedTool)]);
        let res = handler
            .dispatch("walled", None, full_access_principal())
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        let text = res.content[0].as_text().unwrap().text.clone();
        assert_eq!(text, "access denied: collection 'case-b'");
    }

    #[test]
    fn get_info_advertises_tools_capability() {
        let ctx = build_test_ctx_inmem();
        let handler = ScoutMcpHandler::new(ctx);
        let info = handler.get_info();
        assert!(
            info.capabilities.tools.is_some(),
            "ServerInfo 应通告 tools capability"
        );
    }
}
