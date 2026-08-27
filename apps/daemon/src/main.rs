//! scoutd binary 入口。
//!
//! BETA-32 T10 接全套：CLI parse → tracing init → preflight fail-fast →
//! `ServerCtx` 构造（打开 indexer DB + 加载 embedder + 首次全量索引）→
//! axum Router 装配 → [`lifecycle::serve`] 阻塞直到信号。
//!
//! BETA-36：启动形态二选一——legacy 单根（`--root` + `--token` 合成 default
//! collection + 全权 admin token）或 collection 模式（`--config <TOML>`）；
//! per-collection 独立 index.db（物理信息墙），布局见
//! `scout_server::config::collection_db_path`。
//!
//! 显式 allow `print_stdout` / `print_stderr`：daemon binary 启动/收尾阶段
//! 必须直接写 stdout/stderr（在 tracing subscriber 初始化前后）。同款做法
//! 见 apps/scout-cli。

#![forbid(unsafe_code)]
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod cli;
mod lifecycle;
mod personal;
mod preflight;
mod service;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use parking_lot::{Mutex, RwLock};
use secrecy::{ExposeSecret as _, SecretString};
use tracing::{info, level_filters::LevelFilter, warn};
use tracing_subscriber::EnvFilter;

use scout_indexer::embed::TextEmbedder;
use scout_indexer::{
    default_ocr_engine, DocumentIndex, GlobSet, IndexError, IndexStats, MusicIndex, NoopProgress,
    OcrEngine, PopplerPdfRasterizer,
};
use scout_model_runtime::{ModelDaemon, ModelLoadParams};
use scout_server::app::build_app;
use scout_server::collections::{parse_config_toml, DaemonConfigFile};
use scout_server::config::{
    collection_db_path, CollectionRuntime, CollectionState, IndexingProbe, ServerConfig,
};
use scout_server::ServerCtx;

use crate::cli::{Cli, Command};

fn main() -> Result<()> {
    // Windows llama.cpp 原生故障隔离 helper；必须在 clap 解析前截获内部参数。
    scout_model_runtime::run_model_worker_if_requested();

    // 与桌面端共用 PDF 提取子进程隔离协议；必须在 clap 解析前截获内部 helper 参数。
    scout_indexer::run_pdf_extract_worker_if_requested();

    let cli = Cli::parse();

    // BETA-78：`service` 子命令必须**同步**调用 `service_dispatcher::start`——
    // SCM 在拉起进程后只给很短的窗口接收控制权，这一步不能等 tokio runtime
    // 起好、更不能等任何异步逻辑跑完。其余子命令 / 今天的前台模式都在各自
    // 分支里按需建 runtime。
    //
    // 日志目的地在这里分叉：service 模式下 stdout/stderr 没有任何东西在另一端
    // 读——SCM 拉起的进程没有 console，Windows 也不会把它们收进事件日志。此前
    // `service` 走的是和其它子命令一样的 stdout tracing，导致真实故障（如个人
    // 模式首启下载 embedding 模型失败）虽然被 `error!` 记录了，这条记录却写进了
    // 空气——这正是"服务启动失败、Windows 事件里却查不到具体原因"的根因之一。
    // service 模式改写文件（daily 滚动，`<data_dir>\scoutd.log`），并装一个 panic
    // hook 兜底把 panic 也落盘。
    if let Some(Command::Service { data_dir }) = &cli.command {
        let data_dir = data_dir.clone().unwrap_or_else(personal::default_data_dir);
        let _guard = init_tracing_to_file(&data_dir, &cli.log_level);
        install_panic_log_hook();
        return service::run_dispatcher(data_dir);
    }

    init_tracing(&cli.log_level, &cli.log_format)?;

    match cli.command.clone() {
        Some(Command::BootstrapPersonalConfig { data_dir, roots }) => {
            let data_dir = data_dir.unwrap_or_else(personal::default_data_dir);
            let created = personal::bootstrap_config(&data_dir, &roots)?;
            if created {
                info!(path = %data_dir.join("config.toml").display(), "个人模式配置已生成");
            } else {
                info!(path = %data_dir.join("config.toml").display(), "个人模式配置已存在，跳过");
            }
            Ok(())
        }
        Some(Command::InstallService { data_dir }) => {
            let data_dir = data_dir.unwrap_or_else(personal::default_data_dir);
            service::install_service(&data_dir)
        }
        Some(Command::UninstallService) => service::uninstall_service(),
        Some(Command::Service { .. }) => unreachable!("已在上面提前处理并 return"),
        None => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("创建 tokio runtime 失败")?;
            runtime.block_on(run_foreground(cli))
        }
    }
}

/// 今天的前台 serve 路径（团队/企业部署：`--root`+`--token` 或 `--config`），
/// 行为与 BETA-78 之前完全一致——现有部署零迁移。
async fn run_foreground(cli: Cli) -> Result<()> {
    let data_dir = cli
        .data_dir
        .clone()
        .ok_or_else(|| anyhow!("前台模式必须给 --data-dir"))?;
    let model_path = cli
        .model_path
        .clone()
        .ok_or_else(|| anyhow!("前台模式必须给 --model-path"))?;

    // ---- BETA-36：解析访问模型（--config TOML 与 --root/--token 互斥）----
    let access = resolve_access_config(&cli)?;

    // ---- preflight fail-fast ----
    for c in &access.collections {
        for root in &c.roots {
            preflight::check_root(root)
                .with_context(|| format!("collection {} 的 root 检查失败", c.id))?;
        }
    }
    preflight::check_data_dir(&data_dir)?;
    preflight::check_model(&model_path)?;
    // reindex 中断残留按 collection db 目录逐一检查（legacy default = data_dir 平铺）。
    for c in &access.collections {
        let db = collection_db_path(&data_dir, &c.id);
        if let Some(dir) = db.parent() {
            if dir.exists() {
                preflight::check_rebuild_leftover(dir, cli.allow_rebuild_schema)?;
            }
        }
    }
    // bind 端口检查：留 lifecycle::serve 真 bind 时报错（TOCTOU 风险下不重复 try）。

    let log_level = parse_log_level(&cli.log_level);

    // 2026-07-28：cosine 路由阈值按当前生效 embedding 模型查已校准表——不需要真加载模型，
    // `derive_model_id` 纯从文件名解析、和 `load_embedder` 内部派生 model_id 用同一逻辑
    // （保证 ServerConfig 里存的阈值与 embedder 实际加载后 `model_id()` 对应的是同一条
    // 校准记录）。未收录模型回落 DEFAULT_COSINE_ROUTING_THRESHOLD 并告警——阈值与模型版本
    // 结构化绑定，取代此前 harness 内部写死的全局常量。
    let model_id_for_threshold = derive_model_id(&model_path);
    let cosine_threshold =
        scout_result_normalizer::cosine_threshold_for_model(&model_id_for_threshold)
            .unwrap_or_else(|| {
                warn!(
                    model_id = %model_id_for_threshold,
                    fallback = scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD,
                    "当前 embedding 模型未收录 cosine 路由阈值校准记录，回落默认值——语义/FTS 路由\
                     可能非最优，需要给该模型补一条 CALIBRATED_COSINE_THRESHOLDS 记录"
                );
                scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD
            });

    let config = ServerConfig {
        bind_addr: cli.bind,
        data_dir,
        model_path,
        log_level,
        semantic_weight: cli
            .semantic_weight
            .unwrap_or(scout_server::tools::search::DEFAULT_SEMANTIC_WEIGHT),
        cosine_threshold,
        embed_images: !cli.disable_image_semantics,
        match_mode: if cli.match_any_condition {
            scout_search_backend::MatchMode::Any
        } else {
            scout_search_backend::MatchMode::All
        },
        access,
    };

    // ---- ctx 构造（打开 db + 加载模型 + 首次全量索引）----
    let ctx = Arc::new(build_runtime_ctx(config).await?);

    // ---- 装配 Router + 跑 server（阻塞到信号）----
    let app = build_app(ctx.clone());
    lifecycle::serve(ctx, app).await
}

/// BETA-78：个人模式 `service` 子命令用——读 `<data_dir>/config.toml`（假定
/// `bootstrap-personal-config` 已跑过）、按需下载 embedding 模型、构造
/// `ServerCtx`。与 [`run_foreground`] 的关键区别：bind 地址固定
/// [`personal::DEFAULT_PERSONAL_BIND`]（loopback-only）、`data_dir`/token 都
/// 从个人模式配置文件推导，不接受 CLI flags。
///
/// 返回 `(ctx, bind_addr, token明文)`——token 明文只为回写 `connection.json`
/// 供桌面客户端发现服务用，真正鉴权由 ctx 内部已解析的 `TokenConfig` 生效。
pub(crate) async fn build_personal_service(
    data_dir: PathBuf,
) -> Result<(Arc<ServerCtx>, SocketAddr, String)> {
    let config_path = data_dir.join("config.toml");
    let text = std::fs::read_to_string(&config_path).with_context(|| {
        format!(
            "读取个人模式配置失败（需先跑 bootstrap-personal-config）：{}",
            config_path.display()
        )
    })?;
    let access = parse_config_toml(&text)
        .with_context(|| format!("个人模式配置非法：{}", config_path.display()))?;
    let token = access
        .tokens
        .first()
        .ok_or_else(|| anyhow!("个人模式配置无任何 token：{}", config_path.display()))?
        .token
        .expose_secret()
        .to_string();

    let model_path = personal::ensure_embedding_model(&data_dir)
        .await
        .context("准备 embedding 模型失败")?;

    let model_id_for_threshold = derive_model_id(&model_path);
    let cosine_threshold =
        scout_result_normalizer::cosine_threshold_for_model(&model_id_for_threshold)
            .unwrap_or(scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD);

    let bind_addr: SocketAddr = personal::personal_bind_addr();

    let config = ServerConfig {
        bind_addr,
        data_dir,
        model_path,
        log_level: LevelFilter::INFO,
        semantic_weight: scout_server::tools::search::DEFAULT_SEMANTIC_WEIGHT,
        cosine_threshold,
        embed_images: true,
        match_mode: scout_search_backend::MatchMode::All,
        access,
    };

    let ctx = Arc::new(build_runtime_ctx(config).await?);
    Ok((ctx, bind_addr, token))
}

/// 解析访问模型：`--config` TOML（collection 模式）或 `--root`+`--token`（legacy）。
///
/// 互斥把守：两者都给 / 都不给 / legacy 缺一半 → 启动错误。
fn resolve_access_config(cli: &Cli) -> Result<DaemonConfigFile> {
    match (&cli.config, &cli.root, &cli.token) {
        (Some(path), None, None) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("读取配置文件失败：{}", path.display()))?;
            let cfg = parse_config_toml(&text)
                .with_context(|| format!("配置文件非法：{}", path.display()))?;
            info!(
                collections = cfg.collections.len(),
                tokens = cfg.tokens.len(),
                "collection 模式启动（TOML 配置）"
            );
            Ok(cfg)
        }
        (None, Some(root), Some(token)) => {
            preflight::check_token(token)?;
            info!(root = %root.display(), "legacy 单根模式启动（合成 default collection + 全权 token）");
            Ok(DaemonConfigFile::legacy_single_root(
                root.clone(),
                SecretString::from(token.clone()),
            ))
        }
        (Some(_), _, _) => Err(anyhow!(
            "--config 与 --root/--token 互斥：collection 模式下 token 在 TOML [[tokens]] 里声明"
        )),
        _ => Err(anyhow!(
            "启动参数不完整：要么给 --config <TOML>（collection 模式），要么同时给 --root 与 --token（legacy 单根）"
        )),
    }
}

/// 初始化 tracing subscriber：env-filter（fallback `info`）+ text/json 二选一。
fn init_tracing(level: &str, format: &str) -> Result<()> {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|e| {
        eprintln!("[scoutd] 警告：--log-level={level} 无法解析（{e}），回退 info 级别");
        EnvFilter::new("info")
    });
    match format {
        "json" => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .try_init()
            .map_err(|e| anyhow!("tracing init 失败：{e}"))?,
        "text" => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .try_init()
            .map_err(|e| anyhow!("tracing init 失败：{e}"))?,
        other => return Err(anyhow!("不支持的 log_format（应为 text 或 json）：{other}")),
    }
    Ok(())
}

/// service 模式专用：日志写 `<data_dir>\scoutd.log`（daily 滚动），不写 stdout——
/// Windows Service 进程没有 console，写 stdout 等于写进空气。镜像桌面端
/// `apps/desktop/src-tauri/src/main.rs::init_tracing` 同款 tracing-appender 用法。
///
/// 返回的 `WorkerGuard` **必须**存活到进程退出（`service::run_dispatcher` 全程
/// 阻塞在其调用栈上）——否则 worker 线程提前退出、尾部日志会丢。调用方用
/// `let _guard = init_tracing_to_file(...)` 绑到 `main()` 的栈帧上：`return
/// service::run_dispatcher(..)` 触发的作用域展开会在它返回后才 drop 这个 guard。
fn init_tracing_to_file(
    data_dir: &Path,
    log_level: &str,
) -> tracing_appender::non_blocking::WorkerGuard {
    let _ = std::fs::create_dir_all(data_dir); // 失败时下方 appender 自己会报；不阻塞启动
    let appender = tracing_appender::rolling::daily(data_dir, "scoutd.log");
    let (file_writer, guard) = tracing_appender::non_blocking(appender);

    let filter = EnvFilter::try_new(log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(file_writer)
        .with_target(true)
        .with_thread_ids(true)
        .with_ansi(false); // 文件不要 ANSI 颜色码
    if let Err(e) = builder.try_init() {
        // 走到这说明已有 subscriber 抢先注册（理论上不会，同进程 init 一次）；
        // stderr 在 service 模式下同样没人看，纯兜底。
        eprintln!("[scoutd] tracing subscriber (file) init 失败：{e}");
    }
    guard
}

/// service 模式的 panic 兜底：默认 panic hook 只写 stderr（service 模式下等于
/// 写进空气），这里额外把 panic 信息经 tracing 落盘（[`init_tracing_to_file`]
/// 已把 subscriber 指到 `scoutd.log`），保证"进程为什么死了"不会比一次网络请求
/// 失败更难查。
fn install_panic_log_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(panic = %info, "scoutd service 进程 panic");
        default_hook(info);
    }));
}

/// 解析 log level 字符串到 [`LevelFilter`]。
fn parse_log_level(level: &str) -> LevelFilter {
    match level.to_ascii_lowercase().as_str() {
        "trace" => LevelFilter::TRACE,
        "debug" => LevelFilter::DEBUG,
        "warn" => LevelFilter::WARN,
        "error" => LevelFilter::ERROR,
        _ => LevelFilter::INFO,
    }
}

/// 构造运行时 [`ServerCtx`]：逐 collection 打开独立 index.db（music + documents
/// 两套 schema 共存单文件——dual-db 单文件共识，ultra-review C-1）、首次全量索引
/// 其 roots、装配候选链缓存与 [`CollectionState`]。
///
/// 索引层是 sync API（rusqlite），放进 [`tokio::task::spawn_blocking`] 跑、
/// 避免阻塞 tokio runtime worker。
async fn build_runtime_ctx(config: ServerConfig) -> Result<ServerCtx> {
    if !config.data_dir.exists() {
        std::fs::create_dir_all(&config.data_dir)
            .with_context(|| format!("创建 data_dir 失败：{}", config.data_dir.display()))?;
    }

    info!(model_path = %config.model_path.display(), "加载 embedder 模型");
    let embedder = load_embedder_or_degrade(&config.model_path).await?;

    // reviewer M-6：默认 daemon binary 不开 llama-cpp feature → ModelDaemon 走
    // StubLoader、`embed()` 返 Err。真启动跑一次 ping probe 发 warn、让 ops
    // 立刻知道运行在 FTS-only 降级模式。
    // BETA-40 收尾：probe 结果同时决定 ① 索引期是否跑 embed pass（写
    // document_vectors）② 候选链是否装语义臂——此前二者都缺席，daemon 实为 FTS-only。
    let semantic_ready = embedder.is_ready() && embedder.embed("ping").is_ok();
    if !semantic_ready {
        warn!(
            "embedder 不支持 embed()（默认 stub backend）；语义召回已禁用、\
             daemon 退化为 FTS-only。生产请用 --features semantic-recall（或\
             同款 llama-cpp 系列 feature）编译"
        );
    }

    let ocr_engine = probe_ocr_dependencies();

    // reviewer I-2：首次全量索引期间 axum 尚未 bind、/health 不响应；ops 需调大
    // supervisor 启动超时（README 已注明）。
    warn!(
        collections = config.access.collections.len(),
        "首次全量索引开始（逐 collection）；大目录可能耗时数分钟、期间 /health 不响应。\
         部署时请适当调高 launchd ThrottleInterval / systemd TimeoutStartSec"
    );

    let mut collections: BTreeMap<String, CollectionRuntime> = BTreeMap::new();
    for meta in config.access.collections.clone() {
        let db_path = collection_db_path(&config.data_dir, &meta.id);
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "创建 collection {} 的索引目录失败：{}",
                    meta.id,
                    parent.display()
                )
            })?;
        }

        info!(collection = %meta.id, db = %db_path.display(), "打开索引数据库（music + documents 共用单文件）");
        let music_index = MusicIndex::open(&db_path)
            .with_context(|| format!("打开 MusicIndex 失败：{}", db_path.display()))?;
        let document_index = DocumentIndex::open(&db_path)
            .with_context(|| format!("打开 DocumentIndex 失败：{}", db_path.display()))?;

        // 首次全量索引：spawn_blocking 包 sync indexer；多 root 一次传入。
        // BETA-64 T6：embedding 已从本函数摘出（见下方 `spawn_background_embedding`），
        // 这里只跑 FTS 相关三轮（音乐/文档/图片OCR），完成即可搜索。
        let (music_index, document_index) =
            run_initial_collection_index(&meta, music_index, document_index, ocr_engine.clone())
                .await?;

        let music_count = music_index.count().context("MusicIndex.count() 失败")?;
        let document_count = document_index
            .count()
            .context("DocumentIndex.count() 失败")?;
        let doc_count = music_count.saturating_add(document_count);

        let state = CollectionState {
            indexed_at: Some(chrono::Utc::now()),
            doc_count,
            reindex_in_flight: false,
        };

        // 语义臂随 embedder probe 结果装配：ready → hybrid（FTS + 语义 RRF 融合）；
        // 否则 FTS-only（与旧行为一致）。
        let search_candidates =
            Arc::new(scout_server::tools::search::build_local_search_candidates(
                db_path.clone(),
                semantic_ready.then(|| embedder.clone()),
            ));

        let rt = CollectionRuntime {
            meta,
            db_path,
            music_index: Arc::new(Mutex::new(music_index)),
            document_index: Arc::new(Mutex::new(document_index)),
            search_candidates,
            state: Arc::new(RwLock::new(state)),
        };
        // BETA-64 T6：语义向量补齐挪到后台低优先级任务，不阻塞本 collection 变为可搜索、
        // 更不阻塞后续 collection 的 FTS 索引（此前 embedding 是四阶段链路最后一环，
        // 任一 collection 语料多都会拖慢所有 collection 的启动可用时间）。
        if semantic_ready {
            spawn_background_embedding(
                rt.meta.id.clone(),
                rt.meta.roots.clone(),
                rt.document_index.clone(),
                embedder.clone(),
                config.embed_images,
            );
        }
        collections.insert(rt.meta.id.clone(), rt);
    }

    let audit = Arc::new(scout_server::audit::AuditSink::new(
        &config.data_dir,
        config.access.audit.log_query,
    ));

    let indexing_probe: IndexingProbe = {
        let states: Vec<_> = collections.values().map(|rt| rt.state.clone()).collect();
        Arc::new(move || states.iter().any(|state| state.read().reindex_in_flight))
    };

    Ok(ServerCtx {
        config,
        embedder,
        collections,
        audit,
        indexing_probe,
    })
}

/// BETA-40 收尾：OCR / PDF 渲染依赖启动期探测留日志（此前静默缺失——图片不入索引、
/// 扫描 PDF 计 failed 都无从察觉）。OCR 引擎探测一次、全 collection 复用。
fn probe_ocr_dependencies() -> Option<Arc<dyn OcrEngine>> {
    let ocr_engine: Option<Arc<dyn OcrEngine>> = default_ocr_engine().map(Arc::from);
    if let Some(engine) = &ocr_engine {
        info!(
            engine = engine.name(),
            "OCR 引擎可用（图片 / 扫描 PDF 文字识别）"
        );
    } else {
        warn!(
            "无可用 OCR 引擎（Windows.Media.Ocr / Tesseract）——JPG/PNG 图片不入索引、\
             无文本层的扫描 PDF 将计 failed 并留痕 index_failures 表"
        );
    }
    if !PopplerPdfRasterizer::detect() {
        warn!(
            "未检测到 pdftoppm（poppler-utils）——扫描版 PDF 无法渲染页、\
             将计 failed 并留痕 index_failures 表"
        );
    }
    ocr_engine
}

/// 单 collection 首次全量索引：document + **图片 OCR** + music 三轮增量（FTS 相关，
/// 完成后即可搜索）。2026-07-28 起执行顺序为「文档 → 图片 → 音频」（文档优先，
/// 详见 docs/index-performance-design.md §9）。语义向量 pass 已挪至 [`spawn_background_embedding`]（BETA-64 T6）——
/// embedding 是可选增强层（daemon 本就支持 embedder 不可用时的 FTS-only 降级），没有
/// 理由让它挡在"用户能不能搜到刚索引完的文件"这条关键路径上。
/// 完成后打统计日志（含 `index_failures` 留痕条数），归还两个 index 句柄。
async fn run_initial_collection_index(
    meta: &scout_server::collections::CollectionConfig,
    music_index: MusicIndex,
    document_index: DocumentIndex,
    ocr: Option<Arc<dyn OcrEngine>>,
) -> Result<(MusicIndex, DocumentIndex)> {
    let roots = meta.roots.clone();
    // BETA-64 T1：三阶段（音乐/文档/图片OCR）各自已在 indexer 内部打点，这里额外记总
    // 耗时——三阶段严格串行不重叠，本值即"首次全量索引"用户实际等待（到可搜索为止）时长。
    let phase_start = std::time::Instant::now();
    // 2026-07-28：执行顺序改为「文档 → 图片 → 音频」——面向工作场景，文档量最大
    // 且最先被用户搜索，图片（含扫描件 OCR）次之，音频文件量少、优先级最低，
    // 排最后不耽误文档尽快可搜（详见 docs/index-performance-design.md §9）。
    let (music_index, document_index, music_stats, document_stats, image_stats) =
        tokio::task::spawn_blocking(move || -> Result<_, IndexError> {
            let d = document_index.index_dirs_with_progress(&roots, &NoopProgress)?;
            let i = if let Some(engine) = &ocr {
                document_index.index_image_dirs_excluding_with_progress(
                    &roots,
                    engine.as_ref(),
                    &GlobSet::empty(),
                    &NoopProgress,
                )?
            } else {
                IndexStats::default()
            };
            let m = music_index.index_dirs_with_progress(&roots, &NoopProgress)?;
            Ok((music_index, document_index, m, d, i))
        })
        .await
        .context("indexer 任务 panic 或被取消")??;
    let total_index_ms = u64::try_from(phase_start.elapsed().as_millis()).unwrap_or(u64::MAX);

    let extraction_failures = document_index.extraction_failure_count().unwrap_or(0);
    info!(
        collection = %meta.id,
        music_scanned = music_stats.scanned,
        music_added = music_stats.added,
        document_scanned = document_stats.scanned,
        document_added = document_stats.added,
        image_scanned = image_stats.scanned,
        image_added = image_stats.added,
        image_failed = image_stats.failed,
        extraction_failures,
        total_index_ms,
        "collection 首次全量索引完成（FTS 可搜；语义向量后台补齐中，另有独立完成日志）"
    );
    Ok((music_index, document_index))
}

/// BETA-64 T6：语义向量补齐后台任务——`purge_short_body_vectors` + `embed_pending`
/// 从 [`run_initial_collection_index`] 摘出，改为不阻塞启动/FTS 可用性的独立任务。
/// detached 运行（不持有 `JoinHandle`）：服务关闭时随 runtime drop 一并终止；embedding
/// 幂等可续（`vector_is_current` / `content_hash` 去重），下次启动接着补、不丢进度、
/// 不重复计算。`document_index` 与 `CollectionRuntime.document_index` 共用同一把
/// `parking_lot::Mutex`——与并发 search 查询按 `SQLite` 单连接语义排队，业务无损
/// （`CollectionRuntime` 文档注释已载明此设计）。
fn spawn_background_embedding(
    collection_id: String,
    roots: Vec<PathBuf>,
    document_index: Arc<Mutex<DocumentIndex>>,
    embedder: Arc<dyn TextEmbedder>,
    embed_images: bool,
) {
    tokio::spawn(async move {
        let embed_start = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(move || -> Result<_, IndexError> {
            let idx = document_index.lock();
            idx.purge_short_body_vectors(embed_images)?;
            idx.embed_pending(&roots, embedder.as_ref(), embed_images, &mut |_, _| {})
        })
        .await;
        let embed_ms = u64::try_from(embed_start.elapsed().as_millis()).unwrap_or(u64::MAX);
        match result {
            Ok(Ok((embed_ok, embed_reused, embed_failed))) => info!(
                collection = %collection_id,
                embedded = embed_ok,
                embed_reused,
                embed_failed,
                embed_ms,
                "collection 语义向量后台补齐完成"
            ),
            Ok(Err(error)) => warn!(
                collection = %collection_id,
                %error,
                "collection 语义向量后台补齐失败（FTS 检索不受影响）"
            ),
            Err(error) => warn!(
                collection = %collection_id,
                %error,
                "collection 语义向量后台补齐任务 panic 或被取消（FTS 检索不受影响）"
            ),
        }
    });
}

/// [`load_embedder`] 的 async 包装：真正的模型加载（mmap GGUF 文件 + 初始化
/// llama.cpp 推理线程池，真机上可能耗时数秒到数十秒）是同步阻塞调用，此前直接
/// 摆在 async fn 里跑会占住 tokio worker 线程。`service::run_async` 靠一个跟本
/// 任务共用 runtime 的独立 ticker task 给 SCM 发 `StartPending` checkpoint 兜底
/// 30s 启动超时（见该文件注释）——本调用摞在同一 worker 线程上不至于致命
/// （work-stealing 调度器通常会把 ticker 挪到别的空闲线程执行），但在核心数很少
/// 的机器上仍有让 checkpoint 迟发的风险。首次全量索引等其它耗时步骤都已经走
/// `spawn_blocking`（见 [`run_initial_collection_index`]），这里补齐同样处理，
/// 保持"async runtime worker 线程上不跑任何长阻塞调用"这条既有约定一致。
///
/// 模型文件缺失/损坏（如个人模式首启下载失败后仍以目标路径继续，见
/// `personal::ensure_embedding_model`）不应该拖垮整个服务——降级为一个恒不可用
/// 的 [`UnavailableEmbedder`]，走既有的 `semantic_ready` 探测→FTS-only 降级路径，
/// 而不是 `?` 直接终止 daemon（BETA-79 之后真机反馈：service 模式下这一步曾是
/// 唯一会让整个 Windows Service 起不来的失败点）。
async fn load_embedder_or_degrade(model_path: &Path) -> Result<Arc<dyn TextEmbedder>> {
    let owned = model_path.to_path_buf();
    let loaded = tokio::task::spawn_blocking(move || load_embedder(&owned))
        .await
        .context("加载 embedder 模型任务 panic 或被取消")?;
    Ok(loaded.unwrap_or_else(|e| {
        warn!(
            error = %e,
            model_path = %model_path.display(),
            "加载 embedder 模型失败，语义召回禁用、降级为 FTS-only 运行"
        );
        Arc::new(UnavailableEmbedder) as Arc<dyn TextEmbedder>
    }))
}

/// 加载 embedder：调 [`ModelDaemon::load_blocking`]（model-runtime 自动按
/// feature 选 stub / llama-cpp），包成 [`DaemonEmbedder`] 暴露给 indexer。
fn load_embedder(model_path: &Path) -> Result<Arc<dyn TextEmbedder>> {
    let params = ModelLoadParams {
        gpu_layers: 99,
        context_size: 2048,
    };
    let daemon = ModelDaemon::load_blocking(model_path, params)
        .map_err(|e| anyhow!("加载 embedder 模型失败：{e}"))?;
    let model_id = derive_model_id(model_path);
    Ok(Arc::new(DaemonEmbedder {
        daemon: Arc::new(daemon),
        model_id,
    }))
}

/// 从 GGUF 文件名派生 `model_id`（写入 `document_vectors.embed_model`）。
///
/// 2026-07-28：委托 `scout_model_runtime::canonical_model_id`——此前是裸
/// `file_stem()`（保留量化后缀，如 `"embeddinggemma-300m-q8_0"`），跟桌面端此前的固定常量
/// `"embeddinggemma-300m"` 不一致；现在两端共用同一份剥后缀逻辑，统一口径（详见该函数文档：
/// 对已有独立部署的 daemon，这是一次性触发向量重新 embed 的迁移，不是无成本变更）。
fn derive_model_id(model_path: &Path) -> String {
    scout_model_runtime::canonical_model_id(model_path)
}

/// [`ModelDaemon`] → [`TextEmbedder`] 适配器。
struct DaemonEmbedder {
    daemon: Arc<ModelDaemon>,
    model_id: String,
}

impl std::fmt::Debug for DaemonEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonEmbedder")
            .field("model_id", &self.model_id)
            .finish_non_exhaustive()
    }
}

impl TextEmbedder for DaemonEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>, IndexError> {
        // reviewer I-1：用 Io variant + `<embedder>` 占位 path（详 BETA-32 注）。
        self.daemon.embed(text).map_err(|e| IndexError::Io {
            path: "<embedder>".to_owned(),
            detail: e.to_string(),
        })
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }
}

/// `load_embedder` 失败时的降级占位——恒 `is_ready() == false`，让候选链装配
/// 阶段直接跳过语义臂（不进查询链），语义与"stub backend `embed()` 返 Err"一致，
/// 但省一次必败的 `embed()` 调用。
#[derive(Debug)]
struct UnavailableEmbedder;

impl TextEmbedder for UnavailableEmbedder {
    fn embed(&self, _text: &str) -> Result<Vec<f32>, IndexError> {
        Err(IndexError::Io {
            path: "<embedder>".to_owned(),
            detail: "embedder 模型未加载（模型文件缺失或加载失败）".to_owned(),
        })
    }

    fn model_id(&self) -> &'static str {
        "unavailable"
    }

    fn is_ready(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn base_cli() -> Cli {
        Cli::parse_from([
            "scoutd",
            "--data-dir",
            "/tmp/d",
            "--model-path",
            "/tmp/m.gguf",
        ])
    }

    #[test]
    fn resolve_access_rejects_missing_both_modes() {
        let cli = base_cli();
        assert!(
            resolve_access_config(&cli).is_err(),
            "无 --config 也无 --root/--token 应报错"
        );
    }

    #[test]
    fn resolve_access_rejects_mixed_modes() {
        let mut cli = base_cli();
        cli.config = Some("/tmp/c.toml".into());
        cli.root = Some("/tmp/r".into());
        cli.token = Some("t".repeat(32));
        assert!(
            resolve_access_config(&cli).is_err(),
            "--config 与 --root/--token 互斥"
        );
    }

    #[test]
    fn resolve_access_legacy_synthesizes_default() {
        let mut cli = base_cli();
        cli.root = Some("/tmp/r".into());
        cli.token = Some("t".repeat(32));
        let cfg = resolve_access_config(&cli).unwrap();
        assert_eq!(cfg.collections.len(), 1);
        assert_eq!(cfg.collections[0].id, "default");
        assert!(cfg.tokens[0].admin);
    }

    #[test]
    fn resolve_access_legacy_short_token_rejected() {
        let mut cli = base_cli();
        cli.root = Some("/tmp/r".into());
        cli.token = Some("short".into());
        assert!(resolve_access_config(&cli).is_err());
    }
}
