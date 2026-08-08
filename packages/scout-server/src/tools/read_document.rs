//! `read_document` tool —— 读取命中文档内容，受 collection 级 `allow_full_read`
//! 权限闸门约束（BETA-43 验收 ②）。
//!
//! 两种模式：
//! - **片段模式**（`full=false`，缺省）：必带 `query`，仅返回关键词命中片段 +
//!   有限上下文窗口 + 命中页摘录——**绝不吐全文**。任何 collection 都可用。
//! - **全文模式**（`full=true`）：仅当该 collection 配置 `allow_full_read=true`；
//!   否则 [`ToolError::Denied`] + audit denied 留痕（验收 ④）。
//!
//! 内容一律来自**索引 db**（`documents_fts.body` / `document_passages`），不读磁盘
//! 原文件——读取面与检索面同边界，roots 之外的文件天然不可达。
//! 每次成功读取记 audit `read` 动作（subject + collection + path + 模式）。

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{Tool, ToolError};
use crate::audit::{AuditAction, AuditRecord};
use crate::auth::AuthedPrincipal;
use crate::config::ServerCtx;
use crate::provenance::{
    matching_page_excerpts, query_terms, snippet_windows, PageHit, MAX_PAGES, MAX_READ_WINDOWS,
    READ_CONTEXT_CHARS,
};

/// 全文模式返回正文的字符上限（防超大文档冲爆 MCP 响应；超限截断并置 `truncated`）。
pub const FULL_BODY_CHAR_CAP: usize = 200_000;

#[derive(Deserialize)]
struct ReadDocumentInput {
    /// 目标文档路径（`search` 命中的 `path` 字段原样传入）。
    path: String,
    /// 文档所属 collection id（`search` 命中的 `collection` 字段原样传入）。
    collection: String,
    /// 片段模式的定位查询（找哪些词的上下文）；全文模式可省略。
    #[serde(default)]
    query: Option<String>,
    /// true = 请求全文（需该 collection `allow_full_read=true`）。
    #[serde(default)]
    full: bool,
}

#[derive(Serialize)]
struct ReadDocumentOutput {
    path: String,
    collection: String,
    doc_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_count: Option<u32>,
    /// 实际生效的模式（`full` / `snippets`）。
    mode: &'static str,
    /// 全文（仅 `mode=full`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    /// 全文被 [`FULL_BODY_CHAR_CAP`] 截断时 true。
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
    /// 命中片段（仅 `mode=snippets`；含上下文窗口、被裁边带 `…`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    snippets: Option<Vec<String>>,
    /// 命中页摘录（仅 `mode=snippets` 且为扫描件；页号起于 1）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pages: Option<Vec<PageHit>>,
}

/// `read_document` tool。
#[derive(Debug)]
pub struct ReadDocumentTool;

#[async_trait]
impl Tool for ReadDocumentTool {
    fn name(&self) -> &'static str {
        "read_document"
    }

    fn description(&self) -> &'static str {
        "Read an indexed document's content. Default mode returns only query-matched \
         snippets with limited context; full-text mode requires the collection's \
         allow_full_read policy."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "search 命中的 path 字段原样传入"},
                "collection": {"type": "string", "description": "search 命中的 collection 字段原样传入"},
                "query": {"type": "string", "description": "片段模式定位查询（full=false 时必填）"},
                "full": {
                    "type": "boolean",
                    "default": false,
                    "description": "true 请求全文；需该 collection 配置 allow_full_read=true，否则拒绝"
                }
            },
            "required": ["path", "collection"]
        })
    }

    #[tracing::instrument(skip(self, args, ctx, principal), fields(subject = %principal.subject))]
    async fn invoke(
        &self,
        args: Value,
        ctx: Arc<ServerCtx>,
        principal: Arc<AuthedPrincipal>,
    ) -> Result<Value, ToolError> {
        let input: ReadDocumentInput =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        if input.path.trim().is_empty() {
            return Err(ToolError::InvalidParams("path 不能为空".into()));
        }

        // collection 授权：未授权与不存在同文案（防探测，与 search 一致）+ denied 留痕。
        let cid = &input.collection;
        if !principal.can_access(cid) || !ctx.collections.contains_key(cid) {
            let reason = format!("collection '{cid}'");
            ctx.audit.record(
                &AuditRecord::now(&principal.subject, AuditAction::Denied, vec![cid.clone()])
                    .with_path(&input.path)
                    .with_denied_reason(&reason),
            );
            return Err(ToolError::Denied(reason));
        }
        let rt = ctx
            .collection(cid)
            .ok_or_else(|| ToolError::Internal("collection runtime 缺失".into()))?;

        // 全文闸门（验收 ②/④）：禁全文集合的 full 请求 → Denied + denied 留痕。
        if input.full && !rt.meta.allow_full_read {
            let reason = format!("full read disabled for collection '{cid}'");
            ctx.audit.record(
                &AuditRecord::now(&principal.subject, AuditAction::Denied, vec![cid.clone()])
                    .with_path(&input.path)
                    .with_denied_reason(&reason),
            );
            return Err(ToolError::Denied(reason));
        }

        // 取索引内正文（只读 db，不触磁盘原文件）。search 命中的 path 是
        // canonicalize 后形态、documents.path 存原始扫描路径 → 逐候选尝试。
        let (entry, body, passages) = {
            let docs = rt.document_index.lock();
            let mut found = None;
            for cand in crate::provenance::lookup_candidates(&input.path) {
                let preview = docs
                    .preview_for_path(&cand, None)
                    .map_err(|e| ToolError::Internal(e.to_string()))?;
                if let Some(p) = preview {
                    let passages = docs
                        .passages_for_doc(&cand)
                        .map_err(|e| ToolError::Internal(e.to_string()))?;
                    found = Some((p.entry, p.body, passages));
                    break;
                }
            }
            // caller 已有该 collection 权限，"不在索引中"不构成泄漏。
            found.ok_or_else(|| {
                ToolError::InvalidParams(format!("document not found in collection '{cid}'"))
            })?
        };

        let output = if input.full {
            let total = body.chars().count();
            let truncated = total > FULL_BODY_CHAR_CAP;
            let body_out: String = if truncated {
                body.chars().take(FULL_BODY_CHAR_CAP).collect()
            } else {
                body
            };
            ReadDocumentOutput {
                path: input.path.clone(),
                collection: cid.clone(),
                doc_type: entry.doc_type,
                page_count: entry.page_count,
                mode: "full",
                body: Some(body_out),
                truncated,
                snippets: None,
                pages: None,
            }
        } else {
            let query = input
                .query
                .as_deref()
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .ok_or_else(|| {
                    ToolError::InvalidParams(
                        "片段模式需要 query 参数（禁全文集合仅返回命中片段）".into(),
                    )
                })?;
            let intent = scout_intent_parser::parse(query);
            let expanded = super::search::expand_intent_for_daemon(intent);
            let terms = query_terms(query, &expanded.keyword_groups);
            let snippets = snippet_windows(&body, &terms, MAX_READ_WINDOWS, READ_CONTEXT_CHARS);
            let pages = matching_page_excerpts(&passages, &terms, MAX_PAGES, READ_CONTEXT_CHARS);
            ReadDocumentOutput {
                path: input.path.clone(),
                collection: cid.clone(),
                doc_type: entry.doc_type,
                page_count: entry.page_count,
                mode: "snippets",
                body: None,
                truncated: false,
                snippets: Some(snippets),
                pages: if pages.is_empty() { None } else { Some(pages) },
            }
        };

        // 读取留痕（验收 ③：谁读了哪份材料、什么模式）。
        ctx.audit.record(
            &AuditRecord::now(&principal.subject, AuditAction::Read, vec![cid.clone()])
                .with_path(&input.path)
                .with_read_mode(output.mode),
        );

        serde_json::to_value(output).map_err(|e| ToolError::Internal(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::test_support::{
        build_test_ctx_inmem, build_test_ctx_multi_inmem, full_access_principal,
        restricted_principal,
    };

    #[test]
    fn input_schema_requires_path_and_collection() {
        let s = ReadDocumentTool.input_schema();
        assert_eq!(s["required"], json!(["path", "collection"]));
        assert_eq!(s["properties"]["full"]["default"], json!(false));
    }

    #[tokio::test]
    async fn unauthorized_collection_denied_same_shape_as_search() {
        let ctx = build_test_ctx_multi_inmem();
        let err = ReadDocumentTool
            .invoke(
                json!({"path": "/x/a.txt", "collection": "case-b", "query": "合同"}),
                ctx,
                restricted_principal("zhang.san", &["case-a"]),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Denied(msg) => assert_eq!(msg, "collection 'case-b'"),
            other => panic!("应返 Denied，实得：{other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_collection_denied_no_existence_leak() {
        let ctx = build_test_ctx_multi_inmem();
        let err = ReadDocumentTool
            .invoke(
                json!({"path": "/x/a.txt", "collection": "ghost", "query": "x"}),
                ctx,
                full_access_principal(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Denied(msg) => assert_eq!(msg, "collection 'ghost'"),
            other => panic!("应返 Denied，实得：{other:?}"),
        }
    }

    /// multi ctx 的 collection（TOML 姿态合成）缺省禁全文 → full=true 被拒且
    /// 拒绝先于"文档不存在"判定（不泄漏索引内容）。
    #[tokio::test]
    async fn full_read_denied_when_policy_disallows() {
        let ctx = build_test_ctx_multi_inmem();
        assert!(!ctx.collections["case-a"].meta.allow_full_read);
        let err = ReadDocumentTool
            .invoke(
                json!({"path": "/x/a.txt", "collection": "case-a", "full": true}),
                ctx,
                full_access_principal(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Denied(msg) => {
                assert_eq!(msg, "full read disabled for collection 'case-a'");
            }
            other => panic!("应返 Denied，实得：{other:?}"),
        }
    }

    #[tokio::test]
    async fn snippet_mode_requires_query() {
        // legacy inmem ctx：default collection allow_full_read=true，但片段模式仍需 query。
        let ctx = build_test_ctx_inmem();
        let err = ReadDocumentTool
            .invoke(
                json!({"path": "/x/a.txt", "collection": "default"}),
                ctx,
                full_access_principal(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn missing_document_reports_not_found() {
        let ctx = build_test_ctx_inmem();
        let err = ReadDocumentTool
            .invoke(
                json!({"path": "/nope.txt", "collection": "default", "query": "x"}),
                ctx,
                full_access_principal(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidParams(msg) => {
                assert!(msg.contains("not found"), "{msg}");
            }
            other => panic!("应返 InvalidParams，实得：{other:?}"),
        }
    }
}
