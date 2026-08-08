//! BETA-21 隐私 / 索引数据可视化轻量版（信任面板后端）。
//!
//! 提供两个命令：
//! - [`get_privacy_overview`]：聚合「索引了什么 / 数据在哪 / 配置如何」的只读快照，供隐私页活信任面板渲染。
//! - [`clear_local_index`]：一键清空本地索引（音频 + 文档 + 图片，含 FTS）。
//!
//! **隐私硬约束**：本模块只读 / 写 Scout 自身的数据目录（索引 DB / 审计日志 / 配置），
//! 展示的是**文件路径、字节大小、记录条数**，**不读取用户文件正文、不外发任何数据、不进 trace**
//! （命令不调用 `Tracer`，按构造保证）。

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::AppHandle;

use crate::search::{IndexStatus, SearchDeps};

/// 单条数据位置（索引 DB / 审计日志 / 配置文件）。前端按表格展示「日志在哪」。
#[derive(Debug, Clone, Serialize)]
pub struct DataLocation {
    /// 人类可读标签（如「索引数据库」）。
    pub label: String,
    /// 绝对路径（展示用，本机自身数据，非用户文件内容）。
    pub path: String,
    /// 文件当前是否存在于磁盘。
    pub exists: bool,
    /// 文件大小（字节）；不存在或取不到时为 0。
    pub size_bytes: u64,
}

/// 隐私信任面板的完整只读快照（BETA-21）。
#[derive(Debug, Clone, Serialize)]
pub struct PrivacyOverview {
    /// 已索引音频条数。
    pub music_count: u64,
    /// 已索引文档条数（不含图片 OCR）。
    pub document_count: u64,
    /// 已索引图片（OCR）条数。
    pub image_count: u64,
    /// 索引总数能否取到（DB 不存在 / 读失败 → false，前端显示「尚未索引」）。
    pub index_available: bool,
    /// 上次完成索引时间（rfc3339），来自 BETA-07 索引状态。
    pub last_indexed: Option<String>,
    /// 当前是否正在后台索引。
    pub indexing: bool,
    /// Scout 数据根目录（索引 DB / 审计日志所在）。
    pub data_root: String,
    /// 各数据文件位置（索引 DB / 审计日志 / 配置文件）。
    pub locations: Vec<DataLocation>,
    /// 当前搜索范围（BETA-27：真实索引根，来自配置 index_roots 解析；空配置 → 系统三夹）。
    pub search_scope: Vec<String>,
    /// 审计记录条数。
    pub audit_count: usize,
    /// BETA-22：搜索历史条数（自动记录的最近查询；不含保存的搜索）。
    pub search_history_count: usize,
    /// BETA-11D：用户同义词库组数。
    pub user_synonym_count: usize,
    /// 是否启用调试追踪（来自配置）。
    pub tracing_enabled: bool,
}

/// 取单个路径的位置信息（存在性 + 大小，best-effort）。
fn data_location(label: &str, path: &Path) -> DataLocation {
    let meta = std::fs::metadata(path).ok();
    DataLocation {
        label: label.to_owned(),
        path: path.display().to_string(),
        exists: meta.is_some(),
        size_bytes: meta.map(|m| m.len()).unwrap_or(0),
    }
}

/// 聚合隐私面板快照（命令 impl，便于单测注入 deps）。
/// `db_path` / `audit_path` 注入便于测试；生产由命令传入 crate 全局路径。
#[allow(clippy::too_many_arguments)]
pub(crate) fn privacy_overview_impl(
    deps: &SearchDeps,
    db_path: &Path,
    audit_path: &Path,
    settings_path: Option<&Path>,
    history_path: Option<&Path>,
    search_history_count: usize,
    synonyms_path: Option<&Path>,
    user_synonym_count: usize,
    search_scope: Vec<String>,
    tracing_enabled: bool,
) -> PrivacyOverview {
    // 索引概览：复用 BETA-07 的 compute_index_totals（单一信源，与状态摘要口径一致）。
    // **先判存在**：compute_index_totals 内部 `open` 会创建空 DB，缺库时不可调用——
    // 否则只读的隐私面板会副作用建一个空 index.db，且把「尚未索引」误报为「已索引 0 条」。
    let totals = if db_path.exists() {
        crate::search::compute_index_totals(db_path)
    } else {
        None
    };
    let (music_count, document_count, image_count) = totals.unwrap_or((0, 0, 0));

    // 索引状态（上次索引时间 / 是否正在索引）。
    let status = crate::search::index_status_snapshot(deps);

    // 数据位置：索引 DB + 审计日志 +（可选）配置文件。
    let mut locations = vec![
        data_location("索引数据库", db_path),
        data_location("操作记录", audit_path),
    ];
    if let Some(cfg) = settings_path {
        locations.push(data_location("配置文件", cfg));
    }
    if let Some(hist) = history_path {
        locations.push(data_location("搜索历史", hist));
    }
    if let Some(syn) = synonyms_path {
        locations.push(data_location("用户同义词库", syn));
    }

    // 数据根目录：取索引 DB 的父目录（审计日志同目录）。
    let data_root = db_path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let audit_count = deps.audit().read_all().len();

    PrivacyOverview {
        music_count,
        document_count,
        image_count,
        index_available: totals.is_some(),
        last_indexed: status.last_indexed,
        indexing: status.indexing,
        data_root,
        locations,
        search_scope,
        audit_count,
        search_history_count,
        user_synonym_count,
        tracing_enabled,
    }
}

/// BETA-21：返回隐私信任面板所需的只读快照。
#[tauri::command]
pub async fn get_privacy_overview(
    app: AppHandle,
    deps: tauri::State<'_, SearchDeps>,
) -> Result<PrivacyOverview, String> {
    let db_path = crate::local_index_db_path();
    let audit_path = crate::audit_log_path();
    let settings_path = crate::settings::settings_file_path(&app);
    // BETA-22：搜索历史位置 / 条数（与历史模块单一信源）。
    let history_path = crate::history::search_history_path(&app);
    let search_history_count = history_path
        .as_deref()
        .map(crate::history::recent_count_at)
        .unwrap_or(0);
    // BETA-11D：用户同义词库位置 / 组数（与 user_synonyms 模块单一信源）。
    let synonyms_path = crate::user_synonyms::user_synonyms_path(&app);
    let user_synonym_count = synonyms_path
        .as_deref()
        .map(crate::user_synonyms::group_count_at)
        .unwrap_or(0);
    // 搜索范围 / 追踪开关来自配置；读失败退到默认值（不阻断面板渲染）。
    let settings = crate::settings::get_settings(app.clone()).unwrap_or_default();
    // BETA-27：隐私面板「搜索范围」展示真实索引根（解析 index_roots：空 → 系统三夹），
    // 与 reindex live-read 的口径一致（保留字段名 search_scope，前端无需改动）。
    // cycle 6 v4：追加模式（include_system_defaults=true）时系统三夹也纳入展示，与 reindex 口径一致。
    let index_roots = crate::settings::resolve_index_roots_tagged(
        &settings.index_roots,
        settings.include_system_defaults,
    )
    .into_iter()
    .map(|(p, _)| p.display().to_string())
    .collect::<Vec<_>>();
    Ok(privacy_overview_impl(
        deps.inner(),
        &db_path,
        &audit_path,
        settings_path.as_deref(),
        history_path.as_deref(),
        search_history_count,
        synonyms_path.as_deref(),
        user_synonym_count,
        index_roots,
        settings.enable_tracing,
    ))
}

/// 清空本地索引（命令 impl，便于单测）。**并发守卫**：正在索引时拒绝（返回 Err 提示）。
/// 清空后复位 BETA-07 状态摘要（last_indexed/last_summary 置空），避免显示陈旧总数。
/// 只取 `index_status` Arc（`SearchDeps` 非 `Clone`，无法整体送进阻塞闭包）。
pub(crate) fn clear_local_index_impl(
    status: &Arc<Mutex<IndexStatus>>,
    db_path: &Path,
) -> Result<(), String> {
    {
        let s = status.lock().unwrap_or_else(|e| e.into_inner());
        if s.indexing {
            return Err("正在索引中，请待索引完成后再清除".to_owned());
        }
    }
    let backend = scout_local_index_backend::LocalIndexBackend::new(db_path.to_path_buf());
    backend.clear().map_err(|e| e.to_string())?;
    // 复位状态摘要：索引已空，旧的「音频 N / 文档 N」摘要不再成立。
    {
        let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
        s.last_indexed = None;
        s.last_summary = None;
    }
    Ok(())
}

/// BETA-21：一键清空本地索引（音频 + 文档 + 图片 OCR，含 FTS）。在阻塞线程跑（SQLite 写）。
/// **不可逆**：前端必须二次确认后才调用。下次 reindex 会重建。
#[tauri::command]
pub async fn clear_local_index(deps: tauri::State<'_, SearchDeps>) -> Result<(), String> {
    let status = deps.index_status_arc();
    let db = crate::local_index_db_path();
    tauri::async_runtime::spawn_blocking(move || clear_local_index_impl(&status, &db))
        .await
        .map_err(|e| format!("清除索引任务失败: {e}"))?
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use scout_harness::context::ContextMemory;
    use scout_harness::file_action_tool::{FileActionTool, LocalFileActionExecutor};
    use scout_harness::{NoopExpander, PolicyEngine, SynonymExpander, ToolRegistry, Tracer};

    /// 构造最小可用 `SearchDeps`（默认内存审计 + 默认索引状态）。
    fn test_deps() -> SearchDeps {
        SearchDeps::new(
            Arc::new(ToolRegistry::new()),
            Arc::new(PolicyEngine::new()),
            Arc::new(Tracer::with_hooks(vec![])),
            Arc::new(Mutex::new(ContextMemory::new())),
            Arc::new(FileActionTool::new(
                Arc::new(LocalFileActionExecutor),
                PolicyEngine::new(),
            )),
            Arc::new(Mutex::new(None)),
            Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
        )
    }

    #[test]
    fn data_location_reports_existence_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.bin");
        std::fs::write(&f, b"hello").unwrap();
        let loc = data_location("测试", &f);
        assert!(loc.exists);
        assert_eq!(loc.size_bytes, 5);
        assert_eq!(loc.label, "测试");

        let missing = dir.path().join("nope.bin");
        let loc = data_location("缺", &missing);
        assert!(!loc.exists);
        assert_eq!(loc.size_bytes, 0);
    }

    #[test]
    fn overview_missing_db_reports_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let audit = dir.path().join("audit.jsonl");
        let cfg = dir.path().join("settings.json");
        let deps = test_deps();

        let hist = dir.path().join("search_history.json");
        let syn = dir.path().join("user-synonyms.yaml");
        let ov = privacy_overview_impl(
            &deps,
            &db,
            &audit,
            Some(&cfg),
            Some(&hist),
            0,
            Some(&syn),
            0,
            vec!["~".to_string()],
            false,
        );
        assert!(!ov.index_available, "缺库应不可用");
        assert_eq!(
            (ov.music_count, ov.document_count, ov.image_count),
            (0, 0, 0)
        );
        assert_eq!(ov.audit_count, 0, "内存审计为空");
        assert_eq!(ov.search_scope, vec!["~".to_string()]);
        assert!(!ov.tracing_enabled);
        // 数据位置含索引库 + 审计 + 配置三项。
        let labels: Vec<&str> = ov.locations.iter().map(|l| l.label.as_str()).collect();
        assert!(labels.contains(&"索引数据库"));
        assert!(labels.contains(&"操作记录"));
        assert!(labels.contains(&"配置文件"));
        assert!(
            labels.contains(&"搜索历史"),
            "BETA-22 搜索历史应作为数据位置展示"
        );
        assert!(
            labels.contains(&"用户同义词库"),
            "BETA-11D 用户同义词库应作为数据位置展示"
        );
        assert_eq!(ov.search_history_count, 0, "缺历史应为 0 条");
        assert_eq!(ov.user_synonym_count, 0, "缺同义词库应为 0 组");
        // data_root 为索引库父目录。
        assert_eq!(ov.data_root, dir.path().display().to_string());
    }

    #[test]
    fn overview_populated_db_reports_counts() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), "预算与营收分析报告正文").unwrap();
        let db = dir.path().join("index.db");
        scout_local_index_backend::LocalIndexBackend::new(&db)
            .reindex(&[], std::slice::from_ref(&docs), &[])
            .unwrap();

        let deps = test_deps();
        let ov = privacy_overview_impl(
            &deps,
            &db,
            &dir.path().join("audit.jsonl"),
            None,
            None,
            0,
            None,
            0,
            vec![],
            false,
        );
        assert!(ov.index_available, "已索引应可用");
        assert!(ov.document_count >= 1, "应至少一篇文档");
        // 无配置 / 历史 / 同义词路径 → locations 仅两项。
        assert_eq!(ov.locations.len(), 2);
    }

    #[test]
    fn overview_includes_user_synonym_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user-synonyms.yaml");
        std::fs::write(
            &path,
            "version: 1\ngroups:\n  - head: 报告\n    aliases: [汇报]\n",
        )
        .unwrap();
        assert_eq!(crate::user_synonyms::group_count_at(&path), 1);
    }

    #[test]
    fn clear_rejected_while_indexing() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let status = Arc::new(Mutex::new(IndexStatus {
            indexing: true,
            ..Default::default()
        }));
        let err = clear_local_index_impl(&status, &db).unwrap_err();
        assert!(err.contains("正在索引"), "应拒绝并提示，实得 {err}");
    }

    #[test]
    fn clear_empties_index_and_resets_summary() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), "机密内容正文").unwrap();
        let db = dir.path().join("index.db");
        scout_local_index_backend::LocalIndexBackend::new(&db)
            .reindex(&[], std::slice::from_ref(&docs), &[])
            .unwrap();

        // 预置陈旧摘要，验证清除后被复位。
        let status = Arc::new(Mutex::new(IndexStatus {
            indexing: false,
            last_indexed: Some("2026-06-03T00:00:00Z".to_string()),
            last_summary: Some("音频 0 / 文档 1 / 图片 0".to_string()),
            // BETA-15B-2：新增语义字段取默认（false/None），保持本测试行为不变。
            ..Default::default()
        }));

        clear_local_index_impl(&status, &db).unwrap();

        // DROP+VACUUM：文件仍在但内容清空（open 重建空表 → count 0）。
        assert_eq!(
            scout_indexer::DocumentIndex::open(&db)
                .unwrap()
                .count()
                .unwrap(),
            0,
            "clear 后内容应为空"
        );
        let s = status.lock().unwrap();
        assert!(s.last_indexed.is_none(), "应复位 last_indexed");
        assert!(s.last_summary.is_none(), "应复位 last_summary");
    }
}
