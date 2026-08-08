//! scoutd CLI 参数定义。
//!
//! BETA-32 T9 骨架：只定义 clap derive 结构；T10 起在 main.rs 消费。

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

/// `Scout` 团队归档 MCP daemon 命令行参数。
///
/// BETA-36：两种启动形态二选一（互斥，main 里把守）——
/// - **legacy 单根**：`--root` + `--token`（合成 default collection + 全权 admin token）；
/// - **collection 模式**：`--config <TOML>`（`[[collections]]` + `[[tokens]]` + `[audit]`）。
#[derive(Parser, Debug)]
#[command(name = "scoutd", version, about = "Scout 团队归档 MCP daemon")]
pub struct Cli {
    /// 索引根目录（legacy 单根模式；与 --config 互斥）。
    #[arg(long)]
    pub root: Option<PathBuf>,

    /// 监听地址（默认 0.0.0.0:8765）。
    #[arg(long, default_value = "0.0.0.0:8765")]
    pub bind: SocketAddr,

    /// Bearer token（legacy 单根模式；或 `SCOUTD_TOKEN` 环境变量；与 --config 互斥）。
    #[arg(long, env = "SCOUTD_TOKEN")]
    pub token: Option<String>,

    /// 索引 DB 目录。
    #[arg(long)]
    pub data_dir: PathBuf,

    /// embedder GGUF 文件路径。
    #[arg(long, env = "SCOUTD_MODEL_PATH")]
    pub model_path: PathBuf,

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
