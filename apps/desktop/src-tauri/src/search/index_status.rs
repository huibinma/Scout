//! BETA-07 索引状态与 reindex 执行（从 search.rs 拆出，逻辑零改动）。

use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;

use super::embedding_model::EmbeddingModelHandle;
use super::SearchDeps;

/// 2026-07-28：一轮 reindex 里「文档 / 图片 / 音频」三个 phase 各自的
/// [`scout_indexer::StageTimings`]（walk/extract/write/recycle 耗时）。跨轮持久
/// 不清零——索引空闲时 UI 仍要能展示"上次索引用时明细"，靠 [`fts_begin`]/[`fts_finish`]
/// 都不动这个字段来保证。
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct RunStageTimings {
    pub doc: Option<scout_indexer::StageTimings>,
    pub image: Option<scout_indexer::StageTimings>,
    pub music: Option<scout_indexer::StageTimings>,
}

/// BETA-07 索引状态（后台/手动 reindex 共享）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct IndexStatus {
    /// 是否正在索引（并发守卫）。
    pub indexing: bool,
    /// 上次完成索引的时间（rfc3339）。
    pub last_indexed: Option<String>,
    /// 上次结果摘要，如 `"音频 947 / 文档 320 / 图片 58"`。
    pub last_summary: Option<String>,
    /// BETA-33 cycle 6 v4：当前正在索引的目录（reindex 循环每换一个 root 更新一次）。
    /// 非索引中为 `None`；UI 显示"正在索引 …"用它。
    ///
    /// **注**：这是当前文件的**父目录**（bridge 每个 on_file 回调更新）、
    /// 不是配置 root。UI 文案叫「当前目录」而不是「索引根」，避免歧义
    /// （Codex §10 SUGGEST 5）。
    pub current_root: Option<String>,
    /// BETA-33 cycle 6 v4：FTS 索引累计进度 `(已扫描文件数, 已入库文件数)`。
    /// 每次单文件回调（`IndexProgress::on_file`）+1 scanned；`indexed=true` 时 +1 indexed。
    /// 非索引中为 `None`；跨 root 累计不清零（reindex 期总量视图）。
    pub fts_progress: Option<(u64, u64)>,
    /// BETA-33 cycle 7-a：当前索引阶段（`music_discovery` / `music_scan` / `doc` / `image`）。
    /// 桌面 UI 用它显示 phase chip；Everything 全盘发现阶段特别需要（无 per-file 进度、
    /// 用户看"已扫描 0 · 已入库 0"卡住会误判死机）。非索引中为 `None`。
    pub current_phase: Option<scout_indexer::IndexPhase>,
    /// BETA-15B-2：语义嵌入 pass 进行中（独立于 `indexing` 的并发守卫）。
    pub semantic_indexing: bool,
    /// BETA-15B-2：语义嵌入进度 `(已嵌, 待嵌总数)`；非进行中为 `None`。
    pub semantic_progress: Option<(usize, usize)>,
    /// BETA-15B-2：语义索引摘要，如 `"语义索引就绪 320 篇"` / `"暖机中…"`。
    pub semantic_summary: Option<String>,
    /// BETA-33 cycle 9：**全库**索引总数 `(音频, 文档-非图片, 图片)`（`compute_index_totals`
    /// 口径，与 `last_summary` 数字同源）。结构化暴露给前端，供「索引」pane 与概貌
    /// （当前生效目录内 GLOB 统计）比对——两口径可合法不一致（「仅移除」目录保留的
    /// 记录 / override 前旧默认目录的记录仍在库），差值时 UI 显式提示来源而非放任
    /// 两个数字各说各话。填充点：reindex 完成（`apply_reindex_result`）+ 启动回填
    /// （main.rs setup，与 last_summary 同步）。
    pub db_totals: Option<(u64, u64, u64)>,
    /// 2026-07-28：当前 phase 的总文件数（`IndexProgress::on_scope_known` 触发时写入，
    /// walk/发现结束、逐文件进度开始之前就已知）。与 `phase_scanned` 搭配算真百分比；
    /// `None` 表示尚未知道总数（phase 刚切换、或极端 fallback 场景下发现层/walk 尚未完成）。
    pub phase_total: Option<u64>,
    /// 2026-07-28：当前 phase 已扫描文件数（`on_scope_known` 时归零，每次 `on_file` +1）。
    /// 与 `fts_progress.0`（跨全轮所有 phase 累计、不因 phase 切换清零）不同——这个是
    /// **phase 内**计数，专供 `phase_total` 配对算百分比用；`fts_progress` 继续保留给
    /// "本轮目前为止总共处理了多少"这个既有心智模型，两者用途不同、都保留。
    pub phase_scanned: Option<u64>,
    /// 2026-07-28：当前 phase 的处理速率（个/分钟，累计平均，非滑动窗口）。配合
    /// `phase_total`/`phase_scanned` 算 ETA；速率或总数未知时为 `None`，前端不编造。
    pub phase_rate_per_min: Option<f64>,
    /// 2026-07-28：上一次完整 reindex 的分阶段耗时明细（文档/图片/音频各自的
    /// walk/extract/write/recycle）。索引空闲时也保留，供"本次索引用时明细"UI 使用。
    pub last_run_stage_ms: RunStageTimings,
}

/// 语义 pass 并发守卫：空闲 → 置 `semantic_indexing=true` + 暖机摘要、返 true；已在跑 → 返 false。
pub(crate) fn semantic_begin(status: &Arc<Mutex<IndexStatus>>) -> bool {
    let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
    if s.semantic_indexing {
        return false;
    }
    s.semantic_indexing = true;
    s.semantic_progress = None;
    s.semantic_summary = Some("暖机中…".to_owned());
    true
}

/// 写语义嵌入进度 `(done, total)`。
pub(crate) fn semantic_set_progress(status: &Arc<Mutex<IndexStatus>>, done: usize, total: usize) {
    let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
    s.semantic_progress = Some((done, total));
}

/// 语义 pass 完成：清守卫 + 进度，写就绪摘要。
pub(crate) fn semantic_done(status: &Arc<Mutex<IndexStatus>>, count: usize) {
    let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
    s.semantic_indexing = false;
    s.semantic_progress = None;
    s.semantic_summary = Some(format!("语义索引就绪 {count} 篇"));
}

/// 语义 pass 中止（无模型 / 暖机失败）：清守卫 + 进度，摘要写原因。
pub(crate) fn semantic_abort(status: &Arc<Mutex<IndexStatus>>, reason: &str) {
    let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
    s.semantic_indexing = false;
    s.semantic_progress = None;
    s.semantic_summary = Some(reason.to_owned());
}

/// BETA-33 cycle 6 v4：reindex 开始时初始化 FTS 进度（清 current_root、进度归零）。
/// `perform_reindex` 拿到 indexing 守卫后立即调；避免上一轮残留数据泄到本轮。
/// cycle 7-a：同时清 current_phase。
pub(crate) fn fts_begin(status: &Arc<Mutex<IndexStatus>>) {
    let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
    s.current_root = None;
    s.current_phase = None;
    s.fts_progress = Some((0, 0));
    // 2026-07-28：新一轮开始，旧 phase 的总数/进度/速率不该残留到这一轮；
    // last_run_stage_ms **不清**——索引进行中仍要能看"上一次完整跑完的耗时明细"，
    // 各 phase 的 on_stage_timings 触发时会覆盖对应槽位，无需提前清空。
    s.phase_total = None;
    s.phase_scanned = None;
    s.phase_rate_per_min = None;
}

/// BETA-33 cycle 6 v4：reindex 收尾时清 current_root + fts_progress。
/// 无论成功/失败都要清（避免"上次索引"结束后 UI 仍显示"正在索引 xxx"）。
/// cycle 7-a：同时清 current_phase。
pub(crate) fn fts_finish(status: &Arc<Mutex<IndexStatus>>) {
    let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
    s.current_root = None;
    s.current_phase = None;
    s.fts_progress = None;
    // 同 fts_begin：清掉这一轮的 phase 进度/速率，last_run_stage_ms 留着不清。
    s.phase_total = None;
    s.phase_scanned = None;
    s.phase_rate_per_min = None;
}

/// BETA-33 cycle 6 v4：`IndexProgress` 桥。内部持 `Arc<Mutex<IndexStatus>>`，每次
/// indexer 单文件回调时 `+1 scanned`，`indexed=true` 时 `+1 indexed`；累计跨 root
/// 不清零。`on_batch_done` 不动状态（一批完 = 一 root 结束、由外层 reindex 循环
/// 用 fts_set_current_root 推进）。
///
/// 2026-07-28：新增 `phase_started_at`——纯 bridge 内部记账，不进 `IndexStatus`
/// （`Instant` 不可序列化）。`on_scope_known` 时打点，`on_file` 里用它算
/// `phase_rate_per_min`（phase 内已扫描数 / 已过去分钟数，累计平均非滑动窗口）。
pub(crate) struct StatusProgressBridge {
    status: Arc<Mutex<IndexStatus>>,
    phase_started_at: Mutex<Option<Instant>>,
}

impl StatusProgressBridge {
    pub(crate) fn new(status: Arc<Mutex<IndexStatus>>) -> Self {
        Self {
            status,
            phase_started_at: Mutex::new(None),
        }
    }
}

impl scout_indexer::IndexProgress for StatusProgressBridge {
    fn on_file(&self, path: &std::path::Path, _mime: &str, indexed: bool) {
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        let (scanned, done) = s.fts_progress.unwrap_or((0, 0));
        s.fts_progress = Some((
            scanned.saturating_add(1),
            if indexed {
                done.saturating_add(1)
            } else {
                done
            },
        ));
        // 2026-07-28：phase 内计数（配 phase_total 算百分比）+ 速率。发现层路径
        // （`index_paths`/`index_discovered_paths`）此前完全不进本回调，本轮已在
        // indexer 侧补上逐文件调用，这里不用再区分"是不是发现层"。
        let phase_scanned = s.phase_scanned.unwrap_or(0).saturating_add(1);
        s.phase_scanned = Some(phase_scanned);
        let started_at = *self
            .phase_started_at
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(started) = started_at {
            let elapsed_min = started.elapsed().as_secs_f64() / 60.0;
            if elapsed_min > 0.0 {
                s.phase_rate_per_min = Some(phase_scanned as f64 / elapsed_min);
            }
        }
        // 用文件的**父目录**做 current_root 显示：跨 root、跨阶段都对。
        if let Some(parent) = path.parent() {
            s.current_root = Some(parent.display().to_string());
        }
    }
    fn on_batch_done(&self, _scanned: u64, _indexed: u64) {}
    /// BETA-33 cycle 7-a：把新 phase 写回 IndexStatus，UI 可显示对应 chip。
    /// MusicDiscovery（Everything 全盘发现）此阶段没有 per-file 进度、切进 phase 时
    /// 顺便把 current_root 清一下，避免"进入音频发现"时 UI 还残留上一阶段的父目录。
    fn on_phase(&self, phase: scout_indexer::IndexPhase) {
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        s.current_phase = Some(phase);
        if matches!(phase, scout_indexer::IndexPhase::MusicDiscovery) {
            s.current_root = None;
        }
    }
    /// 2026-07-28：本 phase 总文件数已知——打 phase 起始时间戳（供速率计算），
    /// 归零 phase 内计数，写 phase_total。
    fn on_scope_known(&self, total: u64) {
        *self
            .phase_started_at
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        s.phase_total = Some(total);
        s.phase_scanned = Some(0);
        s.phase_rate_per_min = None;
    }
    /// 2026-07-28：按当前 phase 落到 `last_run_stage_ms` 对应槽位。`current_phase`
    /// 在这之前已经被 `on_phase` 设过（调用顺序：`on_phase` → …逐文件… → `on_stage_timings`），
    /// 理论上不会是 `None`；防御性地在 `None` 时不落，不 panic。
    fn on_stage_timings(&self, timings: scout_indexer::StageTimings) {
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        match s.current_phase {
            Some(scout_indexer::IndexPhase::Doc) => s.last_run_stage_ms.doc = Some(timings),
            Some(scout_indexer::IndexPhase::Image) => s.last_run_stage_ms.image = Some(timings),
            Some(
                scout_indexer::IndexPhase::MusicDiscovery | scout_indexer::IndexPhase::MusicScan,
            ) => {
                s.last_run_stage_ms.music = Some(timings);
            }
            None => {}
        }
    }
}

/// `reindex` 命令的返回统计。
#[derive(Debug, Clone, Serialize)]
pub struct ReindexStats {
    pub music_scanned: usize,
    pub music_added: usize,
    pub music_updated: usize,
    pub doc_scanned: usize,
    pub doc_added: usize,
    pub doc_updated: usize,
    pub image_scanned: usize,
    pub image_added: usize,
    pub image_updated: usize,
}

/// BETA-27：从 settings.json live-read 索引配置（roots + 排除 GlobSet）。读/解析失败 → 默认。
/// roots 去重（系统三夹可能重叠/为空）+ 仅保留存在的目录。
/// **注**：cycle 7-b 起主入口 [`read_index_config_with_filter`]（basename + per-root 双层）；
/// 本 fn 仅 basename 排除、用于旧 API 委托（如 BETA-32 daemon）。
pub(crate) fn read_index_config(
    settings_path: &Option<std::path::PathBuf>,
) -> (Vec<std::path::PathBuf>, scout_indexer::GlobSet) {
    let settings = settings_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<crate::settings::AppSettings>(&s).ok())
        .unwrap_or_default();
    // cycle 6 v4：追加系统默认的判定跟 settings.include_system_defaults 走（默认 false = 旧覆盖语义）。
    let mut roots: Vec<std::path::PathBuf> = crate::settings::resolve_index_roots_tagged(
        &settings.index_roots,
        settings.include_system_defaults,
    )
    .into_iter()
    .map(|(p, _)| p)
    .collect();
    // 去重（保序）+ 去掉不存在的目录（walkdir 对不存在 root 本就跳过，这里提前清理避免重复扫）。
    let mut seen = std::collections::HashSet::new();
    roots.retain(|p| p.exists() && seen.insert(p.clone()));
    let globs = crate::settings::resolve_exclude_globs(&settings.exclude_globs);
    (roots, scout_indexer::build_exclude_set(&globs))
}

/// BETA-33 cycle 7-b：从 settings.json live-read 索引配置 + 构造 [`ExcludeFilter`]（双层排除）。
/// 桌面 `perform_reindex` 用此入口，包含 basename 全局排除 + per-root 相对路径排除。
/// 读 / 解析失败 → 默认（空 root_excludes、走纯 basename 表）。
pub(crate) fn read_index_config_with_filter(
    settings_path: &Option<std::path::PathBuf>,
) -> (Vec<std::path::PathBuf>, scout_indexer::ExcludeFilter) {
    let settings = settings_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<crate::settings::AppSettings>(&s).ok())
        .unwrap_or_default();
    let mut roots: Vec<std::path::PathBuf> = crate::settings::resolve_index_roots_tagged(
        &settings.index_roots,
        settings.include_system_defaults,
    )
    .into_iter()
    .map(|(p, _)| p)
    .collect();
    let mut seen = std::collections::HashSet::new();
    roots.retain(|p| p.exists() && seen.insert(p.clone()));

    let exclude_globs = crate::settings::resolve_exclude_globs(&settings.exclude_globs);
    // root_excludes 转成 (root_key, patterns) 供 ExcludeFilter::build 消费。
    let root_ex: Vec<(String, Vec<String>)> = settings
        .root_excludes
        .into_iter()
        .filter(|re| !re.patterns.is_empty())
        .map(|re| (re.root, re.patterns))
        .collect();
    let filter = scout_indexer::ExcludeFilter::build(
        &exclude_globs,
        &root_ex,
        crate::settings::normalize_root_key,
    );
    (roots, filter)
}

/// 执行一次 FTS reindex 并更新 [`IndexStatus`]（BETA-07）。**并发守卫**：已在索引中 → 返 `Ok(None)`。
/// BETA-15B-2：只做 FTS，语义向量嵌入解耦到 `spawn_semantic_index` 后台 worker。阻塞函数，应在 `spawn_blocking` 内调。
/// BETA-27：`settings_path` live-read 索引配置（roots + 排除），走 `reindex_scoped`。
pub(crate) fn perform_reindex(
    status: &Arc<Mutex<IndexStatus>>,
    db_path: std::path::PathBuf,
    settings_path: Option<std::path::PathBuf>,
) -> Result<Option<ReindexStats>, String> {
    perform_reindex_for_roots(status, db_path, settings_path, None)
}

/// BETA-33 cycle 7-c（Codex SUGGEST 6）：带 roots override 的 reindex 内部实现。
/// `roots_override = Some(...)` 时只替换扫描 roots（单目录重扫）；**排除过滤器 / OCR /
/// progress bridge 仍从 settings live-read**——不给"绕过 exclude 配置的旁路"留口子。
/// `None` = 旧全量语义（roots 也从 settings 解析）。
pub(crate) fn perform_reindex_for_roots(
    status: &Arc<Mutex<IndexStatus>>,
    db_path: std::path::PathBuf,
    settings_path: Option<std::path::PathBuf>,
    roots_override: Option<Vec<std::path::PathBuf>>,
) -> Result<Option<ReindexStats>, String> {
    // 守卫：已在索引中则跳过。
    {
        let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
        if s.indexing {
            return Ok(None);
        }
        s.indexing = true;
    }
    // cycle 6 v4：init FTS 进度（清 current_root + 归零 fts_progress）。
    fts_begin(status);
    // 2026-07-29：本轮开始前记一次 DB 体量诊断（真机复盘：14570 文档索引 25 小时，
    // 定位到长期反复增删的旧库比全新库慢 8 倍，但旧库已被用户删除、无法回溯当时体量）。
    // 下次再出现明显变慢，这行日志能直接告诉我们是不是"DB 已经很大/碎片化"这个方向，
    // 不必再靠"删了重装、对比前后速度"这种会丢证据的办法。
    log_db_size_diagnostics(&db_path);

    // cycle 7-b：走 filter 版本（basename + per-root 双层排除）。
    let (settings_roots, filter) = read_index_config_with_filter(&settings_path);
    // cycle 7-c：override 只换 roots；与 settings 路径同口径过滤不存在的目录。
    let roots = match roots_override {
        Some(o) => o.into_iter().filter(|p| p.exists()).collect(),
        None => settings_roots,
    };
    // BETA-47：Everything 集成开关 live-read——关闭时音频轮跳过 es.exe 全盘发现、
    // 回退目录扫描。仅 Windows 受设置控制（macOS 发现器是 Spotlight、与该开关无关）。
    let use_audio_discovery =
        cfg!(not(target_os = "windows")) || crate::settings::read_enable_everything(&settings_path);
    let result = {
        let backend = scout_local_index_backend::LocalIndexBackend::new(&db_path);
        let bridge = StatusProgressBridge::new(Arc::clone(status));
        backend.reindex_scoped_with_filter_progress_and_discovery(
            &roots,
            &filter,
            crate::settings::normalize_root_key,
            &bridge,
            use_audio_discovery,
        )
    };
    // 成功后查**总索引数**（而非本轮 delta）供状态摘要——增量轮 delta 多为 0，显示总数才不误导。
    let totals = result
        .as_ref()
        .ok()
        .and_then(|_| compute_index_totals(&db_path));
    // 无论成功/失败都清 FTS 进度（避免"已完成"后 UI 还显示"正在索引 xxx"）。
    let out = apply_reindex_result(status, result, totals);
    fts_finish(status);
    out
}

/// 语义嵌入 pass 同步核心（可单测）。守卫被占→跳过；`prewarmed=false`→中止写原因；否则 embed_pending+进度+就绪摘要。
/// `embed_images`（BETA-39）：图片语义索引 opt-in，透传 `embed_pending`；false = cycle 4 现状（图片直跳）。
pub(crate) fn semantic_index_pass(
    status: &Arc<Mutex<IndexStatus>>,
    db_path: &std::path::Path,
    prewarmed: bool,
    embedder: &dyn scout_indexer::embed::TextEmbedder,
    roots: &[std::path::PathBuf],
    embed_images: bool,
) {
    if !semantic_begin(status) {
        return; // 已有语义 pass 在跑，跳过（不排队）。
    }
    if !prewarmed {
        semantic_abort(status, "未找到 embedding 模型");
        return;
    }
    let Ok(idx) = scout_indexer::DocumentIndex::open(db_path) else {
        semantic_abort(status, "打开文档库失败，语义索引跳过");
        return;
    };
    let mut on_progress = |done, total| semantic_set_progress(status, done, total);
    match idx.embed_pending(roots, embedder, embed_images, &mut on_progress) {
        Ok((embedded, reused, failed)) => {
            if failed > 0 || reused > 0 {
                eprintln!("语义索引：嵌入 {embedded} 篇，复用副本 {reused} 篇，失败 {failed} 篇");
            }
            let total = idx
                .vector_count()
                .ok()
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(embedded);
            semantic_done(status, total);
        }
        Err(e) => {
            eprintln!("文档向量嵌入失败（语义召回降级 FTS-only）: {e}");
            semantic_abort(status, "语义索引失败，已降级 FTS-only");
        }
    }
}

/// 语义 worker 的 panic 兜底外壳（可单测）：`catch_unwind` 包 `semantic_index_pass`，
/// panic 时清守卫（`semantic_abort`）降级 FTS-only——守卫不泄漏，UI 不卡「语义索引中」。
/// （`embedder.embed()` 走 FFI 等理论上可能 panic；正常 `Result` 错误仍由 `semantic_index_pass` 内部处理。）
pub(crate) fn run_semantic_worker(
    status: &Arc<Mutex<IndexStatus>>,
    db_path: &std::path::Path,
    prewarmed: bool,
    embedder: &dyn scout_indexer::embed::TextEmbedder,
    roots: &[std::path::PathBuf],
    embed_images: bool,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        semantic_index_pass(status, db_path, prewarmed, embedder, roots, embed_images);
    }));
    if result.is_err() {
        eprintln!("语义索引 worker panic，已清守卫降级 FTS-only");
        semantic_abort(status, "语义索引意外中断，已降级 FTS-only");
    }
}

/// 生产入口：后台 worker 跑语义嵌入 pass。仅 `is_active()` 时实际工作。
/// BETA-27：`settings_path` live-read 索引配置 roots（与 FTS 阶段口径一致）；
/// 语义 worker 的 `embed_pending` 遍历的是 documents 表内文档（不走 walkdir，排除已在 FTS 阶段生效），
/// 故此处忽略排除 GlobSet，仅用 roots 框定 `paths_under(roots)` 范围。
///
/// BETA-31-v3 cycle 4（v0.8.5，2026-06-30）：**默认禁用 embed_pending**——env
/// `SCOUT_ENABLE_EMBED=1` 才跑。**Why**：cycle 3 修了 ranker 污染 bug 后 worker 首次
/// 真正进入 `embedder.embed()` 调用真文档 → 立即触发 llama-cpp / ggml native crash
/// (ucrtbase.dll exception 0xc0000409 = `STATUS_STACK_BUFFER_OVERRUN` / `abort()` fail-fast)
/// → 整个进程被 OS 杀掉、Rust `catch_unwind` 兜不住 native abort。Win 事件日志 5 次崩溃
/// 指纹一致（v0.8.3 + v0.8.4 都崩）。短期止血默认禁、保现有 369 真文档向量 + FTS +
/// Everything + Windows Search 全部可用；新文档无法进语义召回（trade-off）。用户用
/// `SCOUT_ENABLE_EMBED=1` 主动诊断 + 配合 [`embed_pending`] 的 per-doc 日志锁定触发文档、
/// 为下个 cycle 真修（升级 llama-cpp / batch size 守门 / 切模型）提供输入。
pub(crate) fn spawn_semantic_index(
    status: Arc<Mutex<IndexStatus>>,
    db_path: std::path::PathBuf,
    embedding: Arc<EmbeddingModelHandle>,
    settings_path: Option<std::path::PathBuf>,
) {
    if std::env::var("SCOUT_ENABLE_EMBED").ok().as_deref() != Some("1") {
        tracing::warn!(
            "spawn_semantic_index: 默认禁用（防 llama-cpp native crash 杀进程、BETA-31-v3 cycle 4）。\
             set SCOUT_ENABLE_EMBED=1 启用嵌入新文档；现有 document_vectors 残留向量仍参与语义召回。"
        );
        semantic_abort(
            &status,
            "嵌入暂停（防 native crash，set SCOUT_ENABLE_EMBED=1 开启）",
        );
        return;
    }
    if !embedding.is_active() {
        tracing::info!(
            "spawn_semantic_index: embedding.is_active()=false（feature 关或模型文件缺失）、跳过"
        );
        return; // feature 关 / 无模型 → 不 spawn，逐字节一致。
    }
    tracing::info!("spawn_semantic_index: 启动后台 worker（prewarm + embed_pending）");
    tauri::async_runtime::spawn_blocking(move || {
        let prewarm_start = std::time::Instant::now();
        let prewarmed = embedding.prewarm();
        let prewarm_elapsed_ms =
            u64::try_from(prewarm_start.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::info!(
            prewarmed,
            prewarm_elapsed_ms,
            "spawn_semantic_index: prewarm 完成"
        );
        let (roots, _exclude) = read_index_config(&settings_path);
        // BETA-39：live-read「图片语义索引」opt-in，与 roots 同节奏（改设置后下一轮语义 pass 生效）。
        let embed_images = crate::settings::read_enable_image_semantics(&settings_path);
        let worker_start = std::time::Instant::now();
        run_semantic_worker(
            &status,
            &db_path,
            prewarmed,
            embedding.as_ref(),
            &roots,
            embed_images,
        );
        let worker_elapsed_ms =
            u64::try_from(worker_start.elapsed().as_millis()).unwrap_or(u64::MAX);
        // 收尾时再 snapshot 一次状态，把 summary 一起打出来供诊断（如「语义索引就绪 N 篇」/
        // 「未找到 embedding 模型」/「语义索引失败」）。
        let summary = status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .semantic_summary
            .clone();
        tracing::info!(
            worker_elapsed_ms,
            summary = ?summary,
            "spawn_semantic_index: 后台 worker 结束"
        );
    });
}

/// 查当前索引总数 `(音频, 文档-非图片, 图片)`。best-effort：任一步出错返回 `None`（摘要退回 delta）。
/// 图片 doc_type 复用 indexer 索引侧白名单 `IMAGE_EXTS`（单一信源，与索引写入口径一致）。
/// BETA-21 隐私面板亦复用此函数取索引概览（单一信源）。
pub(crate) fn compute_index_totals(db_path: &std::path::Path) -> Option<(u64, u64, u64)> {
    let music = scout_indexer::MusicIndex::open(db_path)
        .ok()?
        .count()
        .ok()?;
    let docs = scout_indexer::DocumentIndex::open(db_path).ok()?;
    let doc_all = docs.count().ok()?;
    let images = docs.count_in_doc_types(scout_indexer::IMAGE_EXTS).ok()?;
    Some((music, doc_all.saturating_sub(images), images))
}

/// 2026-07-29：`perform_reindex_for_roots` 每轮开始前调一次——记录 `index.db` 文件体量 +
/// 关键表行数（documents / 向量 / 提取失败留痕 / music）到日志，供事后诊断"这次是不是
/// 撞上了旧库体量大/碎片化"这个方向。best-effort：db 不存在（首次索引前）或任一查询失败
/// 都不阻塞正常索引流程，缺字段照记、不 panic。
///
/// **2026-08-10**：4 个 `COUNT(*)` 各自开一个连接查询，在大库上不是"诊断日志"这个用途
/// 名字听起来那么轻——没有覆盖索引时 `COUNT(*)` 要扫表/扫最小索引，且这里在
/// **每一轮**reindex（含启动那一轮，恰是资源最紧张的窗口）开始前都会付这笔全表扫描的
/// 开销。默认 info 级别只记文件体量（纯 `stat`，几乎零成本）；完整行数下钻改到 debug
/// 级别，`SCOUT_LOG=debug` 时才真正查——保留诊断能力，但不再是默认路径上的隐性负担。
fn log_db_size_diagnostics(db_path: &std::path::Path) {
    let file_size_bytes = std::fs::metadata(db_path).ok().map(|m| m.len());
    if !tracing::enabled!(tracing::Level::DEBUG) {
        tracing::info!(
            db_path = %db_path.display(),
            ?file_size_bytes,
            "本轮索引开始前 DB 体量诊断（仅文件体量；SCOUT_LOG=debug 可看完整行数下钻）"
        );
        return;
    }
    let music_count = scout_indexer::MusicIndex::open(db_path)
        .ok()
        .and_then(|m| m.count().ok());
    let docs = scout_indexer::DocumentIndex::open(db_path).ok();
    let doc_count = docs.as_ref().and_then(|d| d.count().ok());
    let vector_count = docs.as_ref().and_then(|d| d.vector_count().ok());
    let extraction_failure_count = docs
        .as_ref()
        .and_then(|d| d.extraction_failure_count().ok());
    tracing::debug!(
        db_path = %db_path.display(),
        ?file_size_bytes,
        ?music_count,
        ?doc_count,
        ?vector_count,
        ?extraction_failure_count,
        "本轮索引开始前 DB 体量诊断"
    );
}

/// 把 reindex 结果写回 [`IndexStatus`]（清 indexing + 成功填 last_indexed/summary）。
/// 抽出便于单测成功/失败分支，无需真跑全盘 reindex。
pub(crate) type ReindexResult = Result<
    (
        scout_indexer::IndexStats,
        scout_indexer::IndexStats,
        scout_indexer::IndexStats,
    ),
    scout_search_backend::SearchError,
>;
pub(crate) fn apply_reindex_result(
    status: &Arc<Mutex<IndexStatus>>,
    result: ReindexResult,
    totals: Option<(u64, u64, u64)>,
) -> Result<Option<ReindexStats>, String> {
    let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
    s.indexing = false;
    match result {
        Ok((music, doc, image)) => {
            s.last_indexed = Some(chrono::Utc::now().to_rfc3339());
            // 摘要显示**当前总索引数**（totals）；拿不到时退回本轮 delta（旧行为）。
            s.last_summary = Some(match totals {
                Some((m, d, i)) => format!("音频 {m} / 文档 {d} / 图片 {i}"),
                None => format!(
                    "音频 {} / 文档 {} / 图片 {}",
                    music.added + music.updated,
                    doc.added + doc.updated,
                    image.added + image.updated
                ),
            });
            // cycle 9：结构化全库总数同步写（拿不到保留旧值，不清空）。
            if totals.is_some() {
                s.db_totals = totals;
            }
            Ok(Some(ReindexStats {
                music_scanned: music.scanned,
                music_added: music.added,
                music_updated: music.updated,
                doc_scanned: doc.scanned,
                doc_added: doc.added,
                doc_updated: doc.updated,
                image_scanned: image.scanned,
                image_added: image.added,
                image_updated: image.updated,
            }))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// 读当前索引状态（命令 impl，可单测）。
pub(crate) fn index_status_snapshot(deps: &SearchDeps) -> IndexStatus {
    deps.index_status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[cfg(test)]
mod semantic_status_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::{Arc, Mutex};

    /// 2026-07-29：`log_db_size_diagnostics` 是纯 best-effort 诊断，db 不存在（首次索引前
    /// 的常见状态）不该 panic；真实 db 上也不该 panic（具体字段值不断言，日志内容不是
    /// 单测该锁的契约，只锁"不崩"）。
    #[test]
    fn log_db_size_diagnostics_does_not_panic_on_missing_or_real_db() {
        log_db_size_diagnostics(std::path::Path::new("/no/such/index.db"));

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        scout_indexer::MusicIndex::open(&db).unwrap();
        log_db_size_diagnostics(&db);
    }

    #[test]
    fn semantic_lifecycle_updates_status() {
        let status = Arc::new(Mutex::new(IndexStatus::default()));

        // begin：空闲 → true，置守卫 + 暖机摘要。
        assert!(semantic_begin(&status), "空闲时 begin 返 true");
        {
            let s = status.lock().unwrap();
            assert!(s.semantic_indexing);
            assert_eq!(s.semantic_summary.as_deref(), Some("暖机中…"));
        }
        // 已在跑 → begin 返 false（守卫）。
        assert!(!semantic_begin(&status), "已在跑时 begin 返 false");

        // progress：写 (done,total)。
        semantic_set_progress(&status, 3, 10);
        assert_eq!(status.lock().unwrap().semantic_progress, Some((3, 10)));

        // done：清守卫 + 进度，写就绪摘要。
        semantic_done(&status, 10);
        let s = status.lock().unwrap();
        assert!(!s.semantic_indexing);
        assert_eq!(s.semantic_progress, None);
        assert_eq!(s.semantic_summary.as_deref(), Some("语义索引就绪 10 篇"));
    }

    /// cycle 6 v4：FTS 进度桥的核心闭环——begin 初始化、on_file 累加 (scanned, indexed) +
    /// 更新 current_root 为父目录、finish 清理。
    #[test]
    fn fts_progress_bridge_ticks_and_updates_current_root() {
        use scout_indexer::IndexProgress;
        let status = Arc::new(Mutex::new(IndexStatus::default()));

        fts_begin(&status);
        {
            let s = status.lock().unwrap();
            assert_eq!(s.fts_progress, Some((0, 0)));
            assert!(s.current_root.is_none());
        }

        let bridge = StatusProgressBridge::new(Arc::clone(&status));
        let p1 = std::path::Path::new("/tmp/docs/a.txt");
        let p2 = std::path::Path::new("/tmp/docs/b.txt");
        bridge.on_file(p1, "text/plain", true);
        bridge.on_file(p2, "text/plain", false);
        {
            let s = status.lock().unwrap();
            assert_eq!(s.fts_progress, Some((2, 1)), "两文件扫描、一文件入库");
            assert_eq!(s.current_root.as_deref(), Some("/tmp/docs"));
        }

        fts_finish(&status);
        let s = status.lock().unwrap();
        assert!(s.current_root.is_none());
        assert!(s.fts_progress.is_none());
    }

    /// 2026-07-28：`on_scope_known` 写 phase_total、归零 phase_scanned；`on_file`
    /// 累加 phase_scanned 且不影响跨 phase 累计的 `fts_progress`；速率在有总数 + 有
    /// 已过去时间时才给出（这里只断言"有值"，不断言具体数字——真实耗时不确定）。
    #[test]
    fn scope_known_and_on_file_track_phase_scanned_independent_of_fts_progress() {
        use scout_indexer::{IndexPhase, IndexProgress};
        let status = Arc::new(Mutex::new(IndexStatus::default()));
        fts_begin(&status);
        let bridge = StatusProgressBridge::new(Arc::clone(&status));
        bridge.on_phase(IndexPhase::Doc);

        bridge.on_scope_known(5);
        {
            let s = status.lock().unwrap();
            assert_eq!(s.phase_total, Some(5));
            assert_eq!(s.phase_scanned, Some(0));
            assert!(s.phase_rate_per_min.is_none(), "还没处理任何文件，速率未知");
        }

        bridge.on_file(std::path::Path::new("/tmp/a.txt"), "text/plain", true);
        bridge.on_file(std::path::Path::new("/tmp/b.txt"), "text/plain", false);
        {
            let s = status.lock().unwrap();
            assert_eq!(s.phase_scanned, Some(2), "phase 内计数只算这 2 个");
            assert_eq!(
                s.fts_progress,
                Some((2, 1)),
                "跨 phase 累计计数不受 phase_scanned 引入影响，行为不变"
            );
        }

        // 切到下一个 phase：phase_total/phase_scanned 归零，fts_progress 继续累计不受影响。
        bridge.on_phase(IndexPhase::Image);
        bridge.on_scope_known(3);
        bridge.on_file(std::path::Path::new("/tmp/c.png"), "image/png", true);
        {
            let s = status.lock().unwrap();
            assert_eq!(s.phase_total, Some(3));
            assert_eq!(s.phase_scanned, Some(1), "新 phase 从 0 重新计");
            assert_eq!(
                s.fts_progress,
                Some((3, 2)),
                "fts_progress 是全轮跨 phase 累计，第二个 phase 继续往上加"
            );
        }

        fts_finish(&status);
        let s = status.lock().unwrap();
        assert!(s.phase_total.is_none());
        assert!(s.phase_scanned.is_none());
        assert!(s.phase_rate_per_min.is_none());
    }

    /// 2026-07-28：`on_stage_timings` 按当前 `current_phase` 落到 `last_run_stage_ms`
    /// 对应槽位；`fts_finish` 不清这个字段（索引空闲后仍要能展示"上次用时明细"）。
    #[test]
    fn stage_timings_land_in_correct_slot_and_survive_fts_finish() {
        use scout_indexer::{IndexPhase, IndexProgress, StageTimings};
        let status = Arc::new(Mutex::new(IndexStatus::default()));
        let bridge = StatusProgressBridge::new(Arc::clone(&status));

        bridge.on_phase(IndexPhase::Doc);
        bridge.on_stage_timings(StageTimings {
            walk_ms: 1,
            extract_ms: 2,
            write_ms: 3,
            recycle_ms: 4,
        });
        bridge.on_phase(IndexPhase::Image);
        bridge.on_stage_timings(StageTimings {
            walk_ms: 5,
            extract_ms: 6,
            write_ms: 7,
            recycle_ms: 0,
        });
        bridge.on_phase(IndexPhase::MusicScan);
        bridge.on_stage_timings(StageTimings {
            walk_ms: 9,
            extract_ms: 10,
            write_ms: 11,
            recycle_ms: 12,
        });

        {
            let s = status.lock().unwrap();
            assert_eq!(s.last_run_stage_ms.doc.unwrap().extract_ms, 2);
            assert_eq!(s.last_run_stage_ms.image.unwrap().walk_ms, 5);
            assert_eq!(s.last_run_stage_ms.music.unwrap().recycle_ms, 12);
        }

        fts_finish(&status);
        let s = status.lock().unwrap();
        assert!(
            s.last_run_stage_ms.doc.is_some(),
            "last_run_stage_ms 在 fts_finish 后仍保留，供空闲态展示用时明细"
        );
    }

    /// cycle 7-a：phase 桥的核心闭环——on_phase 更新 current_phase、
    /// MusicDiscovery 时同步清 current_root（避免上一阶段残留父目录污染 UI）。
    #[test]
    fn phase_bridge_sets_current_phase_and_clears_root_on_music_discovery() {
        use scout_indexer::{IndexPhase, IndexProgress};
        let status = Arc::new(Mutex::new(IndexStatus::default()));
        // 预置 current_root 模拟上一阶段残留
        status.lock().unwrap().current_root = Some("/leftover".to_owned());
        let bridge = StatusProgressBridge::new(Arc::clone(&status));

        // Doc phase：只更 phase，不动 current_root
        bridge.on_phase(IndexPhase::Doc);
        {
            let s = status.lock().unwrap();
            assert_eq!(s.current_phase, Some(IndexPhase::Doc));
            assert_eq!(s.current_root.as_deref(), Some("/leftover"));
        }

        // MusicDiscovery：清 current_root
        bridge.on_phase(IndexPhase::MusicDiscovery);
        {
            let s = status.lock().unwrap();
            assert_eq!(s.current_phase, Some(IndexPhase::MusicDiscovery));
            assert!(
                s.current_root.is_none(),
                "MusicDiscovery 阶段清 current_root"
            );
        }

        // fts_finish 清所有状态
        fts_finish(&status);
        let s = status.lock().unwrap();
        assert!(s.current_phase.is_none());
    }

    #[test]
    fn semantic_abort_clears_guard_with_reason() {
        let status = Arc::new(Mutex::new(IndexStatus::default()));
        assert!(semantic_begin(&status));
        semantic_set_progress(&status, 1, 5); // 进度非 None，才能真正验证 abort 清进度
        semantic_abort(&status, "未找到 embedding 模型");
        let s = status.lock().unwrap();
        assert!(!s.semantic_indexing);
        assert_eq!(s.semantic_progress, None);
        assert_eq!(s.semantic_summary.as_deref(), Some("未找到 embedding 模型"));
    }
}

#[cfg(test)]
mod decouple_tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::sync::{Arc, Mutex};

    /// 本地 stub embedder：确定性 2 维向量，隔离真模型。
    struct StubEmbedder;
    impl scout_indexer::embed::TextEmbedder for StubEmbedder {
        fn embed(&self, text: &str) -> Result<Vec<f32>, scout_indexer::IndexError> {
            let n = text.chars().count() as f32;
            Ok(vec![n, n + 1.0])
        }
        fn model_id(&self) -> &str {
            "stub"
        }
    }

    /// 建临时 FTS 库（模拟 perform_reindex 已跑完 FTS），返回 (db 路径, 文档 roots)。
    fn temp_fts_db(dir: &std::path::Path) -> (std::path::PathBuf, Vec<std::path::PathBuf>) {
        let db = dir.join("idx.db");
        let docs = dir.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        // BETA-31-v3 cycle 5：body ≥ 20 字符通过 cycle 3 加的 is_embed_worthy 守门
        std::fs::write(
            docs.join("a.txt"),
            "alpha body content padded to twenty plus chars for embed test.",
        )
        .unwrap();
        std::fs::write(
            docs.join("b.txt"),
            "beta body content padded to twenty plus chars for embed test.",
        )
        .unwrap();
        let roots = vec![docs];
        let idx = scout_indexer::DocumentIndex::open(&db).unwrap();
        idx.index_dirs(&roots).unwrap();
        (db, roots)
    }

    #[test]
    fn semantic_index_pass_embeds_and_finalizes() {
        let dir = tempfile::tempdir().unwrap();
        let (db, roots) = temp_fts_db(dir.path());
        let status = Arc::new(Mutex::new(IndexStatus::default()));

        semantic_index_pass(&status, &db, true, &StubEmbedder, &roots, false);

        let s = status.lock().unwrap();
        assert!(!s.semantic_indexing, "完成后清守卫");
        assert_eq!(s.semantic_progress, None);
        assert_eq!(s.semantic_summary.as_deref(), Some("语义索引就绪 2 篇"));
        drop(s);
        let idx = scout_indexer::DocumentIndex::open(&db).unwrap();
        assert_eq!(idx.vector_count().unwrap(), 2, "两篇向量已写");
    }

    #[test]
    fn semantic_index_pass_aborts_when_not_prewarmed() {
        let dir = tempfile::tempdir().unwrap();
        let (db, _roots) = temp_fts_db(dir.path());
        let status = Arc::new(Mutex::new(IndexStatus::default()));
        semantic_index_pass(&status, &db, false, &StubEmbedder, &[], false);
        let s = status.lock().unwrap();
        assert!(!s.semantic_indexing);
        assert_eq!(s.semantic_summary.as_deref(), Some("未找到 embedding 模型"));
    }

    #[test]
    fn semantic_index_pass_skips_when_guard_held() {
        let dir = tempfile::tempdir().unwrap();
        let (db, _roots) = temp_fts_db(dir.path());
        let status = Arc::new(Mutex::new(IndexStatus::default()));
        assert!(semantic_begin(&status)); // 预先占守卫
        semantic_index_pass(&status, &db, true, &StubEmbedder, &[], false);
        let s = status.lock().unwrap();
        assert!(s.semantic_indexing, "守卫被外部持有，pass 跳过未动它");
        assert_eq!(s.semantic_summary.as_deref(), Some("暖机中…"));
    }

    /// 故意在 embed 时 panic，验证 worker 兜底清守卫。
    struct PanicEmbedder;
    impl scout_indexer::embed::TextEmbedder for PanicEmbedder {
        fn embed(&self, _text: &str) -> Result<Vec<f32>, scout_indexer::IndexError> {
            panic!("boom: simulated embed panic");
        }
        fn model_id(&self) -> &str {
            "panic"
        }
    }

    #[test]
    fn run_semantic_worker_clears_guard_on_panic() {
        let dir = tempfile::tempdir().unwrap();
        let (db, roots) = temp_fts_db(dir.path());
        let status = Arc::new(Mutex::new(IndexStatus::default()));

        // embed_pending 触达 PanicEmbedder（temp_fts_db 有 2 篇待嵌）→ panic 经 catch_unwind 兜底。
        // 注：catch_unwind 仍会经默认 panic hook 往 stderr 打一行 backtrace，属预期、无害。
        run_semantic_worker(&status, &db, true, &PanicEmbedder, &roots, false);

        let s = status.lock().unwrap();
        assert!(!s.semantic_indexing, "panic 后守卫已清，不泄漏");
        assert_eq!(s.semantic_progress, None);
        assert_eq!(
            s.semantic_summary.as_deref(),
            Some("语义索引意外中断，已降级 FTS-only")
        );
    }

    #[test]
    fn run_semantic_worker_normal_path_finalizes() {
        let dir = tempfile::tempdir().unwrap();
        let (db, roots) = temp_fts_db(dir.path());
        let status = Arc::new(Mutex::new(IndexStatus::default()));

        run_semantic_worker(&status, &db, true, &StubEmbedder, &roots, false);

        let s = status.lock().unwrap();
        assert!(!s.semantic_indexing);
        assert_eq!(s.semantic_summary.as_deref(), Some("语义索引就绪 2 篇"));
    }
}
