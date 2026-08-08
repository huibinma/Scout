//! BETA-36 验收 ③④：daemon 检索留痕（audit.jsonl）。
//!
//! **与 ops tracing 的分工**（spec §7，两套规则各守其职）：
//! - ops tracing log **永不**记 query 内容（BETA-32 spec §6.2 隐私硬规则不变）；
//! - `<data_dir>/audit.jsonl` 是给取证 / 合规的**专用留痕**——默认记 query 明文
//!   （审计取证的核心诉求是"谁在什么时候搜了什么"），`[audit] log_query=false`
//!   可降级为只记 query 长度。audit 文件属于与被检索数据同级的敏感资产，
//!   部署文档要求与 `data_dir` 同权限管控。
//!
//! 形态与桌面 BETA-06 同款：append-only JSONL、每条一行、写后 flush。
//! **写失败不阻断请求**：tracing warn 后继续（检索可用性优先；连续写失败由 ops
//! 通过 warn 日志发现）。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde::Serialize;

/// audit 动作类型。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    /// 检索。
    Search,
    /// 列出授权集合。
    ListCollections,
    /// admin reindex。
    Reindex,
    /// 读取文档内容（BETA-43 `read_document`，全文或片段模式见 `read_mode`）。
    Read,
    /// 越权 / 无权访问被拒（MCP Denied 或 REST 403）。
    Denied,
}

/// 一条留痕记录（序列化为 JSONL 单行）。
#[derive(Debug, Serialize)]
pub struct AuditRecord {
    /// RFC 3339 UTC 时间戳。
    pub ts: String,
    /// 谁（token → subject 映射，验收 ③）。
    pub subject: String,
    /// 动作。
    pub action: AuditAction,
    /// 涉及的 collection id。
    pub collections: Vec<String>,
    /// query 明文（`log_query=true` 时）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// query 长度（`log_query=false` 降级时）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_len: Option<usize>,
    /// 命中数（search 成功时）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub results: Option<usize>,
    /// 拒绝原因（denied 时）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denied_reason: Option<String>,
    /// 被读取的文档路径（read 时，BETA-43 验收 ③——留痕能回答"谁读了哪份材料"）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 读取模式：`full`（全文）或 `snippets`（命中片段，禁全文集合唯一模式）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_mode: Option<String>,
    /// 检索时 FTS 索引是否仍在构建中；仅 true 时落盘，保持旧记录形状不变。
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub indexing_in_progress: bool,
}

impl AuditRecord {
    /// 构造基础记录（ts 取当前 UTC）。
    #[must_use]
    pub fn now(subject: &str, action: AuditAction, collections: Vec<String>) -> Self {
        Self {
            ts: chrono::Utc::now().to_rfc3339(),
            subject: subject.to_string(),
            action,
            collections,
            query: None,
            query_len: None,
            results: None,
            denied_reason: None,
            path: None,
            read_mode: None,
            indexing_in_progress: false,
        }
    }

    /// 按 `log_query` 配置填 query 字段（明文或长度）。
    #[must_use]
    pub fn with_query(mut self, query: &str, log_query: bool) -> Self {
        if log_query {
            self.query = Some(query.to_string());
        } else {
            self.query_len = Some(query.chars().count());
        }
        self
    }

    /// 填命中数。
    #[must_use]
    pub fn with_results(mut self, n: usize) -> Self {
        self.results = Some(n);
        self
    }

    /// 填拒绝原因。
    #[must_use]
    pub fn with_denied_reason(mut self, reason: &str) -> Self {
        self.denied_reason = Some(reason.to_string());
        self
    }

    /// 填被读取的文档路径（BETA-43 read 留痕）。
    #[must_use]
    pub fn with_path(mut self, path: &str) -> Self {
        self.path = Some(path.to_string());
        self
    }

    /// 填读取模式（`full` / `snippets`）。
    #[must_use]
    pub fn with_read_mode(mut self, mode: &str) -> Self {
        self.read_mode = Some(mode.to_string());
        self
    }

    /// 标记检索发生时索引仍在构建中，便于事后诊断空结果是否因索引未完成。
    #[must_use]
    pub fn with_indexing_in_progress(mut self, indexing_in_progress: bool) -> Self {
        self.indexing_in_progress = indexing_in_progress;
        self
    }
}

/// append-only JSONL sink（`<data_dir>/audit.jsonl`）。
///
/// 懒打开：首条记录时才 `create_dir_all` + append-open（test 内存 ctx 不产生
/// audit 时零落盘）。`Mutex<Option<File>>` 串行 append + 每条 flush。
pub struct AuditSink {
    path: PathBuf,
    file: Mutex<Option<File>>,
    /// query 明文开关（`[audit] log_query`）。
    pub log_query: bool,
}

impl std::fmt::Debug for AuditSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditSink")
            .field("path", &self.path)
            .field("log_query", &self.log_query)
            .finish_non_exhaustive()
    }
}

impl AuditSink {
    /// audit 文件名。
    pub const FILE_NAME: &'static str = "audit.jsonl";

    /// 以 `<data_dir>/audit.jsonl` 为落点构造（不立即建文件）。
    #[must_use]
    pub fn new(data_dir: &Path, log_query: bool) -> Self {
        Self {
            path: data_dir.join(Self::FILE_NAME),
            file: Mutex::new(None),
            log_query,
        }
    }

    /// audit 文件路径（`/admin/audit` 读取用）。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 追加一条记录（JSONL 单行 + flush）。写失败 tracing warn、不阻断请求。
    pub fn record(&self, rec: &AuditRecord) {
        let line = match serde_json::to_string(rec) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "audit 记录序列化失败（跳过本条）");
                return;
            }
        };
        let mut guard = self.file.lock();
        if guard.is_none() {
            match self.open_append() {
                Ok(f) => *guard = Some(f),
                Err(e) => {
                    tracing::warn!(error = %e, path = %self.path.display(), "audit 文件打开失败（本条丢失）");
                    return;
                }
            }
        }
        if let Some(f) = guard.as_mut() {
            if let Err(e) = writeln!(f, "{line}").and_then(|()| f.flush()) {
                tracing::warn!(error = %e, path = %self.path.display(), "audit 写入失败（本条丢失）");
                // 下条重试重新打开（文件句柄可能已失效）。
                *guard = None;
            }
        }
    }

    fn open_append(&self) -> std::io::Result<File> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
    }

    /// 读取全部记录（BETA-43 审计导出用）。文件不存在 → 空列表。
    ///
    /// # Errors
    ///
    /// 文件存在但读取失败时返回 IO 错误。
    pub fn read_all(&self) -> std::io::Result<Vec<serde_json::Value>> {
        self.read_tail(usize::MAX)
    }

    /// 读取最近 `tail` 条记录（每条为原始 JSON 值）。文件不存在 → 空列表。
    ///
    /// # Errors
    ///
    /// 文件存在但读取失败时返回 IO 错误。
    pub fn read_tail(&self, tail: usize) -> std::io::Result<Vec<serde_json::Value>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&self.path)?;
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let start = lines.len().saturating_sub(tail);
        Ok(lines[start..]
            .iter()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn record_and_read_tail_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let sink = AuditSink::new(dir.path(), true);
        sink.record(
            &AuditRecord::now("zhang.san", AuditAction::Search, vec!["case-a".into()])
                .with_query("采购合同", true)
                .with_results(3),
        );
        sink.record(
            &AuditRecord::now("li.si", AuditAction::Denied, vec!["case-b".into()])
                .with_denied_reason("collection 'case-b'"),
        );

        let entries = sink.read_tail(10).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["subject"], "zhang.san");
        assert_eq!(entries[0]["action"], "search");
        assert_eq!(entries[0]["query"], "采购合同");
        assert_eq!(entries[0]["results"], 3);
        assert_eq!(entries[1]["action"], "denied");
        assert_eq!(entries[1]["denied_reason"], "collection 'case-b'");
    }

    #[test]
    fn log_query_false_records_len_only() {
        let dir = tempfile::tempdir().unwrap();
        let sink = AuditSink::new(dir.path(), false);
        sink.record(
            &AuditRecord::now("s", AuditAction::Search, vec![]).with_query("机密查询词", false),
        );
        let entries = sink.read_tail(10).unwrap();
        assert!(entries[0].get("query").is_none(), "降级模式不应记明文");
        assert_eq!(entries[0]["query_len"], 5);
    }

    #[test]
    fn read_tail_limits_and_missing_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        let sink = AuditSink::new(dir.path(), true);
        assert!(sink.read_tail(10).unwrap().is_empty(), "无文件应返空");
        for i in 0..5 {
            sink.record(&AuditRecord::now(
                &format!("s{i}"),
                AuditAction::Search,
                vec![],
            ));
        }
        let tail2 = sink.read_tail(2).unwrap();
        assert_eq!(tail2.len(), 2);
        assert_eq!(tail2[0]["subject"], "s3");
        assert_eq!(tail2[1]["subject"], "s4");
    }
}
