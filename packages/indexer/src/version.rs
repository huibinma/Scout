//! 索引 schema 版本（BETA-32 C1b）：在 `schema_meta(key, value)` 表里维护一行
//! `key='version'`。daemon 启动时读出比对 [`INDEXER_SCHEMA_VERSION`]，不匹配则
//! fail-fast 要求 `--allow-rebuild-schema` 显式重建（实际比对逻辑在后续 T10
//! preflight 中实现）。
//!
//! 桌面 app 路径透明升级：[`MusicIndex`](crate::MusicIndex) / [`DocumentIndex`](crate::DocumentIndex)
//! 的 `open` 在 schema execute 完后调用 [`ensure_schema_version`]，老 db 第一次打开
//! 就会 INSERT 进当前版本，对现有数据 / 查询零影响。

use rusqlite::Connection;

use crate::IndexError;

/// 索引 SQLite schema 版本。**增 schema 字段 / 表时必须 bump**（同步改 daemon
/// preflight、桌面 app 迁移路径）。
pub const INDEXER_SCHEMA_VERSION: &str = "1";

/// 保证 `schema_meta` 表中存在 `key='version'` 一行（不覆盖已有 value）。
/// 由 [`MusicIndex`](crate::MusicIndex) / [`DocumentIndex`](crate::DocumentIndex)
/// 在 `open` 时调用，老 db 初次打开 → INSERT 写入；后续打开 / 已有 version 行
/// → no-op（`INSERT OR IGNORE`）。
pub fn ensure_schema_version(conn: &Connection) -> Result<(), IndexError> {
    conn.execute(
        "INSERT OR IGNORE INTO schema_meta(key, value) VALUES('version', ?1)",
        [INDEXER_SCHEMA_VERSION],
    )?;
    Ok(())
}

/// 读 `schema_meta` 表的 `version` 值。表存在但无该行 → `None`；表不存在调用方
/// 自行处理 `IndexError::Db`（daemon preflight 视作非法 db / 要求 rebuild）。
pub fn read_schema_version(conn: &Connection) -> Result<Option<String>, IndexError> {
    let mut stmt = conn.prepare("SELECT value FROM schema_meta WHERE key='version'")?;
    let mut rows = stmt.query([])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get::<_, String>(0)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn fresh() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute(
            "CREATE TABLE schema_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            [],
        )
        .unwrap();
        c
    }

    #[test]
    fn ensure_then_read_returns_current_version() {
        let c = fresh();
        ensure_schema_version(&c).unwrap();
        assert_eq!(
            read_schema_version(&c).unwrap().as_deref(),
            Some(INDEXER_SCHEMA_VERSION)
        );
    }

    #[test]
    fn read_returns_none_when_empty() {
        let c = fresh();
        assert!(read_schema_version(&c).unwrap().is_none());
    }

    #[test]
    fn ensure_is_idempotent_and_preserves_existing_value() {
        // 已有非当前版本的旧 row（模拟未来 bump 后老 db）→ ensure 不覆盖、保留旧值。
        let c = fresh();
        c.execute(
            "INSERT INTO schema_meta(key, value) VALUES('version', '0')",
            [],
        )
        .unwrap();
        ensure_schema_version(&c).unwrap();
        assert_eq!(read_schema_version(&c).unwrap().as_deref(), Some("0"));
    }
}
