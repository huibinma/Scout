//! BETA-06 Audit Log：敏感操作（文件动作）的**持久、可查看、可清除**审计记录。
//!
//! 与 [`crate::tracing`]（开发调试观测，默认 noop / env 开关 / 临时文件 / 路径脱敏）不同：
//! audit log 是**面向用户**的——记录文件操作（open/locate/copy/move/rename）做了什么、对哪些
//! 文件、结果如何，持久到 data_dir，用户可在设置页查看 / 一键清除。本地优先：**永不上传**。
//!
//! 存储 = append-only JSONL（每行一条 JSON）：追加为主、整读展示、一键清，契合审计语义。

// 审计写失败用 eprintln「记录并继续」（绝不让主流程崩，这是有意的 fallback）；
// 文档含 open/locate 等领域词，沿用项目对 doc_markdown 的处理。
#![allow(clippy::print_stderr, clippy::doc_markdown)]

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 审计的文件操作类型（delete 永不执行，不在此列）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOperation {
    /// 打开文件。
    Open,
    /// 在文件管理器中定位。
    Locate,
    /// 复制。
    Copy,
    /// 移动。
    Move,
    /// 重命名。
    Rename,
}

/// 操作结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditResult {
    /// 已执行。
    Executed,
    /// 执行失败。
    Failed,
}

/// 一条审计记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// 记录时间。
    pub timestamp: DateTime<Utc>,
    /// 操作类型。
    pub operation: AuditOperation,
    /// 操作的源路径（本地全路径——用户自有的本地记录、可清除、不上传）。
    pub source_paths: Vec<String>,
    /// copy/move 的目标目录。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    /// rename 的新名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_name: Option<String>,
    /// 结果。
    pub result: AuditResult,
    /// 失败时的错误分类（如 `"PathConflict"`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 持久审计日志：追加、整读、清空。**失败内部记录、绝不让主流程崩**（不返 `Result`）。
pub trait AuditLog: std::fmt::Debug + Send + Sync {
    /// 追加一条记录。
    fn record(&self, entry: &AuditEntry);
    /// 整读全部记录（最旧在前）。
    fn read_all(&self) -> Vec<AuditEntry>;
    /// 清空全部记录。
    fn clear(&self);
}

/// append-only JSONL 文件实现（`data_dir/Scout/audit.jsonl`）。
#[derive(Debug)]
pub struct JsonlAuditLog {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl JsonlAuditLog {
    /// 用日志文件路径构造（父目录按需创建）。
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            write_lock: Mutex::new(()),
        }
    }
}

impl AuditLog for JsonlAuditLog {
    fn record(&self, entry: &AuditEntry) {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(parent) = self.path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("audit: 创建目录失败 {}: {e}", parent.display());
                return;
            }
        }
        let line = match serde_json::to_string(entry) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("audit: 序列化失败: {e}");
                return;
            }
        };
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(mut f) => {
                if let Err(e) = writeln!(f, "{line}") {
                    eprintln!("audit: 写入失败: {e}");
                }
            }
            Err(e) => eprintln!("audit: 打开失败 {}: {e}", self.path.display()),
        }
    }

    fn read_all(&self) -> Vec<AuditEntry> {
        let Ok(file) = std::fs::File::open(&self.path) else {
            return Vec::new(); // 不存在 = 空
        };
        BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter(|l| !l.trim().is_empty())
            // 坏行容错跳过。
            .filter_map(|l| serde_json::from_str(&l).ok())
            .collect()
    }

    fn clear(&self) {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Err(e) = std::fs::remove_file(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("audit: 清除失败: {e}");
            }
        }
    }
}

/// 内存审计日志（测试用）。
#[derive(Debug, Default)]
pub struct InMemoryAuditLog {
    entries: Mutex<Vec<AuditEntry>>,
}

impl AuditLog for InMemoryAuditLog {
    fn record(&self, entry: &AuditEntry) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(entry.clone());
    }

    fn read_all(&self) -> Vec<AuditEntry> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn clear(&self) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn entry(op: AuditOperation, paths: &[&str], result: AuditResult) -> AuditEntry {
        AuditEntry {
            timestamp: Utc::now(),
            operation: op,
            source_paths: paths.iter().map(|s| (*s).to_string()).collect(),
            destination: None,
            new_name: None,
            result,
            error: None,
        }
    }

    #[test]
    fn jsonl_record_read_roundtrip_order_and_cjk() {
        let dir = tempfile::tempdir().unwrap();
        let log = JsonlAuditLog::new(dir.path().join("audit.jsonl"));
        log.record(&entry(
            AuditOperation::Open,
            &["/u/周华健-朋友.mp3"],
            AuditResult::Executed,
        ));
        log.record(&entry(
            AuditOperation::Copy,
            &["/u/a.txt"],
            AuditResult::Executed,
        ));
        let all = log.read_all();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].operation, AuditOperation::Open);
        assert_eq!(all[0].source_paths, vec!["/u/周华健-朋友.mp3".to_string()]);
        assert_eq!(all[1].operation, AuditOperation::Copy);
    }

    #[test]
    fn jsonl_clear_empties() {
        let dir = tempfile::tempdir().unwrap();
        let log = JsonlAuditLog::new(dir.path().join("audit.jsonl"));
        log.record(&entry(AuditOperation::Open, &["/a"], AuditResult::Executed));
        assert_eq!(log.read_all().len(), 1);
        log.clear();
        assert!(log.read_all().is_empty());
        // clear 不存在的文件不报错。
        log.clear();
    }

    #[test]
    fn jsonl_skips_corrupt_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = JsonlAuditLog::new(path.clone());
        log.record(&entry(AuditOperation::Open, &["/a"], AuditResult::Executed));
        // 手插一行非法 JSON。
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "这不是合法 JSON").unwrap();
        log.record(&entry(AuditOperation::Locate, &["/b"], AuditResult::Failed));
        let all = log.read_all();
        assert_eq!(all.len(), 2, "坏行应被跳过，合法行保留");
        assert_eq!(all[1].operation, AuditOperation::Locate);
    }

    #[test]
    fn entry_serde_roundtrip_with_optional_fields() {
        let e = AuditEntry {
            timestamp: Utc::now(),
            operation: AuditOperation::Move,
            source_paths: vec!["/a".into(), "/b".into()],
            destination: Some("/dest".into()),
            new_name: None,
            result: AuditResult::Failed,
            error: Some("PathConflict".into()),
        };
        let s = serde_json::to_string(&e).unwrap();
        let back: AuditEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn in_memory_record_read_clear() {
        let log = InMemoryAuditLog::default();
        log.record(&entry(AuditOperation::Open, &["/a"], AuditResult::Executed));
        log.record(&entry(AuditOperation::Rename, &["/b"], AuditResult::Failed));
        assert_eq!(log.read_all().len(), 2);
        log.clear();
        assert!(log.read_all().is_empty());
    }
}
