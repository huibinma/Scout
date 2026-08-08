//! BETA-01 音乐索引数据模型（计划书 §10.1 字段）。

/// 一条音乐索引记录。`path` 为 UNIQUE 键；除 path / file_name / modified_time
/// 外其余字段允许缺失（音频无对应标签时存 NULL）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MusicEntry {
    /// 绝对路径（OS 原生形式），唯一键。
    pub path: String,
    /// 文件名（含扩展名）。
    pub file_name: String,
    /// 演唱者 / 艺术家。
    pub artist: Option<String>,
    /// 标题。
    pub title: Option<String>,
    /// 专辑。
    pub album: Option<String>,
    /// 时长（秒）。
    pub duration_secs: Option<f64>,
    /// 容器格式短名，如 `"MP3"` / `"FLAC"` / `"MP4"`。
    pub format: Option<String>,
    /// 音频码率（kbps）。
    pub bitrate: Option<u32>,
    /// 文件修改时间（unix 秒），增量比对锚点。
    pub modified_time: i64,
}

/// 查询条件。各字段 AND 组合；全空表示「全部」（受 `limit`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MusicQuery {
    /// 全文检索（artist / title / album / file_name 任一匹配）。普通文本，经 `fts_sanitize`
    /// 包成单 phrase。与 `fts_match` 互斥：`fts_match` 非空时优先、`text` 被忽略。
    pub text: Option<String>,
    /// 预构造的**原始 FTS5 MATCH 表达式**（如 `("a" OR "b") AND "c"`），**绕过** `fts_sanitize`，
    /// 供同义词/词组（组内 OR、组间 AND）查询用。调用方须保证各词项已正确转义（双引号包裹）。
    pub fts_match: Option<String>,
    /// artist 子串（大小写不敏感）。
    pub artist: Option<String>,
    /// album 子串（大小写不敏感）。
    pub album: Option<String>,
    /// format 精确（大小写不敏感）。
    pub format: Option<String>,
    /// 返回上限，缺省 50。
    pub limit: Option<u32>,
}

/// 一条文档索引记录（BETA-02，计划书 §10.2）。正文文本不存此结构，只进 FTS5。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DocumentEntry {
    /// 绝对路径，唯一键。
    pub path: String,
    /// 文件名（含扩展名）。
    pub file_name: String,
    /// 标题（OOXML core.xml dc:title / html `<title>`，缺省 None）。
    pub title: Option<String>,
    /// 作者（OOXML core.xml dc:creator，缺省 None）。
    pub author: Option<String>,
    /// 文档类型短名：`docx`/`xlsx`/`pptx`/`pdf`/`txt`/`md`/`html`/`xls`/`ods`。
    pub doc_type: String,
    /// 页/节总数：pptx 幻灯片数 / xlsx 工作表数 / pdf 页数（best-effort）；其余 None。
    pub page_count: Option<u32>,
    /// 文件修改时间（unix 秒），增量比对锚点。
    pub modified_time: i64,
    /// BETA-38 doc identity：**文件原始全字节内容指纹**（FNV-1a，[`crate::embed::file_identity_hash`]）。
    /// 冷归档重复副本（同内容多副本存多盘/迁移盘/压缩包展开）据此判等——索引期同 hash 只嵌一次、
    /// 结果期同 hash 合并留痕全部副本位置。老库 / 未回填 / 读取失败时 `None`（不阻断索引）。
    pub content_hash: Option<String>,
}

/// 文档查询命中（含可选 FTS 片段）。
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentHit {
    /// 文档元信息。
    pub entry: DocumentEntry,
    /// FTS5 `snippet()` 命中上下文片段；仅文本查询时有。
    pub snippet: Option<String>,
}

/// 文档预览（BETA-20 结果预览面板）：元信息 + 完整正文（从 FTS `body` 列取回）+ 可选命中片段。
/// **只读已索引数据**——正文来自索引，不读磁盘原文件。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DocumentPreview {
    /// 文档元信息。
    pub entry: DocumentEntry,
    /// 完整索引正文（OCR 图片即 OCR 文本）。
    pub body: String,
    /// 命中上下文片段：调用方提供 `fts_match` 且该文档命中时，由 `snippet()` 产出
    /// （命中词用 `\x02` / `\x03` 哨兵包裹，供前端转 `<mark>`）；否则 `None`。
    pub snippet: Option<String>,
}

/// 文档查询条件。各字段 AND 组合；全空表示「全部」（受 `limit`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DocumentQuery {
    /// 全文检索（title / author / body 任一匹配）。普通文本，经 `fts_sanitize` 包成单 phrase。
    /// 与 `fts_match` 互斥：`fts_match` 非空时优先、`text` 被忽略。
    pub text: Option<String>,
    /// 预构造的**原始 FTS5 MATCH 表达式**（如 `("a" OR "b") AND "c"`），**绕过** `fts_sanitize`，
    /// 供同义词/词组（组内 OR、组间 AND）查询用。调用方须保证各词项已正确转义（双引号包裹）。
    pub fts_match: Option<String>,
    /// author 子串（大小写不敏感）。
    pub author: Option<String>,
    /// doc_type 精确（大小写不敏感）。
    pub doc_type: Option<String>,
    /// 限定 doc_type 属于此集合（None / 空 = 不限）。MediaSearch(Image) 用它框定图片类型。
    pub doc_types: Option<Vec<String>>,
    /// 返回上限，缺省 50。
    pub limit: Option<u32>,
}

/// 扫描版 PDF 一页 OCR 成功的段落（BETA-35 cycle 4）。
///
/// 落进 `document_passages` 表，`page_no` 起于 1（对齐 pdftoppm 命名）；`seq`
/// 是页内段落序号（起于 0）——cycle 4 阶段每页对应 1 段（`seq=0`），后续可
/// 按段落切分展开。命中回页由 UI 通过 `document_passages.page_no` 展示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PagePassage {
    /// 页码（从 1 起，对齐 pdftoppm `page-N.png` 命名）。
    pub page_no: u32,
    /// 页内段落序号（从 0 起，cycle 4 默认 0）。
    pub seq: u32,
    /// OCR 识别出的段落文本（已经过 `crate::ocr::normalize_ocr_text` 归一化）。
    pub text: String,
}

/// 扫描版 PDF 一页 OCR 失败留痕（BETA-35 cycle 4，验收 ③）。
///
/// 落进 `document_failed_pages` 表。文档级验收：`SELECT page_no, reason
/// FROM document_failed_pages WHERE doc_id = ?`——BETA-40 场景 playbook 里
/// 给"取证复核用"的引导。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageFailure {
    /// 页码（从 1 起）。
    pub page_no: u32,
    /// 失败原因（OCR 引擎错 / 超时 / 解码错等）。
    pub reason: String,
}

/// **文件级**提取失败留痕（BETA-40 收尾，2026-07-04；区别于 [`PageFailure`] 的页级）。
///
/// 落进 `index_failures` 表：整份文件提取失败（pdf-extract 不支持编码 / OCR 依赖
/// 缺失 / 畸形文件）不再只累计 `IndexStats.failed`。成功重扫或磁盘删除后自动清除。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionFailure {
    /// 失败文件的完整路径。
    pub path: String,
    /// 失败原因（提取器报错细节）。
    pub reason: String,
    /// 失败时间（Unix 秒，最近一次）。
    pub failed_time: i64,
}

/// 文档提取结果（BETA-35 cycle 4：从 `(DocumentEntry, String)` tuple 升级）。
///
/// 兼容语义：
/// - 文本层 PDF / docx / xlsx / ... 提取 → `passages` / `failed_pages` 皆空，
///   落库时只走 `documents` + `documents_fts`，与 tuple 时代**逐字节相同**
///   （BETA-27 byte-equal 保护）；
/// - 扫描版 PDF 走 OCR pipeline（BETA-35 cycle 3）→ `passages` 携页级段落 +
///   `failed_pages` 携失败页，落库同时写 `documents` + `document_passages` +
///   `document_failed_pages` 三表。
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractedDoc {
    /// 文档元信息。
    pub entry: DocumentEntry,
    /// 正文（进 `documents_fts`；扫描 PDF 时是所有成功页 OCR 文本拼接）。
    pub body: String,
    /// 页级段落（扫描 PDF 时非空；其他文档为空）。
    pub passages: Vec<PagePassage>,
    /// 失败页记录（扫描 PDF 时可能非空；其他文档为空）。
    pub failed_pages: Vec<PageFailure>,
}

/// 一轮增量索引的统计。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexStats {
    /// 命中音乐扩展名白名单的文件数。
    pub scanned: usize,
    /// 新增记录数。
    pub added: usize,
    /// 更新记录数（mtime 变化）。
    pub updated: usize,
    /// 跳过数（mtime 未变）。
    pub skipped: usize,
    /// 回收数（磁盘已删，从索引移除）。
    pub removed: usize,
    /// 标签读取失败数（不中断整轮）。
    pub failed: usize,
}
