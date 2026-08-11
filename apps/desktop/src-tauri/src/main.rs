// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod history;
mod mcp_service;
mod model_download;
mod permissions;
mod privacy;
mod search;
mod settings;
mod shortcut;
mod status;
#[cfg(target_os = "windows")]
mod tray;
mod uninstall;
mod update;
mod user_synonyms;

use std::sync::{Arc, Mutex};

use std::path::{Path, PathBuf};

use tauri::Manager;
use tracing::{info, warn};

use scout_harness::context::ContextMemory;
use scout_harness::file_action_tool::{FileActionTool, LocalFileActionExecutor};
use scout_harness::{
    LayeredSynonymExpander, NoopExpander, PolicyEngine, SearchTool, SupportedIntent,
    SynonymExpander, ToolRegistry, Tracer, UserIndex, YamlSynonymExpander,
};
use scout_search_backend::FileAction;

use scout_local_index_backend::LocalIndexBackend;

#[cfg(target_os = "macos")]
use scout_search_backend_spotlight::SpotlightBackend;

#[cfg(target_os = "windows")]
use scout_search_backend_everything::EverythingBackend;
#[cfg(target_os = "windows")]
use scout_search_backend_windows_search::WindowsSearchBackend;

/// Scout 数据目录（**全栈单一信源**）。Windows 实际路径为
/// `%APPDATA%\Roaming\Scout\`、macOS 为 `~/Library/Application Support/Scout/`、
/// Linux 为 `~/.local/share/Scout/`。索引数据库 / 审计日志 / 模型文件 / 模型下载 /
/// 设置 / 搜索历史 / 用户同义词库 / 引导状态**全部**经此 helper 派生，**不要**在任何
/// 子模块独立用 `app.path().app_config_dir()` / `app_data_dir()`（Tauri 路径基于 bundle id
/// `ai.scout.desktop`、与此处不一致；BETA-31 下载模块曾踩过此坑，导致下载文件
/// `EmbeddingModelHandle` 永远找不到、用户重复下载死循环）。2026-07-29 起把设置 /
/// 历史 / 同义词库 / 引导状态也统一收进来之前，这几个模块曾各自用 Tauri 默认
/// `app_config_dir()`、悄悄落在旁边的 `ai.scout.desktop` 目录——两个目录并存，隐私面板
/// 「数据存在哪」列表里几行路径和「数据目录」摘要对不上，见 [`migrate_legacy_config_dir`]。
pub(crate) fn scout_data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Scout")
}

/// 一次性迁移：把曾经落在 Tauri 默认 `app_config_dir()`（bundle id `ai.scout.desktop`
/// 目录）里的用户数据文件搬到 [`scout_data_dir`]，统一数据存储位置、不再让用户看到两个
/// 目录。**best-effort**：任何一步失败只记警告，不阻断启动（老用户此时会退化为该文件
/// 重新走默认值，而不是整个应用打不开）。只在**新目录还没有同名文件**时才搬，避免
/// 覆盖用户在新目录已产生的数据。必须在 `.setup()` 里任何读取这些文件的逻辑**之前**调用。
fn migrate_legacy_config_dir(app: &tauri::AppHandle) {
    use tauri::Manager;

    let Ok(old_dir) = app.path().app_config_dir() else {
        return;
    };
    migrate_legacy_config_dir_impl(&old_dir, &scout_data_dir());
}

/// 迁移逻辑本体（纯路径参数，便于单测；`migrate_legacy_config_dir` 只负责解析
/// Tauri 侧的旧目录路径）。
fn migrate_legacy_config_dir_impl(old_dir: &Path, new_dir: &Path) {
    if !old_dir.exists() {
        return;
    }

    for name in [
        "settings.json",
        "search_history.json",
        "user-synonyms.yaml",
        "onboarding.json",
    ] {
        let old_file = old_dir.join(name);
        let new_file = new_dir.join(name);
        if !old_file.exists() || new_file.exists() {
            continue;
        }
        if let Err(e) = std::fs::create_dir_all(new_dir) {
            warn!(file = name, error = %e, "迁移旧配置文件失败：无法创建 Scout 数据目录，本次启动该文件将退化为默认值");
            continue;
        }
        match std::fs::rename(&old_file, &new_file) {
            Ok(()) => info!(
                file = name,
                from = %old_file.display(),
                to = %new_file.display(),
                "迁移旧配置文件到 Scout 数据目录（统一数据存储位置）"
            ),
            Err(e) => {
                warn!(file = name, error = %e, "迁移旧配置文件失败，本次启动该文件将退化为默认值")
            }
        }
    }
}

/// 本地索引数据库路径（音频 + 文档表共用一个 sqlite 文件）。
/// `LocalIndexBackend`（搜索）与 `reindex` 命令（写入）共用此路径。
/// BETA-20：`search::preview` 模块经 `crate::local_index_db_path()` 取同一路径读预览。
pub(crate) fn local_index_db_path() -> PathBuf {
    scout_data_dir().join("index.db")
}

/// 桌面 app 日志文件路径（BETA-31-v3 cycle 2，v0.8.3）。tracing-appender daily 滚动、
/// 写到 `<scout_data_dir>/scout.log` + 同目录每日历史 `scout.log.YYYY-MM-DD`。
/// 设置页 / SettingsPage 后续可加「打开日志目录」按钮（follow-up cycle）。
pub(crate) fn log_dir() -> PathBuf {
    scout_data_dir()
}

/// 初始化 tracing subscriber：写日志到 `<scout_data_dir>/scout.log`（daily 滚动）+
/// stderr（debug build only）。env `SCOUT_LOG` 覆盖级别（默认 info）。
///
/// 返回 [`tracing_appender::non_blocking::WorkerGuard`]，调用方必须把它 bind 到一个变量
/// 让它活到 `main()` 结束——drop 时 worker thread join + flush 剩余 buffered log。如果
/// 提前 drop（如 `_ = init_tracing(...)`），程序退出时尾部日志会丢。
///
/// daily 滚动：当天日志写 `scout.log`，跨日时 rename 旧文件为 `scout.log.YYYY-MM-DD`、
/// 起新 `scout.log`。这里不设保留天数（tracing-appender 不支持自动清理）、用户可手动清。
fn init_tracing(log_dir: &Path) -> tracing_appender::non_blocking::WorkerGuard {
    use tracing_subscriber::EnvFilter;

    let _ = std::fs::create_dir_all(log_dir); // 失败时下方 appender 会自己报；不阻塞启动
    let appender = tracing_appender::rolling::daily(log_dir, "scout.log");
    let (file_writer, guard) = tracing_appender::non_blocking(appender);

    let filter = EnvFilter::try_from_env("SCOUT_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

    // 文件 sink（结构化、带 thread id + 模块路径，便于追溯）。debug build 同时写 stderr 方便
    // `cargo tauri dev` 直接看；release build 只写文件（exe 默认无 console）。
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(file_writer)
        .with_target(true)
        .with_thread_ids(true)
        .with_ansi(false); // 文件不要 ANSI 颜色码

    // try_init 失败说明已有 subscriber（理论上不会）；non-fatal。
    if let Err(e) = builder.try_init() {
        eprintln!("tracing subscriber init 失败（已被前驱占？）: {e}");
    }
    guard
}

/// BETA-06 审计日志文件路径（append-only JSONL）。
/// BETA-21：隐私面板复用此路径展示「日志在哪」（单一信源）。
pub(crate) fn audit_log_path() -> PathBuf {
    scout_data_dir().join("audit.jsonl")
}

fn build_registry(
    embedding: Arc<search::embedding_model::EmbeddingModelHandle>,
    settings_path: Option<PathBuf>,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();

    // BETA-04：本地音频/文档索引后端（跨平台，纯 Rust，恒可用）。内容/媒体查询时与
    // 系统搜索一起 fan-out 合并（见 search.rs）。未 reindex 时返回空、不影响系统后端。
    {
        let local = LocalIndexBackend::new(local_index_db_path());
        let tool = SearchTool::new(
            "search.local",
            "本地索引",
            local,
            vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
            "Scout 本地音频/文档索引（artist/正文 FTS）",
        );
        if let Err(err) = registry.register_search(tool) {
            eprintln!("注册 LocalIndexBackend 失败: {err}");
            warn!(backend = "local-index", error = %err, "注册 backend 失败");
        } else {
            info!(backend = "local-index", "backend 注册成功");
        }
    }

    // BETA-15B-1：本地语义召回后端（embedding + cosine，按意思/跨语言模糊召回）。
    // 与 FTS 内容后端并列 fan-out，融合走加权 RRF（见 search/fanout.rs）。embedding 句柄
    // 与索引期文档嵌入、F5 状态命令共用同一 Arc（main.rs setup 构造）。句柄探测不可用
    // （feature 关 / 模型缺失 / 加载失败，TextEmbedder::is_ready()=false）时
    // is_available()=false，语义臂路由期即退出、整链优雅降级 FTS-only（BETA-33 cycle 9）。
    {
        let floor_settings_path = settings_path.clone();
        let semantic = scout_search_backend_semantic::SemanticIndexBackend::new(
            local_index_db_path(),
            Some(embedding.clone() as Arc<dyn scout_indexer::embed::TextEmbedder>),
            std::sync::Arc::new(move || settings::read_similarity_floor(&floor_settings_path)),
        );
        let tool = SearchTool::new(
            "search.semantic",
            "语义召回",
            semantic,
            vec![SupportedIntent::FileSearch],
            "Scout 本地语义召回（embedding + cosine，按意思/跨语言）",
        );
        if let Err(err) = registry.register_search(tool) {
            eprintln!("注册 SemanticIndexBackend 失败: {err}");
            warn!(backend = "semantic", error = %err, "注册 backend 失败");
        } else {
            info!(backend = "semantic", "backend 注册成功");
        }
    }

    #[cfg(target_os = "macos")]
    match SpotlightBackend::new() {
        Ok(backend) => {
            let tool = SearchTool::new(
                "search.spotlight",
                "Spotlight",
                backend,
                vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
                "macOS Spotlight 系统搜索（mdfind）",
            );
            if let Err(err) = registry.register_search(tool) {
                eprintln!("注册 SpotlightBackend 失败: {err}");
                warn!(backend = "spotlight", error = %err, "注册 backend 失败");
            } else {
                info!(backend = "spotlight", "backend 注册成功");
            }
        }
        Err(err) => {
            eprintln!("初始化 SpotlightBackend 失败: {err}");
            warn!(backend = "spotlight", error = %err, "backend 初始化失败");
        }
    }

    #[cfg(target_os = "windows")]
    {
        match WindowsSearchBackend::new() {
            Ok(backend) => {
                let tool = SearchTool::new(
                    "search.windows",
                    "Windows Search",
                    backend,
                    vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
                    "Windows Search 系统索引（OLE DB / SystemIndex SQL）",
                );
                if let Err(err) = registry.register_search(tool) {
                    eprintln!("注册 WindowsSearchBackend 失败: {err}");
                    warn!(backend = "windows-search", error = %err, "注册 backend 失败");
                } else {
                    info!(backend = "windows-search", "backend 注册成功");
                }
            }
            Err(err) => {
                eprintln!("初始化 WindowsSearchBackend 失败: {err}");
                warn!(backend = "windows-search", error = %err, "backend 初始化失败");
            }
        }
        // BETA-47：Everything 集成开关（设置默认开）。关闭时不注册后端——改动开关
        // 需重启应用生效（与 model_path 覆盖同口径）；索引/模型发现两处 live-read 另行门控。
        if !settings::read_enable_everything(&settings_path) {
            info!(
                backend = "everything",
                "Everything 集成已在设置中关闭，跳过注册"
            );
        } else {
            match EverythingBackend::new() {
                Ok(backend) => {
                    let tool = SearchTool::new(
                        "search.everything",
                        "Everything",
                        backend,
                        vec![SupportedIntent::FileSearch, SupportedIntent::MediaSearch],
                        "Everything 加速搜索（es.exe CLI）",
                    );
                    if let Err(err) = registry.register_search(tool) {
                        eprintln!("注册 EverythingBackend 失败: {err}");
                        warn!(backend = "everything", error = %err, "注册 backend 失败");
                    } else {
                        info!(backend = "everything", "backend 注册成功");
                    }
                }
                Err(err) => {
                    eprintln!("初始化 EverythingBackend 失败: {err}");
                    warn!(backend = "everything", error = %err, "backend 初始化失败");
                }
            }
        }
    }

    registry
}

/// 构造 Tracer。环境变量 SCOUT_TRACE 控制 hook:
/// - 未设/空 → 0 hook(默认无开销)
/// - 设非空 path → 尝试 OpenOptions append 打开,成功挂 JsonLinesHook,失败 fallback noop + stderr warn
fn build_tracer() -> Arc<Tracer> {
    use scout_harness::{JsonLinesHook, TracingHook};
    use std::fs::OpenOptions;

    let path = std::env::var("SCOUT_TRACE").ok().filter(|s| !s.is_empty());
    let hooks: Vec<Box<dyn TracingHook>> = match path {
        None => vec![],
        Some(p) => match OpenOptions::new().create(true).append(true).open(&p) {
            Ok(file) => vec![Box::new(JsonLinesHook::new(file))],
            Err(err) => {
                eprintln!("SCOUT_TRACE 打开 {p} 失败 ({err}), tracing 禁用");
                vec![]
            }
        },
    };
    Arc::new(Tracer::with_hooks(hooks))
}

/// 解析同义词词典路径：打包态优先用 Tauri resource_dir，开发态 fallback 到工作区根。
fn resolve_synonym_paths(app: &tauri::AppHandle) -> (PathBuf, PathBuf) {
    // 打包态：从 .app bundle 内 resource_dir 找词典
    if let Ok(resource_dir) = app.path().resource_dir() {
        let zh = resource_dir.join("synonyms/zh.yaml");
        let en = resource_dir.join("synonyms/en.yaml");
        if zh.exists() && en.exists() {
            return (zh, en);
        }
    }
    // 便携版 fallback：exe 同级目录的 synonyms/（免安装时 resource_dir 行为不保证）。
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let zh = dir.join("synonyms/zh.yaml");
            let en = dir.join("synonyms/en.yaml");
            if zh.exists() && en.exists() {
                return (zh, en);
            }
        }
    }
    // 开发态 fallback：沿 cwd 向上找含 Cargo.toml + packages/ 的工作区根
    let workspace_root = std::env::current_dir()
        .ok()
        .and_then(|cwd| {
            std::iter::successors(Some(cwd), |p| p.parent().map(Path::to_path_buf))
                .find(|p| p.join("Cargo.toml").exists() && p.join("packages").exists())
        })
        .unwrap_or_else(|| PathBuf::from("."));
    (
        workspace_root.join("resources/synonyms/zh.yaml"),
        workspace_root.join("resources/synonyms/en.yaml"),
    )
}

/// 构造双层同义词扩展器：系统层加载失败退到 noop；同时返回与之共享的用户词典 Arc。
fn build_synonym_expander(
    app: &tauri::AppHandle,
) -> (Arc<dyn SynonymExpander>, Arc<std::sync::RwLock<UserIndex>>) {
    let user_index = user_synonyms::user_synonyms_path(app)
        .as_deref()
        .map(user_synonyms::load_user_index)
        .unwrap_or_else(UserIndex::empty);
    let user = Arc::new(std::sync::RwLock::new(user_index));

    let (zh, en) = resolve_synonym_paths(app);
    let expander: Arc<dyn SynonymExpander> = match YamlSynonymExpander::from_paths(&zh, &en) {
        Ok(system) => Arc::new(LayeredSynonymExpander::new(system, Arc::clone(&user))),
        Err(err) => {
            eprintln!("synonym: 系统词典加载失败，退到 noop: {err}");
            Arc::new(NoopExpander)
        }
    };
    (expander, user)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AutoIndexTickDecision {
    Disabled,
    RunAfter(std::time::Duration),
}

/// 自动增量索引调度的纯决策：0 = 关闭；其他值按分钟等待一整轮后执行。
fn auto_index_tick_decision(interval_minutes: u32) -> AutoIndexTickDecision {
    if interval_minutes == 0 {
        AutoIndexTickDecision::Disabled
    } else {
        AutoIndexTickDecision::RunAfter(std::time::Duration::from_secs(
            u64::from(interval_minutes) * 60,
        ))
    }
}

async fn run_auto_index_loop(
    status: Arc<Mutex<search::IndexStatus>>,
    embedding: Arc<search::embedding_model::EmbeddingModelHandle>,
    settings_path: Option<PathBuf>,
) {
    loop {
        let configured_minutes = settings::read_auto_index_interval_minutes(&settings_path);
        let AutoIndexTickDecision::RunAfter(delay) = auto_index_tick_decision(configured_minutes)
        else {
            tracing::debug!("自动增量索引已关闭，60 秒后重读设置");
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            continue;
        };

        tokio::time::sleep(delay).await;

        // tick 前再读一次，让用户在等待期间关闭自动索引时本轮不执行。
        let tick_minutes = settings::read_auto_index_interval_minutes(&settings_path);
        if matches!(
            auto_index_tick_decision(tick_minutes),
            AutoIndexTickDecision::Disabled
        ) {
            tracing::info!("自动增量索引跳过：设置已关闭");
            continue;
        }

        tracing::info!(interval_minutes = tick_minutes, "自动增量索引开始");
        let reindex_start = std::time::Instant::now();
        let db = local_index_db_path();
        let fts_db = db.clone();
        let fts_status = status.clone();
        let fts_settings_path = settings_path.clone();
        match tauri::async_runtime::spawn_blocking(move || {
            search::perform_reindex(&fts_status, fts_db, fts_settings_path)
        })
        .await
        {
            Ok(Ok(Some(stats))) => {
                let elapsed_ms =
                    u64::try_from(reindex_start.elapsed().as_millis()).unwrap_or(u64::MAX);
                tracing::info!(
                    elapsed_ms,
                    music_added = stats.music_added,
                    music_updated = stats.music_updated,
                    doc_added = stats.doc_added,
                    doc_updated = stats.doc_updated,
                    image_added = stats.image_added,
                    image_updated = stats.image_updated,
                    "自动增量索引完成"
                );
                search::spawn_semantic_index(
                    status.clone(),
                    db,
                    embedding.clone(),
                    settings_path.clone(),
                );
            }
            Ok(Ok(None)) => {
                let elapsed_ms =
                    u64::try_from(reindex_start.elapsed().as_millis()).unwrap_or(u64::MAX);
                tracing::info!(elapsed_ms, "自动增量索引跳过：已有索引任务正在运行");
            }
            Ok(Err(msg)) => {
                let elapsed_ms =
                    u64::try_from(reindex_start.elapsed().as_millis()).unwrap_or(u64::MAX);
                warn!(elapsed_ms, error = %msg, "自动增量索引失败");
            }
            Err(e) => {
                let elapsed_ms =
                    u64::try_from(reindex_start.elapsed().as_millis()).unwrap_or(u64::MAX);
                warn!(elapsed_ms, error = %e, "自动增量索引任务 join 失败");
            }
        }

        // 2026-08-10：FTS reindex tick 之后顺带检查 index.db 是否到期做一次整理——重建
        // `music_fts`/`documents_fts` 两个 FTS5 影子表 + `VACUUM`，真正回收用户删除文件后
        // `prune_deleted`/`purge_under_root` 留下的、`optimize_fts`/`VACUUM` 单独都回收不了
        // 的死数据（见 `scout_indexer::compact_index_if_due` 文档）。**只挂在这个已经在
        // 定期等待的 tick 上，不挂在应用启动那一轮**——启动阶段的 IO/连接窗口已经够挤，不
        // 需要再叠一个独占的 VACUUM。到期判断本身很轻（一次 `schema_meta` 读），真正执行
        // 时才有 VACUUM 的开销，函数内部按 `DEFAULT_COMPACT_INTERVAL_DAYS`（14 天）门控，
        // 默认不会频繁触发。加一层"当前没有索引任务在跑"的守卫，避免和并发的手动 reindex
        // 抢锁——即便撞上，`compact_index_if_due` 失败也只是跳过、下个 tick（30 分钟默认
        // 间隔）会因为还没到期而快速判断跳过，14 天窗口内自然能等到一次空闲 tick 补上。
        let is_indexing = status.lock().unwrap_or_else(|e| e.into_inner()).indexing;
        if !is_indexing {
            let compact_db = local_index_db_path();
            match tauri::async_runtime::spawn_blocking(move || {
                scout_indexer::compact_index_if_due(
                    &compact_db,
                    scout_indexer::DEFAULT_COMPACT_INTERVAL_DAYS,
                )
            })
            .await
            {
                Ok(Ok(true)) => {
                    info!("index.db 定期整理完成（重建 FTS 影子表 + VACUUM，回收删除文件遗留空间）")
                }
                Ok(Ok(false)) => tracing::debug!("index.db 定期整理：未到期或库不存在，跳过"),
                Ok(Err(e)) => warn!(error = %e, "index.db 定期整理失败，留待下次 tick 重试"),
                Err(e) => warn!(error = %e, "index.db 定期整理任务 join 失败"),
            }
        }
    }
}

/// 手动触发本地索引（BETA-04）。BETA-07：经 `perform_reindex` 更新索引状态 + 并发守卫
/// （已在后台索引中则返回提示）。在阻塞线程跑（SQLite + walkdir + lofty）。
#[tauri::command]
async fn reindex(
    app: tauri::AppHandle,
    deps: tauri::State<'_, search::SearchDeps>,
) -> Result<search::ReindexStats, String> {
    let status = deps.index_status_arc();
    let embedding = deps.embedding().clone();
    let db = local_index_db_path();
    // BETA-27：reindex 命令经 AppHandle 取 settings.json 路径，live-read 索引配置（roots + 排除）。
    let settings_path = settings::settings_file_path(&app);
    let fts_status = status.clone();
    let fts_db = db.clone();
    let fts_settings_path = settings_path.clone();
    let out = tauri::async_runtime::spawn_blocking(move || {
        search::perform_reindex(&fts_status, fts_db, fts_settings_path)
    })
    .await;
    match out {
        Ok(Ok(Some(stats))) => {
            // FTS 完成后接语义嵌入后台 worker（解耦，不阻塞命令返回）。
            search::spawn_semantic_index(status, db, embedding, settings_path);
            Ok(stats)
        }
        Ok(Ok(None)) => Err("正在索引中，请稍候".to_owned()),
        Ok(Err(msg)) => Err(msg),
        Err(e) => Err(format!("reindex 任务失败: {e}")),
    }
}

/// BETA-33 cycle 7-c：单目录重扫（RootRow「重扫」按钮）。只替换扫描 roots；
/// exclude_globs / root_excludes / OCR / progress bridge 与全量 reindex 走同一份
/// settings 解析（Codex SUGGEST 6——不给绕过排除配置的旁路留口子）。
#[tauri::command]
async fn reindex_root(
    root: String,
    app: tauri::AppHandle,
    deps: tauri::State<'_, search::SearchDeps>,
) -> Result<search::ReindexStats, String> {
    let root_path = PathBuf::from(&root);
    if !root_path.is_dir() {
        return Err(format!("目录不存在或不可访问：{root}"));
    }
    let status = deps.index_status_arc();
    let embedding = deps.embedding().clone();
    let db = local_index_db_path();
    let settings_path = settings::settings_file_path(&app);
    let fts_status = status.clone();
    let fts_db = db.clone();
    let fts_settings_path = settings_path.clone();
    let out = tauri::async_runtime::spawn_blocking(move || {
        search::perform_reindex_for_roots(
            &fts_status,
            fts_db,
            fts_settings_path,
            Some(vec![root_path]),
        )
    })
    .await;
    match out {
        Ok(Ok(Some(stats))) => {
            // 与全量 reindex 一致：FTS 完成后接语义嵌入后台 worker（默认禁用时立即返回）。
            search::spawn_semantic_index(status, db, embedding, settings_path);
            Ok(stats)
        }
        Ok(Ok(None)) => Err("正在索引中，请稍候".to_owned()),
        Ok(Err(msg)) => Err(msg),
        Err(e) => Err(format!("reindex 任务失败: {e}")),
    }
}

fn main() {
    // Windows llama.cpp 原生故障隔离 helper 必须早于 Tauri/single-instance/tracing 初始化。
    // 普通启动只做一次参数比较；helper 模式在子进程内常驻模型并直接退出本入口。
    scout_model_runtime::run_model_worker_if_requested();

    // PDF 文本层解析故障隔离 helper 必须早于 Tauri/single-instance/tracing 初始化。
    // 普通启动是一次参数比较后立即返回；helper 模式在这里完成单文件解析并退出。
    scout_indexer::run_pdf_extract_worker_if_requested();

    // BETA-31-v3 cycle 2：诊断日志。先初始化 subscriber 再做任何 info!/warn! 调用。
    // _log_guard 必须 bind 变量保活到 main() 结束，保证 drop 时 worker thread flush 残留日志。
    let _log_guard = init_tracing(&log_dir());

    // BETA-31-v3 cycle 4（v0.8.5）：Rust panic hook 写日志。catch_unwind 之外的 panic
    // （未被 spawn_blocking / spawn 兜住的）也能进 scout.log；含 thread / location /
    // message。**注意**：兜不住 native crash（如 llama-cpp ucrtbase 0xc0000409 abort、
    // 由 OS 直接终止进程、绕过 Rust panic 机制）—— 那种崩溃需 Win32 SEH handler 才能截获、
    // 不在本 cycle 范围；本 hook 仅多一层 Rust panic 保险。
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string panic payload>");
        tracing::error!(
            thread = thread.name().unwrap_or("<unnamed>"),
            location = %location,
            message = %msg,
            "Rust panic（catch_unwind 外）"
        );
        // 沿用默认 hook 行为：dev build stderr 也打一份方便 cargo tauri dev 直接看
        eprintln!(
            "[panic] thread={:?} at {}: {}",
            thread.name(),
            location,
            msg
        );
    }));

    // 启动 dump：版本 + OS + 关键路径 + feature 状态。下次诊断「为什么模型没加载 / 索引在哪」
    // 时 grep 这一行就有完整背景。
    info!(
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        data_dir = %scout_data_dir().display(),
        index_db = %local_index_db_path().display(),
        audit_log = %audit_log_path().display(),
        semantic_recall_feature = cfg!(feature = "semantic-recall"),
        model_fallback_feature = cfg!(feature = "model-fallback"),
        embed_pending_enabled = std::env::var("SCOUT_ENABLE_EMBED").ok().as_deref() == Some("1"),
        "Scout 桌面 app 启动"
    );

    let policy = Arc::new(PolicyEngine::new());
    let tracer = build_tracer();
    let context = Arc::new(Mutex::new(ContextMemory::new()));
    // FileActionTool：下方收拢进 SearchDeps，由 search/confirm_action/cancel_action 经 deps 取用
    let file_action_tool = Arc::new(FileActionTool::new(
        Arc::new(LocalFileActionExecutor),
        PolicyEngine::new(),
    ));
    // 待确认文件操作暂存槽：下方收拢进 SearchDeps，confirm_action/cancel_action 经 deps.pending 共享
    let pending_action: Arc<Mutex<Option<FileAction>>> = Arc::new(Mutex::new(None));

    tauri::Builder::default()
        // BETA-33 cycle 9：单实例锁——须注册在**所有插件之前**（官方约定：第二实例检测
        // 要抢在任何初始化前短路）。已有实例在跑时，第二实例不进 setup 直接退出（不会
        // 并发写同一 index.db / settings.json），并把既有主窗口取消最小化 + 带到前台。
        // 2026-07-26：补 `show()`——「关闭到托盘」开着时窗口是 hide() 而非 minimize 状态，
        // 只 unminimize 唤不出来。
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
        }))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        // BETA-27：dialog 插件（设置页索引目录选择器从 JS 侧调 open({directory:true})）。
        .plugin(tauri_plugin_dialog::init())
        .setup(move |app| {
            // 2026-07-29：必须在下面任何读取 settings.json / search_history.json /
            // user-synonyms.yaml / onboarding.json 的逻辑之前跑——把老用户散落在
            // `ai.scout.desktop` 目录的文件一次性搬到 scout_data_dir()，见函数注释。
            migrate_legacy_config_dir(&app.handle().clone());

            // 同义词扩展器：需 AppHandle 确定 resource_dir（打包态）或 fallback 工作区根（开发态）。
            // BETA-11D：返回双层扩展器 + 共享用户词典 Arc（与 UserSynonymState 共用同一把锁）。
            let (synonym_expander, user_index) = build_synonym_expander(&app.handle().clone());
            // BETA-15B-1：进程级单例 embedding 句柄，构造一次后三处共用——
            //   (1) registry 内 SemanticIndexBackend；(2) reindex 文档嵌入 pass；(3) F5 状态命令。
            //   与 ModelFallbackHandle 同款 args（settings.json 路径 + Scout 数据目录）。
            let embedding = Arc::new(search::embedding_model::EmbeddingModelHandle::new(
                settings::settings_file_path(&app.handle().clone()),
                scout_data_dir(),
            ));
            // registry 需 embedding 句柄（注册语义后端）→ 在此构造（原在 main() 体外，已下移）。
            let registry = Arc::new(build_registry(
                embedding.clone(),
                settings::settings_file_path(&app.handle().clone()),
            ));
            // 收拢所有命令共享依赖为单一 managed 状态；BETA-06 注入持久审计日志。
            let audit: Arc<dyn scout_harness::AuditLog> =
                Arc::new(scout_harness::JsonlAuditLog::new(audit_log_path()));
            let deps = search::SearchDeps::new(
                registry,
                policy,
                tracer,
                context,
                file_action_tool,
                pending_action,
                synonym_expander,
            )
            .with_audit(audit)
            // BETA-23：注入模型 fallback 真句柄（settings.json 开关 + 数据目录 models/）。
            .with_model(Arc::new(search::model_fallback::ModelFallbackHandle::new(
                settings::settings_file_path(&app.handle().clone()),
                scout_data_dir(),
            )))
            // BETA-15B-1：注入 embedding 句柄（reindex 文档嵌入 + F5 状态命令经 deps 取用）。
            .with_embedding(embedding.clone())
            // BETA-15B-3 A-2：注入 semantic weight provider（live-read settings.json，模仿 floor_provider）。
            .with_weight_provider({
                let weight_settings_path = settings::settings_file_path(&app.handle().clone());
                std::sync::Arc::new(move || {
                    settings::read_semantic_weight(&weight_settings_path)
                })
            })
            // 2026-07-28：注入 cosine 路由阈值 provider——按当前生效 embedding 模型（真实解析
            // 出的 model_id，见 `EmbeddingModelHandle::model_id`）查已校准阈值表，取代此前内部
            // 写死的 `DEFAULT_COSINE_ROUTING_THRESHOLD`（阈值与模型版本结构化绑定，换模型忘补
            // 校准记录时告警而非静默沿用上一代模型的旧阈值）。闭包捕获同一个 embedding 句柄
            // （与 `.with_embedding` 共享 Arc），不读 settings.json，形态只为跟
            // weight_provider/match_mode_provider 保持一致的 SearchDeps 注入约定。
            .with_threshold_provider({
                let embedding = embedding.clone();
                std::sync::Arc::new(move || embedding.cosine_threshold())
            })
            // BETA-39：注入图片语义 opt-in provider（live-read，段落级 explain 图片分支用）。
            .with_image_semantics_provider({
                let img_settings_path = settings::settings_file_path(&app.handle().clone());
                std::sync::Arc::new(move || {
                    settings::read_enable_image_semantics(&img_settings_path)
                })
            })
            // 2026-07-20：注入复合条件匹配模式 provider（live-read，search_impl/预览高亮共用）。
            .with_match_mode_provider({
                let match_mode_settings_path = settings::settings_file_path(&app.handle().clone());
                std::sync::Arc::new(move || {
                    if settings::read_search_match_all_conditions(&match_mode_settings_path) {
                        scout_search_backend::MatchMode::All
                    } else {
                        scout_search_backend::MatchMode::Any
                    }
                })
            });
            // BETA-25：dev 构建窗口标题加后缀，避免与安装版（同名同 bundle id）在手测时混淆。
            #[cfg(debug_assertions)]
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.set_title("Scout (dev)");
            }

            // 2026-07-26：Windows「关闭到托盘」——托盘图标 + 主窗口 CloseRequested 拦截。
            // 开关来自 settings.json，每次关闭请求时 live-read（用户运行期切换设置无需重启）；
            // 关闭失败（如托盘初始化失败）只告警，不影响正常关窗退出。
            #[cfg(target_os = "windows")]
            {
                if let Err(err) = tray::setup_tray(&app.handle().clone()) {
                    eprintln!("tray: 初始化系统托盘失败，已跳过（不影响搜索主功能）: {err}");
                }
                if let Some(window) = app.get_webview_window("main") {
                    let window_for_close = window.clone();
                    let close_settings_path = settings::settings_file_path(&app.handle().clone());
                    window.on_window_event(move |event| {
                        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                            if settings::read_close_to_tray(&close_settings_path) {
                                api.prevent_close();
                                let _ = window_for_close.hide();
                            }
                        }
                    });
                }
            }

            // BETA-07：克隆索引状态 Arc 给后台启动任务（在 manage 移走 deps 前取）。
            let bg_status = deps.index_status_arc();
            // BETA-15B-1：后台索引也带 embedding 句柄，模型就绪时一并嵌入文档向量。
            let bg_embedding = embedding.clone();
            // BETA-27：后台启动索引也 live-read 索引配置（roots + 排除）。
            let bg_settings_path = settings::settings_file_path(&app.handle().clone());
            // BETA-31-v3：启动时从 SQLite 回填 IndexStatus.last_summary，避免 UI 显示「尚未索引」
            // 误导用户以为索引数据丢失（IndexStatus 是内存对象、不持久化，但 index.db 是持久化的）。
            // compute_index_totals 走 3 个 COUNT 查询、毫秒级；last_indexed 字段不在此回填
            // （无可信时间戳来源、仅在 reindex 完成后写）。先判 db 文件存在、避免 indexer::open 创建空库。
            //
            // 2026-07-26：整块挪进 spawn_blocking，不再直接同步跑在 setup() 里——理由见下方
            // 合并后的启动链注释（原因未变，只是执行位置从这里挪到了链的第一步）。
            app.manage(deps);
            // BETA-11D：注册用户词典 managed 状态，与 LayeredSynonymExpander 共享同一 Arc。
            let user_synonyms_path = user_synonyms::user_synonyms_path(&app.handle().clone())
                .unwrap_or_else(|| std::path::PathBuf::from("user-synonyms.yaml"));
            app.manage(user_synonyms::UserSynonymState::new(
                Arc::clone(&user_index),
                user_synonyms_path,
            ));
            // BETA-53：本机 MCP 服务 managed 状态——复用桌面 embedding 句柄 + index.db 目录，
            // 起停由工具菜单 / 选项页开关驱动。以 Arc 交给 manage，便于自动启动任务共享同一实例。
            let mcp_state = Arc::new(mcp_service::McpServiceState::new(
                embedding.clone(),
                scout_data_dir(),
                settings::settings_file_path(&app.handle().clone()),
                bg_status.clone(),
            ));
            // 上次会话开着 → 本次启动自动拉起（非阻塞，失败仅告警不影响主功能）。
            let mcp_autostart = {
                let enabled = settings::read_settings_or_default(&settings::settings_file_path(
                    &app.handle().clone(),
                ))
                .mcp_service_enabled;
                enabled.then(|| mcp_state.clone())
            };
            app.manage(mcp_state);
            if let Some(svc) = mcp_autostart {
                tauri::async_runtime::spawn(async move {
                    match svc.start().await {
                        Ok(_) => info!("本机 MCP 服务已按上次开关态自动启动"),
                        Err(e) => warn!(error = %e, "本机 MCP 服务自动启动失败（不影响搜索主功能）"),
                    }
                });
            }
            let auto_status = bg_status.clone();
            let auto_embedding = bg_embedding.clone();
            let auto_settings_path = bg_settings_path.clone();
            tauri::async_runtime::spawn(run_auto_index_loop(
                auto_status,
                auto_embedding,
                auto_settings_path,
            ));
            // BETA-07：启动后台自动索引（非阻塞，UI 立即可用）；incremental 后续启动秒级。
            //
            // 2026-08-10：总数回填 + 脏向量清理、FTS reindex、语义嵌入这三步原先各自独立
            // `spawn`/`spawn_blocking`，会在窗口刚显示的头几秒内并发抢同一个 index.db——
            // 各自都要新开连接、可能触发 schema/迁移检查，purge 还带一次写事务，几件事叠在
            // 一起时不光互相排队等 SQLite 写锁，还会把磁盘 IO 带宽一次性占满，放大成用户
            // 感知到的"页面短时挂死"。改成一条链：先做总数回填 + 脏向量清理（读多写少，
            // 借助 indexer 侧新加的 schema 验证缓存 + purge 检查点，多数启动这步几乎是
            // 空转），**等它完全让出连接后**再开始 FTS reindex，reindex 完成后再接语义嵌入。
            // 仍然整体在后台跑、不阻塞 setup()；只是把"并发抢跑"改成"排队执行"。
            tauri::async_runtime::spawn(async move {
                let db = local_index_db_path();

                // 第一步：总数回填（UI last_summary/db_totals）+ 清理脏向量。
                let totals_db = db.clone();
                let totals_status = bg_status.clone();
                let totals_settings_path = bg_settings_path.clone();
                let _ = tauri::async_runtime::spawn_blocking(move || {
                    if !totals_db.exists() {
                        return;
                    }
                    if let Some((music, doc, image)) = search::compute_index_totals(&totals_db) {
                        if music > 0 || doc > 0 || image > 0 {
                            let mut s = totals_status.lock().unwrap_or_else(|e| e.into_inner());
                            s.last_summary =
                                Some(format!("音频 {music} / 文档 {doc} / 图片 {image}"));
                            // cycle 9：结构化全库总数同步回填（与 last_summary 同源同步）。
                            s.db_totals = Some((music, doc, image));
                        }
                    }

                    // BETA-31-v3 cycle 3（v0.8.4）：一次性清理 document_vectors 中
                    // 关联到 body 极短文档的旧脏向量。Why：BETA-15B-1 以来 embed_pending
                    // 对 documents 表所有条目一视同仁、Windows OCR 跳过的图片 body 为空 →
                    // 嵌入产出 "neutral" 高 cosine 向量、占满 ranker top-N、用户搜任何词
                    // 都返缓存图片。本 cycle indexer 加 is_embed_worthy 守门防新污染、
                    // 但旧脏向量必须显式 DELETE（vector_is_current 不重嵌、source_hash 没变）。
                    // BETA-39：keep_worthy_images 依「图片语义索引」opt-in 动态判——
                    // 关（默认）清全部图片向量（现状 + 开过再关自动回收）；开只清不过 0.75 门槛的。
                    let keep_worthy_images =
                        settings::read_enable_image_semantics(&totals_settings_path);
                    match scout_indexer::DocumentIndex::open(&totals_db) {
                        Ok(idx) => match idx.purge_short_body_vectors(keep_worthy_images) {
                            Ok(0) => {
                                info!("启动清理脏向量：0 条需删（已清理过或本来就干净）");
                            }
                            Ok(n) => {
                                info!(
                                    purged = n,
                                    "启动清理脏向量：删除 N 条 body 极短文档关联的旧污染向量（BETA-31-v3 cycle 3 fix）"
                                );
                            }
                            Err(e) => {
                                warn!(error = %e, "启动清理脏向量失败、跳过（不阻塞启动）");
                            }
                        },
                        Err(e) => {
                            warn!(error = %e, "启动清理脏向量：DocumentIndex::open 失败、跳过");
                        }
                    }
                })
                .await;

                // 第二步：FTS reindex（总数回填/脏向量清理已让出连接，此时才开始写）。
                info!("启动后台 FTS reindex（spawn_blocking）");
                let reindex_start = std::time::Instant::now();
                let fts_db = db.clone();
                let fts_status = bg_status.clone();
                let fts_settings_path = bg_settings_path.clone();
                match tauri::async_runtime::spawn_blocking(move || {
                    search::perform_reindex(&fts_status, fts_db, fts_settings_path)
                })
                .await
                {
                    Ok(Ok(stats_opt)) => {
                        let elapsed_ms = u64::try_from(reindex_start.elapsed().as_millis())
                            .unwrap_or(u64::MAX);
                        match &stats_opt {
                            Some(s) => info!(
                                elapsed_ms,
                                music_added = s.music_added,
                                music_updated = s.music_updated,
                                doc_added = s.doc_added,
                                doc_updated = s.doc_updated,
                                image_added = s.image_added,
                                image_updated = s.image_updated,
                                "后台 FTS reindex 完成"
                            ),
                            None => info!(elapsed_ms, "后台 FTS reindex 跳过（已在索引中）"),
                        }
                        // 第三步：FTS 完成后接语义嵌入后台 worker（模型就绪才实跑）。
                        search::spawn_semantic_index(bg_status, db, bg_embedding, bg_settings_path);
                    }
                    Ok(Err(msg)) => {
                        eprintln!("后台索引失败: {msg}");
                        warn!(error = %msg, "后台 FTS reindex 失败");
                    }
                    Err(e) => {
                        eprintln!("后台索引任务失败: {e}");
                        warn!(error = %e, "后台 FTS reindex spawn_blocking join 失败");
                    }
                }
            });
            // 自动更新：默认每 4 小时轮询 GitHub Releases（设置 → 常规可调 30 分钟~24
            // 小时/关闭），发现新版本经 `update://available` event 通知前端左下角提醒；
            // 前端挂载时也会主动即时查一次兜底，避免后台首检的 30 秒延迟 + 事件丢失窗口
            // 让真机测试"看起来没反应"（2026-08-10 真机反馈修复，见 update.rs 顶部注释：
            // 轻量自研，不接 tauri-plugin-updater，不需要签名密钥/改动 release workflow）。
            tauri::async_runtime::spawn(update::run_update_check_loop(
                app.handle().clone(),
                settings::settings_file_path(&app.handle().clone()),
            ));

            // 全局快捷键是锦上添花：注册失败（如默认 Ctrl+Space 被其他程序占用）只告警，
            // 绝不让整个 app 崩溃——否则用户表现为「双击没反应 / 闪退」。用户可在「常规」
            // 设置里通过 shortcut::update_global_shortcut 改用其他组合避开冲突。
            if let Err(err) = shortcut::register_global_shortcut(&app.handle().clone()) {
                eprintln!("global shortcut: 注册失败，已跳过（不影响搜索主功能）: {err}");
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            reindex,
            reindex_root,
            search::get_index_status,
            search::get_model_status,
            search::embedding_model_status,
            search::probe_model_file,
            search::search,
            search::search_with_adhoc_synonyms,
            search::search_with_intent,
            search::preview_intent,
            search::confirm_action,
            search::cancel_action,
            search::get_audit_log,
            search::clear_audit_log,
            search::open_path,
            search::locate_path,
            search::get_preview,
            search::explain_semantic_hit,
            status::get_backend_status,
            settings::get_settings,
            settings::update_settings,
            settings::get_effective_index_roots,
            settings::get_index_overview,
            settings::purge_root_from_db,
            settings::get_extraction_failures,
            shortcut::update_global_shortcut,
            privacy::get_privacy_overview,
            privacy::clear_local_index,
            uninstall::uninstall_cleanup,
            history::record_search,
            history::get_search_history,
            history::clear_search_history,
            history::save_search,
            history::delete_saved_search,
            permissions::check_macos_full_disk_access,
            permissions::open_macos_fda_settings,
            permissions::check_windows_search_indexed,
            permissions::open_windows_indexing_options,
            permissions::check_everything_available,
            permissions::check_pdftoppm_available,
            permissions::get_onboarding_state,
            permissions::complete_onboarding,
            model_download::download_embedding_model,
            model_download::cancel_embedding_download,
            model_download::download_generation_model,
            model_download::cancel_generation_download,
            model_download::discover_local_model,
            model_download::discover_gguf_models,
            model_download::import_local_model,
            model_download::model_download_in_flight,
            user_synonyms::get_user_synonyms,
            user_synonyms::add_user_synonym,
            user_synonyms::update_user_synonym,
            user_synonyms::delete_user_synonym,
            user_synonyms::export_user_synonyms,
            user_synonyms::import_user_synonyms,
            mcp_service::start_mcp_service,
            mcp_service::stop_mcp_service,
            mcp_service::mcp_service_status,
            mcp_service::reset_mcp_token,
            update::check_for_updates,
            update::install_update,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use scout_harness::ImplementationStatus;
    // CapabilityDiscovery / ToolKind 仅被 macOS 的 build_registry 测试使用；
    // 在 Windows 上该测试被 cfg 编译掉，故按平台 gate 避免 unused_imports。
    #[cfg(target_os = "macos")]
    use scout_harness::{CapabilityDiscovery, ToolKind};
    use std::sync::{Mutex, OnceLock};

    // env 变量是进程级状态; 串行化所有读写 SCOUT_TRACE 的测试
    static TRACER_ENV_MUTEX_INNER: OnceLock<Mutex<()>> = OnceLock::new();
    #[allow(non_snake_case)]
    fn TRACER_ENV_MUTEX() -> &'static Mutex<()> {
        TRACER_ENV_MUTEX_INNER.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn auto_index_tick_decision_handles_disabled_and_intervals() {
        assert_eq!(auto_index_tick_decision(0), AutoIndexTickDecision::Disabled);
        assert_eq!(
            auto_index_tick_decision(15),
            AutoIndexTickDecision::RunAfter(std::time::Duration::from_secs(15 * 60))
        );
        assert_eq!(
            auto_index_tick_decision(settings::DEFAULT_AUTO_INDEX_INTERVAL_MINUTES),
            AutoIndexTickDecision::RunAfter(std::time::Duration::from_secs(30 * 60))
        );
    }

    /// 测试结束时自动 unset SCOUT_TRACE + 可选删除 tmpfile,即便 panic 也保证清理。
    struct TraceTestEnvGuard {
        path: Option<std::path::PathBuf>,
    }

    impl TraceTestEnvGuard {
        fn with_path(path: std::path::PathBuf) -> Self {
            Self { path: Some(path) }
        }
        fn without_path() -> Self {
            Self { path: None }
        }
    }

    impl Drop for TraceTestEnvGuard {
        fn drop(&mut self) {
            // SAFETY: 测试串行化由 TRACER_ENV_MUTEX 保证;Drop 在 lock 仍持有时运行
            unsafe { std::env::remove_var("SCOUT_TRACE") };
            if let Some(p) = &self.path {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn build_registry_exposes_real_spotlight_on_macos() {
        let embedding = Arc::new(search::embedding_model::EmbeddingModelHandle::new(
            None,
            PathBuf::from("."),
        ));
        let registry = build_registry(embedding, None);

        // 通用 tools 表
        let tool = registry
            .find_by_id("search.spotlight")
            .expect("macOS 构建应注册 search.spotlight");
        assert_eq!(tool.kind(), ToolKind::Search);
        assert_eq!(tool.implementation_status(), ImplementationStatus::Real);
        assert!(
            tool.capability()
                .supported_intents
                .contains(&SupportedIntent::FileSearch),
            "SpotlightBackend 应支持 FileSearch"
        );

        // search-typed 表也应找到（验证 register_search 双入表）
        let search_tool = registry
            .find_search_tool("search.spotlight")
            .expect("register_search 应同时填充 searchable 表");
        assert_eq!(search_tool.id(), "search.spotlight");

        // available_search_tools_supporting 也应包含
        let available = registry.available_search_tools_supporting(SupportedIntent::FileSearch);
        assert!(
            available.iter().any(|t| t.id() == "search.spotlight"),
            "search.spotlight 应在 FileSearch available 列表中"
        );

        // 兼容现有 CapabilityDiscovery
        let summaries = CapabilityDiscovery::new(&registry).backend_summary();
        assert!(
            summaries.iter().any(|s| s.id == "search.spotlight"),
            "StatusIndicator 应能看到 search.spotlight"
        );
    }

    /// MVP-26 自动切换 registry 真机实测：生产 `build_registry()` 在 Windows 真机上
    /// 同时注册 WindowsSearch（内容型）+ Everything（文件名型）两个**真实可用**后端，
    /// 能力感知路由（`IntentRouter::route_search`）应据 intent 自动切换：
    ///   - 内容/关键词查询 → `search.windows`（即使 Everything 在 id 序更靠前）
    ///   - 纯扩展名查询 → `search.everything`（id 序首位，更快）
    /// 并执行各自路由到的后端、确认对同一合成语料返回预期文件——把单测里的 fake 路由
    /// 升级为真后端 + 真 es.exe / Windows Search 的端到端切换证据。
    ///
    /// 前置同 `mvp26_corpus_consistency`：生成语料并 `set SCOUT_MVP26_CORPUS=<dir>`。
    #[cfg(target_os = "windows")]
    #[tokio::test]
    #[ignore = "requires Windows 真机 with Everything + Windows Search + indexed corpus; set SCOUT_MVP26_CORPUS"]
    async fn registry_auto_switches_between_content_and_filename_backends() {
        use futures::StreamExt;
        use scout_harness::IntentRouter;
        use scout_search_backend::{CancellationToken, SearchIntent};

        let corpus = std::env::var("SCOUT_MVP26_CORPUS")
            .expect("set SCOUT_MVP26_CORPUS to the generated+indexed corpus dir");

        let embedding = std::sync::Arc::new(search::embedding_model::EmbeddingModelHandle::new(
            None,
            PathBuf::from("."),
        ));
        let registry = build_registry(embedding, None);
        // 两个真实后端都必须真机可用，否则路由切换无从谈起——premise 失败应大声报错。
        for id in ["search.windows", "search.everything"] {
            let tool = registry
                .find_search_tool(id)
                .unwrap_or_else(|| panic!("Windows 构建应注册 {id}"));
            assert_eq!(tool.implementation_status(), ImplementationStatus::Real);
            assert!(tool.is_available(), "{id} 应在真机上可用");
        }

        let router = IntentRouter::new(&registry);

        // 构造一个注入了 location.include = corpus 的 intent。
        let scoped = |mut intent: serde_json::Value| -> SearchIntent {
            intent["location"] = serde_json::json!({ "include": [corpus.clone()] });
            serde_json::from_value(intent).expect("intent")
        };
        let run = |tool: std::sync::Arc<dyn scout_harness::SearchableTool>,
                   intent: SearchIntent| async move {
            let stream = tool
                .search(&intent, CancellationToken::new())
                .await
                .expect("search ok");
            stream
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
                .expect("collect results")
        };

        // 1) 内容/关键词查询 → 自动切到内容型 WindowsSearch。
        let content_intent = scoped(serde_json::json!({
            "schema_version":"1.0","intent":"file_search","keywords":["预算"]
        }));
        let content_tool = router.route_search(&content_intent).expect("route content");
        assert_eq!(
            content_tool.id(),
            "search.windows",
            "关键词查询应路由到内容型 WindowsSearch"
        );
        let content_results = run(content_tool, content_intent).await;
        assert!(
            content_results
                .iter()
                .any(|r| r.name == "合成-预算-2026.docx"),
            "WindowsSearch 关键词 预算 应命中合成文件: {:?}",
            content_results
                .iter()
                .map(|r| r.name.clone())
                .collect::<Vec<_>>()
        );

        // 2) 纯扩展名查询 → 沿用 id 序首位的 Everything（文件名引擎，更快）。
        let ext_intent = scoped(serde_json::json!({
            "schema_version":"1.0","intent":"file_search","extensions":["pdf"]
        }));
        let ext_tool = router.route_search(&ext_intent).expect("route ext");
        assert_eq!(
            ext_tool.id(),
            "search.everything",
            "纯扩展名查询应路由到文件名型 Everything"
        );
        let ext_results = run(ext_tool, ext_intent).await;
        assert!(
            ext_results
                .iter()
                .any(|r| r.name == "synthetic-received-last-week.pdf"),
            "Everything 扩展名 pdf 应命中合成文件: {:?}",
            ext_results
                .iter()
                .map(|r| r.name.clone())
                .collect::<Vec<_>>()
        );
    }

    /// fallback chain 真双后端集成验证（Windows-only 缺口闭合，spec §6.3 交接项）。
    ///
    /// 场景=「WindowsSearch 漏 → Everything 兜底」的真实价值：探针文件放在
    /// `%TEMP%`（`AppData\Local\Temp`，Windows Search 默认**不索引**）。WindowsSearch
    /// 对该 scope 干净返回 0 → `SwitchReason::Empty` 切到 Everything（扫 NTFS MFT，不依赖
    /// 系统索引）命中。直接驱动生产 [`run_fallback_chain`](scout_harness::run_fallback_chain)
    /// + 两个**真实** backend，断言：① 切换事件 from=windows/to=everything/reason=empty；
    /// ② `served_by`=everything（telemetry 归属正确，spec 交接 (c)）；③ 去重累积命中探针文件。
    ///
    /// 与 mock 单测（harness `fallback_chain::tests`）的区别：那里验证编排逻辑，这里验证
    /// 真 es.exe + 真 Windows Search 在真机上的端到端回退（macOS 仅 Spotlight 单候选无法触发）。
    ///
    /// spike 实证（2026-06-02）：`%TEMP%` 子目录的新文件 es.exe 秒级命中、Windows Search
    /// scoped 查询返回 0，强制场景确定性成立。
    #[cfg(target_os = "windows")]
    #[tokio::test]
    #[ignore = "requires Windows 真机 with Everything (es.exe) + Windows Search running"]
    #[allow(clippy::print_stderr)]
    async fn fallback_chain_windows_search_misses_then_everything_serves() {
        use scout_harness::{
            run_fallback_chain, BackendSwitch, SearchTool, SearchableTool, SupportedIntent,
            SwitchReason,
        };
        use scout_search_backend::{
            CancellationToken, ExpandedSearchIntent, SearchBackend, SearchIntent, SearchResult,
        };
        use std::sync::Arc;

        // 1) 探针文件放进 %TEMP%（Windows Search 默认不索引），唯一名供 Everything 按名命中。
        let marker = format!("lociprobe{}", std::process::id());
        let dir = std::env::temp_dir().join(format!("scout-fallback-{marker}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let file = dir.join(format!("{marker}.txt"));
        std::fs::write(&file, b"probe").expect("write probe file");
        // 给 Everything 索引新文件留时间（MFT 近实时；与 real_everything 同款 2s）。
        std::thread::sleep(std::time::Duration::from_secs(2));

        // 2) 构造两个真实 backend，按 [windows, everything] 顺序入链（强制 windows 先行）。
        let windows = WindowsSearchBackend::new().expect("construct WindowsSearchBackend");
        let everything = EverythingBackend::new().expect("construct EverythingBackend");
        assert!(
            everything.is_available(),
            "es.exe 应在 PATH 上（本测试前置）"
        );
        let win_tool: Arc<dyn SearchableTool> = Arc::new(SearchTool::new(
            "search.windows",
            "Windows Search",
            windows,
            vec![SupportedIntent::FileSearch],
            "Windows Search 系统索引",
        ));
        let es_tool: Arc<dyn SearchableTool> = Arc::new(SearchTool::new(
            "search.everything",
            "Everything",
            everything,
            vec![SupportedIntent::FileSearch],
            "Everything es.exe",
        ));
        let candidates = vec![win_tool, es_tool];

        // 3) 按文件名 keyword 查询，scope 限定到探针目录。
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search",
            "keywords": [marker],
            "location": { "include": [dir.to_string_lossy()] }
        }))
        .expect("intent");
        let expanded = ExpandedSearchIntent::identity(intent);

        // 4) 驱动生产 fallback chain，收集结果与切换事件。
        let mut results: Vec<SearchResult> = Vec::new();
        let mut switches: Vec<BackendSwitch> = Vec::new();
        let outcome = run_fallback_chain(
            &candidates,
            &expanded,
            CancellationToken::new(),
            &mut |r| results.push(r),
            &mut |s| switches.push(s),
        )
        .await;

        eprintln!(
            "fallback chain: total={} served_by={:?} switches={:?} names={:?}",
            outcome.total,
            outcome.served_by,
            switches
                .iter()
                .map(|s| format!("{}→{}({})", s.from, s.to, s.reason.as_str()))
                .collect::<Vec<_>>(),
            results.iter().map(|r| r.name.clone()).collect::<Vec<_>>(),
        );

        // 5) 断言真回退发生。
        // ① WindowsSearch 干净漏 → 恰一次切换 windows→everything，原因 Empty。
        assert_eq!(switches.len(), 1, "应恰好发生一次后端切换");
        assert_eq!(switches[0].from, "search.windows");
        assert_eq!(switches[0].to, "search.everything");
        assert_eq!(
            switches[0].reason,
            SwitchReason::Empty,
            "Windows Search 对未索引 scope 应干净返回 0 → Empty 切换"
        );
        // ② telemetry 归属（spec 交接 (c)）：实际服务者是 Everything。
        assert_eq!(
            outcome.served_by.as_deref(),
            Some("search.everything"),
            "served_by 应归属实际产出结果的 Everything"
        );
        // ③ 去重累积命中探针文件。
        assert!(outcome.total >= 1, "应至少命中探针文件");
        assert!(
            results.iter().any(|r| r.name.contains(&marker)),
            "结果应含探针文件 {marker}.txt"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// fan-out 文件名兜底真双后端集成验证（Windows-only，闭合「内容查询漏非索引位置文件」缺口）。
    ///
    /// 生产 wiring 里内容查询走 fan-out（仅 content-capable：本地索引 + WindowsSearch），**不含
    /// Everything**。文件在系统索引/本地索引未覆盖的位置、但文件名含关键词时会漏。本测试验证
    /// [`run_fanout_merge_with_fallback`](scout_harness::run_fanout_merge_with_fallback)：
    /// 内容轮（WindowsSearch 对 `%TEMP%` 未索引 scope）干净零结果 → 触发 `on_fallback` → 对
    /// 纯文件名后端 Everything 补一轮、按文件名命中探针文件。
    ///
    /// 与 fallback chain 测试的区别：那条验证**纯文件名查询**的链式回退；这条验证**内容查询**
    /// 的 fan-out 文件名兜底（两条是不同路由路径）。
    #[cfg(target_os = "windows")]
    #[tokio::test]
    #[ignore = "requires Windows 真机 with Everything (es.exe) + Windows Search running"]
    #[allow(clippy::print_stderr)]
    async fn fanout_filename_fallback_when_content_misses() {
        use scout_harness::{
            run_fanout_merge_with_fallback, SearchTool, SearchableTool, SupportedIntent,
        };
        use scout_search_backend::{
            CancellationToken, ExpandedSearchIntent, SearchBackend, SearchIntent,
        };
        use std::sync::Arc;

        // 探针文件名含唯一关键词，放 %TEMP%（WindowsSearch 默认不索引）。
        let marker = format!("locifanout{}", std::process::id());
        let dir = std::env::temp_dir().join(format!("scout-fanout-{marker}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let file = dir.join(format!("{marker}.txt"));
        std::fs::write(&file, b"probe").expect("write probe file");
        std::thread::sleep(std::time::Duration::from_secs(2));

        // 内容后端 = WindowsSearch（系统索引）；文件名兜底 = Everything（MFT）。
        let windows = WindowsSearchBackend::new().expect("construct WindowsSearchBackend");
        let everything = EverythingBackend::new().expect("construct EverythingBackend");
        assert!(everything.is_available(), "es.exe 应在 PATH（本测试前置）");
        let content: Vec<Arc<dyn SearchableTool>> = vec![Arc::new(SearchTool::new(
            "search.windows",
            "Windows Search",
            windows,
            vec![SupportedIntent::FileSearch],
            "Windows Search 系统索引",
        ))];
        let fallback: Vec<Arc<dyn SearchableTool>> = vec![Arc::new(SearchTool::new(
            "search.everything",
            "Everything",
            everything,
            vec![SupportedIntent::FileSearch],
            "Everything es.exe",
        ))];

        // 内容查询（keyword）scope 到探针目录。
        let intent: SearchIntent = serde_json::from_value(serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_search",
            "keywords": [marker],
            "location": { "include": [dir.to_string_lossy()] }
        }))
        .expect("intent");
        let expanded = ExpandedSearchIntent::identity(intent);

        let mut got: Vec<scout_harness::MergedResult> = Vec::new();
        let mut fallback_used = false;
        let outcome = run_fanout_merge_with_fallback(
            &content,
            &fallback,
            &expanded,
            CancellationToken::new(),
            &mut |m| got.push(m),
            &mut || fallback_used = true,
        )
        .await;

        eprintln!(
            "fanout fallback: total={} fallback_used={} names={:?}",
            outcome.total,
            fallback_used,
            got.iter()
                .map(|m| m.result.name.clone())
                .collect::<Vec<_>>(),
        );

        assert!(
            fallback_used,
            "WindowsSearch 对未索引 %TEMP% scope 应零结果 → 触发文件名兜底"
        );
        assert!(outcome.total >= 1, "Everything 应按文件名命中探针文件");
        assert!(
            got.iter().any(|m| m.result.name.contains(&marker)),
            "结果应含探针文件 {marker}.txt"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_tracer_default_is_noop() {
        // 防止其他 test set 了 SCOUT_TRACE
        let _guard = TRACER_ENV_MUTEX().lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: 测试串行化由 TRACER_ENV_MUTEX 保证
        unsafe { std::env::remove_var("SCOUT_TRACE") };
        let tracer = build_tracer();
        let debug = format!("{tracer:?}");
        assert!(
            debug.contains("hook_count: 0"),
            "默认应 0 hook, 实得: {debug}"
        );
    }

    #[test]
    fn build_tracer_with_valid_env_attaches_jsonlines() {
        let _guard = TRACER_ENV_MUTEX().lock().unwrap_or_else(|e| e.into_inner());
        let tmpdir = std::env::temp_dir();
        let path = tmpdir.join(format!("scout-trace-test-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        // SAFETY: 测试串行化由 TRACER_ENV_MUTEX 保证
        unsafe { std::env::set_var("SCOUT_TRACE", &path) };
        let _cleanup = TraceTestEnvGuard::with_path(path.clone());

        let tracer = build_tracer();
        let debug = format!("{tracer:?}");
        assert!(
            debug.contains("hook_count: 1"),
            "valid path 应 1 hook, 实得: {debug}"
        );
        assert!(
            path.exists(),
            "build_tracer 应已创建文件 {}",
            path.display()
        );

        use scout_harness::{SupportedIntent, ToolCallEvent, ToolKind};
        tracer.on_tool_call(&ToolCallEvent {
            tool_id: "test.tool".into(),
            tool_kind: ToolKind::Search,
            intent_variant: SupportedIntent::FileSearch,
        });
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("test.tool"),
            "JSONL 应含 tool_id, 实得: {content}"
        );
    }

    #[test]
    fn build_tracer_with_invalid_path_falls_back() {
        let _guard = TRACER_ENV_MUTEX().lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: 测试串行化由 TRACER_ENV_MUTEX 保证
        unsafe { std::env::set_var("SCOUT_TRACE", "/dev/null/不可创建/x.jsonl") };
        let _cleanup = TraceTestEnvGuard::without_path();

        let tracer = build_tracer();
        let debug = format!("{tracer:?}");
        assert!(
            debug.contains("hook_count: 0"),
            "invalid path 应 fallback 0 hook, 实得: {debug}"
        );
    }

    /// 老目录四个文件齐全 → 全部搬到新目录，老目录清空（rename 而非 copy）。
    #[test]
    fn migrate_legacy_config_dir_moves_all_known_files() {
        let tmp = tempfile::tempdir().unwrap();
        let old_dir = tmp.path().join("ai.scout.desktop");
        let new_dir = tmp.path().join("Scout");
        std::fs::create_dir_all(&old_dir).unwrap();
        for name in [
            "settings.json",
            "search_history.json",
            "user-synonyms.yaml",
            "onboarding.json",
        ] {
            std::fs::write(old_dir.join(name), b"legacy").unwrap();
        }

        migrate_legacy_config_dir_impl(&old_dir, &new_dir);

        for name in [
            "settings.json",
            "search_history.json",
            "user-synonyms.yaml",
            "onboarding.json",
        ] {
            assert!(
                new_dir.join(name).exists(),
                "{name} 应已搬到新目录 {new_dir:?}"
            );
            assert!(
                !old_dir.join(name).exists(),
                "{name} 搬迁后不应仍留在老目录（rename 而非 copy）"
            );
            assert_eq!(std::fs::read(new_dir.join(name)).unwrap(), b"legacy");
        }
    }

    /// 新目录已有同名文件（用户已用过新版本）→ 不覆盖，老目录原样保留。
    #[test]
    fn migrate_legacy_config_dir_does_not_overwrite_existing_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        let old_dir = tmp.path().join("ai.scout.desktop");
        let new_dir = tmp.path().join("Scout");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::create_dir_all(&new_dir).unwrap();
        std::fs::write(old_dir.join("settings.json"), b"legacy").unwrap();
        std::fs::write(new_dir.join("settings.json"), b"already-current").unwrap();

        migrate_legacy_config_dir_impl(&old_dir, &new_dir);

        assert_eq!(
            std::fs::read(new_dir.join("settings.json")).unwrap(),
            b"already-current",
            "新目录已有文件不应被老目录内容覆盖"
        );
        assert!(
            old_dir.join("settings.json").exists(),
            "跳过迁移时老文件应原样保留（不误删用户数据）"
        );
    }

    /// 老目录整个不存在（全新安装，从未产生过 `ai.scout.desktop`）→ 直接 no-op，不报错、不建目录。
    #[test]
    fn migrate_legacy_config_dir_noop_when_old_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let old_dir = tmp.path().join("ai.scout.desktop");
        let new_dir = tmp.path().join("Scout");

        migrate_legacy_config_dir_impl(&old_dir, &new_dir);

        assert!(!new_dir.exists(), "全新安装不应凭空建出 Scout 数据目录");
    }
}
