//! `list_collections` tool —— 返回当前 token 授权的归档集合与索引统计。
//!
//! BETA-36（替换 BETA-32 的 `list_roots`）：collection 是归档主体边界（案件 /
//! 员工 / 审计项目），本 tool 只列 principal 授权的集合——**未授权集合的存在性
//! 也不泄漏**（律所信息墙语义）。

use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use super::{Tool, ToolError};
use crate::auth::AuthedPrincipal;
use crate::config::ServerCtx;

#[derive(Serialize)]
struct CollectionEntry {
    id: String,
    display_name: String,
    subject_kind: crate::collections::SubjectKind,
    read_only: bool,
    /// 读取类工具是否允许全文（BETA-43；false = 仅片段模式可用）。
    allow_full_read: bool,
    roots: Vec<String>,
    audit_tags: Vec<String>,
    doc_count: u64,
    indexed_at: Option<String>,
}

#[derive(Serialize)]
struct ListCollectionsOutput {
    collections: Vec<CollectionEntry>,
}

/// `list_collections` tool —— 让 MCP 客户端确认自己能检索哪些归档集合与索引新鲜度。
#[derive(Debug)]
pub struct ListCollectionsTool;

#[async_trait]
impl Tool for ListCollectionsTool {
    fn name(&self) -> &'static str {
        "list_collections"
    }

    fn description(&self) -> &'static str {
        "List the archive collections this token is authorized to search, with index stats."
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }

    async fn invoke(
        &self,
        _args: Value,
        ctx: Arc<ServerCtx>,
        principal: Arc<AuthedPrincipal>,
    ) -> Result<Value, ToolError> {
        let collections: Vec<CollectionEntry> = ctx
            .collections
            .values()
            .filter(|rt| principal.can_access(&rt.meta.id))
            .map(|rt| {
                let st = rt.state.read();
                CollectionEntry {
                    id: rt.meta.id.clone(),
                    display_name: rt.meta.display_name().to_string(),
                    subject_kind: rt.meta.subject_kind,
                    read_only: rt.meta.read_only,
                    allow_full_read: rt.meta.allow_full_read,
                    roots: rt
                        .meta
                        .roots
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect(),
                    audit_tags: rt.meta.audit_tags.clone(),
                    doc_count: st.doc_count,
                    indexed_at: st.indexed_at.map(|t| t.to_rfc3339()),
                }
            })
            .collect();
        // 留痕（验收 ③：谁在什么时候看了哪些集合的清单）。
        ctx.audit.record(&crate::audit::AuditRecord::now(
            &principal.subject,
            crate::audit::AuditAction::ListCollections,
            collections.iter().map(|c| c.id.clone()).collect(),
        ));
        let output = ListCollectionsOutput { collections };
        serde_json::to_value(output).map_err(|e| ToolError::Internal(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::test_support::{
        build_test_ctx_inmem, build_test_ctx_multi_inmem, full_access_principal,
        restricted_principal,
    };

    #[test]
    fn name_is_list_collections() {
        assert_eq!(ListCollectionsTool.name(), "list_collections");
    }

    #[test]
    fn input_schema_takes_no_input() {
        let s = ListCollectionsTool.input_schema();
        assert_eq!(s["properties"], json!({}));
        assert_eq!(s["additionalProperties"], false);
    }

    #[tokio::test]
    async fn legacy_ctx_lists_default_collection() {
        let ctx = build_test_ctx_inmem();
        let v = ListCollectionsTool
            .invoke(json!({}), ctx.clone(), full_access_principal())
            .await
            .expect("ListCollectionsTool 不应失败");
        let arr = v["collections"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "default");
        assert_eq!(arr[0]["doc_count"], json!(0));
        assert!(arr[0]["indexed_at"].is_null());
    }

    /// 信息墙：restricted token 只见授权集合，未授权集合的存在性不泄漏。
    #[tokio::test]
    async fn restricted_principal_sees_only_granted_collections() {
        let ctx = build_test_ctx_multi_inmem();
        let v = ListCollectionsTool
            .invoke(
                json!({}),
                ctx,
                restricted_principal("zhang.san", &["case-a"]),
            )
            .await
            .unwrap();
        let arr = v["collections"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "只应列出授权的 case-a：{arr:?}");
        assert_eq!(arr[0]["id"], "case-a");
        assert_eq!(arr[0]["read_only"], json!(true));
        assert_eq!(arr[0]["subject_kind"], "case");
    }

    #[tokio::test]
    async fn full_access_sees_all_collections() {
        let ctx = build_test_ctx_multi_inmem();
        let v = ListCollectionsTool
            .invoke(json!({}), ctx, full_access_principal())
            .await
            .unwrap();
        let arr = v["collections"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }
}
