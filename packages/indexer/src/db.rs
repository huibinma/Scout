//! 存储层：SQLite + FTS5（[`rusqlite`] bundled）。
//!
//! FTS 设计说明：`music_fts` 采用**独立** FTS5 表（非 `content=` external-content），
//! 其 `rowid` 与 `music.id` 手动对齐。external-content 表的删除需借助特殊
//! `'delete'` 命令、易出错；独立表 `DELETE FROM music_fts WHERE rowid=?` 直接可用，
//! 仅多存一份 artist/title/album 文本（音乐 metadata 量级可忽略）。
//!
//! tokenizer 用 **`trigram`**（非 `unicode61`）：`unicode61` 把连续 CJK 当单个 token，
//! 子串/前缀搜不到中文片段（BETA-04 暴露）；`trigram` 支持任意 **≥3 字符**子串匹配
//! （CJK + 英文，默认大小写不敏感）。代价：<3 字符查询无法命中（trigram 固有限制）——
//! BETA-56 为此补 **短查询 metadata LIKE 兜底**（见 [`short_metadata_like_terms`]）：纯 <3
//! 字符查询改走 LIKE 子串匹配 metadata 列（music: artist/title/album/file_name；
//! documents: title/author/file_name），让 2 字人名/常用词也能命中元数据（正文不扫）。

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::types::ToSql;
use rusqlite::{named_params, params, Connection, OptionalExtension};

use crate::model::{MusicEntry, MusicQuery};
use crate::scan::IncrementalStore;
use crate::version::ensure_schema_version;
use crate::IndexError;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS music (
  id            INTEGER PRIMARY KEY,
  path          TEXT NOT NULL UNIQUE,
  file_name     TEXT NOT NULL,
  artist        TEXT,
  title         TEXT,
  album         TEXT,
  duration_secs REAL,
  format        TEXT,
  bitrate       INTEGER,
  modified_time INTEGER NOT NULL,
  indexed_time  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_music_modified ON music(modified_time);
CREATE VIRTUAL TABLE IF NOT EXISTS music_fts USING fts5(
  artist, title, album, file_name,
  tokenize='trigram'
);
CREATE TABLE IF NOT EXISTS schema_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
";

/// 一键清除整个索引库（BETA-21）：显式 `DROP` 全部索引数据表（含向量、OCR 段落、
/// 失败留痕）及 FTS5 影子表后 `VACUUM` 回收磁盘，让 index.db 文件**真正缩小**。
/// 表结构下次 `MusicIndex`/`DocumentIndex::open` 时自动重建。
///
/// 为何用 `DROP` 而非 `DELETE`：本库的 `music_fts`/`documents_fts` 是带 content 的 FTS5 表，
/// `DELETE` 只写 tombstone 删除标记、倒排段（`*_fts_data`）不减反增、`VACUUM` 回收不掉；而
/// 官方 `'delete-all'` 快捷命令仅支持 contentless/external 表。`DROP TABLE` 会连带删除 FTS5 的
/// 全部影子表，是唯一能彻底回收磁盘的方式。全程走 SQL 连接，**绕开 Windows 删文件的独占锁**
/// （app 自身持有 db 句柄时 `remove_file` 会失败，但 SQL 写操作可经新连接执行）。
pub fn clear_index(db_path: &Path) -> Result<(), IndexError> {
    let conn = Connection::open(db_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    configure_file_db_pragmas(&conn)?;
    // 子表必须显式 DROP，不能依赖 documents 的 ON DELETE CASCADE：
    // SQLite foreign_keys 默认关闭，而本清理连接不应靠隐含连接状态决定隐私数据是否删除。
    // 若只 DROP documents，旧 document_vectors/doc_id 还可能在重建后错误挂到复用 id 的新文档。
    // VACUUM 不可在事务内执行；execute_batch 不包显式事务，逐句 autocommit，故与 DROP 分批执行。
    // schema_meta 故意保留：它描述 db schema 代数（不是索引内容），clear 数据不改 schema。
    conn.execute_batch(
        "DROP TABLE IF EXISTS document_failed_pages;
         DROP TABLE IF EXISTS document_passages;
         DROP TABLE IF EXISTS document_vectors;
         DROP TABLE IF EXISTS index_failures;
         DROP TABLE IF EXISTS music_fts;
         DROP TABLE IF EXISTS music;
         DROP TABLE IF EXISTS documents_fts;
         DROP TABLE IF EXISTS documents;",
    )?;
    conn.execute_batch("VACUUM;")?;
    Ok(())
}

/// 音乐 metadata 索引（持有一个 SQLite 连接）。
pub(crate) fn configure_file_db_pragmas(conn: &Connection) -> Result<(), IndexError> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    configure_common_db_pragmas(conn)
}

pub(crate) fn configure_common_db_pragmas(conn: &Connection) -> Result<(), IndexError> {
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

#[derive(Debug)]
pub struct MusicIndex {
    conn: Connection,
}

/// BETA-33 cycle 5：某 root 子树下的音乐索引统计。
///
/// `total` = 该 root 下音乐条数；
/// `last_indexed_time` = 最近一次 indexed_time（Unix 秒；无记录 → None）。
///
/// 与 [`crate::DocRootStats`] 平行、桌面「选项 → 索引」pane 一起渲染。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MusicRootStats {
    pub total: u64,
    pub last_indexed_time: Option<i64>,
}

impl MusicIndex {
    /// 打开（或创建）索引数据库并建表。
    pub fn open(db_path: &Path) -> Result<Self, IndexError> {
        let conn = Connection::open(db_path)?;
        Self::from_conn(conn, true)
    }

    /// 内存库（测试用）。
    pub fn open_in_memory() -> Result<Self, IndexError> {
        let conn = Connection::open_in_memory()?;
        Self::from_conn(conn, false)
    }

    fn from_conn(conn: Connection, file_backed: bool) -> Result<Self, IndexError> {
        // reindex 写与 search 读可能并发（BETA-04），给锁等待留 5s 窗口。
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        if file_backed {
            configure_file_db_pragmas(&conn)?;
        } else {
            configure_common_db_pragmas(&conn)?;
        }
        conn.execute_batch(SCHEMA)?;
        migrate_music_fts(&conn)?;
        // BETA-32 C1b：老 db 第一次打开 → INSERT schema 版本；已有则 no-op。
        ensure_schema_version(&conn)?;
        Ok(Self { conn })
    }

    /// 记录总数。
    pub fn count(&self) -> Result<u64, IndexError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM music", [], |r| r.get(0))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// 2026-07-29：FTS5 索引维护——把多个 segment b-tree 合并成一个。真机复盘（14570
    /// 文档索引，旧库比全新库慢 8 倍）确认到的一个具体机制：`music_fts`/`documents_fts`
    /// 此前只在"一键清空索引"（`clear_index`，DROP + VACUUM）时才有等价的整理动作，
    /// 日常增删从不 optimize——trigram 分词器对长期反复增删（月级开发测试机的典型场景）
    /// 积累的 segment 碎片格外敏感，写入会越来越慢。调用方应只在本轮确有变更
    /// （added/updated/removed 之和 > 0）时调用，避免空轮跑无谓 I/O。
    pub fn optimize_fts(&self) -> Result<(), IndexError> {
        self.conn
            .execute_batch("INSERT INTO music_fts(music_fts) VALUES('optimize');")?;
        Ok(())
    }

    /// BETA-64 T7：全表 `(path, modified_time)`，无 root 过滤。供发现层
    /// （[`crate::scan`] 的 `MusicIndex::index_paths`，Everything/Spotlight 枚举出的显式
    /// 路径列表）一次性批量预取 mtime，替代逐路径 `modified_time_of` 查询——发现层拿到的
    /// 路径本就可能跨多个 / 甚至没有 root 前缀（全盘枚举），没有 `modified_times_under`
    /// 那样的 root 可过滤，直接整表读；调用场景本就是"全盘规模"文件量，这次全表扫描
    /// 换掉的是同等规模的逐路径查询，净省往返次数。
    pub(crate) fn all_modified_times(&self) -> Result<HashMap<String, i64>, IndexError> {
        let mut stmt = self.conn.prepare("SELECT path, modified_time FROM music")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (p, mt) = row?;
            out.insert(p, mt);
        }
        Ok(out)
    }

    /// BETA-33 cycle 5：某 root 子树下的音乐索引统计（总数 + 上次索引时间）。
    /// 单一 SQL 一次查完；子树判定 = `path == root` OR `path GLOB root+'/*'` OR
    /// `path GLOB root+'\*'`（同时支持 Windows 和 Unix 分隔符）。
    /// 空 root（无匹配）→ `(0, None)`。
    pub fn stats_under_root(&self, root: &str) -> Result<MusicRootStats, IndexError> {
        // cycle 7-c：边界谓词抽到 root_glob_predicate/params，与 purge_under_root 共用同一口径。
        let p = root_glob_params(root);
        let sql = format!(
            "SELECT COUNT(*), MAX(indexed_time) FROM music WHERE {}",
            root_glob_predicate("path")
        );
        let (total, last_indexed): (i64, Option<i64>) =
            self.conn
                .query_row(&sql, rusqlite::params![p[0], p[1], p[2]], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
        Ok(MusicRootStats {
            total: u64::try_from(total).unwrap_or(0),
            last_indexed_time: last_indexed,
        })
    }

    /// BETA-33 cycle 7-c：清除 root 子树下所有音乐条目（同事务内同步删 `music_fts`）。
    /// 返回删除条数。边界口径与 [`Self::stats_under_root`] 共用 [`root_glob_predicate`]——
    /// 概貌统计到的条目就是会被清除的条目。**只删 Scout 数据库缓存，不碰磁盘文件。**
    pub fn purge_under_root(&self, root: &str) -> Result<u64, IndexError> {
        let p = root_glob_params(root);
        let pred = root_glob_predicate("path");
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            &format!("DELETE FROM music_fts WHERE rowid IN (SELECT id FROM music WHERE {pred})"),
            rusqlite::params![p[0], p[1], p[2]],
        )?;
        let n = tx.execute(
            &format!("DELETE FROM music WHERE {pred}"),
            rusqlite::params![p[0], p[1], p[2]],
        )?;
        tx.commit()?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// 回收（BETA-07）：删除磁盘上**已不存在**的记录（含 FTS）。返回删除数。
    /// 用 `Path::exists()` 判定（非发现集）——OneDrive 占位符路径存在不误删、发现遗漏也不误删。
    pub fn prune_deleted(&self) -> Result<u64, IndexError> {
        let paths: Vec<String> = {
            let mut stmt = self.conn.prepare("SELECT path FROM music")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut removed = 0u64;
        for path in paths {
            if !Path::new(&path).exists() && self.delete_by_path(&path)? {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// [`MusicIndex::upsert_entry`] / [`MusicIndex::upsert_entries`]（BETA-64 T2）共用的核心
/// SQL 逻辑：在调用方已开的事务 `tx` 内插入或更新一条记录 + 同步 `music_fts`，不负责
/// 开启/提交事务（由调用方决定是每条各自一个事务、还是一批共用一个事务）。
fn upsert_music_entry_tx(
    tx: &rusqlite::Transaction<'_>,
    e: &MusicEntry,
) -> Result<bool, IndexError> {
    let now = unix_now();

    let existing: Option<i64> = tx
        .query_row("SELECT id FROM music WHERE path = ?1", [&e.path], |r| {
            r.get(0)
        })
        .optional()?;

    let id = if let Some(id) = existing {
        tx.execute(
            "UPDATE music SET file_name=?2, artist=?3, title=?4, album=?5,
                 duration_secs=?6, format=?7, bitrate=?8, modified_time=?9, indexed_time=?10
             WHERE id=?1",
            params![
                id,
                e.file_name,
                e.artist,
                e.title,
                e.album,
                e.duration_secs,
                e.format,
                e.bitrate,
                e.modified_time,
                now
            ],
        )?;
        id
    } else {
        tx.execute(
            "INSERT INTO music
                 (path, file_name, artist, title, album, duration_secs, format, bitrate, modified_time, indexed_time)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                e.path,
                e.file_name,
                e.artist,
                e.title,
                e.album,
                e.duration_secs,
                e.format,
                e.bitrate,
                e.modified_time,
                now
            ],
        )?;
        tx.last_insert_rowid()
    };

    tx.execute("DELETE FROM music_fts WHERE rowid = ?1", [id])?;
    tx.execute(
        "INSERT INTO music_fts(rowid, artist, title, album, file_name) VALUES (?1,?2,?3,?4,?5)",
        params![id, e.artist, e.title, e.album, e.file_name],
    )?;
    Ok(existing.is_none())
}

impl IncrementalStore for MusicIndex {
    type Entry = MusicEntry;

    /// 插入或更新一条记录。返回 `true` 表示新增、`false` 表示更新（mtime 变化）。
    /// 同事务内同步 `music_fts`。`music.id` 跨更新保持稳定（用 UPDATE 而非 REPLACE），
    /// 以维持 FTS rowid 对齐。
    fn upsert_entry(&self, e: &MusicEntry) -> Result<bool, IndexError> {
        let tx = self.conn.unchecked_transaction()?;
        let result = upsert_music_entry_tx(&tx, e)?;
        tx.commit()?;
        Ok(result)
    }

    /// BETA-64 T2：单事务批量 upsert——把 `entries.len()` 次 commit 收敛为 1 次，
    /// 减少大批量索引（如首次全量索引上万文件）时逐文件事务提交的固定开销。
    /// SQL 逻辑与 [`Self::upsert_entry`] 逐字节一致（共用 [`upsert_music_entry_tx`]），
    /// 仅事务边界不同；任一条目写入失败整批回滚（与单条各自独立提交相比的行为差异，
    /// 见调用点 `scan.rs` 注释）。
    fn upsert_entries(&self, entries: &[MusicEntry]) -> Result<Vec<bool>, IndexError> {
        let tx = self.conn.unchecked_transaction()?;
        let mut results = Vec::with_capacity(entries.len());
        for e in entries {
            results.push(upsert_music_entry_tx(&tx, e)?);
        }
        tx.commit()?;
        Ok(results)
    }

    /// 按 path 删除一条记录（含 FTS）。返回是否删到了行。
    fn delete_by_path(&self, path: &str) -> Result<bool, IndexError> {
        let tx = self.conn.unchecked_transaction()?;
        let id: Option<i64> = tx
            .query_row("SELECT id FROM music WHERE path = ?1", [path], |r| r.get(0))
            .optional()?;
        let Some(id) = id else {
            return Ok(false);
        };
        tx.execute("DELETE FROM music_fts WHERE rowid = ?1", [id])?;
        tx.execute("DELETE FROM music WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(true)
    }

    /// 取某 path 的 `modified_time`（增量比对用）；不存在返回 `None`。
    fn modified_time_of(&self, path: &str) -> Result<Option<i64>, IndexError> {
        let mt = self
            .conn
            .query_row(
                "SELECT modified_time FROM music WHERE path = ?1",
                [path],
                |r| r.get(0),
            )
            .optional()?;
        Ok(mt)
    }

    /// 取索引中所有 path 落在 `roots` 任一子树下的记录路径（增量删除回收用）。
    fn paths_under(&self, roots: &[String]) -> Result<Vec<String>, IndexError> {
        let mut stmt = self.conn.prepare("SELECT path FROM music")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for p in rows {
            let p = p?;
            if roots.iter().any(|root| path_is_under(&p, root)) {
                out.push(p);
            }
        }
        Ok(out)
    }

    /// BETA-64 T7a：单条 SQL 一次性批量取 `(path, modified_time)`，覆写 trait 默认的
    /// 逐文件 `modified_time_of` 回退——与 `paths_under` 同一张表同一次全表扫描，
    /// 只是多带一列，SQL 侧零额外成本。
    fn modified_times_under(&self, roots: &[String]) -> Result<HashMap<String, i64>, IndexError> {
        let mut stmt = self.conn.prepare("SELECT path, modified_time FROM music")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (p, mt) = row?;
            if roots.iter().any(|root| path_is_under(&p, root)) {
                out.insert(p, mt);
            }
        }
        Ok(out)
    }
}

impl MusicIndex {
    /// 查询（结构化过滤 + 可选 FTS 文本）。
    pub fn query(&self, q: &MusicQuery) -> Result<Vec<MusicEntry>, IndexError> {
        let limit = i64::from(q.limit.unwrap_or(50));
        // 结构化过滤公共片段（用 `:param IS NULL OR ...` 让缺省参数匹配全部）。
        let filters = "(:artist IS NULL OR m.artist LIKE '%' || :artist || '%')
             AND (:album IS NULL OR m.album LIKE '%' || :album || '%')
             AND (:format IS NULL OR m.format = :format COLLATE NOCASE)";
        let select = "SELECT m.path, m.file_name, m.artist, m.title, m.album,
                             m.duration_secs, m.format, m.bitrate, m.modified_time
                      FROM music m";

        // fts_match（原始 FTS5 表达式）优先；否则把 text 经 fts_sanitize 包成单 phrase。
        let match_expr = q
            .fts_match
            .clone()
            .or_else(|| q.text.as_deref().map(fts_sanitize));

        // BETA-56 短查询 metadata LIKE 兜底（与 `documents_fts` 同理：`music_fts` 也是 trigram，
        // <3 字符查询 0 命中）。无 fts_match 且 text 全为 <3 字符纯 alnum/CJK → LIKE 子串匹配
        // artist/title/album/file_name（判据见 [`short_metadata_like_terms`]）。
        let like_terms = if q.fts_match.is_none() {
            short_metadata_like_terms(q.text.as_deref())
        } else {
            Vec::new()
        };

        let rows = if !like_terms.is_empty() {
            // 短词全为 alnum/CJK、不含 LIKE 元字符，直接两端加 `%` 作子串模式。
            let like_patterns: Vec<String> = like_terms.iter().map(|t| format!("%{t}%")).collect();
            let like_keys: Vec<String> = (0..like_terms.len()).map(|i| format!(":lk{i}")).collect();
            let like_clause = like_keys
                .iter()
                .map(|k| {
                    format!(
                        "(m.artist LIKE {k} OR m.title LIKE {k} OR m.album LIKE {k} OR m.file_name LIKE {k})"
                    )
                })
                .collect::<Vec<_>>()
                .join(" AND ");
            let sql = format!(
                "{select} WHERE {like_clause} AND {filters} ORDER BY m.artist, m.title LIMIT :limit"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let mut bound: Vec<(&str, &dyn ToSql)> = vec![
                (":artist", &q.artist),
                (":album", &q.album),
                (":format", &q.format),
                (":limit", &limit),
            ];
            for (k, v) in like_keys.iter().zip(&like_patterns) {
                bound.push((k.as_str(), v));
            }
            let rows = stmt
                .query_map(&bound[..], row_to_entry)?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        } else if let Some(sanitized) = match_expr {
            let sql = format!(
                "{select} JOIN music_fts f ON f.rowid = m.id
                 WHERE music_fts MATCH :match AND {filters}
                 ORDER BY m.artist, m.title LIMIT :limit"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt
                .query_map(
                    named_params! {
                        ":match": sanitized,
                        ":artist": q.artist,
                        ":album": q.album,
                        ":format": q.format,
                        ":limit": limit,
                    },
                    row_to_entry,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        } else {
            let sql = format!("{select} WHERE {filters} ORDER BY m.artist, m.title LIMIT :limit");
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt
                .query_map(
                    named_params! {
                        ":artist": q.artist,
                        ":album": q.album,
                        ":format": q.format,
                        ":limit": limit,
                    },
                    row_to_entry,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        Ok(rows)
    }

    /// 按绝对路径取单条音乐记录（BETA-20 预览面板）。无匹配 → `None`。
    pub fn entry_for_path(&self, path: &str) -> Result<Option<MusicEntry>, IndexError> {
        let entry = self
            .conn
            .query_row(
                "SELECT m.path, m.file_name, m.artist, m.title, m.album,
                        m.duration_secs, m.format, m.bitrate, m.modified_time
                 FROM music m WHERE m.path = ?1",
                [path],
                row_to_entry,
            )
            .optional()?;
        Ok(entry)
    }
}

/// 旧库迁移（BETA-01A）：`music_fts` 缺 `file_name` 列（建库时为 3 列）→ drop + 按新 4 列
/// schema 重建，**从 music 主表重填**（不重读文件，秒级）。新库 / 已迁移库为 no-op。
fn migrate_music_fts(conn: &Connection) -> Result<(), IndexError> {
    if music_fts_has_file_name(conn)? {
        return Ok(());
    }
    conn.execute_batch(
        "DROP TABLE music_fts;
         CREATE VIRTUAL TABLE music_fts USING fts5(artist, title, album, file_name, tokenize='trigram');
         INSERT INTO music_fts(rowid, artist, title, album, file_name)
           SELECT id, artist, title, album, file_name FROM music;",
    )?;
    Ok(())
}

fn music_fts_has_file_name(conn: &Connection) -> Result<bool, IndexError> {
    let mut stmt = conn.prepare("PRAGMA table_info(music_fts)")?;
    // table_info 第 1 列（index 1）是列名。
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for name in names {
        if name? == "file_name" {
            return Ok(true);
        }
    }
    Ok(false)
}

fn row_to_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<MusicEntry> {
    Ok(MusicEntry {
        path: r.get(0)?,
        file_name: r.get(1)?,
        artist: r.get(2)?,
        title: r.get(3)?,
        album: r.get(4)?,
        duration_secs: r.get(5)?,
        format: r.get(6)?,
        bitrate: r.get(7)?,
        modified_time: r.get(8)?,
    })
}

/// 把任意用户文本转成单个合法 FTS5 查询：包成双引号短语、内部 `"` 转义为 `""`。
/// 杜绝 FTS5 语法错误 / 注入。trigram tokenizer 下，引号短语即做子串匹配（无需 `*`）；
/// <3 字符的查询不产生 trigram、自然命中 0 行（已知限制）。
pub(crate) fn fts_sanitize(text: &str) -> String {
    let escaped = text.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

/// BETA-56：抽取「trigram 无法命中」的短查询词，供 `music_fts` / `documents_fts` 两侧
/// 的 metadata LIKE 兜底共用（trigram tokenizer 下 <3 字符查询生不成 3-gram、必然 0 命中）。
///
/// whitespace 切分后，仅当 **全部** 词都满足「<3 字符且纯 alphanumeric/CJK」时返回这些词
/// （纯短查询，FTS 结构性 0 命中）——`char::is_alphanumeric` 对 CJK 表意字（Unicode `Lo`）
/// 亦为 true，故「燎原」/「AI」命中；含符号/空白的病态输入（如 `a" OR b`）不命中、
/// 保持原 `fts_sanitize` 路径零回归。长短混合、含 ≥3 字长词、`text` 为空 → 返回空 vec（不兜底，
/// 交 FTS：长词可命中，短词为已知限制、语义臂兜底）。
pub(crate) fn short_metadata_like_terms(text: Option<&str>) -> Vec<String> {
    let Some(t) = text else {
        return Vec::new();
    };
    let terms: Vec<String> = t.split_whitespace().map(str::to_owned).collect();
    let all_short_wordlike = !terms.is_empty()
        && terms.iter().all(|w| {
            let n = w.chars().count();
            n < 3 && w.chars().all(char::is_alphanumeric)
        });
    if all_short_wordlike {
        terms
    } else {
        Vec::new()
    }
}

/// `path` 是否在 `root` 子树下（前缀 + 分隔符边界，大小写敏感按 OS 原生）。
pub(crate) fn path_is_under(path: &str, root: &str) -> bool {
    let root_trim = root.trim_end_matches(['/', '\\']);
    if path == root_trim {
        return true;
    }
    if let Some(rest) = path.strip_prefix(root_trim) {
        rest.starts_with('/') || rest.starts_with('\\')
    } else {
        false
    }
}

/// BETA-33 cycle 7-c：root 子树边界 SQL 谓词（三参：`?1` = root、`?2` = `root/*`、`?3` = `root\*`）。
/// `stats_under_root` 与 `purge_under_root` 共用，保证「统计口径」与「清除口径」一致——
/// 概貌里数到多少条，清除就删多少条。
pub(crate) fn root_glob_predicate(col: &str) -> String {
    format!("{col} = ?1 OR {col} GLOB ?2 OR {col} GLOB ?3")
}

/// 与 [`root_glob_predicate`] 配套的三参数：trim 尾部分隔符后的 root、`root/*`（Unix）、
/// `root\*`（Windows）。两分隔符 GLOB 同时给，Windows / Unix 路径都能命中。
pub(crate) fn root_glob_params(root: &str) -> [String; 3] {
    let t = root.trim_end_matches(['/', '\\']);
    [t.to_owned(), format!("{t}/*"), format!("{t}\\*")]
}

pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::scan::IncrementalStore;

    fn entry(path: &str, artist: &str, title: &str, format: &str) -> MusicEntry {
        MusicEntry {
            path: path.to_string(),
            file_name: path.rsplit(['/', '\\']).next().unwrap_or(path).to_string(),
            artist: Some(artist.to_string()),
            title: Some(title.to_string()),
            album: Some("专辑X".to_string()),
            duration_secs: Some(180.0),
            format: Some(format.to_string()),
            bitrate: Some(320),
            modified_time: 1000,
        }
    }

    #[test]
    fn open_in_memory_starts_empty() {
        let idx = MusicIndex::open_in_memory().unwrap();
        assert_eq!(idx.count().unwrap(), 0);
    }

    /// 2026-07-29：`optimize_fts` 在空库、有数据库上都不出错；调用后查询仍能命中
    /// （optimize 只重组 segment，不改变可查询内容）。
    #[test]
    fn optimize_fts_is_safe_on_empty_and_populated_db() {
        let idx = MusicIndex::open_in_memory().unwrap();
        idx.optimize_fts().unwrap();

        idx.upsert_entry(&entry("/m/a.mp3", "周华健", "朋友", "MP3"))
            .unwrap();
        idx.optimize_fts().unwrap();

        let hits = idx
            .query(&crate::model::MusicQuery {
                text: Some("周华健".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1, "optimize 后查询仍正常命中");
    }

    #[test]
    fn upsert_two_distinct_paths_counts_two() {
        let idx = MusicIndex::open_in_memory().unwrap();
        assert!(idx
            .upsert_entry(&entry("/m/a.mp3", "周华健", "朋友", "MP3"))
            .unwrap());
        assert!(idx
            .upsert_entry(&entry("/m/b.flac", "Eason", "Hua", "FLAC"))
            .unwrap());
        assert_eq!(idx.count().unwrap(), 2);
    }

    #[test]
    fn entry_for_path_returns_full_metadata() {
        let idx = MusicIndex::open_in_memory().unwrap();
        idx.upsert_entry(&entry("/m/a.mp3", "周华健", "朋友", "MP3"))
            .unwrap();
        let got = idx.entry_for_path("/m/a.mp3").unwrap().unwrap();
        assert_eq!(got.artist.as_deref(), Some("周华健"));
        assert_eq!(got.title.as_deref(), Some("朋友"));
        assert_eq!(got.album.as_deref(), Some("专辑X"));
        assert_eq!(got.duration_secs, Some(180.0));
        assert_eq!(got.format.as_deref(), Some("MP3"));
        // 不存在的路径 → None。
        assert!(idx.entry_for_path("/m/none.mp3").unwrap().is_none());
    }

    #[test]
    fn fts_text_matches_cjk_artist() {
        let idx = MusicIndex::open_in_memory().unwrap();
        idx.upsert_entry(&entry("/m/a.mp3", "周华健", "朋友", "MP3"))
            .unwrap();
        idx.upsert_entry(&entry("/m/b.flac", "Eason", "Hua", "FLAC"))
            .unwrap();
        let out = idx
            .query(&MusicQuery {
                text: Some("周华健".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].artist.as_deref(), Some("周华健"));
    }

    /// BETA-56：2 字中文查询经 trigram `music_fts` 必 0 命中，短查询 LIKE 兜底应命中
    /// artist / title / file_name；三列都无 → 0。
    #[test]
    fn short_cjk_query_hits_music_metadata_via_like_fallback() {
        let idx = MusicIndex::open_in_memory().unwrap();
        idx.upsert_entry(&entry("/m/a.mp3", "燎原", "夜曲", "MP3"))
            .unwrap();
        idx.upsert_entry(&entry("/m/b.flac", "Eason", "浮夸", "FLAC"))
            .unwrap();

        // 2 字 artist 经 LIKE 兜底命中。
        let by_artist = idx
            .query(&MusicQuery {
                text: Some("燎原".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_artist.len(), 1, "2 字 artist 应经 LIKE 兜底命中");
        assert_eq!(by_artist[0].artist.as_deref(), Some("燎原"));

        // 2 字 title 命中。
        let by_title = idx
            .query(&MusicQuery {
                text: Some("浮夸".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_title.len(), 1);
        assert_eq!(by_title[0].title.as_deref(), Some("浮夸"));

        // 三列均无 → 0。
        let none = idx
            .query(&MusicQuery {
                text: Some("张三".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn fts_matches_file_name() {
        // BETA-01A：标签稀疏时按文件名搜应命中本地索引（artist 故意设为无关值）。
        let idx = MusicIndex::open_in_memory().unwrap();
        idx.upsert_entry(&entry("/m/周华健-朋友.mp3", "未知艺术家", "T", "MP3"))
            .unwrap();
        let out = idx
            .query(&MusicQuery {
                text: Some("周华健".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(out.len(), 1, "应按文件名（非 artist 标签）命中");
        assert_eq!(out[0].file_name, "周华健-朋友.mp3");
    }

    #[test]
    fn prune_deleted_removes_only_missing() {
        // BETA-07 回收：磁盘不存在的记录删掉，存在的（含占位符路径）保留。
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.mp3");
        std::fs::write(&real, b"x").unwrap();
        let real_str = real.to_string_lossy().into_owned();
        let idx = MusicIndex::open_in_memory().unwrap();
        idx.upsert_entry(&entry(&real_str, "周华健", "朋友", "MP3"))
            .unwrap();
        idx.upsert_entry(&entry("/no/such/gone.mp3", "GoneArtist", "X", "MP3"))
            .unwrap();
        assert_eq!(idx.count().unwrap(), 2);

        let removed = idx.prune_deleted().unwrap();
        assert_eq!(removed, 1, "只删磁盘不存在的");
        assert_eq!(idx.count().unwrap(), 1);
        // 存在的还在。
        let hit = idx
            .query(&MusicQuery {
                text: Some("周华健".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hit.len(), 1);
        // 删掉的 FTS 也清了。
        let gone = idx
            .query(&MusicQuery {
                text: Some("GoneArtist".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert!(gone.is_empty());
    }

    #[test]
    fn migration_old_3col_fts_repopulates_file_name() {
        // 旧库（3 列 music_fts，无 file_name）打开后应自动迁移 + 从 music 重填，按文件名可搜。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE music(id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                   file_name TEXT NOT NULL, artist TEXT, title TEXT, album TEXT,
                   duration_secs REAL, format TEXT, bitrate INTEGER,
                   modified_time INTEGER NOT NULL, indexed_time INTEGER NOT NULL);
                 CREATE VIRTUAL TABLE music_fts USING fts5(artist, title, album, tokenize='trigram');
                 INSERT INTO music(path,file_name,modified_time,indexed_time)
                   VALUES('/m/周华健-朋友.mp3','周华健-朋友.mp3',1000,1000);
                 INSERT INTO music_fts(rowid,artist,title,album) VALUES(1,NULL,NULL,NULL);",
            )
            .unwrap();
        }
        // open 触发迁移。
        let idx = MusicIndex::open(&path).unwrap();
        assert_eq!(idx.count().unwrap(), 1, "迁移不丢主表数据");
        let out = idx
            .query(&MusicQuery {
                text: Some("周华健".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(out.len(), 1, "迁移后应能按文件名命中");
    }

    #[test]
    fn artist_substring_case_insensitive() {
        let idx = MusicIndex::open_in_memory().unwrap();
        idx.upsert_entry(&entry("/m/b.flac", "Eason Chan", "Hua", "FLAC"))
            .unwrap();
        let out = idx
            .query(&MusicQuery {
                artist: Some("eason".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn format_filter_case_insensitive() {
        let idx = MusicIndex::open_in_memory().unwrap();
        idx.upsert_entry(&entry("/m/a.mp3", "A", "T", "MP3"))
            .unwrap();
        idx.upsert_entry(&entry("/m/b.flac", "B", "T", "FLAC"))
            .unwrap();
        let out = idx
            .query(&MusicQuery {
                format: Some("flac".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].format.as_deref(), Some("FLAC"));
    }

    #[test]
    fn limit_truncates() {
        let idx = MusicIndex::open_in_memory().unwrap();
        for i in 0..5 {
            idx.upsert_entry(&entry(&format!("/m/{i}.mp3"), &format!("A{i}"), "T", "MP3"))
                .unwrap();
        }
        let out = idx
            .query(&MusicQuery {
                limit: Some(2),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn reupsert_same_path_updates_in_place() {
        let idx = MusicIndex::open_in_memory().unwrap();
        assert!(idx
            .upsert_entry(&entry("/m/a.mp3", "A", "旧标题", "MP3"))
            .unwrap());
        // 第二次：同 path，新标题 → 更新（非新增），count 不变。
        assert!(!idx
            .upsert_entry(&entry("/m/a.mp3", "A", "新标题", "MP3"))
            .unwrap());
        assert_eq!(idx.count().unwrap(), 1);
        let out = idx
            .query(&MusicQuery {
                text: Some("新标题".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(out.len(), 1, "FTS 应已同步刷新到新标题");
        // 旧标题不再命中。
        let old = idx
            .query(&MusicQuery {
                text: Some("旧标题".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert!(old.is_empty(), "旧标题应已从 FTS 移除");
    }

    #[test]
    fn fts_sanitize_handles_syntax_chars() {
        let idx = MusicIndex::open_in_memory().unwrap();
        idx.upsert_entry(&entry("/m/a.mp3", "A", "T", "MP3"))
            .unwrap();
        // 含 FTS5 语法字符的输入不应 panic / 报错（应安全地匹配 0 条）。
        let out = idx
            .query(&MusicQuery {
                text: Some("a\" OR b *".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn delete_by_path_removes_from_fts() {
        let idx = MusicIndex::open_in_memory().unwrap();
        idx.upsert_entry(&entry("/m/a.mp3", "周华健", "朋友", "MP3"))
            .unwrap();
        assert!(idx.delete_by_path("/m/a.mp3").unwrap());
        assert_eq!(idx.count().unwrap(), 0);
        let out = idx
            .query(&MusicQuery {
                text: Some("周华健".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert!(out.is_empty());
        // 重复删不报错、返回 false。
        assert!(!idx.delete_by_path("/m/a.mp3").unwrap());
    }

    #[test]
    fn paths_under_filters_by_root() {
        let idx = MusicIndex::open_in_memory().unwrap();
        idx.upsert_entry(&entry("/music/a.mp3", "A", "T", "MP3"))
            .unwrap();
        idx.upsert_entry(&entry("/other/b.mp3", "B", "T", "MP3"))
            .unwrap();
        let under = idx.paths_under(&["/music".to_string()]).unwrap();
        assert_eq!(under, vec!["/music/a.mp3".to_string()]);
    }

    /// BETA-64 T7a：批量 mtime 预取——root 过滤口径与 `paths_under` 一致，且带回
    /// 正确的 `modified_time`（供增量扫描内存比对，替代逐文件 `modified_time_of`）。
    #[test]
    fn modified_times_under_filters_by_root_and_carries_mtime() {
        let idx = MusicIndex::open_in_memory().unwrap();
        idx.upsert_entry(&entry("/music/a.mp3", "A", "T", "MP3"))
            .unwrap();
        idx.upsert_entry(&entry("/other/b.mp3", "B", "T", "MP3"))
            .unwrap();
        let map = idx.modified_times_under(&["/music".to_string()]).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("/music/a.mp3"), Some(&1000));
        assert!(!map.contains_key("/other/b.mp3"));
    }

    #[test]
    fn path_is_under_boundary() {
        assert!(path_is_under("/music/a.mp3", "/music"));
        assert!(path_is_under("/music/a.mp3", "/music/"));
        assert!(path_is_under(r"C:\Music\a.mp3", r"C:\Music"));
        // 前缀但非子树边界 → 不算。
        assert!(!path_is_under("/musicians/a.mp3", "/music"));
    }

    #[test]
    fn schema_version_persists_across_open() {
        // BETA-32 C1b 持久化集成测试：`MusicIndex::open` 走真实文件路径后，schema_meta
        // 表 + version 行应已落盘；用 raw rusqlite::Connection 重开同一文件读出 "1"。
        // 防 `ensure_schema_version` 调用被挪到 SCHEMA execute 之前——单测过、生产炸。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("music.db");
        {
            let _idx = MusicIndex::open(&path).unwrap();
        } // drop 关连接、落盘
        let conn = Connection::open(&path).unwrap();
        let v = crate::version::read_schema_version(&conn).unwrap();
        assert_eq!(v.as_deref(), Some(crate::version::INDEXER_SCHEMA_VERSION));
    }

    #[test]
    fn file_open_enables_wal_journal_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("music.db");
        {
            let _idx = MusicIndex::open(&path).unwrap();
        }
        let conn = Connection::open(&path).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
    }

    /// P0 回归：一键清除必须同时删除向量、OCR 段落与失败留痕，不能让旧 doc_id 在
    /// documents 重建后重新挂到新文档。
    #[test]
    fn clear_index_removes_all_derived_and_sensitive_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let _music = MusicIndex::open(&path).unwrap();
            let _docs = crate::DocumentIndex::open(&path).unwrap();
        }
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "INSERT INTO documents
                 (id,path,file_name,doc_type,modified_time,indexed_time)
                 VALUES (1,'/secret/a.pdf','a.pdf','pdf',1,1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO documents_fts(rowid,title,author,body,entity)
                 VALUES (1,'','','敏感正文','')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO document_vectors
                 (doc_id,dim,vector,embed_model,source_hash,embedded_time)
                 VALUES (1,1,x'0000803f','model-a','hash-a',1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO document_passages(doc_id,page_no,seq,text)
                 VALUES (1,1,0,'敏感 OCR')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO document_failed_pages(doc_id,page_no,reason,failed_time)
                 VALUES (1,2,'敏感失败原因',1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO index_failures(path,reason,failed_time)
                 VALUES ('/secret/b.pdf','敏感路径',1)",
                [],
            )
            .unwrap();
        }

        clear_index(&path).unwrap();

        let conn = Connection::open(&path).unwrap();
        for table in [
            "documents",
            "documents_fts",
            "document_vectors",
            "document_passages",
            "document_failed_pages",
            "index_failures",
            "music",
            "music_fts",
        ] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 0, "{table} 应被完整删除");
        }
        drop(conn);

        // 重开会重建空 schema；旧 doc_id/向量/失败留痕不得复活。
        let docs = crate::DocumentIndex::open(&path).unwrap();
        assert_eq!(docs.count().unwrap(), 0);
        assert_eq!(docs.vector_count().unwrap(), 0);
        assert_eq!(docs.extraction_failure_count().unwrap(), 0);
    }

    /// BETA-33 cycle 5：`stats_under_root` 按 root 前缀边界统计音乐条数 + 上次索引，
    /// 兄弟目录（前缀相同但非子树）不误伤。
    #[test]
    fn stats_under_root_counts_and_boundary() {
        let idx = MusicIndex::open_in_memory().unwrap();
        idx.upsert_entry(&entry("/music/a.mp3", "A", "T", "MP3"))
            .unwrap();
        idx.upsert_entry(&entry("/music/sub/b.mp3", "B", "T", "MP3"))
            .unwrap();
        // 兄弟目录 /musicians 不算 /music 子树
        idx.upsert_entry(&entry("/musicians/c.mp3", "C", "T", "MP3"))
            .unwrap();

        let s = idx.stats_under_root("/music").unwrap();
        assert_eq!(s.total, 2, "/music 下应有 2 条");
        assert!(s.last_indexed_time.is_some());

        // 尾部 / 归一
        let s2 = idx.stats_under_root("/music/").unwrap();
        assert_eq!(s2, s);

        // Windows path
        idx.upsert_entry(&entry(r"C:\Music\d.mp3", "D", "T", "MP3"))
            .unwrap();
        let s3 = idx.stats_under_root(r"C:\Music").unwrap();
        assert_eq!(s3.total, 1);
    }

    /// BETA-33 cycle 5：空 root（无匹配）时返 0 / None。
    #[test]
    fn stats_under_root_empty_returns_zero() {
        let idx = MusicIndex::open_in_memory().unwrap();
        let s = idx.stats_under_root("/nonexistent").unwrap();
        assert_eq!(s.total, 0);
        assert_eq!(s.last_indexed_time, None);
    }

    /// BETA-33 cycle 7-c：purge_under_root 删子树（含 FTS 同步删）、兄弟前缀目录不误删、
    /// 与 stats_under_root 口径一致、幂等（再清返 0）。
    #[test]
    fn purge_under_root_removes_subtree_and_fts() {
        let idx = MusicIndex::open_in_memory().unwrap();
        idx.upsert_entry(&entry("/music/a.mp3", "ArtistAAA", "SongAAA", "MP3"))
            .unwrap();
        idx.upsert_entry(&entry("/music/sub/b.mp3", "ArtistBBB", "SongBBB", "MP3"))
            .unwrap();
        // 兄弟前缀目录 /musicians 不算 /music 子树，必须保留。
        idx.upsert_entry(&entry("/musicians/c.mp3", "ArtistCCC", "SongCCC", "MP3"))
            .unwrap();

        // 清除数 = 概貌统计数（同一边界谓词）。
        let expect = idx.stats_under_root("/music").unwrap().total;
        let removed = idx.purge_under_root("/music").unwrap();
        assert_eq!(removed, expect, "清除口径应与统计口径一致");
        assert_eq!(removed, 2);
        assert_eq!(idx.count().unwrap(), 1, "边界外 /musicians 保留");

        // FTS 同步删：已清条目搜不到、边界外条目仍可搜。
        let gone = idx
            .query(&MusicQuery {
                text: Some("SongAAA".to_owned()),
                ..Default::default()
            })
            .unwrap();
        assert!(gone.is_empty(), "已清条目不应再命中 FTS");
        let kept = idx
            .query(&MusicQuery {
                text: Some("SongCCC".to_owned()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(kept.len(), 1, "边界外条目 FTS 仍可搜");

        // 幂等：再清返 0。
        assert_eq!(idx.purge_under_root("/music").unwrap(), 0);
    }
}
