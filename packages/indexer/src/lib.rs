//! `scout-indexer` — BETA-01 本地音乐 metadata 索引层。
//!
//! 职责（仅索引层 + 查询 API，不接 Agent）：
//! - 扫描音乐目录（可配置多目录，mtime 增量）；
//! - 用 [`lofty`] 提取音频标签（artist / title / album / duration / format / bitrate）；
//! - 存入跨平台 SQLite + FTS5（[`rusqlite`] bundled）；
//! - 对外暴露 [`MusicIndex::query`]。

// 文档含 SQLite / FTS5 / 字段名等领域词，沿用项目对 doc_markdown 的处理。
#![allow(clippy::doc_markdown)]

mod db;
mod discovery;
mod doc_db;
mod doc_extract;
mod email_extract;
pub mod embed;
mod extract;
mod model;
mod ocr;
mod pdf_rasterizer;
mod pii;
mod placeholder;
pub mod progress;
mod scan;
pub mod vectors;
pub mod version;

pub use db::{
    clear_index, compact_index_if_due, MusicIndex, MusicRootStats, DEFAULT_COMPACT_INTERVAL_DAYS,
};
pub use discovery::{
    default_audio_discovery, default_document_discovery, default_image_discovery, AudioDiscovery,
    DiscoveryError, PathDiscovery,
};
pub use doc_db::{CandidateVector, DocRootStats, DocumentIndex};
pub use doc_extract::{
    extract_document, run_pdf_extract_worker_if_requested, PDF_EXTRACT_WORKER_ARG,
};
pub use extract::extract_metadata;
pub use globset::GlobSet;
pub use model::{
    DocumentEntry, DocumentHit, DocumentPreview, DocumentQuery, ExtractedDoc, ExtractionFailure,
    IndexStats, MusicEntry, MusicQuery, PageFailure, PagePassage,
};
#[cfg(windows)]
pub use ocr::WindowsOcrEngine;
pub use ocr::{
    default_ocr_engine, digit_correction_variants, finalize_ocr_text, normalize_ocr_text,
    OcrEngine, TesseractOcrEngine,
};
pub use pdf_rasterizer::{
    default_pdf_rasterizer, PdfRasterizer, PopplerPdfRasterizer, RasterizedPdf,
};
pub use progress::{IndexPhase, IndexProgress, NoopProgress, StageTimings};
pub use scan::{
    build_exclude_set, default_document_roots, default_image_roots, default_music_roots,
    ExcludeFilter, IMAGE_EXTS,
};
pub use vectors::{blob_to_vector, cosine, vector_to_blob};
pub use version::{ensure_schema_version, read_schema_version, INDEXER_SCHEMA_VERSION};

/// 索引层错误。
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// SQLite 操作失败。
    #[error("数据库错误: {0}")]
    Db(#[from] rusqlite::Error),
    /// 读取音频标签失败（损坏 / 非音频 / 不支持的容器）。
    #[error("读取标签失败 {path}: {detail}")]
    Tag {
        /// 出错文件路径。
        path: String,
        /// 失败原因。
        detail: String,
    },
    /// 文件系统 IO 失败。
    #[error("IO 错误 {path}: {detail}")]
    Io {
        /// 出错文件路径。
        path: String,
        /// 失败原因。
        detail: String,
    },
}
