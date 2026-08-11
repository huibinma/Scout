//! BETA-04 `LocalIndexBackend`：把 [`scout_indexer`] 的音乐 / 文档本地索引包成
//! [`SearchBackend`]（[`BackendKind::NativeIndex`]），让本地索引参与 fan-out 多源搜索。
//!
//! 关键约束：rusqlite `Connection` 是 `!Sync`，而 `SearchBackend: Send + Sync` → 本 backend
//! **不持久持有连接**，而是持 db 路径、每次 `search()` 内部开连接查完即关。路径规范化在
//! 产出 `SearchResult` 时做（与 Spotlight 一致），保证跨源去重的 path 一致。

// 文档含 file_search / media_search 等领域词，沿用项目对 doc_markdown 的处理。
#![allow(clippy::doc_markdown)]

use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};

use scout_indexer::{
    DocumentHit, DocumentIndex, DocumentPreview, DocumentQuery, IndexStats, MusicEntry, MusicIndex,
    MusicQuery, NoopProgress, OcrEngine, PageFailure, PagePassage,
};
// 图片 doc_type 集合复用 indexer 的索引侧白名单（单一信源）：indexer 写入 OCR 图片记录时
// doc_type=小写扩展名，此处用同一集合限定 MediaSearch(Image/Screenshot) 查询范围。
use scout_indexer::IMAGE_EXTS as IMAGE_DOC_TYPES;
use scout_search_backend::{
    backend_stream_from_results, BackendKind, BackendSearchFuture, CancellationToken,
    ExpandedSearchIntent, FileSearch, KeywordGroup, MatchMode, MatchType, MediaSearch, MediaType,
    SearchBackend, SearchError, SearchIntent, SearchResult, SearchResultMetadata,
};
// result_id 与三系统后端共用 common 的单一实现，保证跨源去重 ID 口径一致。
use scout_search_backend::result_id_for_path as result_id;

/// 本地索引预览数据（BETA-20 结果预览面板）。**只读已索引数据**，不触碰磁盘原文件
/// （不触发 OneDrive 占位符水合下载）。
#[derive(Debug, Clone, PartialEq)]
pub enum LocalPreview {
    /// 音频元数据（artist / title / album / duration / format / bitrate）。
    Music(MusicEntry),
    /// 文档 / OCR 图片：`doc_type` 区分，`body` = 索引正文（图片即 OCR 文本），
    /// `snippet` = 命中片段（提供 `fts_match` 且命中时）。
    Document(DocumentPreview),
}

/// 2026-07-06 拍板（BETA-46 零索引语义对齐）：全盘发现的路径归一化 key。
/// Windows 分隔符统一 `\` + 全小写（路径大小写不敏感）、Unix 统一 `/` 保留大小写；
/// 尾部分隔符去除（纯做前缀比较、无需保留 `C:\` 展示形态）。
fn discovery_path_key(path: &Path) -> String {
    let s = path.to_string_lossy();
    #[cfg(windows)]
    {
        s.replace('/', "\\").trim_end_matches('\\').to_lowercase()
    }
    #[cfg(not(windows))]
    {
        s.replace('\\', "/").trim_end_matches('/').to_string()
    }
}

fn identity_root(value: &str) -> String {
    value.to_owned()
}

/// 2026-07-06 拍板：全盘发现结果**按生效 roots 与排除策略过滤**后再入库——发现器（Windows
/// Everything / macOS Spotlight）只做加速，不再把 roots 之外的全盘音频带进索引
/// （BETA-46「默认零索引、未经同意不索引」语义对齐；BETA-01A 原「全盘入库」行为
/// 诞生于系统三夹默认索引时代、已废弃）。想收录某处散落音乐 = 把该目录加进索引目录。
/// 判界按「等于 root 或 root + 分隔符前缀」，防 `D:\Music2` 误挂 `D:\Music`。
///
/// `filter`/`normalize_root` 与 WalkDir 分支共用，避免平台发现成功时绕过用户显式配置的
/// basename/per-root 排除目录。
fn filter_discovered_to_scope<N>(
    paths: Vec<PathBuf>,
    roots: &[PathBuf],
    filter: &scout_indexer::ExcludeFilter,
    normalize_root: N,
) -> Vec<PathBuf>
where
    N: Fn(&str) -> String + Copy,
{
    const SEP: char = if cfg!(windows) { '\\' } else { '/' };
    let root_keys: Vec<String> = roots
        .iter()
        .map(|r| discovery_path_key(r))
        .filter(|k| !k.is_empty())
        .collect();
    paths
        .into_iter()
        .filter(|p| {
            let k = discovery_path_key(p);
            root_keys
                .iter()
                .any(|r| k.starts_with(r.as_str()) && k[r.len()..].starts_with(SEP))
                && !filter.is_excluded_path(p, normalize_root)
        })
        .collect()
}

/// 本地索引搜索后端。持 db 路径（音乐 + 文档表共用一个 sqlite 文件）。
#[derive(Debug, Clone)]
pub struct LocalIndexBackend {
    db_path: PathBuf,
}

impl LocalIndexBackend {
    /// 用索引数据库路径构造。`db_path` 不必预先存在；未 reindex 前搜索返回空。
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
        }
    }

    /// 手动索引：音乐发现（BETA-01A 全盘枚举、**2026-07-06 起按 `music_roots` 过滤入库**）、
    /// 文档目录扫描与图片 OCR（BETA-03），写入索引。
    /// 返回 `(音乐统计, 文档统计, 图片统计)`。会创建 db 父目录。
    /// OCR 引擎不可用（无 OCR 语言包 / 无 tesseract）→ 图片轮跳过、统计为零、不报错。
    pub fn reindex(
        &self,
        music_roots: &[PathBuf],
        doc_roots: &[PathBuf],
        image_roots: &[PathBuf],
    ) -> Result<(IndexStats, IndexStats, IndexStats), SearchError> {
        self.reindex_with(
            scout_indexer::default_audio_discovery().as_deref(),
            scout_indexer::default_document_discovery().as_deref(),
            scout_indexer::default_image_discovery().as_deref(),
            scout_indexer::default_ocr_engine().as_deref(),
            music_roots,
            doc_roots,
            image_roots,
            &scout_indexer::GlobSet::empty(),
        )
    }

    /// BETA-27：统一 roots（三臂共用）+ 排除。生产 reindex 路径。
    pub fn reindex_scoped(
        &self,
        roots: &[PathBuf],
        exclude: &scout_indexer::GlobSet,
    ) -> Result<(IndexStats, IndexStats, IndexStats), SearchError> {
        self.reindex_with(
            scout_indexer::default_audio_discovery().as_deref(),
            scout_indexer::default_document_discovery().as_deref(),
            scout_indexer::default_image_discovery().as_deref(),
            scout_indexer::default_ocr_engine().as_deref(),
            roots,
            roots,
            roots,
            exclude,
        )
    }

    /// BETA-33 cycle 6 v4：`reindex_scoped` 的**带进度**变体。
    /// 桌面 app 用它把 FTS 每文件进度 + 当前目录写回 `IndexStatus`。
    /// 文档 / 图片 / 音乐若发现器可用均走 `index_paths`（无逐文件进度、由 Everything CLI /
    /// Spotlight 秒级全盘发现，BETA-64 T5 文档/图片对齐 BETA-01A 音乐），否则各自回退
    /// 带进度的 `index_dirs_..._with_progress`。2026-07-28 起执行顺序为文档 → 图片 →
    /// 音频（面向工作场景优先文档，详见 docs/index-performance-design.md §9）。
    pub fn reindex_scoped_with_progress(
        &self,
        roots: &[PathBuf],
        exclude: &scout_indexer::GlobSet,
        progress: &dyn scout_indexer::IndexProgress,
    ) -> Result<(IndexStats, IndexStats, IndexStats), SearchError> {
        self.reindex_with_progress_inner(
            scout_indexer::default_audio_discovery().as_deref(),
            scout_indexer::default_document_discovery().as_deref(),
            scout_indexer::default_image_discovery().as_deref(),
            scout_indexer::default_ocr_engine().as_deref(),
            roots,
            exclude,
            progress,
        )
    }

    /// BETA-33 cycle 7-b：filter 版 reindex 入口，走 basename + per-root path glob 双层排除。
    /// 桌面 `perform_reindex` 通过 [`scout_indexer::ExcludeFilter::build`] 构造 filter 后调本方法。
    /// `normalize_root` 是 root key 归一化 fn（desktop 传 `settings::normalize_root_key`）。
    pub fn reindex_scoped_with_filter_and_progress<N>(
        &self,
        roots: &[PathBuf],
        filter: &scout_indexer::ExcludeFilter,
        normalize_root: N,
        progress: &dyn scout_indexer::IndexProgress,
    ) -> Result<(IndexStats, IndexStats, IndexStats), SearchError>
    where
        N: Fn(&str) -> String + Copy,
    {
        self.reindex_scoped_with_filter_progress_and_discovery(
            roots,
            filter,
            normalize_root,
            progress,
            true,
        )
    }

    /// BETA-47：带「平台发现开关」的 filter 版 reindex。`use_platform_discovery=false`
    /// 时跳过平台发现器（Windows Everything es.exe / macOS Spotlight mdfind），音乐 / 文档 /
    /// 图片三路一律走目录扫描——桌面「关闭 Everything 集成」后索引期不再 spawn es.exe。
    /// BETA-64 T5：原仅音乐一路（`use_audio_discovery`），现同一开关统一控制三路发现
    /// （用户心智是「关 Everything 集成」这一件事，不是只关音乐发现）。
    pub fn reindex_scoped_with_filter_progress_and_discovery<N>(
        &self,
        roots: &[PathBuf],
        filter: &scout_indexer::ExcludeFilter,
        normalize_root: N,
        progress: &dyn scout_indexer::IndexProgress,
        use_platform_discovery: bool,
    ) -> Result<(IndexStats, IndexStats, IndexStats), SearchError>
    where
        N: Fn(&str) -> String + Copy,
    {
        let (discovery, doc_discovery, image_discovery) = if use_platform_discovery {
            (
                scout_indexer::default_audio_discovery(),
                scout_indexer::default_document_discovery(),
                scout_indexer::default_image_discovery(),
            )
        } else {
            (None, None, None)
        };
        self.reindex_with_filter_and_progress_inner(
            discovery.as_deref(),
            doc_discovery.as_deref(),
            image_discovery.as_deref(),
            scout_indexer::default_ocr_engine().as_deref(),
            roots,
            filter,
            normalize_root,
            progress,
        )
    }

    /// [`reindex_scoped_with_filter_and_progress`] 的 mock 可注入版本，phase 通知同 progress 版本。
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn reindex_with_filter_and_progress_inner<N>(
        &self,
        discovery: Option<&dyn scout_indexer::AudioDiscovery>,
        doc_discovery: Option<&dyn scout_indexer::PathDiscovery>,
        image_discovery: Option<&dyn scout_indexer::PathDiscovery>,
        ocr: Option<&dyn OcrEngine>,
        roots: &[PathBuf],
        filter: &scout_indexer::ExcludeFilter,
        normalize_root: N,
        progress: &dyn scout_indexer::IndexProgress,
    ) -> Result<(IndexStats, IndexStats, IndexStats), SearchError>
    where
        N: Fn(&str) -> String + Copy,
    {
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // 2026-07-28：执行顺序改为「文档 → 图片 → 音频」——面向工作场景，文档量最大
        // 且最先被用户搜索，图片（含扫描件 OCR）次之，音频文件量少、优先级最低，
        // 排最后不耽误文档尽快可搜（详见 docs/index-performance-design.md §9）。
        progress.on_phase(scout_indexer::IndexPhase::Doc);
        let docs = DocumentIndex::open(&self.db_path).map_err(to_search_err)?;
        // BETA-64 T7 收尾：文档轮 + 图片轮若都走发现层，两处共享同一份
        // `all_modified_times()`，不各自现取一遍（见 `known_mtimes_cached` 文档）。
        let mut known_mtimes = None;
        // BETA-64 T5：文档发现层同音乐同款模式——发现成功 → `index_paths` + 显式回收；
        // 发现失败 / 无发现器 / roots 空 → 回退 walkdir（自带回收）。无新增 `IndexPhase`
        // 变体（发现层秒级完成，仍报同一个 `Doc` phase，UI 不需要区分）。
        let doc_stats = if let Some(disc) = doc_discovery.filter(|_| !roots.is_empty()) {
            match disc.discover() {
                Ok(paths) => {
                    let paths = filter_discovered_to_scope(paths, roots, filter, normalize_root);
                    if paths.is_empty() {
                        docs.index_dirs_with_filter_and_progress(
                            roots,
                            filter,
                            normalize_root,
                            progress,
                        )
                        .map_err(to_search_err)?
                    } else {
                        let mtimes = known_mtimes_cached(&docs, &mut known_mtimes)?;
                        let stats = docs
                            .index_paths_with_known_mtimes(&paths, mtimes, progress)
                            .map_err(to_search_err)?;
                        docs.prune_deleted().map_err(to_search_err)?;
                        stats
                    }
                }
                Err(_) => docs
                    .index_dirs_with_filter_and_progress(roots, filter, normalize_root, progress)
                    .map_err(to_search_err)?,
            }
        } else {
            docs.index_dirs_with_filter_and_progress(roots, filter, normalize_root, progress)
                .map_err(to_search_err)?
        };
        let image_stats = match ocr {
            Some(engine) => {
                progress.on_phase(scout_indexer::IndexPhase::Image);
                if let Some(disc) = image_discovery.filter(|_| !roots.is_empty()) {
                    match disc.discover() {
                        Ok(paths) => {
                            let paths =
                                filter_discovered_to_scope(paths, roots, filter, normalize_root);
                            if paths.is_empty() {
                                docs.index_image_dirs_with_filter_and_progress(
                                    roots,
                                    engine,
                                    filter,
                                    normalize_root,
                                    progress,
                                )
                                .map_err(to_search_err)?
                            } else {
                                let mtimes = known_mtimes_cached(&docs, &mut known_mtimes)?;
                                let stats = docs
                                    .index_image_paths_with_known_mtimes(
                                        &paths, engine, mtimes, progress,
                                    )
                                    .map_err(to_search_err)?;
                                docs.prune_deleted().map_err(to_search_err)?;
                                stats
                            }
                        }
                        Err(_) => docs
                            .index_image_dirs_with_filter_and_progress(
                                roots,
                                engine,
                                filter,
                                normalize_root,
                                progress,
                            )
                            .map_err(to_search_err)?,
                    }
                } else {
                    docs.index_image_dirs_with_filter_and_progress(
                        roots,
                        engine,
                        filter,
                        normalize_root,
                        progress,
                    )
                    .map_err(to_search_err)?
                }
            }
            None => IndexStats::default(),
        };
        let music = MusicIndex::open(&self.db_path).map_err(to_search_err)?;
        // phase 通知同 progress 版本（Everything 全盘发现分支无 per_file 进度、仅 phase chip 兜底）。
        // 2026-07-06 拍板：roots 为空直接跳过发现器（零索引 = 连 es.exe 都不 spawn）；
        // 发现结果按 roots 过滤后入库（`filter_discovered_to_roots`）。
        let music_stats = if let Some(disc) = discovery.filter(|_| !roots.is_empty()) {
            progress.on_phase(scout_indexer::IndexPhase::MusicDiscovery);
            if let Ok(paths) = disc.discover_audio() {
                let paths = filter_discovered_to_scope(paths, roots, filter, normalize_root);
                if paths.is_empty() {
                    progress.on_phase(scout_indexer::IndexPhase::MusicScan);
                    music
                        .index_dirs_with_filter_and_progress(
                            roots,
                            filter,
                            normalize_root,
                            progress,
                        )
                        .map_err(to_search_err)?
                } else {
                    let stats = music.index_paths(&paths, progress).map_err(to_search_err)?;
                    music.prune_deleted().map_err(to_search_err)?;
                    stats
                }
            } else {
                progress.on_phase(scout_indexer::IndexPhase::MusicScan);
                music
                    .index_dirs_with_filter_and_progress(roots, filter, normalize_root, progress)
                    .map_err(to_search_err)?
            }
        } else {
            progress.on_phase(scout_indexer::IndexPhase::MusicScan);
            music
                .index_dirs_with_filter_and_progress(roots, filter, normalize_root, progress)
                .map_err(to_search_err)?
        };
        optimize_fts_if_changed(&music, &music_stats, &docs, &doc_stats, &image_stats);
        Ok((music_stats, doc_stats, image_stats))
    }

    /// `reindex_scoped_with_progress` 的可注入版本（测试用 mock 发现器 / OCR 引擎）。
    /// 语义同 `reindex_with`，但文档 / 图片走带 progress 的 indexer API。
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)] // 同 line 232 理由（2026-07-28 加 progress 透传后超限）
    pub(crate) fn reindex_with_progress_inner(
        &self,
        discovery: Option<&dyn scout_indexer::AudioDiscovery>,
        doc_discovery: Option<&dyn scout_indexer::PathDiscovery>,
        image_discovery: Option<&dyn scout_indexer::PathDiscovery>,
        ocr: Option<&dyn OcrEngine>,
        roots: &[PathBuf],
        exclude: &scout_indexer::GlobSet,
        progress: &dyn scout_indexer::IndexProgress,
    ) -> Result<(IndexStats, IndexStats, IndexStats), SearchError> {
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let scope_filter = scout_indexer::ExcludeFilter::from_basename_set(exclude);
        // 2026-07-28：执行顺序改为「文档 → 图片 → 音频」（详见 §9 说明，同 filter 版 inner）。
        progress.on_phase(scout_indexer::IndexPhase::Doc);
        let docs = DocumentIndex::open(&self.db_path).map_err(to_search_err)?;
        // BETA-64 T7 收尾：文档轮 + 图片轮共享同一份 all_modified_times()（见
        // `known_mtimes_cached` 文档），同 filter 版 inner。
        let mut known_mtimes = None;
        // BETA-64 T5：文档发现层（同 filter 版 inner 同款模式，见其注释）。
        let doc_stats = if let Some(disc) = doc_discovery.filter(|_| !roots.is_empty()) {
            match disc.discover() {
                Ok(paths) => {
                    let paths =
                        filter_discovered_to_scope(paths, roots, &scope_filter, identity_root);
                    if paths.is_empty() {
                        docs.index_dirs_excluding_with_progress(roots, exclude, progress)
                            .map_err(to_search_err)?
                    } else {
                        let mtimes = known_mtimes_cached(&docs, &mut known_mtimes)?;
                        let stats = docs
                            .index_paths_with_known_mtimes(&paths, mtimes, progress)
                            .map_err(to_search_err)?;
                        docs.prune_deleted().map_err(to_search_err)?;
                        stats
                    }
                }
                Err(_) => docs
                    .index_dirs_excluding_with_progress(roots, exclude, progress)
                    .map_err(to_search_err)?,
            }
        } else {
            docs.index_dirs_excluding_with_progress(roots, exclude, progress)
                .map_err(to_search_err)?
        };
        let image_stats = match ocr {
            Some(engine) => {
                progress.on_phase(scout_indexer::IndexPhase::Image);
                if let Some(disc) = image_discovery.filter(|_| !roots.is_empty()) {
                    match disc.discover() {
                        Ok(paths) => {
                            let paths = filter_discovered_to_scope(
                                paths,
                                roots,
                                &scope_filter,
                                identity_root,
                            );
                            if paths.is_empty() {
                                docs.index_image_dirs_excluding_with_progress(
                                    roots, engine, exclude, progress,
                                )
                                .map_err(to_search_err)?
                            } else {
                                let mtimes = known_mtimes_cached(&docs, &mut known_mtimes)?;
                                let stats = docs
                                    .index_image_paths_with_known_mtimes(
                                        &paths, engine, mtimes, progress,
                                    )
                                    .map_err(to_search_err)?;
                                docs.prune_deleted().map_err(to_search_err)?;
                                stats
                            }
                        }
                        Err(_) => docs
                            .index_image_dirs_excluding_with_progress(
                                roots, engine, exclude, progress,
                            )
                            .map_err(to_search_err)?,
                    }
                } else {
                    docs.index_image_dirs_excluding_with_progress(roots, engine, exclude, progress)
                        .map_err(to_search_err)?
                }
            }
            None => IndexStats::default(),
        };
        let music = MusicIndex::open(&self.db_path).map_err(to_search_err)?;
        // BETA-33 cycle 7-a：每 phase 开始前调 `on_phase`，让 UI 显示 chip「🎵 扫描音乐 …」等。
        // 全盘发现成功 → MusicDiscovery（无 per-file 进度、Everything 秒级、UI 显示"请稍候"文案）；
        // 发现失败 fallback → MusicScan（走 walkdir 有进度）；无发现器同 MusicScan。
        // 2026-07-06 拍板：roots 空跳过发现器、发现结果按 roots 过滤（同 filter 版 inner）。
        let music_stats = if let Some(disc) = discovery.filter(|_| !roots.is_empty()) {
            progress.on_phase(scout_indexer::IndexPhase::MusicDiscovery);
            if let Ok(paths) = disc.discover_audio() {
                let paths = filter_discovered_to_scope(paths, roots, &scope_filter, identity_root);
                if paths.is_empty() {
                    progress.on_phase(scout_indexer::IndexPhase::MusicScan);
                    music
                        .index_dirs_excluding_with_progress(roots, exclude, progress)
                        .map_err(to_search_err)?
                } else {
                    let stats = music.index_paths(&paths, progress).map_err(to_search_err)?;
                    music.prune_deleted().map_err(to_search_err)?;
                    stats
                }
            } else {
                progress.on_phase(scout_indexer::IndexPhase::MusicScan);
                music
                    .index_dirs_excluding_with_progress(roots, exclude, progress)
                    .map_err(to_search_err)?
            }
        } else {
            progress.on_phase(scout_indexer::IndexPhase::MusicScan);
            music
                .index_dirs_excluding_with_progress(roots, exclude, progress)
                .map_err(to_search_err)?
        };
        optimize_fts_if_changed(&music, &music_stats, &docs, &doc_stats, &image_stats);
        Ok((music_stats, doc_stats, image_stats))
    }

    /// [`reindex`](Self::reindex) 的可注入版本（测试用 mock 发现器 / OCR 引擎）。
    /// 文档 / 图片 / 音乐三路均遵循同一模式（BETA-64 T5 文档/图片对齐 BETA-01A 音乐）：
    /// 发现器可用且 roots 非空 → 枚举结果按对应 roots 过滤后 `index_paths`（不自带回收，
    /// 随后显式 `prune_deleted`）；发现失败 / 无发现器 / roots 空 → 回退 `index_dirs`
    /// （自带回收）。图片仅在 `ocr` 为 Some 时才跑（无论走发现层还是目录扫描）。
    /// 2026-07-28 起执行顺序改为文档 → 图片 → 音频（详见 docs/index-performance-design.md §9）。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reindex_with(
        &self,
        discovery: Option<&dyn scout_indexer::AudioDiscovery>,
        doc_discovery: Option<&dyn scout_indexer::PathDiscovery>,
        image_discovery: Option<&dyn scout_indexer::PathDiscovery>,
        ocr: Option<&dyn OcrEngine>,
        music_roots: &[PathBuf],
        doc_roots: &[PathBuf],
        image_roots: &[PathBuf],
        exclude: &scout_indexer::GlobSet,
    ) -> Result<(IndexStats, IndexStats, IndexStats), SearchError> {
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let scope_filter = scout_indexer::ExcludeFilter::from_basename_set(exclude);
        // 2026-07-28：执行顺序改为「文档 → 图片 → 音频」（详见 §9 说明，同其余两个 inner）。
        let docs = DocumentIndex::open(&self.db_path).map_err(to_search_err)?;
        // BETA-64 T7 收尾：文档轮 + 图片轮共享同一份 all_modified_times()（见
        // `known_mtimes_cached` 文档）——roots 虽分开传（`doc_roots`/`image_roots`），
        // 但两轮读的是同一张 `documents` 表、预取的是同一份全表 mtime，共享安全。
        let mut known_mtimes = None;
        let doc_stats = match doc_discovery.filter(|_| !doc_roots.is_empty()) {
            Some(disc) => match disc.discover() {
                Ok(paths) => {
                    let paths =
                        filter_discovered_to_scope(paths, doc_roots, &scope_filter, identity_root);
                    if paths.is_empty() {
                        docs.index_dirs_excluding(doc_roots, exclude)
                            .map_err(to_search_err)?
                    } else {
                        let mtimes = known_mtimes_cached(&docs, &mut known_mtimes)?;
                        let stats = docs
                            .index_paths_with_known_mtimes(&paths, mtimes, &NoopProgress)
                            .map_err(to_search_err)?;
                        docs.prune_deleted().map_err(to_search_err)?;
                        stats
                    }
                }
                Err(_) => docs
                    .index_dirs_excluding(doc_roots, exclude)
                    .map_err(to_search_err)?,
            },
            None => docs
                .index_dirs_excluding(doc_roots, exclude)
                .map_err(to_search_err)?,
        };
        // 图片 OCR：引擎可用才跑（与文档共用 documents 表）。
        let image_stats = match ocr {
            Some(engine) => match image_discovery.filter(|_| !image_roots.is_empty()) {
                Some(disc) => match disc.discover() {
                    Ok(paths) => {
                        let paths = filter_discovered_to_scope(
                            paths,
                            image_roots,
                            &scope_filter,
                            identity_root,
                        );
                        if paths.is_empty() {
                            docs.index_image_dirs_excluding(image_roots, engine, exclude)
                                .map_err(to_search_err)?
                        } else {
                            let mtimes = known_mtimes_cached(&docs, &mut known_mtimes)?;
                            let stats = docs
                                .index_image_paths_with_known_mtimes(
                                    &paths,
                                    engine,
                                    mtimes,
                                    &NoopProgress,
                                )
                                .map_err(to_search_err)?;
                            docs.prune_deleted().map_err(to_search_err)?;
                            stats
                        }
                    }
                    Err(_) => docs
                        .index_image_dirs_excluding(image_roots, engine, exclude)
                        .map_err(to_search_err)?,
                },
                None => docs
                    .index_image_dirs_excluding(image_roots, engine, exclude)
                    .map_err(to_search_err)?,
            },
            None => IndexStats::default(),
        };
        let music = MusicIndex::open(&self.db_path).map_err(to_search_err)?;
        // 2026-07-06 拍板：music_roots 空跳过发现器（零索引不 spawn es.exe）、
        // 发现结果按 roots 过滤后入库。
        let music_stats = match discovery.filter(|_| !music_roots.is_empty()) {
            // 全盘发现成功 → 索引发现到的路径 + 回收已删（index_paths 不自带回收，BETA-07）。
            Some(disc) => match disc.discover_audio() {
                Ok(paths) => {
                    let paths = filter_discovered_to_scope(
                        paths,
                        music_roots,
                        &scope_filter,
                        identity_root,
                    );
                    if paths.is_empty() {
                        music
                            .index_dirs_excluding(music_roots, exclude)
                            .map_err(to_search_err)?
                    } else {
                        let stats = music
                            .index_paths(&paths, &NoopProgress)
                            .map_err(to_search_err)?;
                        music.prune_deleted().map_err(to_search_err)?;
                        stats
                    }
                }
                // 发现失败（工具不可用）→ 回退目录扫描（index_dirs 已自带回收）。
                Err(_) => music
                    .index_dirs_excluding(music_roots, exclude)
                    .map_err(to_search_err)?,
            },
            // 无发现器（不支持平台）/ roots 空 → 回退目录扫描。
            None => music
                .index_dirs_excluding(music_roots, exclude)
                .map_err(to_search_err)?,
        };
        optimize_fts_if_changed(&music, &music_stats, &docs, &doc_stats, &image_stats);
        Ok((music_stats, doc_stats, image_stats))
    }

    /// 取指定路径的本地索引预览（BETA-20）。先查音乐表（命中即音频元数据），
    /// 再查文档表（命中即文档/OCR 图片正文）；都无 → `None`（交前端回退展示文件信息）。
    /// `fts_match`（可选，原始 FTS5 表达式）仅用于文档命中片段高亮。
    /// **只读索引 DB**：不读磁盘原文件，故不触发占位符水合（`is_online_only` 在此路径无关）。
    pub fn preview(
        &self,
        path: &str,
        fts_match: Option<&str>,
    ) -> Result<Option<LocalPreview>, SearchError> {
        // 未 reindex（db 不存在）→ 无预览。
        if !self.db_path.exists() {
            return Ok(None);
        }
        let music = MusicIndex::open(&self.db_path).map_err(to_search_err)?;
        if let Some(entry) = music.entry_for_path(path).map_err(to_search_err)? {
            return Ok(Some(LocalPreview::Music(entry)));
        }
        let docs = DocumentIndex::open(&self.db_path).map_err(to_search_err)?;
        if let Some(preview) = docs
            .preview_for_path(path, fts_match)
            .map_err(to_search_err)?
        {
            return Ok(Some(LocalPreview::Document(preview)));
        }
        Ok(None)
    }

    /// BETA-35 cycle 5：取扫描版 PDF 的所有 OCR 段落（`document_passages` 表，按 page_no 升序）。
    /// 用于「命中回页」标签数据源——预览面板显示"第 N 页 · OCR"。db 不存在 / 非扫描 PDF → 空 vec。
    /// **只读索引 DB**，不触磁盘原文件。
    pub fn passages_for_doc(&self, path: &str) -> Result<Vec<PagePassage>, SearchError> {
        if !self.db_path.exists() {
            return Ok(Vec::new());
        }
        let docs = DocumentIndex::open(&self.db_path).map_err(to_search_err)?;
        docs.passages_for_doc(path).map_err(to_search_err)
    }

    /// BETA-35 cycle 5：取扫描版 PDF 的失败页记录（`document_failed_pages` 表）。
    /// 用于"失败页不静默丢"UI 提示——BETA-40 场景 playbook 里给取证复核用。
    pub fn failed_pages_for_doc(&self, path: &str) -> Result<Vec<PageFailure>, SearchError> {
        if !self.db_path.exists() {
            return Ok(Vec::new());
        }
        let docs = DocumentIndex::open(&self.db_path).map_err(to_search_err)?;
        docs.failed_pages_for_doc(path).map_err(to_search_err)
    }

    /// 清空本地索引（BETA-21 一键清除）：`DROP` 全部表（含 FTS5 影子表）+ `VACUUM` 回收磁盘，
    /// index.db 文件**真正缩小**（11MB → 数 KB）。表结构下次 reindex 自动重建。
    /// 委托 [`scout_indexer::clear_index`]（全程走 SQL 连接，绕开 Windows 删文件的独占锁）。
    /// db 文件不存在（从未 reindex）→ 视为已清空、不创建空文件、直接返回。
    pub fn clear(&self) -> Result<(), SearchError> {
        if !self.db_path.exists() {
            return Ok(());
        }
        scout_indexer::clear_index(&self.db_path).map_err(to_search_err)
    }

    /// 非扩展查询（直接 `search`）：用 base intent 的 keyword 构造，无同义词/gazetteer 词组。
    fn search_results(&self, intent: &SearchIntent) -> Result<Vec<SearchResult>, SearchError> {
        self.search_results_inner(intent, None)
    }

    /// 扩展查询（`search_expanded`，fan-out/chain 生产路径）：从 `keyword_groups` 构造
    /// FTS5 布尔表达式（组内恒 OR，组间按 `expanded.match_mode` 取 AND/OR）喂给本地 FTS——
    /// 这样 parser 未抽 keyword、由同义词扩展 / BETA-15E gazetteer 注入到词组的关键词也能
    /// 命中本地索引（OCR / 文档 / 音乐）。词组为空（如纯 artist / 纯类型查询）→ `fts` 为
    /// None，回退 base keyword 路径。
    ///
    /// 2026-07-20：移除 BETA-57 的「组间 AND 0 命中静默放宽为 OR」自动兜底——用户反馈该
    /// 静默放宽会把「只命中部分条件」的结果当成正常结果返回，造成大量不符合要求的噪音。
    /// 现改为**显式**的全局 `MatchMode` 配置：`All` 严格要求全部复合条件命中（0 命中就是
    /// 0，不再重试）；`Any` 直接按任一条件命中查询，无需二次查询。
    fn search_results_expanded(
        &self,
        expanded: &ExpandedSearchIntent,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let fts = fts_match_from_groups(&expanded.keyword_groups, expanded.match_mode);
        self.search_results_inner(&expanded.base, fts.as_deref())
    }

    /// 共享查询执行。`fts` 为预构造的原始 FTS5 表达式（来自词组）；`Some` 时优先于 base keyword
    /// 派生的 text，并让「base 无 keyword 但词组有词」的查询也能查 FTS（关键修复点）。
    fn search_results_inner(
        &self,
        intent: &SearchIntent,
        fts: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        // 未 reindex（db 不存在）→ 空结果（非错误），交系统后端服务。
        if !self.db_path.exists() {
            return Ok(Vec::new());
        }
        match intent {
            SearchIntent::MediaSearch(media) if media.media_type == MediaType::Audio => {
                let mut query = build_music_query(media);
                if let Some(f) = fts {
                    query.fts_match = Some(f.to_owned());
                    query.text = None;
                }
                let idx = MusicIndex::open(&self.db_path).map_err(to_search_err)?;
                let entries = idx.query(&query).map_err(to_search_err)?;
                Ok(entries.into_iter().map(music_entry_to_result).collect())
            }
            // 图片 / 截图（BETA-03 OCR）：词组或 base keyword → 查图片 doc_types 的 FTS；都无 → 空。
            SearchIntent::MediaSearch(media)
                if matches!(media.media_type, MediaType::Image | MediaType::Screenshot) =>
            {
                match image_doc_query(media, fts) {
                    None => Ok(Vec::new()),
                    Some(query) => {
                        let idx = DocumentIndex::open(&self.db_path).map_err(to_search_err)?;
                        let hits = idx.query(&query).map_err(to_search_err)?;
                        Ok(hits.into_iter().map(doc_hit_to_result).collect())
                    }
                }
            }
            // 视频等其他媒体：本地无索引 → 空。
            SearchIntent::MediaSearch(_) => Ok(Vec::new()),
            SearchIntent::FileSearch(fs) => match file_doc_query(fs, fts) {
                // 纯扩展名 / 类型查询（无 keyword、无词组）→ 空，交系统后端。
                None => Ok(Vec::new()),
                Some(query) => {
                    let idx = DocumentIndex::open(&self.db_path).map_err(to_search_err)?;
                    let hits = idx.query(&query).map_err(to_search_err)?;
                    Ok(hits.into_iter().map(doc_hit_to_result).collect())
                }
            },
            SearchIntent::Refine(_) | SearchIntent::FileAction(_) | SearchIntent::Clarify(_) => {
                Err(SearchError::UnsupportedIntent {
                    detail: "LocalIndexBackend 仅支持 file_search / media_search".to_string(),
                })
            }
        }
    }
}

/// [`fts_match_from_groups`] 的公开版（BETA-20 预览高亮复用与搜索同款的「词组 → FTS5 表达式」
/// 逻辑：组内 OR、组间按 `match_mode` 取 AND/OR），让命中片段与实际搜索命中口径一致。
#[must_use]
pub fn fts_match_for_groups(groups: &[KeywordGroup], match_mode: MatchMode) -> Option<String> {
    fts_match_from_groups(groups, match_mode)
}

/// 把 `keyword_groups` 译成 FTS5 MATCH 表达式：**组内恒 OR**，组间按 `match_mode` 取
/// AND（[`MatchMode::All`]，全部复合条件命中）或 OR（[`MatchMode::Any`]，任一条件命中），
/// 每词项双引号包裹（内部 `"` 转义为 `""`）。空词项跳过；全空 → `None`。trigram tokenizer
/// 下引号短语即子串匹配。
///
/// BETA-42：`documents_fts` 的 `tokenize='trigram'` 要求词项 ≥3 字符才能生成可匹配 token，
/// 但中文关键词切词允许 2 字（"判决"/"违约"等）。这类短词若原样进 AND 条件，该子句在
/// trigram 索引下恒不可能匹配，会让整个 AND 表达式结构性 0 命中（哪怕其余词项都能命中）。
/// 剔除 <3 字纯 CJK 词项——不参与本地 FTS 匹配约束，但不拖垮同组内可匹配的长词 / 其他组。
fn fts_match_from_groups(groups: &[KeywordGroup], match_mode: MatchMode) -> Option<String> {
    match match_mode {
        MatchMode::All => {
            let mut clauses: Vec<String> = Vec::new();
            for group in groups {
                let terms = sanitized_group_terms(group);
                match terms.len() {
                    0 => {}
                    1 => clauses.push(terms.into_iter().next().unwrap_or_default()),
                    _ => clauses.push(format!("({})", terms.join(" OR "))),
                }
            }
            if clauses.is_empty() {
                None
            } else {
                Some(clauses.join(" AND "))
            }
        }
        MatchMode::Any => {
            let all_terms: Vec<String> = groups.iter().flat_map(sanitized_group_terms).collect();
            match all_terms.len() {
                0 => None,
                1 => all_terms.into_iter().next(),
                _ => Some(format!("({})", all_terms.join(" OR "))),
            }
        }
    }
}

/// 单个词组 → 清洗后的 FTS5 词项列表（trim、剔空、剔 <3 字纯 CJK、内部 `"` 转义为 `""`）。
/// [`fts_match_from_groups`] 的 `All`/`Any` 两种模式共用，保证「哪些词项参与匹配」口径一致。
fn sanitized_group_terms(group: &KeywordGroup) -> Vec<String> {
    group
        .all()
        .into_iter()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .filter(|t| !is_short_cjk_term(t))
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect()
}

/// 词项是否为 <3 字符的纯 CJK 词（trigram tokenizer 结构性无法匹配，见 [`fts_match_from_groups`]）。
/// 只判纯 CJK（不含 ASCII/数字混排词），避免误伤英文缩写等 <3 字符但可正常匹配的词项。
fn is_short_cjk_term(term: &str) -> bool {
    let count = term.chars().count();
    count > 0 && count < 3 && term.chars().all(is_cjk)
}

/// CJK 表意字符（统一表意 + 扩展 A + 兼容表意），沿用项目内既有范围定义。
const fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF
    )
}

/// FileSearch → DocumentQuery，`fts`（词组 FTS）优先于 base keyword；都无 → None（交系统后端）。
fn file_doc_query(fs: &FileSearch, fts: Option<&str>) -> Option<DocumentQuery> {
    match fts {
        Some(f) => Some(DocumentQuery {
            fts_match: Some(f.to_owned()),
            limit: fs.limit,
            ..DocumentQuery::default()
        }),
        None => build_doc_query(fs),
    }
}

/// MediaSearch(Image/Screenshot) → DocumentQuery，`fts` 优先；都无 → None。`doc_types` 框定图片。
fn image_doc_query(media: &MediaSearch, fts: Option<&str>) -> Option<DocumentQuery> {
    match fts {
        Some(f) => Some(DocumentQuery {
            fts_match: Some(f.to_owned()),
            doc_types: Some(IMAGE_DOC_TYPES.iter().map(|s| (*s).to_string()).collect()),
            limit: media.limit,
            ..DocumentQuery::default()
        }),
        None => build_image_query(media),
    }
}

impl SearchBackend for LocalIndexBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::NativeIndex
    }

    fn is_available(&self) -> bool {
        true
    }

    fn search<'a>(
        &'a self,
        intent: &'a SearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        Box::pin(async move {
            let results = self.search_results(intent)?;
            Ok(backend_stream_from_results(results, cancel))
        })
    }

    /// 覆盖默认 `search_expanded`（默认仅 `search(&base)`，丢掉同义词/gazetteer 词组）：
    /// 消费 `keyword_groups` 构造 FTS5 布尔查询，让词组注入的关键词命中本地 FTS。
    fn search_expanded<'a>(
        &'a self,
        expanded: &'a ExpandedSearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        Box::pin(async move {
            let results = self.search_results_expanded(expanded)?;
            Ok(backend_stream_from_results(results, cancel))
        })
    }
}

/// MediaSearch(audio) → MusicQuery。text 取 keywords，否则 title；artist/album 走结构化过滤。
pub(crate) fn build_music_query(media: &MediaSearch) -> MusicQuery {
    let text = media
        .keywords
        .as_ref()
        .map(|k| k.join(" "))
        .filter(|s| !s.trim().is_empty())
        .or_else(|| media.title.clone());
    MusicQuery {
        text,
        fts_match: None,
        artist: media.artist.clone(),
        album: media.album.clone(),
        format: None,
        limit: media.limit,
    }
}

/// FileSearch → DocumentQuery。仅当有非空 keyword 时返回 `Some`（纯扩展名查询交系统后端）。
pub(crate) fn build_doc_query(fs: &FileSearch) -> Option<DocumentQuery> {
    let text = fs
        .keywords
        .as_ref()
        .map(|k| k.join(" "))
        .filter(|s| !s.trim().is_empty())?;
    Some(DocumentQuery {
        text: Some(text),
        fts_match: None,
        author: None,
        doc_type: None,
        doc_types: None,
        limit: fs.limit,
    })
}

/// MediaSearch(Image/Screenshot) → DocumentQuery。仅当有非空 keyword 时返回 `Some`
/// （纯「找截图」无内容词交系统后端按文件名/类型搜）；`doc_types` 框定只返图片记录。
pub(crate) fn build_image_query(media: &MediaSearch) -> Option<DocumentQuery> {
    let text = media
        .keywords
        .as_ref()
        .map(|k| k.join(" "))
        .filter(|s| !s.trim().is_empty())?;
    Some(DocumentQuery {
        text: Some(text),
        fts_match: None,
        author: None,
        doc_type: None,
        doc_types: Some(IMAGE_DOC_TYPES.iter().map(|s| (*s).to_string()).collect()),
        limit: media.limit,
    })
}

pub(crate) fn music_entry_to_result(e: MusicEntry) -> SearchResult {
    let path = canonical(Path::new(&e.path));
    let size_bytes = scout_search_backend::file_size_bytes(&path);
    SearchResult {
        id: result_id(&path),
        name: e.file_name,
        path,
        source: BackendKind::NativeIndex,
        match_type: MatchType::Metadata,
        score: None,
        metadata: SearchResultMetadata {
            modified_time: unix_to_utc(e.modified_time),
            size_bytes,
            artist: e.artist,
            title: e.title,
            album: e.album,
            duration_seconds: e.duration_secs,
            ..Default::default()
        },
    }
}

pub(crate) fn doc_hit_to_result(hit: DocumentHit) -> SearchResult {
    let e = hit.entry;
    let path = canonical(Path::new(&e.path));
    let size_bytes = scout_search_backend::file_size_bytes(&path);
    SearchResult {
        id: result_id(&path),
        name: e.file_name,
        path,
        source: BackendKind::NativeIndex,
        match_type: MatchType::Content,
        score: None,
        metadata: SearchResultMetadata {
            modified_time: unix_to_utc(e.modified_time),
            size_bytes,
            title: e.title,
            ..Default::default()
        },
    }
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn unix_to_utc(secs: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(secs, 0).single()
}

#[allow(clippy::needless_pass_by_value)] // 用于 `.map_err(to_search_err)`，owned 更顺手
fn to_search_err(e: scout_indexer::IndexError) -> SearchError {
    SearchError::Io {
        detail: e.to_string(),
    }
}

/// 2026-07-29：三条生产 reindex 入口收尾时调用——真机复盘（14570 文档索引，长期
/// 反复增删的旧库比全新库慢 8 倍）确认到的一个具体机制：`music_fts`/`documents_fts`
/// 此前只在"一键清空索引"时才整理，日常从不 `optimize`，trigram 分词器对长期碎片化
/// 格外敏感。只在本轮确有变更（added/updated/removed 之和 > 0）时才调用，避免空轮
/// 也做无谓 I/O；失败是尽力而为的维护操作，不影响本轮已经成功落库的索引结果，
/// 静默忽略（本 crate 无 tracing 依赖，不为一行警告日志新增依赖）。
fn optimize_fts_if_changed(
    music: &MusicIndex,
    music_stats: &IndexStats,
    docs: &DocumentIndex,
    doc_stats: &IndexStats,
    image_stats: &IndexStats,
) {
    fn changed(s: &IndexStats) -> bool {
        s.added + s.updated + s.removed > 0
    }
    if changed(music_stats) {
        let _ = music.optimize_fts();
    }
    if changed(doc_stats) || changed(image_stats) {
        let _ = docs.optimize_fts();
    }
}

/// BETA-64 T7 收尾：三条 `reindex_*` 入口在同一轮里可能先后调
/// `docs.index_paths`/`docs.index_image_paths`（发现层文档轮 + 图片轮都命中时），
/// 两者各自内部现取一份 `DocumentIndex::all_modified_times()` 会对 `documents` 表
/// 做两次没必要的全表扫描。`cache` 由调用方持有一个函数级 `Option`，本函数只在第一次
/// 未命中时真的查一次库，后续复用；返回 `Err` 时 `cache` 保持 `None`（下次调用会重试，
/// 不会缓存住一次失败）。
fn known_mtimes_cached<'a>(
    docs: &DocumentIndex,
    cache: &'a mut Option<std::collections::HashMap<String, i64>>,
) -> Result<&'a std::collections::HashMap<String, i64>, SearchError> {
    if cache.is_none() {
        *cache = Some(docs.all_modified_times().map_err(to_search_err)?);
    }
    #[allow(clippy::expect_used)] // 上面刚确保过 Some
    Ok(cache.as_ref().expect("known_mtimes_cached 刚确保过 Some"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use scout_search_backend::SchemaVersion;

    fn media_audio(artist: Option<&str>, keywords: Option<Vec<&str>>) -> SearchIntent {
        SearchIntent::MediaSearch(MediaSearch {
            schema_version: SchemaVersion::V1,
            language: None,
            media_type: MediaType::Audio,
            artist: artist.map(str::to_string),
            title: None,
            album: None,
            genre: None,
            quality: None,
            duration: None,
            keywords: keywords.map(|k| k.iter().map(|s| (*s).to_string()).collect()),
            extensions: None,
            file_type: None,
            location: None,
            modified_time: None,
            created_time: None,
            accessed_time: None,
            size: None,
            exclude_extensions: None,
            exclude_file_type: None,
            sort: None,
            limit: None,
        })
    }

    fn media_image() -> SearchIntent {
        let SearchIntent::MediaSearch(mut m) = media_audio(None, None) else {
            unreachable!()
        };
        m.media_type = MediaType::Image;
        SearchIntent::MediaSearch(m)
    }

    fn media_image_kw(keywords: Vec<&str>) -> SearchIntent {
        let SearchIntent::MediaSearch(mut m) = media_audio(None, Some(keywords)) else {
            unreachable!()
        };
        m.media_type = MediaType::Image;
        SearchIntent::MediaSearch(m)
    }

    /// stub OCR：不读文件，返回固定文字。
    #[derive(Debug)]
    struct StubOcr(String);
    impl OcrEngine for StubOcr {
        fn recognize(&self, _: &Path) -> Result<String, scout_indexer::IndexError> {
            Ok(self.0.clone())
        }
        fn name(&self) -> &'static str {
            "Stub"
        }
    }

    fn file_search(keywords: Option<Vec<&str>>) -> SearchIntent {
        SearchIntent::FileSearch(FileSearch {
            schema_version: SchemaVersion::V1,
            language: None,
            keywords: keywords.map(|k| k.iter().map(|s| (*s).to_string()).collect()),
            extensions: None,
            file_type: None,
            location: None,
            modified_time: None,
            created_time: None,
            accessed_time: None,
            size: None,
            exclude_extensions: None,
            exclude_file_type: None,
            sort: None,
            limit: None,
        })
    }

    fn music_entry(path: &str, artist: &str) -> MusicEntry {
        MusicEntry {
            path: path.to_string(),
            file_name: "song.mp3".to_string(),
            artist: Some(artist.to_string()),
            title: Some("朋友".to_string()),
            album: Some("专辑".to_string()),
            duration_secs: Some(240.0),
            format: Some("MP3".to_string()),
            bitrate: Some(320),
            modified_time: 1000,
        }
    }

    #[test]
    fn music_query_uses_keywords_then_title_and_structured_artist() {
        let SearchIntent::MediaSearch(m) = media_audio(Some("周华健"), Some(vec!["朋友"]))
        else {
            unreachable!()
        };
        let q = build_music_query(&m);
        assert_eq!(q.text.as_deref(), Some("朋友"));
        assert_eq!(q.artist.as_deref(), Some("周华健"));
    }

    #[test]
    fn music_entry_maps_metadata() {
        let r = music_entry_to_result(music_entry("/no/such/song.mp3", "周华健"));
        assert_eq!(r.source, BackendKind::NativeIndex);
        assert_eq!(r.match_type, MatchType::Metadata);
        assert_eq!(r.metadata.artist.as_deref(), Some("周华健"));
        assert_eq!(r.metadata.duration_seconds, Some(240.0));
        assert!(r.metadata.modified_time.is_some());
        // 路径不存在 → canonical 回退原值。
        assert_eq!(r.path, PathBuf::from("/no/such/song.mp3"));
    }

    #[test]
    fn music_entry_existing_file_maps_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("song.mp3");
        std::fs::write(&path, b"synthetic-audio-bytes").unwrap();

        let r = music_entry_to_result(music_entry(&path.to_string_lossy(), "周华健"));

        assert_eq!(r.metadata.size_bytes, Some(21));
    }

    #[test]
    fn doc_query_none_without_keyword() {
        let SearchIntent::FileSearch(fs) = file_search(None) else {
            unreachable!()
        };
        assert!(build_doc_query(&fs).is_none());
        let SearchIntent::FileSearch(fs2) = file_search(Some(vec!["  "])) else {
            unreachable!()
        };
        assert!(build_doc_query(&fs2).is_none(), "空白 keyword 也应 None");
    }

    #[test]
    fn doc_query_some_with_keyword() {
        let SearchIntent::FileSearch(fs) = file_search(Some(vec!["季度", "预算"])) else {
            unreachable!()
        };
        let q = build_doc_query(&fs).unwrap();
        assert_eq!(q.text.as_deref(), Some("季度 预算"));
    }

    #[test]
    fn nonexistent_db_returns_empty() {
        let backend = LocalIndexBackend::new("/no/such/index.db");
        let out = backend
            .search_results(&media_audio(Some("x"), None))
            .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn image_media_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        // 让 db 存在。
        let backend = LocalIndexBackend::new(&db);
        backend.reindex(&[], &[], &[]).unwrap();
        assert!(backend.search_results(&media_image()).unwrap().is_empty());
    }

    #[test]
    fn no_keyword_filesearch_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let backend = LocalIndexBackend::new(&db);
        backend.reindex(&[], &[], &[]).unwrap();
        assert!(backend
            .search_results(&file_search(None))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn refine_intent_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let backend = LocalIndexBackend::new(&db);
        backend.reindex(&[], &[], &[]).unwrap();
        // 用 Clarify 触发 UnsupportedIntent（构造最简单）。
        let intent = SearchIntent::Clarify(scout_search_backend::Clarify {
            schema_version: SchemaVersion::V1,
            language: None,
            question: "?".to_string(),
            options: None,
            reason: scout_search_backend::ClarifyReason::AmbiguousLocation,
        });
        assert!(matches!(
            backend.search_results(&intent),
            Err(SearchError::UnsupportedIntent { .. })
        ));
    }

    #[tokio::test]
    async fn end_to_end_document_search_via_reindex() {
        use futures_util::StreamExt;

        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), "本季度预算与营收分析报告").unwrap();
        std::fs::write(docs.join("b.txt"), "无关内容").unwrap();

        let db = dir.path().join("index.db");
        let backend = LocalIndexBackend::new(&db);
        // BETA-64 T5：显式绕开真实平台发现（`reindex_with(None,...)`），走确定性 WalkDir——
        // 本测试验证的是搜索链路而非发现层，真实 Spotlight/Everything 对刚写入的临时文件
        // 有索引延迟、不该引入非确定性（发现层自身行为由下方"BETA-01A"节的 mock 注入测试覆盖）。
        let (_m, d, _img) = backend
            .reindex_with(
                None,
                None,
                None,
                None,
                &[],
                std::slice::from_ref(&docs),
                &[],
                &scout_indexer::GlobSet::empty(),
            )
            .expect("reindex docs");
        assert_eq!(d.added, 2);

        let intent = file_search(Some(vec!["季度预算"]));
        let stream = backend
            .search(&intent, CancellationToken::new())
            .await
            .expect("search future");
        let results: Vec<_> = stream.collect().await;
        let ok: Vec<_> = results.into_iter().filter_map(Result::ok).collect();
        assert_eq!(ok.len(), 1, "应命中 a.txt");
        assert_eq!(ok[0].source, BackendKind::NativeIndex);
        assert_eq!(ok[0].match_type, MatchType::Content);
        assert!(ok[0].path.ends_with("a.txt"));
        assert_eq!(
            ok[0].metadata.size_bytes,
            Some(std::fs::metadata(docs.join("a.txt")).unwrap().len()),
            "内容命中也必须返回文件基本大小属性"
        );
    }

    // ===== BETA-03：图片 OCR 检索路由 =====

    #[test]
    fn image_query_some_with_keyword_none_without() {
        let SearchIntent::MediaSearch(m) = media_image_kw(vec!["会议纪要"]) else {
            unreachable!()
        };
        let q = build_image_query(&m).expect("有 keyword 应 Some");
        assert_eq!(q.text.as_deref(), Some("会议纪要"));
        assert!(q.doc_types.as_ref().unwrap().contains(&"png".to_string()));
        // 无 keyword → None。
        let SearchIntent::MediaSearch(m2) = media_image() else {
            unreachable!()
        };
        assert!(build_image_query(&m2).is_none());
    }

    #[test]
    fn image_search_with_keyword_hits_and_frames_to_images() {
        let dir = tempfile::tempdir().unwrap();
        let imgs = dir.path().join("pics");
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&imgs).unwrap();
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(imgs.join("shot.png"), b"fake-png").unwrap();
        // 同关键词的文档：MediaSearch(Image) 不应返回它（doc_types 框定）。
        std::fs::write(docs.join("note.txt"), "会议纪要第三季度").unwrap();

        let db = dir.path().join("index.db");
        let backend = LocalIndexBackend::new(&db);
        let stub = StubOcr("会议纪要第三季度".to_string());
        let (_m, _d, img) = backend
            .reindex_with(
                None,
                None,
                None,
                Some(&stub),
                &[],
                std::slice::from_ref(&docs),
                std::slice::from_ref(&imgs),
                &scout_indexer::GlobSet::empty(),
            )
            .expect("reindex");
        assert_eq!(img.added, 1, "1 张图片经 OCR 入库");

        // MediaSearch(Image) 带 keyword → 只命中图片，不含 txt。
        let hits = backend
            .search_results(&media_image_kw(vec!["会议纪要"]))
            .unwrap();
        assert_eq!(hits.len(), 1, "doc_types 框定只返图片");
        assert!(hits[0].path.ends_with("shot.png"));
        assert_eq!(hits[0].source, BackendKind::NativeIndex);
        assert_eq!(
            hits[0].metadata.size_bytes,
            Some(std::fs::metadata(imgs.join("shot.png")).unwrap().len()),
            "OCR 内容命中也必须返回文件大小"
        );

        // 无 keyword → 空（交系统后端）。
        assert!(backend.search_results(&media_image()).unwrap().is_empty());

        // FileSearch 同词 → 同一 FTS 天然也覆盖图片（png + txt 都命中）。
        let file_hits = backend
            .search_results(&file_search(Some(vec!["会议纪要"])))
            .unwrap();
        assert_eq!(file_hits.len(), 2, "FileSearch 跨全类型命中 png + txt");
    }

    // ===== fix：search_expanded 消费 keyword_groups（gazetteer/同义词到本地索引）=====

    fn expanded(base: SearchIntent, groups: Vec<KeywordGroup>) -> ExpandedSearchIntent {
        ExpandedSearchIntent {
            base,
            keyword_groups: groups,
            match_mode: MatchMode::default(),
        }
    }

    fn group(head: &str, synonyms: &[&str]) -> KeywordGroup {
        KeywordGroup {
            head: head.to_string(),
            synonyms: synonyms.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn fts_match_from_groups_builds_boolean() {
        // All 模式：组内 OR、组间 AND；单词组不加括号；引号转义。
        assert_eq!(
            fts_match_from_groups(
                &[group("a", &["b"]), KeywordGroup::singleton("c")],
                MatchMode::All
            )
            .as_deref(),
            Some("(\"a\" OR \"b\") AND \"c\"")
        );
        assert_eq!(
            fts_match_from_groups(&[KeywordGroup::singleton("会议纪要")], MatchMode::All)
                .as_deref(),
            Some("\"会议纪要\"")
        );
        assert_eq!(fts_match_from_groups(&[], MatchMode::All), None);
    }

    #[test]
    fn fts_match_from_groups_any_mode_flattens_all_terms_to_or() {
        // Any 模式：所有词组的词项摊平成单个 OR 子句（任一条件命中即可），全局配置显式选择，
        // 不再是 BETA-57 旧版「AND 0 命中静默重试」，而是从一开始就按此口径查询。
        assert_eq!(
            fts_match_from_groups(
                &[
                    KeywordGroup::singleton("体检报告"),
                    KeywordGroup::singleton("健康检查"),
                    KeywordGroup::singleton("健康体检"),
                ],
                MatchMode::Any
            )
            .as_deref(),
            Some("(\"体检报告\" OR \"健康检查\" OR \"健康体检\")")
        );
        // 组内同义词一并摊平进同一 OR。
        assert_eq!(
            fts_match_from_groups(
                &[
                    group("会议纪要", &["会议记录"]),
                    KeywordGroup::singleton("季度报告"),
                ],
                MatchMode::Any
            )
            .as_deref(),
            Some("(\"会议纪要\" OR \"会议记录\" OR \"季度报告\")")
        );
        // 单个有效词组 → 不加括号，与 All 模式单组结果一致。
        assert_eq!(
            fts_match_from_groups(&[KeywordGroup::singleton("体检报告")], MatchMode::Any)
                .as_deref(),
            Some("\"体检报告\"")
        );
        assert_eq!(fts_match_from_groups(&[], MatchMode::Any), None);
    }

    #[test]
    fn fts_match_from_groups_drops_short_cjk_terms_from_and() {
        // BETA-42 回归：「判决 违约金」类查询——2 字纯 CJK 词组（"判决"）trigram 下恒不可能
        // 匹配，若原样进 AND 会让整个表达式结构性 0 命中；剔除后只留可匹配的 3 字词组。
        assert_eq!(
            fts_match_from_groups(
                &[
                    KeywordGroup::singleton("判决"),
                    KeywordGroup::singleton("违约金"),
                ],
                MatchMode::All
            )
            .as_deref(),
            Some("\"违约金\"")
        );
        // 组内同义词有短有长：短词剔除，组内只剩长词（不再是 OR 括号）。
        assert_eq!(
            fts_match_from_groups(&[group("判决", &["裁决书"])], MatchMode::All).as_deref(),
            Some("\"裁决书\"")
        );
        // 组内全为短 CJK 词 → 整组丢弃，不产生恒假的 AND 子句。
        assert_eq!(
            fts_match_from_groups(
                &[
                    KeywordGroup::singleton("判决"),
                    KeywordGroup::singleton("交接"),
                ],
                MatchMode::All
            ),
            None
        );
        // 英文短词（非 CJK）不受影响，沿用既有行为。
        assert_eq!(
            fts_match_from_groups(&[KeywordGroup::singleton("AI")], MatchMode::All).as_deref(),
            Some("\"AI\"")
        );
    }

    #[test]
    fn search_expanded_finds_via_gazetteer_group_when_base_has_no_keyword() {
        // 复现真机 bug：自然查询 parser 无 base.keyword，关键词只在 gazetteer 词组里。
        let dir = tempfile::tempdir().unwrap();
        let imgs = dir.path().join("pics");
        std::fs::create_dir_all(&imgs).unwrap();
        std::fs::write(imgs.join("shot.png"), b"fake-png").unwrap();
        let db = dir.path().join("index.db");
        let backend = LocalIndexBackend::new(&db);
        let stub = StubOcr("会议纪要测试报告".to_string());
        backend
            .reindex_with(
                None,
                None,
                None,
                Some(&stub),
                &[],
                &[],
                std::slice::from_ref(&imgs),
                &scout_indexer::GlobSet::empty(),
            )
            .expect("reindex");

        // base.keywords=None（parser 没抽到）+ 词组 head=会议纪要 → search_expanded 应命中 OCR 图片。
        let exp = expanded(file_search(None), vec![KeywordGroup::singleton("会议纪要")]);
        let hits = backend.search_results_expanded(&exp).unwrap();
        assert_eq!(hits.len(), 1, "词组关键词应经 search_expanded 命中本地 FTS");
        assert!(hits[0].path.ends_with("shot.png"));

        // 对照 bug 现场：非扩展 search（base 无 keyword）→ 0（词组拿不到）。
        assert!(
            backend
                .search_results(&file_search(None))
                .unwrap()
                .is_empty(),
            "非扩展路径无 keyword → 空（修复前的行为）"
        );
    }

    #[test]
    fn search_expanded_synonym_or_matches_alternate_term() {
        // 文档正文只含**同义词**（会议记录），查询 head 是 会议纪要 → 组内 OR 应命中。
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("note.txt"), "本周会议记录摘要要点").unwrap();
        let db = dir.path().join("index.db");
        let backend = LocalIndexBackend::new(&db);
        backend
            .reindex_with(
                None,
                None,
                None,
                None,
                &[],
                std::slice::from_ref(&docs),
                &[],
                &scout_indexer::GlobSet::empty(),
            )
            .expect("reindex");

        let exp = expanded(file_search(None), vec![group("会议纪要", &["会议记录"])]);
        let hits = backend.search_results_expanded(&exp).unwrap();
        assert_eq!(hits.len(), 1, "同义词 会议记录 应经组内 OR 命中");
        assert!(hits[0].path.ends_with("note.txt"));
    }

    /// BETA-56（2026-07-08 真机沉淀）：2 字中文查询（人名/常用词）经生产 `search_expanded`
    /// 全链路——`fts_match_from_groups` 把 <3 字 CJK 词组剥空（trigram 无法命中）→ fts=None
    /// → 回退 base keyword → `DocumentIndex::query` 短查询 LIKE 兜底命中 file_name/title。
    /// 复现真机 bug：搜「燎原」此前 0 命中，兜底后命中 metadata 含它的文档。
    #[test]
    fn search_expanded_short_cjk_query_hits_via_like_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        // file_name 含 2 字人名「燎原」；正文不含，确保命中来自 metadata LIKE 而非 FTS 正文。
        std::fs::write(docs.join("燎原-季度总结.txt"), "本季度营收与成本分析").unwrap();
        std::fs::write(docs.join("其他.txt"), "无关内容").unwrap();
        let db = dir.path().join("index.db");
        let backend = LocalIndexBackend::new(&db);
        backend
            .reindex_with(
                None,
                None,
                None,
                None,
                &[],
                std::slice::from_ref(&docs),
                &[],
                &scout_indexer::GlobSet::empty(),
            )
            .expect("reindex");

        // base.keywords=["燎原"] + 词组 singleton("燎原")：词组被 fts_match_from_groups 剥空，
        // 回退 base keyword 走短查询 LIKE 兜底。
        let exp = expanded(
            file_search(Some(vec!["燎原"])),
            vec![KeywordGroup::singleton("燎原")],
        );
        let hits = backend.search_results_expanded(&exp).unwrap();
        assert_eq!(hits.len(), 1, "2 字人名应经短查询 LIKE 兜底命中 file_name");
        assert!(hits[0].path.ends_with("燎原-季度总结.txt"));

        // 对照：≥3 字正文词（「营收」在 body）此前即经 FTS trigram 正常命中（documents_fts
        // 只索引 title/author/body，file_name 由本次 LIKE 兜底补齐）——证明未动 FTS 主路径。
        let exp3 = expanded(
            file_search(Some(vec!["营收与成本"])),
            vec![KeywordGroup::singleton("营收与成本")],
        );
        let hits3 = backend.search_results_expanded(&exp3).unwrap();
        assert_eq!(hits3.len(), 1, "≥3 字正文词走 FTS 仍命中");
    }

    /// 2026-07-20：全局 `MatchMode` 复合条件配置——替代 BETA-57 旧版「AND 0 命中静默放宽
    /// 为 OR」的自动兜底。用户反馈该静默放宽会把只命中部分条件的结果当正常结果返回，造成
    /// 大量噪音；现改为显式二选一：`All` 严格要求全部条件命中（0 就是 0，不再重试），
    /// `Any` 任一条件命中即可，且从查询发起时就按该口径，不做二次查询。
    #[test]
    fn search_expanded_all_mode_zero_hit_stays_zero_no_silent_relax() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        // 正文含「健康体检报告」（含「体检报告」「健康体检」子串），但**不含**「健康检查」。
        std::fs::write(
            docs.join("体检报告.txt"),
            "健康体检报告 姓名饶燎原 各项指标",
        )
        .unwrap();
        std::fs::write(docs.join("无关.txt"), "本周会议记录摘要").unwrap();
        let db = dir.path().join("index.db");
        let backend = LocalIndexBackend::new(&db);
        backend
            .reindex_with(
                None,
                None,
                None,
                None,
                &[],
                std::slice::from_ref(&docs),
                &[],
                &scout_indexer::GlobSet::empty(),
            )
            .expect("reindex");

        // 三个 ≥3 字词组，组间 AND：正文缺「健康检查」→ AND 结构性 0 命中。
        let groups = vec![
            KeywordGroup::singleton("体检报告"),
            KeywordGroup::singleton("健康检查"),
            KeywordGroup::singleton("健康体检"),
        ];
        assert_eq!(
            fts_match_from_groups(&groups, MatchMode::All).as_deref(),
            Some("\"体检报告\" AND \"健康检查\" AND \"健康体检\"")
        );

        let mut exp = expanded(file_search(None), groups.clone());
        exp.match_mode = MatchMode::All;
        let hits = backend.search_results_expanded(&exp).unwrap();
        assert!(
            hits.is_empty(),
            "All 模式严格要求全部条件命中，0 命中应保持 0，不再静默放宽为 OR"
        );

        // Any 模式：同一份词组显式选任一命中 → 恢复召回。
        let mut exp_any = expanded(file_search(None), groups);
        exp_any.match_mode = MatchMode::Any;
        let hits_any = backend.search_results_expanded(&exp_any).unwrap();
        assert_eq!(hits_any.len(), 1, "Any 模式应经其余词项命中体检报告");
        assert!(hits_any[0].path.ends_with("体检报告.txt"));

        // 对照：All 本可命中的查询（两词都在正文）不受影响。
        let mut exp_ok = expanded(
            file_search(None),
            vec![
                KeywordGroup::singleton("体检报告"),
                KeywordGroup::singleton("健康体检"),
            ],
        );
        exp_ok.match_mode = MatchMode::All;
        let hits_ok = backend.search_results_expanded(&exp_ok).unwrap();
        assert_eq!(hits_ok.len(), 1, "All 命中的查询保持精确、行为不变");
        assert!(hits_ok[0].path.ends_with("体检报告.txt"));
    }

    // ===== BETA-20：结果预览面板数据源 =====

    // 注：音乐预览的 `entry_for_path` 取数已在 indexer `db.rs` 单测覆盖
    // （`entry_for_path_returns_full_metadata`），此处覆盖 `preview` 的文档分支与 None 兜底（glue）。

    #[test]
    fn preview_returns_document_body_and_highlight() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), "本季度预算与营收分析报告正文").unwrap();
        let db = dir.path().join("index.db");
        let backend = LocalIndexBackend::new(&db);
        // BETA-64 T5：同上，绕开真实平台发现走确定性 WalkDir。
        backend
            .reindex_with(
                None,
                None,
                None,
                None,
                &[],
                std::slice::from_ref(&docs),
                &[],
                &scout_indexer::GlobSet::empty(),
            )
            .expect("reindex docs");

        let stored = DocumentIndex::open(&db)
            .unwrap()
            .query(&DocumentQuery::default())
            .unwrap();
        let path = stored[0].entry.path.clone();

        // 提供与搜索同款的词组 FTS 表达式 → 命中片段含哨兵高亮。
        let fts = fts_match_for_groups(&[KeywordGroup::singleton("季度预算")], MatchMode::All);
        let preview = backend
            .preview(&path, fts.as_deref())
            .unwrap()
            .expect("应有预览");
        match preview {
            LocalPreview::Document(d) => {
                assert!(d.body.contains("营收分析"), "应取回正文");
                let snip = d.snippet.expect("命中应产片段");
                assert!(
                    snip.contains('\u{2}') && snip.contains('\u{3}'),
                    "片段应含高亮哨兵"
                );
            }
            LocalPreview::Music(_) => panic!("应为文档预览"),
        }
    }

    #[test]
    fn preview_none_for_unindexed_path_and_missing_db() {
        // db 不存在 → None。
        let backend = LocalIndexBackend::new("/no/such/index.db");
        assert!(backend.preview("/whatever", None).unwrap().is_none());

        // db 存在但路径未索引 → None。
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let backend = LocalIndexBackend::new(&db);
        backend.reindex(&[], &[], &[]).unwrap();
        assert!(backend
            .preview("/not/indexed/file.txt", None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn clear_empties_index_and_missing_db_is_ok() {
        // db 不存在 → clear 视为已清空、直接 Ok。
        let backend = LocalIndexBackend::new("/no/such/index.db");
        backend.clear().expect("缺库 clear 应 Ok");

        // 索引一批文档让库膨胀 → clear 后 DROP+VACUUM 让文件真实缩小、内容清空、可重建。
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        let body = "本季度预算与营收分析报告正文，包含大量关键词用于撑大 FTS 索引体积。".repeat(40);
        for i in 0..30 {
            std::fs::write(docs.join(format!("doc{i}.txt")), &body).unwrap();
        }
        let db = dir.path().join("index.db");
        let backend = LocalIndexBackend::new(&db);
        backend
            .reindex_with(
                None,
                None,
                None,
                None,
                &[],
                std::slice::from_ref(&docs),
                &[],
                &scout_indexer::GlobSet::empty(),
            )
            .expect("reindex docs");
        assert!(DocumentIndex::open(&db).unwrap().count().unwrap() >= 30);
        let size_before = std::fs::metadata(&db).unwrap().len();

        backend.clear().expect("clear 应成功");
        // 文件仍在（DROP+VACUUM 非删文件），但真实缩小，且内容清空。
        assert!(db.exists(), "clear 后 db 文件仍在（绕开文件锁，非删文件）");
        let size_after = std::fs::metadata(&db).unwrap().len();
        assert!(
            size_after < size_before,
            "DROP+VACUUM 后文件应缩小：clear 前 {size_before} → 后 {size_after}"
        );
        // open 会重建空表，count 为 0。
        assert_eq!(
            DocumentIndex::open(&db).unwrap().count().unwrap(),
            0,
            "clear 后内容应为空"
        );

        // clear 后可再次索引重建。
        backend
            .reindex_with(
                None,
                None,
                None,
                None,
                &[],
                std::slice::from_ref(&docs),
                &[],
                &scout_indexer::GlobSet::empty(),
            )
            .expect("clear 后应可再 reindex");
        assert!(DocumentIndex::open(&db).unwrap().count().unwrap() >= 30);
    }

    // ===== BETA-27：统一 roots + 排除的 reindex_scoped =====

    #[test]
    fn reindex_scoped_indexes_unified_roots_with_exclude() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        let nm = root.join("node_modules");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(root.join("doc.txt"), "hello world").unwrap();
        std::fs::write(nm.join("junk.txt"), "junk").unwrap();
        let db = dir.path().join("idx.db");
        let backend = LocalIndexBackend::new(&db);

        let exclude = scout_indexer::build_exclude_set(&["node_modules".to_string()]);
        // BETA-64 T5：绕开真实平台发现走确定性 WalkDir（同上方注释理由）。
        backend
            .reindex_with(
                None,
                None,
                None,
                None,
                std::slice::from_ref(&root),
                std::slice::from_ref(&root),
                std::slice::from_ref(&root),
                &exclude,
            )
            .unwrap();

        let docs = scout_indexer::DocumentIndex::open(&db).unwrap();
        assert_eq!(
            docs.count().unwrap(),
            1,
            "统一 root 索引 doc.txt、排除 node_modules"
        );
    }

    #[test]
    fn reindex_skips_images_when_no_ocr_engine() {
        let dir = tempfile::tempdir().unwrap();
        let imgs = dir.path().join("pics");
        std::fs::create_dir_all(&imgs).unwrap();
        std::fs::write(imgs.join("a.png"), b"fake").unwrap();
        let backend = LocalIndexBackend::new(dir.path().join("index.db"));
        // ocr=None → 图片轮跳过，统计为零，不报错。
        let (_m, _d, img) = backend
            .reindex_with(
                None,
                None,
                None,
                None,
                &[],
                &[],
                std::slice::from_ref(&imgs),
                &scout_indexer::GlobSet::empty(),
            )
            .unwrap();
        assert_eq!(img, IndexStats::default());
    }

    // ===== BETA-01A：reindex 发现优先 + 回退 =====

    #[derive(Debug)]
    struct MockDiscovery(Result<Vec<PathBuf>, ()>);
    impl scout_indexer::AudioDiscovery for MockDiscovery {
        fn discover_audio(&self) -> Result<Vec<PathBuf>, scout_indexer::DiscoveryError> {
            self.0
                .clone()
                .map_err(|()| scout_indexer::DiscoveryError::Unavailable {
                    detail: "mock".to_owned(),
                })
        }
    }

    /// 2026-07-06 拍板改语义：发现结果按 music_roots 过滤——roots 内的路径入 `index_paths`、
    /// roots 外的丢弃（原「空 roots 也全盘入库」行为废弃，BETA-46 零索引语义对齐）。
    #[test]
    fn reindex_uses_discovery_paths_filtered_to_roots() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalIndexBackend::new(dir.path().join("index.db"));
        let disc = MockDiscovery(Ok(vec![
            PathBuf::from("/no/such/song.wav"),
            PathBuf::from("/elsewhere/other.wav"),
        ]));
        let roots = vec![PathBuf::from("/no/such")];
        let (music, _doc, _img) = backend
            .reindex_with(
                Some(&disc),
                None,
                None,
                None,
                &roots,
                &[],
                &[],
                &scout_indexer::GlobSet::empty(),
            )
            .unwrap();
        assert_eq!(music.scanned, 1, "仅 roots 内的发现路径进 index_paths");
        assert_eq!(music.failed, 1, "不存在的路径计 failed");
    }

    /// 平台发现命令成功但返回空集合不代表配置 root 真的没有文件（Spotlight/Everything
    /// 可能尚未收录或排除了该目录）；必须回退 WalkDir，不能把空结果当完整真相。
    #[test]
    fn reindex_falls_back_when_discovery_succeeds_empty() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("music");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("present.wav"), b"not-a-real-wav").unwrap();
        let backend = LocalIndexBackend::new(dir.path().join("index.db"));
        let disc = MockDiscovery(Ok(Vec::new()));
        let (music, _doc, _img) = backend
            .reindex_with(
                Some(&disc),
                None,
                None,
                None,
                &[root],
                &[],
                &[],
                &scout_indexer::GlobSet::empty(),
            )
            .unwrap();
        assert_eq!(music.scanned, 1, "空发现结果后应回退 WalkDir 看见真实文件");
        assert_eq!(music.failed, 1, "畸形 WAV 被 WalkDir 提取并计失败");
    }

    /// 记调用次数的发现器：验证「roots 空 → 发现器压根不被调用（不 spawn es.exe）」。
    #[derive(Debug, Default)]
    struct CountingDiscovery(std::sync::atomic::AtomicU64);
    impl scout_indexer::AudioDiscovery for CountingDiscovery {
        fn discover_audio(&self) -> Result<Vec<PathBuf>, scout_indexer::DiscoveryError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![PathBuf::from("/anywhere/x.wav")])
        }
    }

    /// 2026-07-06 拍板：music_roots 为空时跳过发现器——默认零索引连 es.exe 都不 spawn
    /// （顺带消除单测在装了 Everything 的机器上真跑 es.exe 的噪声与耗时）。
    #[test]
    fn reindex_skips_discovery_when_music_roots_empty() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalIndexBackend::new(dir.path().join("index.db"));
        let disc = CountingDiscovery::default();
        let (music, _doc, _img) = backend
            .reindex_with(
                Some(&disc),
                None,
                None,
                None,
                &[],
                &[],
                &[],
                &scout_indexer::GlobSet::empty(),
            )
            .unwrap();
        assert_eq!(
            disc.0.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "roots 空不应调用发现器"
        );
        assert_eq!(music.scanned, 0);
    }

    /// `filter_discovered_to_scope` 纯函数：前缀判界（`Music2` 不挂 `Music`）+
    /// 平台归一化（Windows 大小写/分隔符不敏感；Unix 大小写敏感）。
    #[test]
    fn filter_discovered_to_scope_boundaries_and_excludes() {
        let empty = scout_indexer::ExcludeFilter::default();
        #[cfg(windows)]
        {
            let roots = vec![PathBuf::from("D:\\Music")];
            let paths = vec![
                PathBuf::from("d:\\music\\a.mp3"),    // 大小写不同 → 命中
                PathBuf::from("D:/Music/sub/b.flac"), // 正斜杠 → 命中
                PathBuf::from("D:\\Music2\\c.mp3"),   // 兄弟目录前缀 → 不命中
                PathBuf::from("E:\\Other\\d.mp3"),    // roots 外 → 不命中
            ];
            let kept = filter_discovered_to_scope(paths, &roots, &empty, identity_root);
            assert_eq!(kept.len(), 2, "命中 a.mp3 + b.flac，实得 {kept:?}");
        }
        #[cfg(not(windows))]
        {
            let roots = vec![PathBuf::from("/home/u/Music")];
            let paths = vec![
                PathBuf::from("/home/u/Music/a.mp3"),  // 命中
                PathBuf::from("/home/u/music/b.mp3"),  // Unix 大小写敏感 → 不命中
                PathBuf::from("/home/u/Music2/c.mp3"), // 兄弟目录前缀 → 不命中
            ];
            let kept = filter_discovered_to_scope(paths, &roots, &empty, identity_root);
            assert_eq!(kept.len(), 1, "仅 a.mp3 命中，实得 {kept:?}");

            let excluded = scout_indexer::ExcludeFilter::build(
                &["private".to_owned()],
                &[("/home/u/Music".to_owned(), vec!["archive/**".to_owned()])],
                identity_root,
            );
            let kept = filter_discovered_to_scope(
                vec![
                    PathBuf::from("/home/u/Music/ok/a.mp3"),
                    PathBuf::from("/home/u/Music/private/b.mp3"),
                    PathBuf::from("/home/u/Music/archive/c.mp3"),
                ],
                &roots,
                &excluded,
                identity_root,
            );
            assert_eq!(kept, vec![PathBuf::from("/home/u/Music/ok/a.mp3")]);
        }
        // 空 roots → 全丢（调用方本就应先短路、此处兜底语义一致）。
        assert!(filter_discovered_to_scope(
            vec![PathBuf::from("/x/a.mp3")],
            &[],
            &empty,
            identity_root,
        )
        .is_empty());
    }

    #[test]
    fn reindex_falls_back_when_discovery_errors() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalIndexBackend::new(dir.path().join("index.db"));
        let disc = MockDiscovery(Err(()));
        let (music, _doc, _img) = backend
            .reindex_with(
                Some(&disc),
                None,
                None,
                None,
                &[],
                &[],
                &[],
                &scout_indexer::GlobSet::empty(),
            )
            .unwrap();
        assert_eq!(
            music.scanned, 0,
            "发现失败 → 回退 index_dirs（空 roots → 0）"
        );
    }

    #[test]
    fn reindex_falls_back_when_no_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalIndexBackend::new(dir.path().join("index.db"));
        let (music, _doc, _img) = backend
            .reindex_with(
                None,
                None,
                None,
                None,
                &[],
                &[],
                &[],
                &scout_indexer::GlobSet::empty(),
            )
            .unwrap();
        assert_eq!(music.scanned, 0);
    }

    /// 记录 on_phase 序列的测试 progress（BETA-47 音频发现开关断言用）。
    #[derive(Debug, Default)]
    struct PhaseRecorder(std::sync::Mutex<Vec<scout_indexer::IndexPhase>>);
    impl scout_indexer::IndexProgress for PhaseRecorder {
        fn on_file(&self, _: &Path, _: &str, _: bool) {}
        fn on_batch_done(&self, _: u64, _: u64) {}
        fn on_phase(&self, phase: scout_indexer::IndexPhase) {
            self.0.lock().unwrap().push(phase);
        }
    }

    /// BETA-47：`use_audio_discovery=false` 时音乐轮**不进入**全盘发现（无 `MusicDiscovery`
    /// phase、直接 `MusicScan`）——「关闭 Everything 集成」后索引期不 spawn es.exe 的
    /// 可观测面。所有平台/机器确定性成立（不依赖本机是否装 Everything）。
    #[test]
    fn reindex_without_audio_discovery_skips_discovery_phase() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalIndexBackend::new(dir.path().join("index.db"));
        let filter = scout_indexer::ExcludeFilter::build(&[], &[], |s: &str| s.to_string());
        let recorder = PhaseRecorder::default();
        backend
            .reindex_scoped_with_filter_progress_and_discovery(
                &[],
                &filter,
                |s: &str| s.to_string(),
                &recorder,
                false,
            )
            .unwrap();
        let phases = recorder.0.lock().unwrap();
        assert!(
            !phases.contains(&scout_indexer::IndexPhase::MusicDiscovery),
            "关闭发现开关不应出现 MusicDiscovery phase，实得 {phases:?}"
        );
        assert!(
            phases.contains(&scout_indexer::IndexPhase::MusicScan),
            "音乐轮应回退目录扫描（MusicScan），实得 {phases:?}"
        );
    }
}
