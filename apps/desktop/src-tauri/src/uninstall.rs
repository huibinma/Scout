//! BETA-12 卸载流程：应用内「卸载清理」。
//!
//! 删除本机**派生数据**——索引数据库 / 模型文件 / 运行日志 / 操作记录 / 搜索历史 /
//! 用户同义词库（`user-synonyms.yaml`，卡片明示必清）；**保留配置**（`settings.json` 与
//! onboarding 状态不动，重装后设置仍在）。
//!
//! 两条清理路径的分工（**语义不同、不是同一份清单的两处实现**）：
//! - **Windows 安装版**：真正的卸载走 NSIS 卸载器，由 `nsis/uninstall-hooks.nsh`
//!   （`tauri.conf.json > bundle.windows.nsis.installerHooks` 挂载）在卸载器里执行清理——
//!   届时 app 已退出、无文件占用；带 `$UpdateMode` 守卫（版本升级绝不清数据），且模型 +
//!   索引（index.db 及 -wal/-shm）默认都保留（询问一次，静默卸载 `/SD IDNO` = 保留）——
//!   覆盖「先卸载旧版再装新版」这类手动两步操作，重装后无需重新下载模型 / 全量重扫索引。
//! - **本命令**：用户在「隐私」页主动点「卸载清理」——**全删**（含索引、含模型），是
//!   「彻底清空我的本机数据」的显式动作，与 NSIS 路径「卸载是为了重装、想留着索引接着用」
//!   语义相反，不做同等保留。覆盖 macOS（DMG 拖拽卸载无卸载器）、Windows 便携版、以及
//!   「真要卸载前先手动清一遍」场景。删除前先
//!   [`unload`](crate::search::embedding_model::EmbeddingModelHandle::unload)
//!   常驻模型释放 GGUF 文件句柄（Windows 上 mmap 中的模型文件删不掉）。
//!   当天的 `scout.log` 仍被 tracing-appender 持有，但 Rust std 打开文件默认带
//!   `FILE_SHARE_DELETE`、删除可成功（后续写入落在已删除的 inode 上，跨日自然滚新文件）。
//!
//! **隐私**：本模块只删 Scout 自身数据目录 / 配置目录内的派生文件，不触碰用户文件，
//! 不外发、不进 trace。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::search::IndexStatus;

/// 单项清理结果（前端逐行展示；`removed=false` 时 `detail` 给人话原因）。
#[derive(Debug, Clone, Serialize)]
pub struct CleanupItem {
    /// 人类可读标签（如「索引数据库」）。
    pub label: String,
    /// 目标路径（展示用）。
    pub path: String,
    /// 清理前是否存在（不存在 → 无事可做、视为成功）。
    pub existed: bool,
    /// 是否已删除（或本来就不存在）。
    pub removed: bool,
    /// 失败原因（仅 `removed=false` 时给）。
    pub detail: Option<String>,
}

/// 卸载清理总报告。
#[derive(Debug, Clone, Serialize)]
pub struct CleanupReport {
    /// 逐项结果（索引 / 模型 / 日志 / 审计 / 搜索历史 / 用户同义词库）。
    pub items: Vec<CleanupItem>,
    /// 全部成功（含「本来就不存在」）。
    pub all_ok: bool,
}

/// 清理目标路径集合（注入便于单测；生产由命令层从 crate 全局路径 + AppHandle 派生）。
pub(crate) struct CleanupTargets {
    /// Scout 数据目录（含 index.db / models/ / scout.log* / audit.jsonl）。
    pub data_dir: PathBuf,
    /// `user-synonyms.yaml` 路径（配置目录内；None = 取不到配置目录，跳过）。
    pub user_synonyms: Option<PathBuf>,
    /// `search_history.json` 路径（配置目录内；查询词属敏感数据，随「日志」口径一并清）。
    pub search_history: Option<PathBuf>,
}

/// 删单个文件：不存在视为成功；失败进 detail。
fn remove_file_item(label: &str, path: &Path) -> CleanupItem {
    let existed = path.exists();
    let (removed, detail) = if !existed {
        (true, None)
    } else {
        match std::fs::remove_file(path) {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        }
    };
    CleanupItem {
        label: label.to_owned(),
        path: path.display().to_string(),
        existed,
        removed,
        detail,
    }
}

/// 删索引数据库：主文件 + SQLite `-wal` / `-shm` 兄弟文件合并为一项。
fn remove_index_item(db_path: &Path) -> CleanupItem {
    let mut item = remove_file_item("索引数据库", db_path);
    for suffix in ["-wal", "-shm"] {
        let mut os = db_path.as_os_str().to_owned();
        os.push(suffix);
        let sibling = PathBuf::from(os);
        if sibling.exists() {
            if let Err(e) = std::fs::remove_file(&sibling) {
                item.removed = false;
                let msg = format!("{} 删除失败: {e}", sibling.display());
                item.detail = Some(match item.detail.take() {
                    Some(prev) => format!("{prev}; {msg}"),
                    None => msg,
                });
            }
        }
    }
    item
}

/// 删模型目录（`<data_dir>/models/`，含 `.partial` 残片）。
fn remove_models_item(data_dir: &Path) -> CleanupItem {
    let models_dir = data_dir.join("models");
    let existed = models_dir.exists();
    let (removed, detail) = if !existed {
        (true, None)
    } else {
        match std::fs::remove_dir_all(&models_dir) {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        }
    };
    CleanupItem {
        label: "模型文件".to_owned(),
        path: models_dir.display().to_string(),
        existed,
        removed,
        detail,
    }
}

/// 删运行日志：数据目录下所有 `scout.log` 前缀文件（当天 + daily 滚动历史）合并为一项。
fn remove_logs_item(data_dir: &Path) -> CleanupItem {
    let pattern = data_dir.join("scout.log*");
    let mut existed = false;
    let mut failures: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(data_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let is_log = name.to_string_lossy().starts_with("scout.log");
            if is_log && entry.path().is_file() {
                existed = true;
                if let Err(e) = std::fs::remove_file(entry.path()) {
                    failures.push(format!("{} 删除失败: {e}", entry.path().display()));
                }
            }
        }
    }
    CleanupItem {
        label: "运行日志".to_owned(),
        path: pattern.display().to_string(),
        existed,
        removed: failures.is_empty(),
        detail: if failures.is_empty() {
            None
        } else {
            Some(failures.join("; "))
        },
    }
}

/// 卸载清理（命令 impl，便于单测）。
///
/// **并发守卫**：FTS 索引 / 语义嵌入进行中一律拒绝（删 index.db / models/ 会与写入竞争）。
/// 守卫通过后先调 `unload_models`（生产 = 卸载两个常驻模型句柄释放文件占用；测试注入空闭包），
/// 再逐项删除。索引删除后复位状态摘要（与 [`crate::privacy::clear_local_index_impl`] 同口径）。
pub(crate) fn uninstall_cleanup_impl(
    status: &Arc<Mutex<IndexStatus>>,
    targets: &CleanupTargets,
    unload_models: impl FnOnce(),
) -> Result<CleanupReport, String> {
    {
        let s = status.lock().unwrap_or_else(|e| e.into_inner());
        if s.indexing {
            return Err("正在索引中，请待索引完成后再清理".to_owned());
        }
        if s.semantic_indexing {
            return Err("语义嵌入进行中，请待其完成后再清理".to_owned());
        }
    }

    unload_models();

    let mut items = vec![
        remove_index_item(&targets.data_dir.join("index.db")),
        remove_models_item(&targets.data_dir),
        remove_logs_item(&targets.data_dir),
        remove_file_item("操作记录", &targets.data_dir.join("audit.jsonl")),
    ];
    if let Some(p) = &targets.search_history {
        items.push(remove_file_item("搜索历史", p));
    }
    if let Some(p) = &targets.user_synonyms {
        items.push(remove_file_item("用户同义词库", p));
    }

    // 索引已删：复位状态摘要，避免 UI 显示陈旧总数。
    {
        let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
        s.last_indexed = None;
        s.last_summary = None;
        s.db_totals = None;
        s.semantic_summary = None;
    }

    let all_ok = items.iter().all(|i| i.removed);
    Ok(CleanupReport { items, all_ok })
}

/// BETA-12：应用内卸载清理（删索引 / 模型 / 日志 / 审计 / 搜索历史 / 用户同义词库，
/// 保留 settings.json）。**不可逆**：前端必须二次确认后才调用。在阻塞线程跑（文件 IO）。
#[tauri::command]
pub async fn uninstall_cleanup(
    app: tauri::AppHandle,
    deps: tauri::State<'_, crate::search::SearchDeps>,
    synonyms: tauri::State<'_, crate::user_synonyms::UserSynonymState>,
) -> Result<CleanupReport, String> {
    if crate::model_download::any_download_in_flight() {
        return Err("模型正在下载中，请先取消下载再清理".to_owned());
    }
    if deps.model().busy.load(std::sync::atomic::Ordering::Acquire) {
        return Err("模型推理进行中，请稍后再清理".to_owned());
    }

    let status = deps.index_status_arc();
    let embedding = deps.embedding().clone();
    let model = deps.model().clone();
    let targets = CleanupTargets {
        data_dir: crate::scout_data_dir(),
        user_synonyms: crate::user_synonyms::user_synonyms_path(&app),
        search_history: crate::history::search_history_path(&app),
    };
    let report = tauri::async_runtime::spawn_blocking(move || {
        uninstall_cleanup_impl(&status, &targets, || {
            embedding.unload();
            model.unload();
        })
    })
    .await
    .map_err(|e| format!("卸载清理任务失败: {e}"))??;

    // 文件已删，内存用户词典同步清空（LayeredSynonymExpander 共享同一把锁、搜索路径立即生效）。
    if let Ok(mut idx) = synonyms.index.write() {
        *idx = scout_harness::UserIndex::empty();
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"x").unwrap();
    }

    fn targets_in(dir: &Path) -> CleanupTargets {
        CleanupTargets {
            data_dir: dir.join("data"),
            user_synonyms: Some(dir.join("config").join("user-synonyms.yaml")),
            search_history: Some(dir.join("config").join("search_history.json")),
        }
    }

    #[test]
    fn cleanup_removes_derived_data_and_keeps_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let t = targets_in(tmp.path());
        // 派生数据全套：索引（含 wal/shm）/ 模型 / 日志（当天 + 滚动）/ 审计 / 历史 / 同义词。
        touch(&t.data_dir.join("index.db"));
        touch(&t.data_dir.join("index.db-wal"));
        touch(&t.data_dir.join("index.db-shm"));
        touch(
            &t.data_dir
                .join("models")
                .join("embeddinggemma-300m-q8_0.gguf"),
        );
        touch(
            &t.data_dir
                .join("models")
                .join("qwen3-0.6b-q4_k_m.gguf.partial"),
        );
        touch(&t.data_dir.join("scout.log"));
        touch(&t.data_dir.join("scout.log.2026-07-03"));
        touch(&t.data_dir.join("audit.jsonl"));
        touch(t.search_history.as_ref().unwrap());
        touch(t.user_synonyms.as_ref().unwrap());
        // 配置必须保留。
        let settings = tmp.path().join("config").join("settings.json");
        touch(&settings);

        let status = Arc::new(Mutex::new(IndexStatus {
            last_indexed: Some("2026-07-04T00:00:00Z".to_owned()),
            last_summary: Some("音频 1 / 文档 2 / 图片 3".to_owned()),
            db_totals: Some((1, 2, 3)),
            semantic_summary: Some("语义索引就绪 2 篇".to_owned()),
            ..Default::default()
        }));
        let mut unloaded = false;
        let report = uninstall_cleanup_impl(&status, &t, || unloaded = true).unwrap();

        assert!(unloaded, "应先卸载常驻模型再删文件");
        assert!(report.all_ok, "全部应清理成功: {:?}", report.items);
        assert!(!t.data_dir.join("index.db").exists());
        assert!(!t.data_dir.join("index.db-wal").exists());
        assert!(!t.data_dir.join("index.db-shm").exists());
        assert!(!t.data_dir.join("models").exists(), "models 目录应整体删除");
        assert!(!t.data_dir.join("scout.log").exists());
        assert!(!t.data_dir.join("scout.log.2026-07-03").exists());
        assert!(!t.data_dir.join("audit.jsonl").exists());
        assert!(!t.search_history.as_ref().unwrap().exists());
        assert!(!t.user_synonyms.as_ref().unwrap().exists());
        assert!(
            settings.exists(),
            "settings.json 必须保留（BETA-12「保留配置」）"
        );

        // 状态摘要复位。
        let s = status.lock().unwrap();
        assert!(s.last_indexed.is_none());
        assert!(s.last_summary.is_none());
        assert!(s.db_totals.is_none());
        assert!(s.semantic_summary.is_none());

        // 六项逐项标签齐全。
        let labels: Vec<&str> = report.items.iter().map(|i| i.label.as_str()).collect();
        for expect in [
            "索引数据库",
            "模型文件",
            "运行日志",
            "操作记录",
            "搜索历史",
            "用户同义词库",
        ] {
            assert!(labels.contains(&expect), "缺少清理项 {expect}: {labels:?}");
        }
    }

    #[test]
    fn cleanup_with_nothing_to_delete_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let t = targets_in(tmp.path());
        let status = Arc::new(Mutex::new(IndexStatus::default()));
        let report = uninstall_cleanup_impl(&status, &t, || {}).unwrap();
        assert!(report.all_ok);
        assert!(
            report.items.iter().all(|i| !i.existed && i.removed),
            "空目标应逐项报「本来就不存在」: {:?}",
            report.items
        );
    }

    #[test]
    fn cleanup_rejected_while_indexing() {
        let tmp = tempfile::tempdir().unwrap();
        let t = targets_in(tmp.path());
        let status = Arc::new(Mutex::new(IndexStatus {
            indexing: true,
            ..Default::default()
        }));
        let err = uninstall_cleanup_impl(&status, &t, || {}).unwrap_err();
        assert!(err.contains("正在索引"), "应拒绝并提示，实得 {err}");
    }

    #[test]
    fn cleanup_rejected_while_semantic_indexing() {
        let tmp = tempfile::tempdir().unwrap();
        let t = targets_in(tmp.path());
        let status = Arc::new(Mutex::new(IndexStatus {
            semantic_indexing: true,
            ..Default::default()
        }));
        let err = uninstall_cleanup_impl(&status, &t, || {}).unwrap_err();
        assert!(err.contains("语义嵌入"), "应拒绝并提示，实得 {err}");
    }

    /// BETA-12 闸门：NSIS 卸载 hook 必须挂在 tauri.conf.json 且 hook 文件在仓、
    /// 带升级守卫（`$UpdateMode`）——否则 Windows 安装版卸载流程静默失效 / 升级误删数据。
    #[test]
    fn nsis_uninstall_hook_is_wired_and_guarded() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let hook_rel = conf["bundle"]["windows"]["nsis"]["installerHooks"]
            .as_str()
            .expect("tauri.conf.json 应配置 bundle.windows.nsis.installerHooks");
        let hook_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(hook_rel);
        assert!(
            hook_path.exists(),
            "hook 文件应在仓: {}",
            hook_path.display()
        );
        let content = std::fs::read_to_string(&hook_path).unwrap();
        assert!(
            content.contains("NSIS_HOOK_POSTUNINSTALL"),
            "hook 应定义 NSIS_HOOK_POSTUNINSTALL"
        );
        assert!(
            content.contains("$UpdateMode"),
            "hook 必须带 $UpdateMode 升级守卫（版本升级不得清用户索引与模型）"
        );
        assert!(
            content.contains("user-synonyms.yaml"),
            "hook 应清除用户同义词库（BETA-12 卡片明示）"
        );
        // 只查指令行（排除 `;` 注释——注释里允许解释「为什么保留 settings.json」）。
        let directive_lines: Vec<&str> = content
            .lines()
            .filter(|l| !l.trim_start().starts_with(';'))
            .collect();
        // 2026-07-29：数据目录统一后 settings.json 与 index.db/models 同目录，RMDir /r
        // 会连带清空——不再是「活在另一个目录、天然不受影响」，hook 必须像模型/索引
        // 一样显式 Rename 暂存 settings.json 再放回，且保留模型索引 / 彻底删除两条
        // 分支都要保留（各一次暂存 + 共用一次恢复，故至少 2 处 Rename + settings.json）。
        assert!(
            directive_lines
                .iter()
                .filter(|l| l.contains("Rename") && l.contains("settings.json"))
                .count()
                >= 2,
            "hook 应在两条分支都显式暂存 settings.json（保留配置）"
        );
        assert!(
            directive_lines
                .iter()
                .any(|l| l.contains("IfFileExists") && l.contains("settings.json")),
            "hook 应在恢复前检查 settings.json 暂存文件是否存在"
        );
        // 2026-07-06（cycle 9 真机反馈拍板）：模型默认保留——hook 必须带 models 暂存
        // Rename 路径 + MessageBox 询问 + 静默默认保留（/SD IDNO）。
        assert!(
            directive_lines
                .iter()
                .any(|l| l.contains("Rename") && l.contains("models")),
            "hook 应含 models 暂存 Rename（默认保留模型、避免重装 ~700MB 重下）"
        );
        assert!(
            content.contains("/SD IDNO"),
            "hook 的模型删除询问必须带 /SD IDNO（静默卸载默认保留模型、不挂弹窗）"
        );
        // 索引默认保留（用户反馈拍板）：「先卸载旧版再装新版」这类手动两步操作，
        // hook 必须像保留模型一样把 index.db（+ -wal/-shm）暂存 Rename 出来再整目录删、
        // 移回——否则每次手动重装都逼一次小时级全量重扫。
        assert!(
            directive_lines
                .iter()
                .any(|l| l.contains("Rename") && l.contains("index.db\" ")),
            "hook 应含 index.db 暂存 Rename（默认保留索引、避免手动重装后全量重扫）"
        );
        assert!(
            directive_lines
                .iter()
                .any(|l| l.contains("Rename") && l.contains("index.db-wal")),
            "hook 应连带保留 index.db-wal 兄弟文件"
        );
        assert!(
            directive_lines
                .iter()
                .any(|l| l.contains("Rename") && l.contains("index.db-shm")),
            "hook 应连带保留 index.db-shm 兄弟文件"
        );
    }
}
