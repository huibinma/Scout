//! `Scout` 服务端核心 —— 把现有 hybrid 检索能力包装成 MCP server。
//!
//! daemon binary 与未来桌面 app 嵌入模式都通过本 crate 复用同一份 server 逻辑。
//!
//! # 模块布局
//!
//! - [`config`]：`ServerConfig` 与 `ServerCtx`（T4 填充）
//! - [`auth`]：Bearer token 鉴权 + 常量时间比较（T5）
//! - [`tools`]：MCP tool 实现集（search / `file_action` 等，T6+）
//! - [`admin`]：管理面 REST endpoints（T7）
//! - [`reindex`]：reindex 任务调度（T7）
//! - [`mcp`]：MCP server 装配（T8）
//! - [`provenance`]：出处定位纯函数（BETA-43：片段窗口 + 命中回页）
//! - [`app`]：[`axum::Router`] 工厂（T8）
//! - [`test_support`]：stub embedder + 内存 ctx builder（T6 起 ctx-aware 单测 / T11 集成测试用）
//!
//! 本 crate 在 BETA-32 C2a 起仅为骨架；各 mod 由后续 task 填充。

pub mod admin;
pub mod app;
pub mod audit;
pub mod audit_report;
pub mod auth;
pub mod collections;
pub mod config;
pub mod mcp;
pub mod provenance;
pub mod reindex;
pub mod test_support;
pub mod tools;

mod quick_search;
mod search_http;

pub use config::{ServerConfig, ServerCtx};
