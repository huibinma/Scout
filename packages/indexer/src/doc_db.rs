//! 文档存储层：SQLite + FTS5（BETA-02）。
//!
//! 与 [`crate::db::MusicIndex`] 同构：`documents` 主表 + 独立 `documents_fts` FTS5 表
//! （rowid 对齐 `documents.id`）。正文文本只进 FTS（不存主表），查询用 `snippet()` 返回片段。

use std::collections::HashMap;
use std::path::Path;

use rusqlite::types::ToSql;
use rusqlite::{params, Connection, OptionalExtension};

use crate::db::{
    configure_common_db_pragmas, configure_file_db_pragmas, fts_sanitize, path_is_under,
    root_glob_params, root_glob_predicate, short_metadata_like_terms, unix_now,
};
use crate::model::{
    DocumentEntry, DocumentHit, DocumentPreview, DocumentQuery, ExtractedDoc, ExtractionFailure,
    PageFailure, PagePassage,
};
use crate::pii::pii_entity_keywords;
use crate::scan::IncrementalStore;
use crate::version::ensure_schema_version;
use crate::IndexError;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS documents (
  id            INTEGER PRIMARY KEY,
  path          TEXT NOT NULL UNIQUE,
  file_name     TEXT NOT NULL,
  title         TEXT,
  author        TEXT,
  doc_type      TEXT NOT NULL,
  page_count    INTEGER,
  modified_time INTEGER NOT NULL,
  indexed_time  INTEGER NOT NULL,
  content_hash  TEXT
);
CREATE INDEX IF NOT EXISTS idx_documents_modified ON documents(modified_time);
-- 列序固定：title=0, author=1, body=2, entity=3（`query` 的 `snippet()` 硬编码 body=2、
-- 迁移与查询都依赖此序，勿重排）。`entity`（BETA-59）存 PII 类型概念词（身份证/手机号），
-- 与 body 隔离：`MATCH` 裸表达式自动跨所有列可搜到 entity，`snippet()` 固定 body 列不回显。
CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(
  title, author, body, entity,
  tokenize='trigram'
);
CREATE TABLE IF NOT EXISTS document_vectors (
  doc_id        INTEGER PRIMARY KEY REFERENCES documents(id) ON DELETE CASCADE,
  dim           INTEGER NOT NULL,
  vector        BLOB NOT NULL,
  embed_model   TEXT NOT NULL,
  source_hash   TEXT NOT NULL,
  embedded_time INTEGER NOT NULL
);
-- BETA-35 cycle 4：扫描版 PDF 逐页 OCR 段落（每页一段，seq 起于 0；后续可按段
-- 落切分展开）。命中回页由 UI 通过 `page_no` 展示（验收 ②）。UNIQUE (doc_id,
-- page_no, seq) 保 re-upsert 时先 DELETE-INSERT 幂等。CREATE IF NOT EXISTS
-- 保老 db 打开自动加表（无需 schema version bump，同表结构演进套路）。
CREATE TABLE IF NOT EXISTS document_passages (
  id      INTEGER PRIMARY KEY,
  doc_id  INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  page_no INTEGER NOT NULL,
  seq     INTEGER NOT NULL DEFAULT 0,
  text    TEXT NOT NULL,
  UNIQUE(doc_id, page_no, seq)
);
CREATE INDEX IF NOT EXISTS idx_passages_doc ON document_passages(doc_id);
-- BETA-35 cycle 4：扫描 PDF 逐页 OCR 失败留痕（验收 ③——失败页记录不静默丢）。
-- 取证复核：`SELECT page_no, reason FROM document_failed_pages WHERE doc_id=?`。
CREATE TABLE IF NOT EXISTS document_failed_pages (
  id          INTEGER PRIMARY KEY,
  doc_id      INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  page_no     INTEGER NOT NULL,
  reason      TEXT NOT NULL,
  failed_time INTEGER NOT NULL,
  UNIQUE(doc_id, page_no)
);
CREATE INDEX IF NOT EXISTS idx_failed_pages_doc ON document_failed_pages(doc_id);
-- BETA-40 收尾（2026-07-04）：**文件级**提取失败留痕（区别于上表的扫描 PDF **页级**）。
-- 整份文件提取失败（pdf-extract 不支持编码 / OCR 依赖缺失 / 畸形文件）此前只累计
-- IndexStats.failed 静默丢——企业取证复核无从查起。成功重扫或磁盘删除后自动清除。
-- 取证复核：`SELECT path, reason, failed_time FROM index_failures`。
-- CREATE IF NOT EXISTS 保老 db 打开自动加表（同 document_passages 套路，无需 schema bump）。
CREATE TABLE IF NOT EXISTS index_failures (
  path          TEXT PRIMARY KEY,
  reason        TEXT NOT NULL,
  failed_time   INTEGER NOT NULL,
  modified_time INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS schema_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
INSERT OR IGNORE INTO schema_meta(key, value) VALUES('vector_generation', '0');
";

fn bump_vector_generation(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE schema_meta
         SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)
         WHERE key='vector_generation'",
        [],
    )?;
    Ok(())
}

/// 一条候选向量（语义检索暴力扫描用）。
#[derive(Debug, Clone)]
pub struct CandidateVector {
    pub path: String,
    pub vector: Vec<f32>,
    /// BETA-38：文件身份指纹（`documents.content_hash`）。语义臂据此把同内容多副本
    /// 归为一组（只算一次 cosine、结果只出一条代表）。老库 / 未回填 → `None`（各自独立）。
    pub content_hash: Option<String>,
}

/// BETA-33 cycle 5：某 root 子树下的文档索引统计。
///
/// `total` = 该 root 下所有文档条数（含图片 OCR）；
/// `images` = 其中 `doc_type` 属 `IMAGE_EXTS` 的条数（PNG/JPG/...）；
/// `last_indexed_time` = 该 root 下最近一次 indexed_time（Unix 秒；无记录 → None）。
///
/// 桌面「选项 → 索引」pane 用这三个字段渲染每 root 的分类分布 + 新鲜度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocRootStats {
    pub total: u64,
    pub images: u64,
    pub last_indexed_time: Option<i64>,
}

/// 文档内容索引（持有一个 SQLite 连接）。
#[derive(Debug)]
pub struct DocumentIndex {
    conn: Connection,
}

impl DocumentIndex {
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
        // document_vectors 外键级联依赖此 PRAGMA（默认关）。
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        // BETA-38：老库 documents 表无 content_hash 列 → ALTER ADD（列可空、无需 schema-bump）。
        migrate_documents_content_hash(&conn)?;
        // 2026-07-29：老库 index_failures 表无 modified_time 列 → ALTER ADD（同上套路）。
        migrate_index_failures_modified_time(&conn)?;
        // BETA-59：老库 documents_fts 只有 title/author/body 三列 → 加 entity 末列。
        // **必须在任何 4 列 INSERT 之前跑**，否则升级用户首次 upsert 就会列数不匹配崩。
        migrate_documents_fts_entity(&conn)?;
        // BETA-32 C1b：老 db 第一次打开 → INSERT schema 版本；已有则 no-op。
        ensure_schema_version(&conn)?;
        Ok(Self { conn })
    }

    /// 记录总数。
    pub fn count(&self) -> Result<u64, IndexError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// 2026-07-29：FTS5 索引维护，同 [`crate::MusicIndex::optimize_fts`]（理由见其文档）。
    /// `documents_fts` 由文档轮与图片轮共用，调用方应在**两轮都跑完**、且合计确有变更时
    /// 调用一次，不必每个 phase 各调一次。
    pub fn optimize_fts(&self) -> Result<(), IndexError> {
        self.conn
            .execute_batch("INSERT INTO documents_fts(documents_fts) VALUES('optimize');")?;
        Ok(())
    }

    /// BETA-64 T7：全表 `(path, modified_time)`，无 root 过滤（比照
    /// [`crate::MusicIndex::all_modified_times`]）。供 [`crate::scan::index_discovered_paths`]
    /// （`index_paths`/`index_image_paths` 共用骨架，Everything/Spotlight 枚举出的显式路径
    /// 列表）一次性批量预取 mtime，替代逐路径 `modified_time_of` 查询。
    ///
    /// `pub`（而非 `pub(crate)`）：`index_paths`/`index_image_paths` 各自独立调一次本方法，
    /// 若调用方（如 `packages/search-backends/local-index`）在同一轮 reindex 里连续调
    /// 这两个方法，会对 `documents` 表做两次没必要的全表扫描——本方法公开后，调用方可以
    /// 自己只调一次、把结果通过 [`Self::index_paths_with_known_mtimes`]/
    /// [`Self::index_image_paths_with_known_mtimes`] 传进去复用（见这两个方法文档）。
    pub fn all_modified_times(&self) -> Result<HashMap<String, i64>, IndexError> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, modified_time FROM documents")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (p, mt) = row?;
            out.insert(p, mt);
        }
        Ok(out)
    }

    /// BETA-64 T5：回收磁盘已删的记录（比照 [`crate::MusicIndex::prune_deleted`]）。
    /// `index_paths`/`index_image_paths`（发现层给显式路径列表）不像 `index_dirs` 自带
    /// 「本轮 seen 集合外即回收」逻辑，需要发现层调用方额外调本方法。按真实文件存在性
    /// 判定（不区分 doc_type），故文档 / 图片发现各自跑完后调用同一个方法即可、不需要
    /// 按扩展名收窄（与 walkdir 路径的 `recycle_is_scoped_to_round_extensions` 语义不同但
    /// 结果等价——真实磁盘不存在的记录，不管上一轮是哪个 doc_type 写入的，都该被删）。
    pub fn prune_deleted(&self) -> Result<u64, IndexError> {
        let paths: Vec<String> = {
            let mut stmt = self.conn.prepare("SELECT path FROM documents")?;
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

    /// `document_vectors` 行数（BETA-15B-2 解耦验证 + 进度统计用）。
    pub fn vector_count(&self) -> Result<u64, IndexError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM document_vectors", [], |r| r.get(0))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// 指定模型和维度的向量行数。语义缓存用它隔离换模或维度迁移期间的旧向量。
    pub fn vector_count_for_model_dim(
        &self,
        embed_model: &str,
        dim: usize,
    ) -> Result<u64, IndexError> {
        let dim = i64::try_from(dim).unwrap_or(i64::MAX);
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM document_vectors WHERE embed_model=?1 AND dim=?2",
            params![embed_model, dim],
            |r| r.get(0),
        )?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// 向量内容代次。每次向量 INSERT/UPDATE/显式清理都在同一事务内递增，避免 WAL
    /// 写入未改变主 db mtime 或行数不变时，语义缓存继续复用旧内容。
    pub fn vector_generation(&self) -> Result<u64, IndexError> {
        let value: String = self.conn.query_row(
            "SELECT value FROM schema_meta WHERE key='vector_generation'",
            [],
            |r| r.get(0),
        )?;
        Ok(value.parse::<u64>().unwrap_or(0))
    }

    /// BETA-31-v3 cycle 3（2026-06-30）：清理 `document_vectors` 中关联到 body 极短文档
    /// 的旧脏向量，返回被删除条数。**Why**：v0.8.3 用户真机暴露 BETA-15B-1 以来的 ranker
    /// 污染 bug ——`embed_pending` 历史版本对 documents 表所有条目一视同仁、Windows OCR
    /// 跳过的图片 body 为空 → 嵌入产出 "neutral" 高 cosine 向量（与任意 query 都接近）→
    /// 占满 ranker top-N、用户搜任何词全返 Tencent / Rockstar 缓存图片。`vector_is_current`
    /// 检查 model_id + source_hash、对已嵌入的旧脏向量**不会自动重嵌**（hash 仍是空串
    /// 的 hash、source 不变）—— 必须显式 DELETE。
    ///
    /// 判断口径与 [`crate::embed::is_embed_worthy`] **完全一致**（Rust 侧字符数 + trim）、
    /// 不用 SQL length() 字节判（CJK 字节数 ≠ 字符数、不准）。
    ///
    /// **BETA-33 cycle 4（2026-07-01）扩展**：也清图片 doc_type 的旧向量（`embed_pending`
    /// 同步加图片跳过后、旧库里已嵌入的图片向量仍会污染召回，必须显式清）。
    /// 图片扩展名集合以 [`crate::scan::IMAGE_EXTS`] 为准。
    ///
    /// **BETA-39（2026-07-03）**：`keep_worthy_images` 由「图片语义索引」opt-in 设置驱动——
    /// - `false`（开关关，默认）：图片向量**全清**（cycle 4 现状；开过再关，下次启动
    ///   自动回收图片向量、恢复一刀切态）；
    /// - `true`（开关开）：图片向量仅当不过 [`crate::embed::is_image_embed_worthy`]
    ///   （0.75 图片专属门槛）才清——历史乱码向量仍被回收、真文字图片向量保留。
    ///
    /// **幂等**：清理后再调返 0（无脏向量可删）。
    /// **轻量**：典型 v0.8.3 真机数据 3433 向量、纯 join + 内存过滤 + 一次 DELETE、毫秒级。
    pub fn purge_short_body_vectors(&self, keep_worthy_images: bool) -> Result<usize, IndexError> {
        let mut stmt = self.conn.prepare(
            "SELECT v.doc_id, IFNULL(f.body, ''), d.doc_type \
             FROM document_vectors v \
             JOIN documents_fts f ON f.rowid = v.doc_id \
             JOIN documents d ON d.id = v.doc_id",
        )?;
        let rows: Vec<(i64, String, String)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<_, _>>()?;

        let to_delete: Vec<i64> = rows
            .into_iter()
            .filter(|(_, body, doc_type)| {
                if crate::scan::IMAGE_EXTS.contains(&doc_type.as_str()) {
                    // 图片向量：opt-in 关 → 全清（现状）；开 → 只清不过图片门槛的。
                    return !(keep_worthy_images && crate::embed::is_image_embed_worthy(body));
                }
                // 非图片：body 不合格才清。
                !crate::embed::is_embed_worthy(body)
            })
            .map(|(id, _, _)| id)
            .collect();

        if to_delete.is_empty() {
            return Ok(0);
        }

        let placeholders = vec!["?"; to_delete.len()].join(",");
        let sql = format!("DELETE FROM document_vectors WHERE doc_id IN ({placeholders})");
        let tx = self.conn.unchecked_transaction()?;
        let count = tx.execute(&sql, rusqlite::params_from_iter(to_delete.iter()))?;
        bump_vector_generation(&tx)?;
        tx.commit()?;
        Ok(count)
    }

    /// BETA-33 cycle 5：某 root 子树下的文档索引统计（总数 / 图片数 / 上次索引时间）。
    /// 单一 SQL 一次查完，用于「索引概貌」UI；毫秒级。
    ///
    /// `root` 会 trim 尾部 `/` 和 `\`；子树判定 = `path == root` 或 `path GLOB root+'/*'`
    /// 或 `path GLOB root+'\*'`（同时支持 Windows 和 Unix 分隔符）。
    ///
    /// 图片类型判定：`doc_type IN IMAGE_EXTS`（`IMAGE_EXTS` 是编译期常量、直接拼 SQL 无注入）。
    pub fn stats_under_root(&self, root: &str) -> Result<DocRootStats, IndexError> {
        // cycle 7-c：边界谓词抽到 root_glob_predicate/params，与 purge_under_root 共用同一口径。
        let p = root_glob_params(root);
        let types_list: String = crate::scan::IMAGE_EXTS
            .iter()
            .map(|t| format!("'{t}'"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT \
                 COUNT(*), \
                 COALESCE(SUM(CASE WHEN doc_type IN ({types_list}) THEN 1 ELSE 0 END), 0), \
                 MAX(indexed_time) \
             FROM documents \
             WHERE {}",
            root_glob_predicate("path")
        );
        let (total, images, last_indexed): (i64, i64, Option<i64>) =
            self.conn
                .query_row(&sql, rusqlite::params![p[0], p[1], p[2]], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?;
        Ok(DocRootStats {
            total: u64::try_from(total).unwrap_or(0),
            images: u64::try_from(images).unwrap_or(0),
            last_indexed_time: last_indexed,
        })
    }

    /// BETA-33 cycle 7-c：清除 root 子树下所有文档条目（同事务内同步删 `documents_fts`；
    /// `document_vectors` 走外键 `ON DELETE CASCADE` 自动级联，依赖 `from_conn` 里
    /// `PRAGMA foreign_keys = ON`）。返回删除条数。
    ///
    /// 边界口径与 [`Self::stats_under_root`] 共用 [`root_glob_predicate`]——概貌统计到的
    /// 条目就是会被清除的条目。**只删 Scout 数据库缓存，不碰磁盘文件。**
    pub fn purge_under_root(&self, root: &str) -> Result<u64, IndexError> {
        let p = root_glob_params(root);
        let pred = root_glob_predicate("path");
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            &format!(
                "DELETE FROM documents_fts WHERE rowid IN (SELECT id FROM documents WHERE {pred})"
            ),
            rusqlite::params![p[0], p[1], p[2]],
        )?;
        let n = tx.execute(
            &format!("DELETE FROM documents WHERE {pred}"),
            rusqlite::params![p[0], p[1], p[2]],
        )?;
        tx.commit()?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// `doc_type` 属于给定集合的记录数（BETA-07 状态摘要按类型拆分用，如图片）。空集合 → 0。
    pub fn count_in_doc_types(&self, types: &[&str]) -> Result<u64, IndexError> {
        if types.is_empty() {
            return Ok(0);
        }
        let placeholders = vec!["?"; types.len()].join(",");
        let sql = format!("SELECT COUNT(*) FROM documents WHERE doc_type IN ({placeholders})");
        let n: i64 = self
            .conn
            .query_row(&sql, rusqlite::params_from_iter(types.iter()), |r| r.get(0))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// 单测便利入口：插入/更新一条文档 + 正文进 FTS（无 passages / 失败页）。
    ///
    /// **BETA-35 cycle 4**：生产代码统一走 [`Self::upsert_document_with_pages`]
    /// （scan.rs 增量循环 + 图片 OCR 都构造 [`ExtractedDoc`] 后调用）；本方法仅
    /// 单测保留，`#[cfg(test)]` 避免 lib 编译时 dead_code 报错。
    #[cfg(test)]
    pub(crate) fn upsert_document(
        &self,
        e: &DocumentEntry,
        body: &str,
    ) -> Result<bool, IndexError> {
        self.upsert_document_with_pages(e, body, &[], &[])
    }

    /// BETA-35 cycle 4：文档 + 段落 + 失败页三表原子 upsert。
    ///
    /// 语义等价于 [`Self::upsert_document`]（返 `true` 新增 / `false` 更新）+
    /// 额外把 passages / failed_pages 落进对应表；空 vec 时不写额外表——文本层 PDF
    /// / docx / xlsx 走这条路径与走 [`Self::upsert_document`] **逐字节等价**
    /// （BETA-27 byte-equal 保护）。**幂等**：每次 upsert 前 DELETE 该 doc_id
    /// 下所有旧 passages / failed_pages 再 INSERT。
    ///
    /// 原子性：整个操作在同一事务内、失败自动回滚——避免 documents 已更新但
    /// passages 半写崩溃的中间态。
    pub(crate) fn upsert_document_with_pages(
        &self,
        e: &DocumentEntry,
        body: &str,
        passages: &[PagePassage],
        failed_pages: &[PageFailure],
    ) -> Result<bool, IndexError> {
        let tx = self.conn.unchecked_transaction()?;
        let result = upsert_document_with_pages_tx(&tx, e, body, passages, failed_pages)?;
        tx.commit()?;
        Ok(result)
    }

    /// BETA-64 T2：单事务批量 upsert——把 `entries.len()` 次 commit 收敛为 1 次，减少大批量
    /// 索引（如首次全量索引上万文件、大量扫描版 PDF/图片 OCR 落库）时逐文件事务提交的固定
    /// 开销。SQL 逻辑与 [`Self::upsert_document_with_pages`] 逐字节一致（共用
    /// [`upsert_document_with_pages_tx`]），仅事务边界不同；任一条目写入失败整批回滚
    /// （与单条各自独立提交相比的行为差异，见调用点 `scan.rs` 注释）。
    pub(crate) fn upsert_documents_with_pages(
        &self,
        entries: &[ExtractedDoc],
    ) -> Result<Vec<bool>, IndexError> {
        let tx = self.conn.unchecked_transaction()?;
        let mut results = Vec::with_capacity(entries.len());
        for entry in entries {
            results.push(upsert_document_with_pages_tx(
                &tx,
                &entry.entry,
                &entry.body,
                &entry.passages,
                &entry.failed_pages,
            )?);
        }
        tx.commit()?;
        Ok(results)
    }

    /// BETA-35 cycle 4：按路径取扫描版 PDF 的所有 OCR 段落（按 page_no / seq 升序）。
    /// 命中回页（验收 ②）用：UI 拿到段文本 + page_no 后加"第 N 页 · OCR"标签。
    /// 无该文档或文档非扫描版（无 passages）→ 空 vec。
    pub fn passages_for_doc(&self, path: &str) -> Result<Vec<PagePassage>, IndexError> {
        let Some(id) = self.doc_id_of(path)? else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT page_no, seq, text FROM document_passages
             WHERE doc_id = ?1 ORDER BY page_no ASC, seq ASC",
        )?;
        let rows = stmt.query_map([id], |r| {
            // page_no / seq 存写时是 u32、SQLite 出来是 i64；负值不合法（DB 只有我们自己写），
            // try_from 失败极端保底 0（此路径实际不会走到）。
            Ok(PagePassage {
                page_no: u32::try_from(r.get::<_, i64>(0)?).unwrap_or(0),
                seq: u32::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
                text: r.get::<_, String>(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(IndexError::from)
    }

    /// BETA-35 cycle 4：按路径取扫描版 PDF 的失败页记录（验收 ③——不静默丢）。
    /// 无该文档或全页 OCR 成功 → 空 vec。
    pub fn failed_pages_for_doc(&self, path: &str) -> Result<Vec<PageFailure>, IndexError> {
        let Some(id) = self.doc_id_of(path)? else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT page_no, reason FROM document_failed_pages
             WHERE doc_id = ?1 ORDER BY page_no ASC",
        )?;
        let rows = stmt.query_map([id], |r| {
            Ok(PageFailure {
                page_no: u32::try_from(r.get::<_, i64>(0)?).unwrap_or(0),
                reason: r.get::<_, String>(1)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(IndexError::from)
    }

    /// 查询（结构化过滤 + 可选 doc_type 集合 + 可选 FTS 文本 + snippet 片段）。
    pub fn query(&self, q: &DocumentQuery) -> Result<Vec<DocumentHit>, IndexError> {
        let limit = i64::from(q.limit.unwrap_or(50));
        let cols =
            "d.path, d.file_name, d.title, d.author, d.doc_type, d.page_count, d.modified_time";

        // doc_types 动态 IN 子句（None / 空 = 不限）。值虽来自固定常量集，仍走绑定参数。
        let dt_list: Vec<&String> = q
            .doc_types
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .filter(|s| !s.trim().is_empty())
            .collect();
        let dt_keys: Vec<String> = (0..dt_list.len()).map(|i| format!(":dt{i}")).collect();
        let dt_clause = if dt_keys.is_empty() {
            String::new()
        } else {
            format!(" AND d.doc_type IN ({})", dt_keys.join(","))
        };

        let filters = format!(
            "(:author IS NULL OR d.author LIKE '%' || :author || '%')
             AND (:doc_type IS NULL OR d.doc_type = :doc_type COLLATE NOCASE){dt_clause}"
        );

        // fts_match（原始 FTS5 表达式）优先；否则 text 经 fts_sanitize 包成单 phrase。
        let match_expr = q
            .fts_match
            .clone()
            .or_else(|| q.text.as_deref().map(fts_sanitize));

        // BETA-56 短查询 metadata LIKE 兜底：`documents_fts` 是 trigram tokenizer，
        // <3 字符查询生不成 3-gram、必然 0 命中（2 字中文人名「燎原」/ 常用词受此限）。
        // 当查询由 text 派生（无原始 `fts_match`——有则说明存在可命中的长词/词组）且
        // **全部** whitespace 切分词都是 <3 字符纯 alnum/CJK 时，改用 LIKE 子串匹配
        // title/author/file_name（**不扫 body**：正文全表 LIKE 慢且噪声高，内容词由语义臂兜底）。
        // 长短混合查询（如「市场调研 饶」）保持 FTS（长词可命中，短词为已知限制）。
        let like_terms: Vec<String> = if q.fts_match.is_none() {
            short_metadata_like_terms(q.text.as_deref())
        } else {
            Vec::new()
        };
        let use_like = !like_terms.is_empty();
        // 短词全为 alnum/CJK（见 short_metadata_like_terms 守卫），不含 LIKE 元字符 `%`/`_`，
        // 故直接两端加 `%` 作子串模式、无需 ESCAPE 转义。
        let like_patterns: Vec<String> = like_terms.iter().map(|t| format!("%{t}%")).collect();
        let like_keys: Vec<String> = (0..like_terms.len()).map(|i| format!(":lk{i}")).collect();
        let like_clause = like_keys
            .iter()
            .map(|k| format!("(d.title LIKE {k} OR d.author LIKE {k} OR d.file_name LIKE {k})"))
            .collect::<Vec<_>>()
            .join(" AND ");

        let with_snippet = !use_like && match_expr.is_some();
        let sql = if with_snippet {
            format!(
                "SELECT {cols}, snippet(documents_fts, 2, '[', ']', '…', 10) AS snip
                 FROM documents d JOIN documents_fts f ON f.rowid = d.id
                 WHERE documents_fts MATCH :match AND {filters}
                 ORDER BY d.modified_time DESC LIMIT :limit"
            )
        } else if use_like {
            format!(
                "SELECT {cols}, NULL AS snip FROM documents d
                 WHERE {like_clause} AND {filters}
                 ORDER BY d.modified_time DESC LIMIT :limit"
            )
        } else {
            format!(
                "SELECT {cols}, NULL AS snip FROM documents d
                 WHERE {filters} ORDER BY d.modified_time DESC LIMIT :limit"
            )
        };

        let sanitized = match_expr;
        let mut stmt = self.conn.prepare(&sql)?;

        let mut bound: Vec<(&str, &dyn ToSql)> = vec![
            (":author", &q.author),
            (":doc_type", &q.doc_type),
            (":limit", &limit),
        ];
        if with_snippet {
            if let Some(s) = &sanitized {
                bound.push((":match", s));
            }
        }
        if use_like {
            for (k, v) in like_keys.iter().zip(&like_patterns) {
                bound.push((k.as_str(), v));
            }
        }
        for (k, v) in dt_keys.iter().zip(&dt_list) {
            bound.push((k.as_str(), *v));
        }

        let rows = stmt
            .query_map(&bound[..], |r| row_to_hit(r, with_snippet))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 按绝对路径取文档预览（BETA-20 预览面板）：元信息 + 完整正文 + 可选命中片段。无匹配 → `None`。
    ///
    /// `fts_match`（原始 FTS5 MATCH 表达式，非空）存在时，额外把该文档限定到自身 rowid
    /// 跑一次 `snippet()`，产出命中上下文片段（命中词以 `\x02` / `\x03` 哨兵包裹，前端转 `<mark>`）；
    /// 该文档不命中表达式时片段为 `None`。**只读索引，不读磁盘原文件。**
    pub fn preview_for_path(
        &self,
        path: &str,
        fts_match: Option<&str>,
    ) -> Result<Option<DocumentPreview>, IndexError> {
        let row = self
            .conn
            .query_row(
                "SELECT d.id, d.path, d.file_name, d.title, d.author, d.doc_type,
                        d.page_count, d.modified_time, f.body
                 FROM documents d JOIN documents_fts f ON f.rowid = d.id
                 WHERE d.path = ?1",
                [path],
                |r| {
                    let id: i64 = r.get(0)?;
                    let entry = DocumentEntry {
                        path: r.get(1)?,
                        file_name: r.get(2)?,
                        title: r.get(3)?,
                        author: r.get(4)?,
                        doc_type: r.get(5)?,
                        page_count: r.get(6)?,
                        modified_time: r.get(7)?,
                        // 预览路径不需身份指纹（去重在语义臂做）。
                        content_hash: None,
                    };
                    let body: String = r.get(8)?;
                    Ok((id, entry, body))
                },
            )
            .optional()?;
        let Some((id, entry, body)) = row else {
            return Ok(None);
        };

        // 命中片段：限定到该文档 rowid 跑 snippet()（命中标记用 char(2)/char(3) 哨兵）。
        // 索引列序：title=0, author=1, body=2 → 第 3 个参数 2 指向 body 列（与 `query` 一致）。
        let snippet = match fts_match {
            Some(m) if !m.trim().is_empty() => self
                .conn
                .query_row(
                    "SELECT snippet(documents_fts, 2, char(2), char(3), '…', 40)
                     FROM documents_fts WHERE rowid = ?1 AND documents_fts MATCH ?2",
                    params![id, m],
                    |r| r.get::<_, String>(0),
                )
                .optional()?,
            _ => None,
        };

        Ok(Some(DocumentPreview {
            entry,
            body,
            snippet,
        }))
    }

    fn modified_time_of_impl(&self, path: &str) -> Result<Option<i64>, IndexError> {
        let mt = self
            .conn
            .query_row(
                "SELECT modified_time FROM documents WHERE path = ?1",
                [path],
                |r| r.get(0),
            )
            .optional()?;
        Ok(mt)
    }

    fn delete_by_path_impl(&self, path: &str) -> Result<bool, IndexError> {
        let tx = self.conn.unchecked_transaction()?;
        let id: Option<i64> = tx
            .query_row("SELECT id FROM documents WHERE path = ?1", [path], |r| {
                r.get(0)
            })
            .optional()?;
        let Some(id) = id else {
            return Ok(false);
        };
        tx.execute("DELETE FROM documents_fts WHERE rowid = ?1", [id])?;
        tx.execute("DELETE FROM documents WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(true)
    }

    fn paths_under_impl(&self, roots: &[String]) -> Result<Vec<String>, IndexError> {
        let mut stmt = self.conn.prepare("SELECT path FROM documents")?;
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

    /// BETA-64 T7a：`modified_times_under` 的实现内核，比照 [`Self::paths_under_impl`]，
    /// 同一张表多带一列，一次全表扫描换掉增量扫描此前的逐文件 `modified_time_of` 往返。
    fn modified_times_under_impl(
        &self,
        roots: &[String],
    ) -> Result<HashMap<String, i64>, IndexError> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, modified_time FROM documents")?;
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

    // ===== 文件级提取失败留痕（BETA-40 收尾）=====

    /// 记录（或刷新）一条文件级提取失败留痕（增量循环失败分支调用），连带存下失败时的
    /// `modified_time`——下一轮增量比对据此判定"文件自那以后是否真的变过"，未变则跳过
    /// 重新提取（2026-07-29：避免加密文档/权限过期这类稳定复现的失败每轮都重试一次）。
    fn record_extract_failure_impl(
        &self,
        path: &str,
        reason: &str,
        modified_time: i64,
    ) -> Result<(), IndexError> {
        self.conn.execute(
            "INSERT INTO index_failures(path, reason, failed_time, modified_time)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET
                 reason = excluded.reason,
                 failed_time = excluded.failed_time,
                 modified_time = excluded.modified_time",
            params![path, reason, unix_now(), modified_time],
        )?;
        Ok(())
    }

    /// 清除某 path 的失败留痕（提取成功 / 文件已删）。
    fn clear_extract_failure_impl(&self, path: &str) -> Result<(), IndexError> {
        self.conn
            .execute("DELETE FROM index_failures WHERE path = ?1", [path])?;
        Ok(())
    }

    /// 失败留痕中落在 `roots` 子树下的 path（增量回收用）。
    fn failure_paths_under_impl(&self, roots: &[String]) -> Result<Vec<String>, IndexError> {
        let mut stmt = self.conn.prepare("SELECT path FROM index_failures")?;
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

    /// 2026-07-29：全部失败留痕的 `(path, modified_time)`——一次性预取（同
    /// [`Self::all_modified_times`] 的模式），供增量循环判定"上次失败后文件是否真的
    /// 变过"，未变则跳过重新提取。`index_failures` 通常很小（异常文件，非常态），
    /// 不必像 `documents` 表那样跨函数缓存，每轮扫描内查一次即可。
    fn all_failure_modified_times_impl(&self) -> Result<HashMap<String, i64>, IndexError> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, modified_time FROM index_failures")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (p, mt) = row?;
            out.insert(p, mt);
        }
        Ok(out)
    }

    /// 全部文件级提取失败留痕（取证复核 / 诊断 UI 用），按失败时间降序。
    pub fn extraction_failures(&self) -> Result<Vec<ExtractionFailure>, IndexError> {
        let mut stmt = self.conn.prepare(
            "SELECT path, reason, failed_time FROM index_failures ORDER BY failed_time DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ExtractionFailure {
                path: r.get(0)?,
                reason: r.get(1)?,
                failed_time: r.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// 文件级提取失败留痕条数（daemon 索引完成日志 / 健康摘要用）。
    pub fn extraction_failure_count(&self) -> Result<u64, IndexError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM index_failures", [], |r| r.get(0))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// 写/更新某文档的向量（按 path 找 doc_id）。
    /// 返回 `true` = 文档行存在且向量已写/更新；`false` = 无对应文档行、未写。
    pub fn upsert_vector(
        &self,
        path: &str,
        vector: &[f32],
        embed_model: &str,
        source_hash: &str,
    ) -> Result<bool, IndexError> {
        let Some(id) = self.doc_id_of(path)? else {
            return Ok(false);
        };
        let blob = crate::vectors::vector_to_blob(vector);
        // usize→i64 实际不会失败（非 128bit 平台）；保守取 MAX 而非 0，避免 dim 与 BLOB 长度矛盾
        let dim = i64::try_from(vector.len()).unwrap_or(i64::MAX);
        let now = unix_now();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO document_vectors(doc_id, dim, vector, embed_model, source_hash, embedded_time)
                 VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(doc_id) DO UPDATE SET
                 dim=excluded.dim, vector=excluded.vector, embed_model=excluded.embed_model,
                 source_hash=excluded.source_hash, embedded_time=excluded.embedded_time",
            params![id, dim, blob, embed_model, source_hash, now],
        )?;
        bump_vector_generation(&tx)?;
        tx.commit()?;
        Ok(true)
    }

    /// BETA-38 cycle 4：批量种入合成文档 + 预置向量（**评测/基准专用**，不走文件提取）。
    ///
    /// 每条 `(path, content_hash, vector)`：插入一行 `documents`（`doc_type="synthetic"`、
    /// `file_name` 取路径末段、时间戳占位、不写 FTS body）+ 一行 `document_vectors`
    /// （`embed_model` 取参数、`source_hash` 复用 `content_hash` 占位）。同一事务批量提交——
    /// 十万级种入为秒级。`content_hash=Some(..)` 让语义臂按身份去重（同 hash → 同组）。
    ///
    /// **仅供 evals 规模化基准**（BETA-38 §2.4：十万级 p95 延迟 + 内存对比暴力重载）：
    /// 生产索引走 [`Self::index_dirs`] + [`Self::embed_pending`]，绝不调用本方法。
    pub fn seed_synthetic_vectors(
        &self,
        docs: &[(String, Option<String>, Vec<f32>)],
        embed_model: &str,
    ) -> Result<(), IndexError> {
        let tx = self.conn.unchecked_transaction()?;
        let now = unix_now();
        {
            let mut doc_stmt = tx.prepare(
                "INSERT INTO documents
                     (path, file_name, title, author, doc_type, page_count, modified_time, indexed_time, content_hash)
                 VALUES (?1,?2,NULL,NULL,'synthetic',NULL,?3,?3,?4)",
            )?;
            let mut vec_stmt = tx.prepare(
                "INSERT INTO document_vectors(doc_id, dim, vector, embed_model, source_hash, embedded_time)
                 VALUES (?1,?2,?3,?4,?5,?6)",
            )?;
            for (path, content_hash, vector) in docs {
                let file_name = Path::new(path)
                    .file_name()
                    .map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned());
                doc_stmt.execute(params![path, file_name, now, content_hash])?;
                let id = tx.last_insert_rowid();
                let blob = crate::vectors::vector_to_blob(vector);
                // usize→i64 实际不会失败（非 128bit 平台）；保守取 MAX 避免 dim 与 BLOB 长度矛盾。
                let dim = i64::try_from(vector.len()).unwrap_or(i64::MAX);
                // source_hash NOT NULL：合成语料无正文指纹，复用 content_hash 占位（空亦合法）。
                let src = content_hash.clone().unwrap_or_default();
                vec_stmt.execute(params![id, dim, blob, embed_model, src, now])?;
            }
        }
        if !docs.is_empty() {
            bump_vector_generation(&tx)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// 当前向量是否与给定 (model, hash) 一致（调用方据此跳过重嵌）。
    pub fn vector_is_current(
        &self,
        path: &str,
        embed_model: &str,
        source_hash: &str,
    ) -> Result<bool, IndexError> {
        let Some(id) = self.doc_id_of(path)? else {
            return Ok(false);
        };
        let hit: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM document_vectors
                     WHERE doc_id=?1 AND embed_model=?2 AND source_hash=?3",
                params![id, embed_model, source_hash],
                |r| r.get(0),
            )
            .optional()?;
        Ok(hit.is_some())
    }

    /// 全部候选向量（path + 反序列化向量）。语义检索暴力扫描用。
    /// 损坏/维度异常的 BLOB 跳过（不致命）。
    pub fn candidate_vectors(&self) -> Result<Vec<CandidateVector>, IndexError> {
        self.candidate_vectors_where(None)
    }

    /// 只读取当前嵌入模型和维度的候选，防换模期间把不兼容向量混进一次 cosine 扫描。
    pub fn candidate_vectors_for_model_dim(
        &self,
        embed_model: &str,
        dim: usize,
    ) -> Result<Vec<CandidateVector>, IndexError> {
        self.candidate_vectors_where(Some((embed_model, dim)))
    }

    fn candidate_vectors_where(
        &self,
        model_dim: Option<(&str, usize)>,
    ) -> Result<Vec<CandidateVector>, IndexError> {
        let sql = if model_dim.is_some() {
            "SELECT d.path, v.vector, d.content_hash FROM document_vectors v
                 JOIN documents d ON d.id = v.doc_id
                 WHERE v.embed_model=?1 AND v.dim=?2"
        } else {
            "SELECT d.path, v.vector, d.content_hash FROM document_vectors v
                 JOIN documents d ON d.id = v.doc_id"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let mut rows = match model_dim {
            Some((model, dim)) => {
                let dim = i64::try_from(dim).unwrap_or(i64::MAX);
                stmt.query(params![model, dim])?
            }
            None => stmt.query([])?,
        };
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let path: String = r.get(0)?;
            let blob: Vec<u8> = r.get(1)?;
            let content_hash: Option<String> = r.get(2)?;
            if let Some(vector) = crate::vectors::blob_to_vector(&blob) {
                out.push(CandidateVector {
                    path,
                    vector,
                    content_hash,
                });
            }
        }
        Ok(out)
    }

    fn doc_id_of(&self, path: &str) -> Result<Option<i64>, IndexError> {
        let id = self
            .conn
            .query_row("SELECT id FROM documents WHERE path = ?1", [path], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(id)
    }

    /// BETA-38：取某文档的 `content_hash`（文件身份指纹）。无该文档 / 列为 NULL → `None`。
    pub fn content_hash_of(&self, path: &str) -> Result<Option<String>, IndexError> {
        let row = self
            .conn
            .query_row(
                "SELECT content_hash FROM documents WHERE path = ?1",
                [path],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?;
        // 外层 optional() = 文档不存在；内层 Option = 列 NULL。两者都归一为 None。
        Ok(row.flatten())
    }

    /// BETA-33 cycle 4：取某文档的 (body, doc_type) 一起（嵌入判定用）。
    /// 无该文档 → None。doc_type 大小写与 upsert 时一致（`image_entry` 已 lowercased）。
    ///
    /// 取代 cycle 3 `body_of`（只返 body）：cycle 4 图片 doc_type 也参与跳过判定、
    /// 一次 JOIN 一起取更简。
    fn body_and_doctype_of(&self, path: &str) -> Result<Option<(String, String)>, IndexError> {
        let row = self
            .conn
            .query_row(
                "SELECT IFNULL(f.body, ''), d.doc_type \
                 FROM documents d JOIN documents_fts f ON f.rowid = d.id \
                 WHERE d.path = ?1",
                [path],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// 补嵌 `roots` 子树下缺向量 / 陈旧向量的文档。**不建 FTS**（调用方先跑 `index_dirs`）。
    /// 先数出待嵌总数，再逐篇嵌入，每篇回调 `progress(done, total)`（done 含本篇）。
    /// **单篇 embed 失败**计入 failed、跳过、不中断（文档仍 FTS 可搜，镜像 catch_extract 哲学）；
    /// **DB 写失败**（upsert_vector）向上传播中断整轮。返回 `(成功嵌入篇数, 失败篇数)`。
    ///
    /// BETA-39（2026-07-03）：`embed_images` 由「图片语义索引」opt-in 设置驱动——
    /// `false`（默认）图片 doc_type 直跳（BETA-33 cycle 4 现状）；`true` 图片改走更严的
    /// [`crate::embed::is_image_embed_worthy`]（0.75 门槛）过了才入嵌。非图片文档两种取值
    /// 下行为完全一致（仍走 `is_embed_worthy`）。
    /// BETA-38 cycle 2：返回 `(embedded, reused, failed)`——`reused` 为副本去重命中数
    /// （同 `content_hash` 已有当前模型向量、直接复制而非重新 embed）。
    pub fn embed_pending(
        &self,
        roots: &[std::path::PathBuf],
        embedder: &dyn crate::embed::TextEmbedder,
        embed_images: bool,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Result<(usize, usize, usize), IndexError> {
        // BETA-64 T1：耗时埋点——embedding 目前是四阶段里唯一逐篇串行、每次新建 llama.cpp
        // context 的阶段，先测量占比再评估 P1 的 pipeline 化/批量推理是否值得投入。
        let run_start = std::time::Instant::now();
        let root_strs: Vec<String> = roots
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();

        // 先收集待嵌（vector_is_current 未命中）的 (path, truncated, hash)，得 total 供进度。
        // BETA-31-v3 cycle 3：is_embed_worthy 守门——body 极短文档不嵌，避免空字符串向量
        // 产生 "neutral" 高 cosine 污染 ranker top-N（v0.8.3 用户「读后感」搜索 bug 根因）。
        // BETA-33 cycle 4（2026-07-01）：**图片 doc_type 一律跳过语义嵌入**。tesseract 对
        // QQ/微信/浏览器缓存图产出的 OCR 文本 CJK 密度高但语义空、走 is_embed_worthy 挡不住，
        // 与 query 落在中文均值方向 cosine 0.5-0.7 污染语义召回（v0.9.4 用户搜「作文」踩到
        // face-3-efdc54.png 表情包段落级 0.62「强相关」即此 bug）。图片保留 FTS 字面命中能力。
        // 待嵌四元组 `(path, truncated, source_hash, content_hash)`；content_hash（文件身份）
        // 供 BETA-38 cycle 2 索引期副本去重（同身份只嵌一次、其余复制向量）。
        let mut pending: Vec<(String, String, String, Option<String>)> = Vec::new();
        for path in self.paths_under(&root_strs)? {
            let Some((body, doc_type)) = self.body_and_doctype_of(&path)? else {
                continue;
            };
            // B 层：图片 OCR 类型默认直接跳过（IMAGE_EXTS 与 image_entry 写入的 doc_type
            // 都小写）；BETA-39 opt-in 开启时改走更严的图片专属门槛（0.75）。
            if crate::scan::IMAGE_EXTS.contains(&doc_type.as_str()) {
                if !embed_images || !crate::embed::is_image_embed_worthy(&body) {
                    continue;
                }
            } else if !crate::embed::is_embed_worthy(&body) {
                continue;
            }
            let truncated =
                crate::embed::truncate_chars(&body, crate::embed::EMBED_TRUNCATE_CHARS).to_owned();
            let hash = crate::embed::content_hash(&truncated);
            if self.vector_is_current(&path, embedder.model_id(), &hash)? {
                continue;
            }
            let content_hash = self.content_hash_of(&path)?;
            pending.push((path, truncated, hash, content_hash));
        }
        let total = pending.len();

        let mut embed_count = 0usize;
        let mut reused_count = 0usize;
        let mut embed_failed = 0usize;
        for (i, (path, truncated, hash, content_hash)) in pending.iter().enumerate() {
            // BETA-38 cycle 2：副本去重——若已有同 content_hash 文档持当前模型向量，直接复制
            // （相同字节 → 相同正文 → 相同截断 → 相同向量，复制是精确非近似）。序列处理，故
            // 同一轮内多副本：首篇真嵌、后续 upsert 后即可被下一篇查到并复用。
            if let Some(ch) = content_hash {
                if let Some(vector) = self.vector_for_content_hash(ch, embedder.model_id())? {
                    self.upsert_vector(path, &vector, embedder.model_id(), hash)?;
                    reused_count += 1;
                    progress(i + 1, total);
                    continue;
                }
            }
            // BETA-31-v3 cycle 4：per-doc info 日志。如果 embedder.embed() 触发 llama-cpp
            // native crash (ucrtbase 0xc0000409)、整个进程被杀；下次 SCOUT_ENABLE_EMBED=1
            // 重试时、scout.log 最后一行「即将 embed」就是触发 crash 的那个文档（path +
            // body 长度），为真修提供锁定输入。tracing facade 由调用方挂 subscriber、桌面
            // app 已经 wired tracing-appender 写文件、daemon 走 stdout。
            tracing::info!(
                doc_idx = i + 1,
                total,
                path = %path,
                body_len_chars = truncated.chars().count(),
                "即将 embed 文档"
            );
            match embedder.embed(truncated) {
                Ok(vector) => {
                    self.upsert_vector(path, &vector, embedder.model_id(), hash)?;
                    embed_count += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        doc_idx = i + 1,
                        total,
                        path = %path,
                        error = %e,
                        "embed 失败、跳过该文档"
                    );
                    embed_failed += 1;
                }
            }
            progress(i + 1, total);
        }
        tracing::info!(
            total,
            embedded = embed_count,
            reused = reused_count,
            failed = embed_failed,
            total_ms = u64::try_from(run_start.elapsed().as_millis()).unwrap_or(u64::MAX),
            "embed_pending 完成"
        );
        Ok((embed_count, reused_count, embed_failed))
    }

    /// BETA-38 cycle 2：找一条与给定 `content_hash` 相同、且已持 `model_id` 当前向量的文档向量
    /// （复制用）。无则 `None`。索引期副本去重：同内容文件只 embed 一次，其余直接复用向量。
    fn vector_for_content_hash(
        &self,
        content_hash: &str,
        model_id: &str,
    ) -> Result<Option<Vec<f32>>, IndexError> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT v.vector FROM document_vectors v
                 JOIN documents d ON d.id = v.doc_id
                 WHERE d.content_hash = ?1 AND v.embed_model = ?2
                 LIMIT 1",
                params![content_hash, model_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(blob.and_then(|b| crate::vectors::blob_to_vector(&b)))
    }
}

/// [`DocumentIndex::upsert_document_with_pages`] / [`DocumentIndex::upsert_documents_with_pages`]
/// （BETA-64 T2）共用的核心 SQL 逻辑：在调用方已开的事务 `tx` 内插入或更新一条文档 +
/// 段落 + 失败页三表，不负责开启/提交事务（由调用方决定是每条各自一个事务、还是一批
/// 共用一个事务）。逻辑与改造前 `upsert_document_with_pages` 内联版逐字节一致。
fn upsert_document_with_pages_tx(
    tx: &rusqlite::Transaction<'_>,
    e: &DocumentEntry,
    body: &str,
    passages: &[PagePassage],
    failed_pages: &[PageFailure],
) -> Result<bool, IndexError> {
    let now = unix_now();

    let existing: Option<i64> = tx
        .query_row("SELECT id FROM documents WHERE path = ?1", [&e.path], |r| {
            r.get(0)
        })
        .optional()?;

    let id = if let Some(id) = existing {
        tx.execute(
            "UPDATE documents SET file_name=?2, title=?3, author=?4, doc_type=?5,
                 page_count=?6, modified_time=?7, indexed_time=?8, content_hash=?9
             WHERE id=?1",
            params![
                id,
                e.file_name,
                e.title,
                e.author,
                e.doc_type,
                e.page_count,
                e.modified_time,
                now,
                e.content_hash
            ],
        )?;
        id
    } else {
        tx.execute(
            "INSERT INTO documents
                 (path, file_name, title, author, doc_type, page_count, modified_time, indexed_time, content_hash)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                e.path,
                e.file_name,
                e.title,
                e.author,
                e.doc_type,
                e.page_count,
                e.modified_time,
                now,
                e.content_hash
            ],
        )?;
        tx.last_insert_rowid()
    };

    // FTS 刷 body：正文原样进 body 列。PII 类型概念词（身份证/手机号）单独进 entity 列
    // （BETA-59 重构）——只写类型词、绝不复制号码明文。`query` 用裸 `documents_fts MATCH`
    // 自动跨所有列，「身份证」等概念词照样命中 entity；而 `snippet()` 固定打 body 列
    // （index 2）、永不回显 entity 关键词，彻底隔离"可搜的类型标签"与"展示的正文"。
    // 存量索引 entity 列迁移后为空、待下次内容变更增量重抽时回填（body 已保、搜索不受影响）。
    let entity = pii_entity_keywords(body);
    tx.execute("DELETE FROM documents_fts WHERE rowid = ?1", [id])?;
    tx.execute(
        "INSERT INTO documents_fts(rowid, title, author, body, entity) VALUES (?1,?2,?3,?4,?5)",
        params![id, e.title, e.author, body, entity],
    )?;

    // 幂等：先清后写。空 vec 时 INSERT 循环空转、DELETE 空表安全。
    tx.execute("DELETE FROM document_passages WHERE doc_id = ?1", [id])?;
    for p in passages {
        tx.execute(
            "INSERT INTO document_passages(doc_id, page_no, seq, text)
             VALUES (?1,?2,?3,?4)",
            params![id, p.page_no, p.seq, p.text],
        )?;
    }
    tx.execute("DELETE FROM document_failed_pages WHERE doc_id = ?1", [id])?;
    for f in failed_pages {
        tx.execute(
            "INSERT INTO document_failed_pages(doc_id, page_no, reason, failed_time)
             VALUES (?1,?2,?3,?4)",
            params![id, f.page_no, f.reason, now],
        )?;
    }

    Ok(existing.is_none())
}

impl IncrementalStore for DocumentIndex {
    /// 文档提取结果（BETA-35 cycle 4：升级为 [`ExtractedDoc`]，附带段落 / 失败页）。
    type Entry = ExtractedDoc;

    fn modified_time_of(&self, path: &str) -> Result<Option<i64>, IndexError> {
        self.modified_time_of_impl(path)
    }

    fn upsert_entry(&self, entry: &Self::Entry) -> Result<bool, IndexError> {
        self.upsert_document_with_pages(
            &entry.entry,
            &entry.body,
            &entry.passages,
            &entry.failed_pages,
        )
    }

    fn upsert_entries(&self, entries: &[Self::Entry]) -> Result<Vec<bool>, IndexError> {
        self.upsert_documents_with_pages(entries)
    }

    fn paths_under(&self, roots: &[String]) -> Result<Vec<String>, IndexError> {
        self.paths_under_impl(roots)
    }

    fn modified_times_under(&self, roots: &[String]) -> Result<HashMap<String, i64>, IndexError> {
        self.modified_times_under_impl(roots)
    }

    fn delete_by_path(&self, path: &str) -> Result<bool, IndexError> {
        self.delete_by_path_impl(path)
    }

    fn record_extract_failure(
        &self,
        path: &str,
        reason: &str,
        modified_time: i64,
    ) -> Result<(), IndexError> {
        self.record_extract_failure_impl(path, reason, modified_time)
    }

    fn clear_extract_failure(&self, path: &str) -> Result<(), IndexError> {
        self.clear_extract_failure_impl(path)
    }

    fn failure_paths_under(&self, roots: &[String]) -> Result<Vec<String>, IndexError> {
        self.failure_paths_under_impl(roots)
    }

    fn all_failure_modified_times(&self) -> Result<HashMap<String, i64>, IndexError> {
        self.all_failure_modified_times_impl()
    }
}

/// BETA-38：老库 `documents` 表缺 `content_hash` 列时 ALTER ADD（幂等——列已在则 no-op），
/// 随后建 `content_hash` 索引。**列必须在建索引之前存在**——故索引不放 `SCHEMA`（老库跳过
/// `CREATE TABLE`、列尚未 ALTER 时建索引会 `no such column` 崩），统一在此列就绪后建。
/// 列可空、无默认值——老行 `content_hash` 保持 NULL，下次内容变更增量索引时回填。
fn migrate_documents_content_hash(conn: &Connection) -> Result<(), IndexError> {
    let mut stmt = conn.prepare("PRAGMA table_info(documents)")?;
    // table_info 第 1 列（index 1）是列名。
    let mut has_col = false;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for name in names {
        if name? == "content_hash" {
            has_col = true;
            break;
        }
    }
    if !has_col {
        conn.execute_batch("ALTER TABLE documents ADD COLUMN content_hash TEXT;")?;
    }
    // doc identity：按文件原始字节指纹检索副本（索引期去重 + 结果期合并留痕）。
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_documents_content_hash ON documents(content_hash);",
    )?;
    Ok(())
}

/// 2026-07-29：老库 `index_failures` 表缺 `modified_time` 列时 ALTER ADD（幂等，同
/// [`migrate_documents_content_hash`] 套路）。`DEFAULT 0` 而非可空——老留痕没有失败时的
/// mtime 可回填，`0` 是安全哨兵值：真实文件 mtime（unix 秒）不可能是 0，比对时必然不等，
/// 等价于"老留痕升级后至少再重试一次"，不会把升级前的失败静默永久跳过。
fn migrate_index_failures_modified_time(conn: &Connection) -> Result<(), IndexError> {
    let mut stmt = conn.prepare("PRAGMA table_info(index_failures)")?;
    let mut has_col = false;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for name in names {
        if name? == "modified_time" {
            has_col = true;
            break;
        }
    }
    if !has_col {
        conn.execute_batch(
            "ALTER TABLE index_failures ADD COLUMN modified_time INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    Ok(())
}

/// BETA-59：老库 `documents_fts` 只有 `title/author/body` 三列时，加 `entity` 末列
/// （PII 类型概念词专列，与展示正文隔离）。幂等——已含 entity 列则 no-op。
///
/// **为何不能照搬 [`migrate_music_fts`] 从主表重建**：`music_fts` 的所有列在 `music`
/// 主表都有备份、可 `INSERT ... SELECT FROM music` 重灌；而 `documents_fts.body` 是
/// **正文唯一存处**（主表不存），drop 即丢正文、搜索全灭。故这里先把旧三列内容拷进
/// 新四列表（`entity` 灌空串）、再 drop 旧表 rename——**body 逐行保留、零丢失**。
///
/// **迁移策略取舍（不 bump [`INDEXER_SCHEMA_VERSION`]）**：本迁移就地保住 body、升级
/// 用户无运行时崩（4 列 INSERT 只在迁移后才跑）、老文档照常可搜，仅 `entity` 列暂空、
/// 待下次内容变更增量重抽回填 PII 概念词——与 [`migrate_documents_content_hash`] /
/// [`migrate_music_fts`] 同属"透明加列"、均无 schema bump。反之 bump 版本会（T10
/// preflight 版本门生效后）逼所有 daemon 用户 `--allow-rebuild-schema` 全量重建，
/// 为一个出处片段观感优化付全库重抽代价、不划算。
fn migrate_documents_fts_entity(conn: &Connection) -> Result<(), IndexError> {
    if documents_fts_has_entity(conn)? {
        return Ok(());
    }
    conn.execute_batch(
        "CREATE VIRTUAL TABLE documents_fts_new USING fts5(title, author, body, entity, tokenize='trigram');
         INSERT INTO documents_fts_new(rowid, title, author, body, entity)
           SELECT rowid, title, author, body, '' FROM documents_fts;
         DROP TABLE documents_fts;
         ALTER TABLE documents_fts_new RENAME TO documents_fts;",
    )?;
    Ok(())
}

fn documents_fts_has_entity(conn: &Connection) -> Result<bool, IndexError> {
    let mut stmt = conn.prepare("PRAGMA table_info(documents_fts)")?;
    // table_info 第 1 列（index 1）是列名。
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for name in names {
        if name? == "entity" {
            return Ok(true);
        }
    }
    Ok(false)
}

fn row_to_hit(r: &rusqlite::Row<'_>, with_snippet: bool) -> rusqlite::Result<DocumentHit> {
    let entry = DocumentEntry {
        path: r.get(0)?,
        file_name: r.get(1)?,
        title: r.get(2)?,
        author: r.get(3)?,
        doc_type: r.get(4)?,
        page_count: r.get(5)?,
        modified_time: r.get(6)?,
        // 查询命中不回带身份指纹（去重在语义臂按 content_hash 做）。
        content_hash: None,
    };
    let snippet = if with_snippet { r.get(7)? } else { None };
    Ok(DocumentHit { entry, snippet })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn doc(path: &str, doc_type: &str, author: &str) -> DocumentEntry {
        DocumentEntry {
            path: path.to_string(),
            file_name: path.rsplit(['/', '\\']).next().unwrap_or(path).to_string(),
            title: Some("季度报告".to_string()),
            author: Some(author.to_string()),
            doc_type: doc_type.to_string(),
            page_count: Some(3),
            modified_time: 1000,
            content_hash: None,
        }
    }

    /// 合成一枚校验位合法的身份证号（地区码 + 生日 + 顺序码 17 位 → 按 GB 11643 算末位）。
    fn synth_id_card(prefix17: &str) -> String {
        let weights = [7_u32, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
        let check_codes = ['1', '0', 'X', '9', '8', '7', '6', '5', '4', '3', '2'];
        let sum: u32 = prefix17
            .bytes()
            .zip(weights)
            .map(|(b, weight)| u32::from(b - b'0') * weight)
            .sum();
        format!(
            "{prefix17}{}",
            check_codes[usize::try_from(sum % 11).unwrap()]
        )
    }

    #[test]
    fn open_in_memory_starts_empty() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        assert_eq!(idx.count().unwrap(), 0);
    }

    /// 2026-07-29：`optimize_fts` 在空库、有数据库上都不出错；调用后查询仍能命中。
    #[test]
    fn optimize_fts_is_safe_on_empty_and_populated_db() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        idx.optimize_fts().unwrap();

        idx.upsert_document(&doc("/d/a.docx", "docx", "张三"), "本季度预算与营收分析。")
            .unwrap();
        idx.optimize_fts().unwrap();

        let hits = idx
            .query(&DocumentQuery {
                text: Some("季度预算".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1, "optimize 后查询仍正常命中");
    }

    #[test]
    fn fts_matches_body_cjk_and_returns_snippet() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        idx.upsert_document(
            &doc("/d/a.docx", "docx", "张三"),
            "本季度预算与营收分析，季度预算同比增长。",
        )
        .unwrap();
        let hits = idx
            .query(&DocumentQuery {
                text: Some("季度预算".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.doc_type, "docx");
        assert!(hits[0].snippet.is_some(), "文本查询应返回片段");
        assert!(
            hits[0].snippet.as_deref().unwrap().contains('['),
            "片段应含命中标记"
        );
    }

    /// 2026-07-06 真机沉淀（准考证 PNG）：OCR 把 `15013866763` 识成 `1 S013866763`、
    /// 用户按真号码（或其 ≥3 位子串）搜零命中。经 [`crate::finalize_ocr_text`] 追加
    /// 数字校正变体后，正确号码与其子串均可 FTS 命中，原始误识文本也仍可搜。
    #[test]
    fn fts_matches_ocr_digit_corrected_body() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        let body = crate::finalize_ocr_text("会员手机 1 S013866763 会员账号 440307201312314812");
        idx.upsert_document(&doc("/d/准考证.png", "png", "x"), &body)
            .unwrap();
        for q in ["15013866763", "150138", "S013866763"] {
            let hits = idx
                .query(&DocumentQuery {
                    text: Some(q.to_string()),
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(hits.len(), 1, "查询 {q:?} 应命中校正后的 OCR 正文");
        }
    }

    #[test]
    fn pii_identity_card_keyword_injection_hits_concept_query() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        // 合成身份证号：地区码 + 生日 + 顺序码，末位按 GB 11643 校验位计算。
        let prefix17 = "11010519900101123";
        let weights = [7_u32, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
        let check_codes = ['1', '0', 'X', '9', '8', '7', '6', '5', '4', '3', '2'];
        let sum: u32 = prefix17
            .bytes()
            .zip(weights)
            .map(|(b, weight)| u32::from(b - b'0') * weight)
            .sum();
        let card = format!(
            "{prefix17}{}",
            check_codes[usize::try_from(sum % 11).unwrap()]
        );
        idx.upsert_document(
            &doc("/d/准考证.png", "png", "x"),
            &format!("报名信息 {card}，正文不含证件类型词。"),
        )
        .unwrap();

        let hits = idx
            .query(&DocumentQuery {
                text: Some("身份证".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1, "概念词应经 PII 类型关键词命中文档");
        assert_eq!(hits[0].entry.file_name, "准考证.png");
    }

    /// BETA-59：PII 类型概念词写进独立 `entity` 列而非 body——「身份证」仍经 `MATCH`
    /// 跨列命中，但 `snippet()`（固定 body 列）绝不回显注入的类型关键词；`preview` 取回的
    /// 正文也不含关键词尾巴。构造正文**无任何字面证件标签**、只有合成校验位身份证号。
    #[test]
    fn pii_keywords_land_in_entity_not_body_snippet_stays_clean() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        let card = synth_id_card("11010519900101123");
        // 正文只有裸号码、无「身份证」「证件」等字面标签。
        let body = format!("报名表 姓名王五 编号 {card} 备注无。");
        idx.upsert_document(&doc("/d/报名表.png", "png", "x"), &body)
            .unwrap();

        // 「身份证」经 entity 列 MATCH 命中。
        let hits = idx
            .query(&DocumentQuery {
                text: Some("身份证".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1, "概念词应经 entity 列命中");
        // 命中片段固定取自 body 列——正文无字面标签、故片段绝不含注入的类型关键词。
        let snip = hits[0].snippet.as_deref().unwrap_or("");
        for kw in ["身份证", "身份证号", "证件号", "identity_card"] {
            assert!(
                !snip.contains(kw),
                "snippet 不应回显 entity 注入词 {kw:?}，实际片段：{snip:?}"
            );
        }

        // preview 取回的完整正文同样只有原始 body、无关键词尾巴。
        let preview = idx
            .preview_for_path("/d/报名表.png", Some("\"身份证\""))
            .unwrap()
            .unwrap();
        assert_eq!(preview.body, body, "正文列应原样保存、不夹带 entity 关键词");
        for kw in ["身份证号", "证件号", "identity_card"] {
            assert!(
                !preview.body.contains(kw),
                "preview 正文不应含注入词 {kw:?}"
            );
        }
    }

    /// BETA-59：老库 `documents_fts`（3 列 title/author/body）打开时自动迁移到 4 列
    /// （加 entity）——**body 逐行保留**（老正文仍可搜）、迁移后新写入的 PII 概念词经
    /// entity 列可搜且 snippet 干净。防"drop 丢正文"与"列数不匹配运行时崩"双回归。
    #[test]
    fn old_three_col_documents_fts_migrates_to_entity_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy_fts.db");
        // 手工建"老库"：documents_fts 故意只有 3 列 + 一条老正文。
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE documents (
                   id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, file_name TEXT NOT NULL,
                   title TEXT, author TEXT, doc_type TEXT NOT NULL, page_count INTEGER,
                   modified_time INTEGER NOT NULL, indexed_time INTEGER NOT NULL
                 );
                 CREATE VIRTUAL TABLE documents_fts USING fts5(title, author, body, tokenize='trigram');
                 INSERT INTO documents(path, file_name, doc_type, modified_time, indexed_time)
                   VALUES ('/old/a.txt', 'a.txt', 'txt', 1, 1);
                 INSERT INTO documents_fts(rowid, title, author, body)
                   SELECT id, title, author, '本季度营收分析老正文关键词' FROM documents WHERE path='/old/a.txt';",
            )
            .unwrap();
        } // drop 落盘

        // 打开 → 自动加 entity 列、老正文 body 保留。
        let idx = DocumentIndex::open(&path).unwrap();
        assert!(
            documents_fts_has_entity(&idx.conn).unwrap(),
            "迁移后应有 entity 列"
        );
        let old = idx
            .query(&DocumentQuery {
                text: Some("营收分析老正文".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(old.len(), 1, "老正文 body 迁移后仍可搜（未丢失）");

        // 迁移后新写入的 PII 文档：概念词经 entity 命中、snippet 不回显。
        let card = synth_id_card("11010519900101123");
        idx.upsert_document(
            &doc("/new/报名.png", "png", "x"),
            &format!("编号 {card} 无字面标签。"),
        )
        .unwrap();
        let hits = idx
            .query(&DocumentQuery {
                text: Some("身份证".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1, "迁移后新文档的 PII 概念词应经 entity 命中");
        let snip = hits[0].snippet.as_deref().unwrap_or("");
        assert!(
            !snip.contains("identity_card"),
            "snippet 不应回显 entity 注入词"
        );
    }

    #[test]
    fn fts_matches_title_and_author() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        // doc() 的 title="季度报告"，author 用 ≥3 字符（trigram 限制）。
        idx.upsert_document(&doc("/d/a.pdf", "pdf", "李四明"), "正文内容")
            .unwrap();
        // 命中 title 列（FTS）。
        let by_title = idx
            .query(&DocumentQuery {
                text: Some("季度报告".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_title.len(), 1, "应经 title 列 FTS 命中");
        // 命中 author 列（FTS，≥3 字符）。
        let by_author = idx
            .query(&DocumentQuery {
                text: Some("李四明".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_author.len(), 1, "应经 author 列 FTS 命中");
    }

    /// BETA-56（2026-07-08 真机沉淀）：2 字中文查询（人名「燎原」）经 trigram FTS 必 0 命中，
    /// 短查询 metadata LIKE 兜底应命中 author 含它的文档；≥3 字（「饶燎原」）仍走 FTS。
    #[test]
    fn short_cjk_query_hits_author_via_like_fallback() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        idx.upsert_document(&doc("/d/合同.docx", "docx", "燎原 饶"), "正文与人名无关")
            .unwrap();
        idx.upsert_document(&doc("/d/other.docx", "docx", "张三"), "无关内容")
            .unwrap();

        // 2 字查询走 LIKE 兜底命中 author。
        let by_short = idx
            .query(&DocumentQuery {
                text: Some("燎原".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_short.len(), 1, "2 字人名应经 LIKE 兜底命中 author");
        assert!(by_short[0].entry.path.ends_with("合同.docx"));
        // 兜底路径不产 FTS 片段。
        assert!(by_short[0].snippet.is_none());

        // 多段全短词（「燎原」+「饶」）→ 组间 AND，两子串都需在同一文档 metadata 出现。
        let by_two = idx
            .query(&DocumentQuery {
                text: Some("燎原 饶".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_two.len(), 1, "全短词多段经 LIKE 兜底 AND 命中 author");
    }

    /// BETA-56：短查询 LIKE 兜底覆盖 title / file_name；未命中任一 metadata 列 → 0。
    #[test]
    fn short_query_like_fallback_matches_title_and_file_name() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        // doc() 的 title="季度报告"；file_name 取路径末段。
        idx.upsert_document(&doc("/d/发票2024.pdf", "pdf", "无关"), "正文")
            .unwrap();

        // 命中 file_name「发票2024.pdf」中的「发票」。
        let by_fn = idx
            .query(&DocumentQuery {
                text: Some("发票".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_fn.len(), 1, "2 字应命中 file_name 子串");

        // 命中 title「季度报告」中的「季度」。
        let by_title = idx
            .query(&DocumentQuery {
                text: Some("季度".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_title.len(), 1, "2 字应命中 title 子串");

        // 三列都无「预算」→ 0（正文含「预算」也不兜底，body 不参与 LIKE）。
        idx.upsert_document(&doc("/d/x.pdf", "pdf", "无关"), "本季度预算说明")
            .unwrap();
        let none = idx
            .query(&DocumentQuery {
                text: Some("预算".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert!(none.is_empty(), "短查询兜底只扫 metadata、不扫 body");
    }

    /// BETA-56：短查询兜底仍尊重 doc_types 结构化过滤（LIKE 与 filters 复合）。
    #[test]
    fn short_query_like_fallback_respects_doc_type_filter() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        idx.upsert_document(&doc("/d/合同.png", "png", "燎原"), "x")
            .unwrap();
        idx.upsert_document(&doc("/d/合同.docx", "docx", "燎原"), "y")
            .unwrap();
        let only_img = idx
            .query(&DocumentQuery {
                text: Some("燎原".to_string()),
                doc_types: Some(vec!["png".to_string()]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(only_img.len(), 1, "doc_types 应框定兜底结果");
        assert_eq!(only_img[0].entry.doc_type, "png");
    }

    #[test]
    fn author_substring_and_doc_type_filter() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        idx.upsert_document(&doc("/d/a.docx", "docx", "Zhang San"), "x")
            .unwrap();
        idx.upsert_document(&doc("/d/b.pdf", "pdf", "Li Si"), "y")
            .unwrap();
        let by_author = idx
            .query(&DocumentQuery {
                author: Some("zhang".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_author.len(), 1);
        let by_type = idx
            .query(&DocumentQuery {
                doc_type: Some("PDF".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_type.len(), 1);
        assert_eq!(by_type[0].entry.doc_type, "pdf");
        // 结构化查询无 snippet。
        assert!(by_type[0].snippet.is_none());
    }

    #[test]
    fn doc_types_filter_restricts_to_set() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        idx.upsert_document(&doc("/d/shot.png", "png", "A"), "会议纪要正文")
            .unwrap();
        idx.upsert_document(&doc("/d/report.docx", "docx", "A"), "会议纪要正文")
            .unwrap();
        // 限定图片类型集合 → 只返 png（docx 被过滤）。
        let only_img = idx
            .query(&DocumentQuery {
                doc_types: Some(vec!["png".to_string(), "jpg".to_string()]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(only_img.len(), 1);
        assert_eq!(only_img[0].entry.doc_type, "png");
        // 文本 + doc_types 联用：FTS 命中两条但类型框定只返 png。
        let img_text = idx
            .query(&DocumentQuery {
                text: Some("会议纪要".to_string()),
                doc_types: Some(vec!["png".to_string()]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(img_text.len(), 1);
        assert_eq!(img_text[0].entry.doc_type, "png");
        // None = 不限 → 两条都返。
        let all = idx.query(&DocumentQuery::default()).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn limit_truncates() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        for i in 0..5 {
            idx.upsert_document(&doc(&format!("/d/{i}.txt"), "txt", "A"), "body")
                .unwrap();
        }
        let out = idx
            .query(&DocumentQuery {
                limit: Some(2),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn reupsert_refreshes_fts_body() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        assert!(idx
            .upsert_document(&doc("/d/a.docx", "docx", "A"), "旧正文关键词")
            .unwrap());
        assert!(!idx
            .upsert_document(&doc("/d/a.docx", "docx", "A"), "新正文关键词")
            .unwrap());
        assert_eq!(idx.count().unwrap(), 1);
        let new_hit = idx
            .query(&DocumentQuery {
                text: Some("新正文关键词".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(new_hit.len(), 1);
        let old_hit = idx
            .query(&DocumentQuery {
                text: Some("旧正文关键词".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert!(old_hit.is_empty(), "旧正文应已从 FTS 移除");
    }

    #[test]
    fn delete_removes_from_fts() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        idx.upsert_document(&doc("/d/a.docx", "docx", "A"), "机密报告内容")
            .unwrap();
        assert!(idx.delete_by_path("/d/a.docx").unwrap());
        assert_eq!(idx.count().unwrap(), 0);
        let hits = idx
            .query(&DocumentQuery {
                text: Some("机密报告".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert!(hits.is_empty());
        assert!(!idx.delete_by_path("/d/a.docx").unwrap());
    }

    #[test]
    fn fts_sanitize_handles_syntax_chars() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        idx.upsert_document(&doc("/d/a.txt", "txt", "A"), "body")
            .unwrap();
        let out = idx
            .query(&DocumentQuery {
                text: Some("a\" OR b *".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn preview_for_path_returns_body_and_meta() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        idx.upsert_document(
            &doc("/d/a.docx", "docx", "张三"),
            "本季度预算与营收分析报告正文",
        )
        .unwrap();
        // 无 fts_match → 取元信息 + 完整正文，无片段。
        let p = idx.preview_for_path("/d/a.docx", None).unwrap().unwrap();
        assert_eq!(p.entry.doc_type, "docx");
        assert_eq!(p.entry.title.as_deref(), Some("季度报告"));
        assert!(p.body.contains("营收分析"), "应取回完整正文");
        assert!(p.snippet.is_none(), "无 fts_match 不产片段");
        // 不存在的路径 → None。
        assert!(idx
            .preview_for_path("/d/none.docx", None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn preview_for_path_highlights_hit_with_sentinels() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        idx.upsert_document(
            &doc("/d/a.docx", "docx", "张三"),
            "本季度预算与营收分析报告正文",
        )
        .unwrap();
        // 提供命中表达式 → 片段含 \x02/\x03 哨兵高亮命中词。
        let p = idx
            .preview_for_path("/d/a.docx", Some("\"季度预算\""))
            .unwrap()
            .unwrap();
        let snip = p.snippet.expect("命中应产片段");
        assert!(
            snip.contains('\u{2}') && snip.contains('\u{3}'),
            "片段应含高亮哨兵"
        );
        assert!(snip.contains("季度预算"));
        // 不命中的表达式 → 片段 None（但元信息仍返回）。
        let p2 = idx
            .preview_for_path("/d/a.docx", Some("\"完全不相关词\""))
            .unwrap()
            .unwrap();
        assert!(p2.snippet.is_none(), "不命中 → 无片段");
        assert!(!p2.body.is_empty(), "正文不受片段是否命中影响");
    }

    #[test]
    fn paths_under_filters_by_root() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        idx.upsert_document(&doc("/docs/a.txt", "txt", "A"), "x")
            .unwrap();
        idx.upsert_document(&doc("/other/b.txt", "txt", "B"), "y")
            .unwrap();
        let under = idx.paths_under(&["/docs".to_string()]).unwrap();
        assert_eq!(under, vec!["/docs/a.txt".to_string()]);
    }

    /// BETA-64 T7a：文档索引批量 mtime 预取，root 过滤口径同 `paths_under`。
    #[test]
    fn modified_times_under_filters_by_root_and_carries_mtime() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        idx.upsert_document(&doc("/docs/a.txt", "txt", "A"), "x")
            .unwrap();
        idx.upsert_document(&doc("/other/b.txt", "txt", "B"), "y")
            .unwrap();
        let map = idx.modified_times_under(&["/docs".to_string()]).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("/docs/a.txt"), Some(&1000));
        assert!(!map.contains_key("/other/b.txt"));
    }

    #[test]
    fn upsert_and_load_vector_round_trips() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        idx.upsert_document(&doc("/d/a.docx", "docx", "张三"), "正文")
            .unwrap();
        let v = vec![0.1f32, 0.2, 0.3];
        assert!(
            idx.upsert_vector("/d/a.docx", &v, "qwen3-emb-0.6b", "hash1")
                .unwrap(),
            "有文档行→写入返回 true"
        );
        let loaded = idx.candidate_vectors().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].path, "/d/a.docx");
        assert_eq!(loaded[0].vector, v);
    }

    /// BETA-38 cycle 4：`seed_synthetic_vectors` 批量种入——文档行 + 向量齐全、
    /// content_hash 随向量回带（供语义臂身份去重）、同 hash 两副本各成候选一行。
    #[test]
    fn seed_synthetic_vectors_populates_docs_and_vectors() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        let seeds = vec![
            ("/s/a.txt".to_owned(), Some("h1".to_owned()), vec![1.0, 0.0]),
            // 与 a 同身份的副本（同 content_hash、同向量）。
            (
                "/s/a_copy.txt".to_owned(),
                Some("h1".to_owned()),
                vec![1.0, 0.0],
            ),
            ("/s/b.txt".to_owned(), Some("h2".to_owned()), vec![0.0, 1.0]),
            // 老库风格：无 content_hash。
            ("/s/c.txt".to_owned(), None, vec![0.5, 0.5]),
        ];
        idx.seed_synthetic_vectors(&seeds, "synth-model").unwrap();

        assert_eq!(idx.count().unwrap(), 4, "四条文档行");
        assert_eq!(idx.vector_count().unwrap(), 4, "四条向量行");
        let cands = idx.candidate_vectors().unwrap();
        assert_eq!(cands.len(), 4);
        // content_hash 随候选回带（None 亦保留）。
        let a = cands.iter().find(|c| c.path == "/s/a.txt").unwrap();
        assert_eq!(a.content_hash.as_deref(), Some("h1"));
        assert_eq!(a.vector, vec![1.0, 0.0]);
        let a_copy = cands.iter().find(|c| c.path == "/s/a_copy.txt").unwrap();
        assert_eq!(a_copy.content_hash.as_deref(), Some("h1"), "副本同身份");
        let c = cands.iter().find(|c| c.path == "/s/c.txt").unwrap();
        assert!(c.content_hash.is_none(), "无身份候选保留 None");
    }

    #[test]
    fn upsert_vector_for_unknown_path_is_noop() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        let inserted = idx
            .upsert_vector("/d/missing.docx", &[1.0], "m", "h")
            .unwrap();
        assert!(!inserted, "无文档行时不写向量");
        assert!(idx.candidate_vectors().unwrap().is_empty());
    }

    #[test]
    fn vector_hash_lets_caller_skip_reembed() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        idx.upsert_document(&doc("/d/a.docx", "docx", "A"), "正文")
            .unwrap();
        idx.upsert_vector("/d/a.docx", &[1.0], "m", "hashA")
            .unwrap();
        assert!(idx.vector_is_current("/d/a.docx", "m", "hashA").unwrap());
        assert!(!idx.vector_is_current("/d/a.docx", "m", "hashB").unwrap());
        assert!(!idx.vector_is_current("/d/a.docx", "m2", "hashA").unwrap());
        assert!(!idx.vector_is_current("/d/none.docx", "m", "hashA").unwrap());
    }

    #[test]
    fn deleting_document_cascades_vector() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        idx.upsert_document(&doc("/d/a.docx", "docx", "A"), "机密")
            .unwrap();
        idx.upsert_vector("/d/a.docx", &[1.0, 2.0], "m", "h")
            .unwrap();
        assert_eq!(idx.candidate_vectors().unwrap().len(), 1);
        assert!(idx.delete_by_path("/d/a.docx").unwrap());
        assert!(
            idx.candidate_vectors().unwrap().is_empty(),
            "删文档应级联删向量，不留悬挂行"
        );
    }

    /// BETA-31-v3 cycle 3：purge_short_body_vectors 只删 body 极短文档关联的向量、
    /// 保留 body ≥ 20 字符的真文档向量、幂等再调返 0。
    #[test]
    fn purge_short_body_vectors_drops_empty_and_short_only() {
        let idx = DocumentIndex::open_in_memory().unwrap();

        // 真文档（body ≥ 20 字符）→ 保留向量
        let long_body = "本季度预算与营收分析季度预算同比增长十个百分点表现优异。";
        idx.upsert_document(&doc("/d/real.docx", "docx", "A"), long_body)
            .unwrap();
        idx.upsert_vector("/d/real.docx", &[1.0, 2.0], "m", "h_real")
            .unwrap();

        // 空 body 图片（模拟 OCR 跳过的 png）→ 应被 purge
        idx.upsert_document(&doc("/d/avatar.png", "png", "A"), "")
            .unwrap();
        idx.upsert_vector("/d/avatar.png", &[0.0, 0.0], "m", "h_empty")
            .unwrap();

        // 短 body（< 20 字符）→ 应被 purge
        idx.upsert_document(&doc("/d/icon.png", "png", "A"), "short")
            .unwrap();
        idx.upsert_vector("/d/icon.png", &[0.0, 0.0], "m", "h_short")
            .unwrap();

        assert_eq!(idx.vector_count().unwrap(), 3, "初始 3 条向量");

        // 第一次调：删 2 条（空 + 短）。
        let purged = idx.purge_short_body_vectors(false).unwrap();
        assert_eq!(purged, 2, "应删空 body + 短 body 共 2 条");
        assert_eq!(idx.vector_count().unwrap(), 1, "保留真文档向量");

        // 幂等：再调返 0。
        assert_eq!(idx.purge_short_body_vectors(false).unwrap(), 0, "幂等");

        // 真文档向量内容未受影响。
        let remaining = idx.candidate_vectors().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].path, "/d/real.docx");
    }

    /// 空表 / 无向量时 purge_short_body_vectors 安全返 0、不 panic。
    #[test]
    fn purge_short_body_vectors_on_empty_returns_zero() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        assert_eq!(idx.purge_short_body_vectors(false).unwrap(), 0);
    }

    /// BETA-39：embed_pending 的 `embed_images` 双分支——
    /// 关（false，默认）：图片一律不入嵌（cycle 4 现状，真文字图片也不嵌）；
    /// 开（true）：过图片门槛（0.75）的图片入嵌、乱码图片仍被挡、非图片行为不变。
    #[test]
    fn embed_pending_embed_images_branches() {
        struct UnitEmbedder;
        impl crate::embed::TextEmbedder for UnitEmbedder {
            fn embed(&self, _text: &str) -> Result<Vec<f32>, IndexError> {
                Ok(vec![1.0, 0.0])
            }
            fn model_id(&self) -> &'static str {
                "unit-emb"
            }
        }
        let build = || {
            let idx = DocumentIndex::open_in_memory().unwrap();
            // 真文字截图（ratio 1.0、≥20 字）→ 开启时应入嵌
            idx.upsert_document(
                &doc("/d/note.png", "png", "A"),
                "我今天写了一篇关于春天的作文老师说写得很好",
            )
            .unwrap();
            // CJK-heavy 乱码图片（ratio 0.65：过通用 0.6、不过图片 0.75）→ 开启也不嵌
            idx.upsert_document(
                &doc("/d/face.png", "png", "A"),
                "动河的天写在有里上作文好看1234567",
            )
            .unwrap();
            // 真文档（非图片）→ 两种取值都入嵌
            idx.upsert_document(
                &doc("/d/real.docx", "docx", "A"),
                "本季度预算与营收分析季度预算同比增长十个百分点表现优异。",
            )
            .unwrap();
            idx
        };
        let roots = [std::path::PathBuf::from("/d")];

        // 关：只嵌非图片文档。
        let off = build();
        let (e, r, f) = off
            .embed_pending(&roots, &UnitEmbedder, false, &mut |_, _| {})
            .unwrap();
        assert_eq!((e, r, f), (1, 0, 0), "关：只嵌 real.docx");
        assert_eq!(off.candidate_vectors().unwrap()[0].path, "/d/real.docx");

        // 开：真文字图片也入嵌，乱码图片仍被图片门槛挡。
        let on = build();
        let (e, r, f) = on
            .embed_pending(&roots, &UnitEmbedder, true, &mut |_, _| {})
            .unwrap();
        assert_eq!(
            (e, r, f),
            (2, 0, 0),
            "开：real.docx + note.png 入嵌、face.png 被挡"
        );
        let mut paths: Vec<String> = on
            .candidate_vectors()
            .unwrap()
            .into_iter()
            .map(|v| v.path)
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            vec!["/d/note.png".to_string(), "/d/real.docx".to_string()]
        );
    }

    /// BETA-39：purge 的 `keep_worthy_images` 双分支——
    /// 关（false，默认）：图片向量全清（cycle 4 现状，含真文字图片）；
    /// 开（true）：过图片门槛（0.75）的图片向量保留、乱码图片仍清、非图片规则不变。
    #[test]
    fn purge_keep_worthy_images_branches() {
        let build = || {
            let idx = DocumentIndex::open_in_memory().unwrap();
            // 真文字截图（纯中文 ratio 1.0、≥20 字）→ 开启时应保留
            idx.upsert_document(
                &doc("/d/note.png", "png", "A"),
                "我今天写了一篇关于春天的作文老师说写得很好",
            )
            .unwrap();
            idx.upsert_vector("/d/note.png", &[1.0, 0.0], "m", "h_note")
                .unwrap();
            // CJK-heavy 乱码图片（13 汉字 + 7 数字 = ratio 0.65，过通用 0.6 但不过图片 0.75）
            // → 两种取值下都应被清（已知 QQ 表情包污染 case 的靶）
            idx.upsert_document(
                &doc("/d/face.png", "png", "A"),
                "动河的天写在有里上作文好看1234567",
            )
            .unwrap();
            idx.upsert_vector("/d/face.png", &[0.9, 0.1], "m", "h_face")
                .unwrap();
            // 真文档（非图片）→ 两种取值下都保留
            idx.upsert_document(
                &doc("/d/real.docx", "docx", "A"),
                "本季度预算与营收分析季度预算同比增长十个百分点表现优异。",
            )
            .unwrap();
            idx.upsert_vector("/d/real.docx", &[0.5, 0.5], "m", "h_real")
                .unwrap();
            idx
        };

        // 关：图片全清（真文字 note.png 也清——恢复一刀切现状）。
        let off = build();
        assert_eq!(
            off.purge_short_body_vectors(false).unwrap(),
            2,
            "关：2 条图片向量全清"
        );
        let left = off.candidate_vectors().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].path, "/d/real.docx");

        // 开：只清乱码图片，真文字图片保留。
        let on = build();
        assert_eq!(
            on.purge_short_body_vectors(true).unwrap(),
            1,
            "开：只清乱码图片 1 条"
        );
        let mut paths: Vec<String> = on
            .candidate_vectors()
            .unwrap()
            .into_iter()
            .map(|v| v.path)
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            vec!["/d/note.png".to_string(), "/d/real.docx".to_string()]
        );
        // 幂等
        assert_eq!(on.purge_short_body_vectors(true).unwrap(), 0);
    }

    #[test]
    fn doc_schema_version_persists_across_open() {
        // BETA-32 C1b 持久化集成测试：`DocumentIndex::open` 走真实文件路径后，schema_meta
        // 表 + version 行应已落盘；用 raw rusqlite::Connection 重开同一文件读出 "1"。
        // 防 `ensure_schema_version` 调用被挪到 SCHEMA execute 之前——单测过、生产炸。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("documents.db");
        {
            let _idx = DocumentIndex::open(&path).unwrap();
        } // drop 关连接、落盘
        let conn = Connection::open(&path).unwrap();
        let v = crate::version::read_schema_version(&conn).unwrap();
        assert_eq!(v.as_deref(), Some(crate::version::INDEXER_SCHEMA_VERSION));
    }

    #[test]
    fn file_open_enables_wal_journal_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("documents.db");
        {
            let _idx = DocumentIndex::open(&path).unwrap();
        }
        let conn = Connection::open(&path).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
    }

    /// BETA-33 cycle 5：`stats_under_root` 按 root 前缀 + 类型分桶 + 上次索引时间，
    /// 且不误伤兄弟目录（前缀相同但非子树、如 `/docs2/*` vs `/docs/*`）。
    #[test]
    fn stats_under_root_counts_by_type_and_prefix_boundary() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        // 3 docx（同 root）+ 2 png（同 root）+ 1 兄弟 root（前缀相同边界外）
        idx.upsert_document(&doc("/docs/a.docx", "docx", "A"), "hello")
            .unwrap();
        idx.upsert_document(&doc("/docs/b.docx", "docx", "B"), "world")
            .unwrap();
        idx.upsert_document(&doc("/docs/sub/c.docx", "docx", "C"), "foo")
            .unwrap();
        idx.upsert_document(&doc("/docs/i1.png", "png", "X"), "bar")
            .unwrap();
        idx.upsert_document(&doc("/docs/i2.png", "png", "Y"), "baz")
            .unwrap();
        // 兄弟目录 /docs2 应被排除（前缀 /docs 但不是 /docs 子树）
        idx.upsert_document(&doc("/docs2/other.docx", "docx", "Z"), "quux")
            .unwrap();

        let s = idx.stats_under_root("/docs").unwrap();
        assert_eq!(s.total, 5, "/docs 下应有 5 条（3 docx + 2 png）");
        assert_eq!(s.images, 2, "/docs 下 png 应有 2 条");
        assert!(s.last_indexed_time.is_some(), "应有 last_indexed_time");

        // 尾部带斜杠也应生效
        let s2 = idx.stats_under_root("/docs/").unwrap();
        assert_eq!(s2, s, "trim 尾部 / 后一致");

        // Windows 分隔符也生效
        idx.upsert_document(&doc(r"C:\Users\a\docs\x.docx", "docx", "W"), "win")
            .unwrap();
        let s3 = idx.stats_under_root(r"C:\Users\a\docs").unwrap();
        assert_eq!(s3.total, 1);
    }

    /// BETA-33 cycle 5：空 root（无匹配）时 `stats_under_root` 返 0 / 0 / None。
    #[test]
    fn stats_under_root_empty_returns_zero() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        let s = idx.stats_under_root("/nonexistent").unwrap();
        assert_eq!(s.total, 0);
        assert_eq!(s.images, 0);
        assert_eq!(s.last_indexed_time, None);
    }

    /// BETA-33 cycle 7-c：purge_under_root 删子树（含深层 + FTS 同步删）、兄弟前缀目录
    /// 不误删、与 stats_under_root 口径一致、幂等（再清返 0）。
    #[test]
    fn purge_under_root_removes_subtree_and_fts() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        idx.upsert_document(&doc("/docs/a.txt", "txt", "AAA"), "uniquebodyalpha")
            .unwrap();
        idx.upsert_document(&doc("/docs/sub/b.txt", "txt", "BBB"), "uniquebodybeta")
            .unwrap();
        // 兄弟前缀目录 /docs2 不算 /docs 子树，必须保留。
        idx.upsert_document(&doc("/docs2/c.txt", "txt", "CCC"), "uniquebodygamma")
            .unwrap();

        // 清除数 = 概貌统计数（同一边界谓词）。
        let expect = idx.stats_under_root("/docs").unwrap().total;
        let removed = idx.purge_under_root("/docs").unwrap();
        assert_eq!(removed, expect, "清除口径应与统计口径一致");
        assert_eq!(removed, 2);
        assert_eq!(idx.count().unwrap(), 1, "边界外 /docs2 保留");

        // FTS 同步删：已清条目正文搜不到、边界外条目仍可搜。
        let gone = idx
            .query(&DocumentQuery {
                text: Some("uniquebodyalpha".to_owned()),
                ..Default::default()
            })
            .unwrap();
        assert!(gone.is_empty(), "已清条目不应再命中 FTS");
        let kept = idx
            .query(&DocumentQuery {
                text: Some("uniquebodygamma".to_owned()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(kept.len(), 1, "边界外条目 FTS 仍可搜");

        // 幂等：再清返 0。
        assert_eq!(idx.purge_under_root("/docs").unwrap(), 0);
    }

    // ===== BETA-35 cycle 4：document_passages + document_failed_pages 三表 CRUD =====

    #[test]
    fn upsert_with_passages_writes_all_three_tables_atomically() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        let entry = DocumentEntry {
            path: "/scan/a.pdf".to_string(),
            file_name: "a.pdf".to_string(),
            title: None,
            author: None,
            doc_type: "pdf".to_string(),
            page_count: Some(3),
            modified_time: 1000,
            content_hash: None,
        };
        let passages = vec![
            PagePassage {
                page_no: 1,
                seq: 0,
                text: "第一页 OCR 文本".to_string(),
            },
            PagePassage {
                page_no: 3,
                seq: 0,
                text: "第三页 OCR 文本".to_string(),
            },
        ];
        let failed = vec![PageFailure {
            page_no: 2,
            reason: "OCR 引擎错".to_string(),
        }];
        assert!(idx
            .upsert_document_with_pages(
                &entry,
                "第一页 OCR 文本\n第三页 OCR 文本",
                &passages,
                &failed
            )
            .unwrap());
        // 文档主表 + FTS 命中。
        assert_eq!(idx.count().unwrap(), 1);
        let hits = idx
            .query(&DocumentQuery {
                text: Some("OCR 文本".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        // 段落表：升序取回、page_no / seq / text 全对。
        let got_passages = idx.passages_for_doc("/scan/a.pdf").unwrap();
        assert_eq!(got_passages, passages);
        // 失败页表：验收 ③ 数据源。
        let got_failed = idx.failed_pages_for_doc("/scan/a.pdf").unwrap();
        assert_eq!(got_failed, failed);
    }

    #[test]
    fn upsert_with_empty_passages_matches_legacy_upsert_document_byte_equal() {
        // BETA-27 byte-equal 保护关键测试：文本层 PDF / docx / xlsx 传空 vec 时，
        // upsert_document_with_pages 效果与 upsert_document 完全等价。
        let idx = DocumentIndex::open_in_memory().unwrap();
        let entry = doc("/text/a.docx", "docx", "张三");
        assert!(idx
            .upsert_document_with_pages(&entry, "常规文本正文", &[], &[])
            .unwrap());
        // FTS 可查、passages / failed_pages 表都为空。
        let hits = idx
            .query(&DocumentQuery {
                text: Some("常规文本".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(idx.passages_for_doc("/text/a.docx").unwrap().is_empty());
        assert!(idx.failed_pages_for_doc("/text/a.docx").unwrap().is_empty());
    }

    #[test]
    fn re_upsert_replaces_passages_and_failed_pages_idempotently() {
        // 幂等：re-upsert（如 PDF 重扫）应清掉旧 passages / failed_pages 再写新的。
        let idx = DocumentIndex::open_in_memory().unwrap();
        let entry = DocumentEntry {
            path: "/scan/b.pdf".to_string(),
            file_name: "b.pdf".to_string(),
            title: None,
            author: None,
            doc_type: "pdf".to_string(),
            page_count: Some(2),
            modified_time: 1000,
            content_hash: None,
        };
        // 第一轮：2 段 + 1 失败页。
        idx.upsert_document_with_pages(
            &entry,
            "旧段一\n旧段二",
            &[
                PagePassage {
                    page_no: 1,
                    seq: 0,
                    text: "旧段一".to_string(),
                },
                PagePassage {
                    page_no: 2,
                    seq: 0,
                    text: "旧段二".to_string(),
                },
            ],
            &[PageFailure {
                page_no: 3,
                reason: "旧失败".to_string(),
            }],
        )
        .unwrap();
        assert_eq!(idx.passages_for_doc("/scan/b.pdf").unwrap().len(), 2);
        assert_eq!(idx.failed_pages_for_doc("/scan/b.pdf").unwrap().len(), 1);
        // 第二轮：1 段 + 0 失败。旧的应全被清掉。
        idx.upsert_document_with_pages(
            &entry,
            "新段一",
            &[PagePassage {
                page_no: 1,
                seq: 0,
                text: "新段一".to_string(),
            }],
            &[],
        )
        .unwrap();
        let got = idx.passages_for_doc("/scan/b.pdf").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "新段一");
        assert!(
            idx.failed_pages_for_doc("/scan/b.pdf").unwrap().is_empty(),
            "旧失败页应被清"
        );
    }

    #[test]
    fn delete_document_cascades_to_passages_and_failed_pages() {
        // 外键级联（PRAGMA foreign_keys=ON）：删 documents → passages / failed_pages 自动删。
        let idx = DocumentIndex::open_in_memory().unwrap();
        let entry = DocumentEntry {
            path: "/scan/c.pdf".to_string(),
            file_name: "c.pdf".to_string(),
            title: None,
            author: None,
            doc_type: "pdf".to_string(),
            page_count: Some(1),
            modified_time: 1000,
            content_hash: None,
        };
        idx.upsert_document_with_pages(
            &entry,
            "内容",
            &[PagePassage {
                page_no: 1,
                seq: 0,
                text: "内容".to_string(),
            }],
            &[PageFailure {
                page_no: 2,
                reason: "err".to_string(),
            }],
        )
        .unwrap();
        assert_eq!(idx.passages_for_doc("/scan/c.pdf").unwrap().len(), 1);
        assert_eq!(idx.failed_pages_for_doc("/scan/c.pdf").unwrap().len(), 1);
        assert!(idx.delete_by_path("/scan/c.pdf").unwrap());
        assert!(idx.passages_for_doc("/scan/c.pdf").unwrap().is_empty());
        assert!(idx.failed_pages_for_doc("/scan/c.pdf").unwrap().is_empty());
    }

    #[test]
    fn passages_for_missing_path_returns_empty() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        assert!(idx.passages_for_doc("/nope").unwrap().is_empty());
        assert!(idx.failed_pages_for_doc("/nope").unwrap().is_empty());
    }

    /// BETA-33 cycle 7-c（Codex SUGGEST 10）：purge 后 `document_vectors` 外键级联——
    /// 删除 documents 行时对应向量应自动删除；若 `PRAGMA foreign_keys = ON` 未生效
    /// 此测试会暴露（vector_count 不降）。
    #[test]
    fn purge_under_root_cascades_document_vectors() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        idx.upsert_document(&doc("/docs/a.txt", "txt", "AAA"), "uniquebodyalpha")
            .unwrap();
        idx.upsert_document(&doc("/keep/b.txt", "txt", "BBB"), "uniquebodybeta")
            .unwrap();
        idx.upsert_vector("/docs/a.txt", &[1.0, 2.0], "m", "ha")
            .unwrap();
        idx.upsert_vector("/keep/b.txt", &[3.0, 4.0], "m", "hb")
            .unwrap();
        assert_eq!(idx.vector_count().unwrap(), 2);

        let removed = idx.purge_under_root("/docs").unwrap();
        assert_eq!(removed, 1);
        assert_eq!(
            idx.vector_count().unwrap(),
            1,
            "被删文档的向量应随外键级联消失"
        );
        let cands = idx.candidate_vectors().unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].path, "/keep/b.txt", "边界外文档的向量保留");
    }

    /// BETA-38 cycle 1：`index_dirs` 真实提取回填 `content_hash`——相同内容两副本同 hash、
    /// 不同内容异 hash；`content_hash_of` 能读回。
    #[test]
    fn index_dirs_populates_content_hash_and_dedups_copies() {
        let dir = tempfile::tempdir().unwrap();
        let same = "这是一份判决书正文内容用于测试文件身份指纹";
        std::fs::write(dir.path().join("orig.txt"), same).unwrap();
        std::fs::write(dir.path().join("copy.txt"), same).unwrap(); // 完全相同副本
        std::fs::write(dir.path().join("other.txt"), "另一份完全不同的材料内容").unwrap();
        let db = dir.path().join("index.db");
        let idx = DocumentIndex::open(&db).unwrap();
        idx.index_dirs(&[dir.path().to_path_buf()]).unwrap();

        let orig = dir.path().join("orig.txt").to_string_lossy().into_owned();
        let copy = dir.path().join("copy.txt").to_string_lossy().into_owned();
        let other = dir.path().join("other.txt").to_string_lossy().into_owned();
        let h_orig = idx.content_hash_of(&orig).unwrap();
        let h_copy = idx.content_hash_of(&copy).unwrap();
        let h_other = idx.content_hash_of(&other).unwrap();
        assert!(h_orig.is_some(), "回填了 content_hash");
        assert_eq!(h_orig, h_copy, "相同内容两副本同身份 hash");
        assert_ne!(h_orig, h_other, "不同内容异身份 hash");
    }

    /// BETA-38 cycle 1：老库（无 content_hash 列）打开自动 ALTER 迁移——迁移后可写可读，
    /// 老行保持 NULL。
    #[test]
    fn old_db_without_content_hash_column_migrates_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        // 手工建"老库"：documents 表故意不含 content_hash 列 + 一条老行。
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE documents (
                   id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, file_name TEXT NOT NULL,
                   title TEXT, author TEXT, doc_type TEXT NOT NULL, page_count INTEGER,
                   modified_time INTEGER NOT NULL, indexed_time INTEGER NOT NULL
                 );
                 CREATE VIRTUAL TABLE documents_fts USING fts5(title, author, body, tokenize='trigram');
                 INSERT INTO documents(path, file_name, doc_type, modified_time, indexed_time)
                   VALUES ('/old/a.txt', 'a.txt', 'txt', 1, 1);
                 INSERT INTO documents_fts(rowid, title, author, body)
                   SELECT id, title, author, '老正文' FROM documents WHERE path='/old/a.txt';",
            )
            .unwrap();
        } // drop 落盘

        // 打开 → 应自动 ALTER ADD content_hash（不报错）。
        let idx = DocumentIndex::open(&path).unwrap();
        assert_eq!(
            idx.content_hash_of("/old/a.txt").unwrap(),
            None,
            "老行 content_hash 迁移后为 NULL"
        );
        // 迁移后新写入的文档能落 content_hash。
        let mut e = doc("/new/b.txt", "txt", "作者乙");
        e.content_hash = Some("deadbeefdeadbeef".to_string());
        idx.upsert_document(&e, "新正文").unwrap();
        assert_eq!(
            idx.content_hash_of("/new/b.txt").unwrap().as_deref(),
            Some("deadbeefdeadbeef"),
            "迁移后新行可写读 content_hash"
        );
    }

    #[test]
    fn old_db_without_index_failures_modified_time_column_migrates_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy_failures.db");
        // 手工建"老库"：index_failures 表故意不含 modified_time 列 + 一条老留痕。
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE index_failures (
                   path TEXT PRIMARY KEY, reason TEXT NOT NULL, failed_time INTEGER NOT NULL
                 );
                 INSERT INTO index_failures(path, reason, failed_time)
                   VALUES ('/old/broken.pdf', '旧版留痕', 1);",
            )
            .unwrap();
        } // drop 落盘

        // 打开 → 应自动 ALTER ADD modified_time（不报错），老行回填哨兵值 0。
        let idx = DocumentIndex::open(&path).unwrap();
        let mtimes = idx.all_failure_modified_times().unwrap();
        assert_eq!(
            mtimes.get("/old/broken.pdf"),
            Some(&0),
            "老留痕迁移后 modified_time 应为安全哨兵值 0（不等于任何真实 mtime，至少再重试一次）"
        );

        // 迁移后新记录的失败留痕能正确写读 modified_time。
        idx.record_extract_failure("/new/broken.pdf", "新版留痕", 12345)
            .unwrap();
        let mtimes = idx.all_failure_modified_times().unwrap();
        assert_eq!(mtimes.get("/new/broken.pdf"), Some(&12345));
    }
}
