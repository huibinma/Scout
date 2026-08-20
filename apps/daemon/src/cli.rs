//! scoutd CLI 参数定义。
//!
//! BETA-32 T9 骨架：只定义 clap derive 结构；T10 起在 main.rs 消费。
//!
//! BETA-78：新增可选子命令（`command`），用于 Windows Service 化——桌面安装器
//! 装机时调用 `bootstrap-personal-config` → `install-service`，SCM 之后经
//! `service`（真正的服务入口，人不直接用）拉起 daemon。**不给子命令时行为与
//! 今天完全一致**（顶层 flags 直接 serve，前台阻塞），现有团队部署（手动
//! `scoutd --config ...` 或 NSSM 包装）零迁移。

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// `Scout` 团队归档 MCP daemon / 个人后台服务命令行参数。
///
/// BETA-36：两种启动形态二选一（互斥，main 里把守）——
/// - **legacy 单根**：`--root` + `--token`（合成 default collection + 全权 admin token）；
/// - **collection 模式**：`--config <TOML>`（`[[collections]]` + `[[tokens]]` + `[audit]`）。
///
/// BETA-78：`data_dir`/`model_path` 改为 `Option`——不给子命令（即今天的前台
/// serve 用法）时二者仍是必填，main.rs 里手工校验补回（clap derive 无法表达
/// "无子命令时必填、有子命令时不需要"这种依赖子命令的条件必填）。
#[derive(Parser, Debug)]
#[command(
    name = "scoutd",
    version,
    about = "Scout 团队归档 MCP daemon / 个人后台服务"
)]
pub struct Cli {
    /// 子命令（服务安装/卸载/个人模式引导）；不给则走今天的前台 serve 路径。
    #[command(subcommand)]
    pub command: Option<Command>,

    /// 索引根目录（legacy 单根模式；与 --config 互斥）。
    #[arg(long)]
    pub root: Option<PathBuf>,

    /// 监听地址（默认 0.0.0.0:8765；个人模式子命令另有 loopback-only 默认值）。
    #[arg(long, default_value = "0.0.0.0:8765")]
    pub bind: SocketAddr,

    /// Bearer token（legacy 单根模式；或 `SCOUTD_TOKEN` 环境变量；与 --config 互斥）。
    #[arg(long, env = "SCOUTD_TOKEN")]
    pub token: Option<String>,

    /// 索引 DB 目录（无子命令时必填）。
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// embedder GGUF 文件路径（无子命令时必填）。
    #[arg(long, env = "SCOUTD_MODEL_PATH")]
    pub model_path: Option<PathBuf>,

    /// TOML 配置（collection 模式：[[collections]] + [[tokens]] + [audit]；与 --root/--token 互斥）。
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// hybrid 融合中语义臂权重（缺省镜像桌面 `DEFAULT_SEMANTIC_WEIGHT`；
    /// BETA-40 企业评测用于 A/B 排位）。
    #[arg(long)]
    pub semantic_weight: Option<f64>,

    /// 关闭「OCR 图片文本入语义索引」（daemon 默认开启——企业场景图片证据
    /// 检索需求 + 2 字 CJK 词语义臂唯一兜底；BETA-39 质量门槛仍然生效。
    /// 关闭后启动期会清除已嵌的全部图片向量、回到 FTS-only 一刀切态）。
    #[arg(long)]
    pub disable_image_semantics: bool,

    /// 2026-07-20：多个复合检索条件（关键词组）之间的匹配模式改为「任一条件命中」
    /// （组间 OR，广召回）。daemon 无 settings.json、无法像桌面端 live-read，
    /// 启动时一次性决定；默认关（严格要求全部复合条件命中，与桌面端默认口径一致，
    /// 取代 BETA-57 旧版自动 OR 兜底）。
    #[arg(long)]
    pub match_any_condition: bool,

    /// 日志格式（text 或 json）。
    #[arg(long, default_value = "text")]
    pub log_format: String,

    /// 日志级别（trace / debug / info / warn / error）。
    #[arg(long, default_value = "info")]
    pub log_level: String,

    /// 允许启动时检测到 `schema_meta` 不一致或残留 rebuild 文件时重建。
    #[arg(long)]
    pub allow_rebuild_schema: bool,
}

/// BETA-78 新增子命令：Windows Service 化 + 个人模式自举。
#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// 生成个人模式默认配置（幂等：`config.toml` 已存在即跳过）——安装器在
    /// 注册服务前调用一次；`--root` 可重复传多个默认索引目录，不给则用
    /// 当前用户的 Desktop/Documents/Downloads/Pictures/Music 五个系统默认目录。
    BootstrapPersonalConfig {
        /// 数据目录（缺省 `%ProgramData%\Scout\scoutd`）。
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// 默认索引根目录（可重复；缺省用系统默认五个目录）。
        #[arg(long = "root")]
        roots: Vec<PathBuf>,
    },
    /// 注册 Windows Service（LocalSystem 账户、开机自启）。仅 Windows 支持。
    InstallService {
        /// 数据目录（缺省 `%ProgramData%\Scout\scoutd`，需已 bootstrap）。
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// 停止并删除已注册的 Windows Service。仅 Windows 支持。
    UninstallService,
    /// 真正的 SCM 服务入口（由 Service Control Manager 派发拉起，不给人手工用）。
    Service {
        /// 数据目录（缺省 `%ProgramData%\Scout\scoutd`）。
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}
