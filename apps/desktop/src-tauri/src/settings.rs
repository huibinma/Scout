use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tauri::AppHandle;

/// struct 级 `#[serde(default)]`：旧版 settings.json 缺任意字段都能解析成功，
/// 缺失字段取 `Default` impl 的值——避免新增字段导致整体解析失败、用户设置被静默重置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub global_shortcut: String,
    pub search_scope: Vec<String>,
    pub enable_model_fallback: bool,
    pub enable_tracing: bool,
    /// BETA-23：模型文件路径覆盖（None = 默认 app 数据目录 models/qwen3-0.6b-q4_k_m.gguf）。
    pub model_path: Option<String>,
    /// BETA-15B-1：embedding 模型文件路径覆盖（None = 默认 app 数据目录 models/）。
    pub embedding_model_path: Option<String>,
    /// BETA-15B-3 簇A-1：语义相似度下限覆盖（None = 默认 DEFAULT_SIMILARITY_FLOOR）。
    pub semantic_similarity_floor: Option<f32>,
    /// BETA-27：索引的具体文件夹列表（统一，三臂共用）。
    /// **2026-07-06 起：空 + `include_system_defaults=false` = 不索引任何目录**（默认零索引，
    /// 用户显式添加后才扫；旧语义"空 = 系统默认三夹"已废弃，见 `resolve_index_roots_tagged`）。
    pub index_roots: Vec<String>,
    /// 是否纳入系统默认目录（Music+Documents+Pictures）。默认 `false`。
    /// **2026-07-06 起与 `index_roots` 空否解耦**：勾上即纳入三夹（无论有无自定义目录）；
    /// 不勾则只扫自定义目录（自定义也为空 = 零索引）。前端「选项 → 索引」checkbox 常显。
    pub include_system_defaults: bool,
    /// BETA-27：排除的目录名 glob（basename，树中任何同名子目录被剪枝）。空 = 默认噪声表。
    pub exclude_globs: Vec<String>,
    /// BETA-33 cycle 7-b：per-root 子路径排除（相对 root 的 path glob）。
    /// 每项 `{root, patterns}`。空列表 = 该 root 无 per-root 排除（仍走全局 exclude_globs basename 排除）。
    /// **数据模型选 `Vec<RootExclude>` 而非 `HashMap`（Codex §10 OBJECT 1）**：
    /// JSON object key 用路径字符串会让 Windows 盘符/反斜杠/大小写/尾部分隔符更脆，
    /// 且未来加 enabled/comment/created_at 难扩展。
    pub root_excludes: Vec<RootExclude>,
    /// App 运行期间自动增量索引间隔（分钟）。0 = 关闭；缺省 30。
    /// 每轮调度前 live-read，用户改设置后无需重启。
    pub auto_index_interval_minutes: u32,
    /// BETA-15B-3 A-2：融合层语义臂权重覆盖（None = 默认 DEFAULT_SEMANTIC_WEIGHT）。
    /// clamp[0.5, 50.0]：下限防 FTS 倒挂、上限防无意义大值。
    pub semantic_weight: Option<f64>,
    /// BETA-39（2026-07-03）：图片 OCR 文本参与语义索引 opt-in（默认关）。
    /// 开启后图片走更严的 0.75 质量门槛（`IMAGE_MEANINGFUL_RATIO_FLOOR`）入嵌；
    /// 关闭时 embed / purge / 段落级 explain 三处均与 BETA-33 cycle 4 一刀切现状一致。
    pub enable_image_semantics: bool,
    /// BETA-47：Everything 集成总开关（默认开，装了就用、没装优雅降级——现状零回归）。
    /// 关闭后三处 es.exe 调用点全部停用：① 搜索后端不注册（**需重启生效**，与 model_path
    /// 口径一致）；② 索引期音频全盘发现回退目录扫描（live-read）；③ 模型本地发现跳过
    /// es.exe 扫描（live-read）。仅 Windows 有意义，其他平台忽略。
    pub enable_everything: bool,
    /// BETA-53：本机 MCP 服务开关意图（默认关）。为 true 时 app 启动会自动拉起服务
    /// （只绑 `127.0.0.1`，让本机 LLM 客户端经 MCP 检索本机文件）。用户显式关闭即置 false。
    pub mcp_service_enabled: bool,
    /// BETA-53：本机 MCP 服务的 bearer token（首次启用时随机生成、持久化复用）。
    /// None = 尚未生成；重置令牌时清空并重新生成。**属于敏感凭据**——settings.json
    /// 已与 index.db 同权限目录，与其他本机数据同级。
    pub mcp_service_token: Option<String>,
    /// 2026-07-20：多个复合检索条件（关键词组）之间的匹配模式，全局配置。
    /// `true`（默认）= 全部复合条件命中（严格 AND，0 命中即 0，不再静默放宽）；
    /// `false` = 任一条件命中（OR，广召回）。四个检索后端统一读取，详见
    /// `scout_search_backend::MatchMode`。取代 BETA-57 旧版自动 OR 兜底——
    /// 用户反馈该静默放宽会把只命中部分条件的结果当正常结果返回，造成大量噪音。
    pub search_match_all_conditions: bool,
    /// 2026-07-26：Windows 专属——点系统关闭按钮时驻留系统托盘而非退出进程（默认关，
    /// 兼容现状「关窗即退出」）。开启后：主窗口 `CloseRequested` 被拦截、只 `hide()`；
    /// 后台索引 / 全局快捷键监听继续跑；托盘图标常驻，左键或菜单「显示 Scout」唤起，
    /// 菜单「退出 Scout」才真正退出进程（见 `tray.rs`）。仅 Windows 有意义——macOS
    /// 已有原生「关窗不退出、Dock 图标常驻」心智，其他平台忽略本字段。
    pub close_to_tray: bool,
    /// 2026-08-09：自动更新总开关（默认开）。关闭后 `update.rs` 的后台检查循环
    /// 每轮 live-read 到 false 就跳过本轮检查（不是停止循环——用户运行期重新打开
    /// 无需重启即生效，与 `auto_index_interval_minutes` 的 live-read 约定一致）。
    pub auto_update_enabled: bool,
    /// 2026-08-09：自动更新轮询间隔（分钟）。默认 240（4 小时）；UI 侧固定几档
    /// 可选值，允许范围 [30, 1440]（半小时 ~ 24 小时）——读取时经
    /// `resolve_auto_update_interval_minutes` clamp，防手改 settings.json 出界值
    /// 把检查循环钉在不合理的频率上。
    pub auto_update_interval_minutes: u32,
}

/// BETA-33 cycle 7-b：per-root 子路径排除项。
/// `root` 保留 display 形式（不做归一化，跟 index_roots 里字符串对应）；
/// 后端过滤前调用 `normalize_root_key` 归一化再比较（Codex §10 SUGGEST 2）。
///
/// `patterns` 是**相对 root 的 path glob**（非 basename）：
/// - `临时/**` = root 下 `临时` 目录及所有子；
/// - `**/backup/**` = 任意深度下名为 `backup` 的目录及内容；
/// - `*.old/*` = root 下以 `.old` 结尾的目录里所有直接子项。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RootExclude {
    pub root: String,
    pub patterns: Vec<String>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            global_shortcut: DEFAULT_GLOBAL_SHORTCUT.to_string(),
            search_scope: vec!["~".to_string()],
            enable_model_fallback: true,
            enable_tracing: false,
            model_path: None,
            embedding_model_path: None,
            semantic_similarity_floor: None,
            index_roots: Vec::new(),
            include_system_defaults: false,
            exclude_globs: Vec::new(),
            root_excludes: Vec::new(),
            auto_index_interval_minutes: DEFAULT_AUTO_INDEX_INTERVAL_MINUTES,
            semantic_weight: None,
            enable_image_semantics: false,
            enable_everything: true,
            mcp_service_enabled: false,
            mcp_service_token: None,
            search_match_all_conditions: true,
            close_to_tray: false,
            auto_update_enabled: true,
            auto_update_interval_minutes: DEFAULT_AUTO_UPDATE_INTERVAL_MINUTES,
        }
    }
}

/// BETA-33 cycle 7-b（Codex §10 SUGGEST 2）：把路径归一化成用于比较 / 查找 / 去重的 key。
///
/// 语义：
/// - Windows：分隔符统一为 `\`、末尾去 `\` / `/`、字母全部小写（Windows 路径大小写不敏感）
/// - Unix：分隔符统一为 `/`、末尾去 `/`、大小写保留（Unix 路径大小写敏感）
///
/// 不做 `std::fs::canonicalize`——canonicalize 依赖路径存在性、且不同调用点结果可能不一致；
/// 归一化 key 用于 root_excludes 查找 / 前缀匹配 / UI 去重、不需要 canonical 精确性。
///
/// 例：Windows 上 `C:\Users\Alice\Documents\` == `C:/Users/Alice/Documents` == `c:\users\alice\documents`。
#[must_use]
pub fn normalize_root_key(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // 分隔符统一
    #[cfg(windows)]
    let sep_norm: String = trimmed.replace('/', "\\");
    #[cfg(not(windows))]
    let sep_norm: String = trimmed.replace('\\', "/");
    // trim 尾部分隔符（保留根盘符 `C:\` 的分隔符——去掉会变纯盘符改语义）
    let no_tail = {
        let bytes = sep_norm.as_bytes();
        if bytes.len() > 3 && (bytes[bytes.len() - 1] == b'\\' || bytes[bytes.len() - 1] == b'/') {
            sep_norm[..sep_norm.len() - 1].to_string()
        } else {
            sep_norm
        }
    };
    // Windows 大小写归一
    #[cfg(windows)]
    {
        no_tail.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        no_tail
    }
}

/// BETA-27 默认目录名排除表（BETA-26 build_corpus 验证）。
pub(crate) const DEFAULT_EXCLUDE_GLOBS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    ".cargo",
    ".rustup",
    ".venv",
    "venv",
    "__pycache__",
    "dist",
    "build",
    ".next",
    "Pods",
    ".gradle",
    ".Trash",
    "vendor",
    ".cache",
    "DerivedData",
    "Library",
];

/// 系统默认索引三夹（音频 / 文档 / 图片），返存在的部分。
/// 抽出便于跨模块共用（RootIndexOverview 标 is_default 时按路径匹配）。
pub(crate) fn system_default_roots() -> Vec<PathBuf> {
    [dirs::audio_dir(), dirs::document_dir(), dirs::picture_dir()]
        .into_iter()
        .flatten()
        .collect()
}

/// 解析索引根 + 每项是否为系统默认。
///
/// **2026-07-06（cycle 9 真机反馈）新语义**：系统默认三夹**仅当 `include_defaults = true`
/// 时纳入**，与 `raw` 空否解耦——默认（`raw` 空 + `include_defaults = false`）**不索引任何
/// 目录**，用户显式添加目录或勾选系统默认后才开始索引：
/// - `raw` 为空 + `include_defaults = false` → **空**（默认零索引）
/// - `raw` 为空 + `include_defaults = true` → 系统三夹（`is_default = true`）
/// - `raw` 非空 + `include_defaults = false` → 只用 `raw`（`is_default = false`）
/// - `raw` 非空 + `include_defaults = true` → `raw` 与系统三夹合并、去重（保 raw 顺序在前）
///
/// 历史：cycle 6 v4 旧语义在 `raw` 空时兜底返回系统三夹（开箱即索引）；真机反馈认为
/// 未经同意索引用户目录不妥，改为显式 opt-in。
pub(crate) fn resolve_index_roots_tagged(
    raw: &[String],
    include_defaults: bool,
) -> Vec<(PathBuf, bool)> {
    let mut out: Vec<(PathBuf, bool)> = raw.iter().map(|s| (PathBuf::from(s), false)).collect();
    if include_defaults {
        let defaults = system_default_roots();
        let seen: std::collections::HashSet<PathBuf> = out.iter().map(|(p, _)| p.clone()).collect();
        for d in defaults {
            if !seen.contains(&d) {
                out.push((d, true));
            }
        }
    }
    out
}

/// 解析排除 glob：配置非空用配置，空回退默认噪声表。
/// BETA-27：reindex（read_index_config）接入。
pub(crate) fn resolve_exclude_globs(raw: &[String]) -> Vec<String> {
    if raw.is_empty() {
        DEFAULT_EXCLUDE_GLOBS
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    } else {
        raw.to_vec()
    }
}

/// 语义相似度下限默认值（全仓单一默认源）。
pub(crate) const DEFAULT_SIMILARITY_FLOOR: f32 = 0.30;
pub(crate) const DEFAULT_AUTO_INDEX_INTERVAL_MINUTES: u32 = 30;
/// 自动更新轮询间隔默认值（4 小时）与允许范围（半小时 ~ 24 小时）。
pub(crate) const DEFAULT_AUTO_UPDATE_INTERVAL_MINUTES: u32 = 240;
pub(crate) const MIN_AUTO_UPDATE_INTERVAL_MINUTES: u32 = 30;
pub(crate) const MAX_AUTO_UPDATE_INTERVAL_MINUTES: u32 = 1440;
/// 全局唤起快捷键默认值：跨平台统一为 `Ctrl+Space`（此前 macOS/Windows 各写死一份、
/// 且不可改；用户可在「常规」设置里通过 `shortcut::update_global_shortcut` 自定义，
/// 遇到与其他程序冲突时不必等发版改默认值，自己换一个组合即可）。
pub(crate) const DEFAULT_GLOBAL_SHORTCUT: &str = "Ctrl+Space";

/// 把设置里的原始下限值规整：有限值 clamp 到 [0,1]；None / 非有限 → 默认。
pub(crate) fn resolve_similarity_floor(raw: Option<f32>) -> f32 {
    match raw {
        Some(v) if v.is_finite() => v.clamp(0.0, 1.0),
        _ => DEFAULT_SIMILARITY_FLOOR,
    }
}

/// 从 settings.json live-read 语义相似度下限（每次查询调）。读/解析失败 → 默认。
pub(crate) fn read_similarity_floor(settings_path: &Option<std::path::PathBuf>) -> f32 {
    let raw = settings_path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<AppSettings>(&s).ok())
        .and_then(|v| v.semantic_similarity_floor);
    resolve_similarity_floor(raw)
}

/// 把设置里的原始 weight 值规整：有限值 clamp 到 [0.5, 50.0]
/// （下限防 FTS 倒挂、上限防无意义大值，详 spec §4）；None / 非有限 → 默认。
pub(crate) fn resolve_semantic_weight(raw: Option<f64>) -> f64 {
    match raw {
        Some(v) if v.is_finite() => v.clamp(0.5, 50.0),
        _ => scout_result_normalizer::DEFAULT_SEMANTIC_WEIGHT,
    }
}

/// 从 settings.json live-read 语义臂权重（每次查询调）。读/解析失败 → 默认。
pub(crate) fn read_semantic_weight(settings_path: &Option<std::path::PathBuf>) -> f64 {
    let raw = settings_path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<AppSettings>(&s).ok())
        .and_then(|v| v.semantic_weight);
    resolve_semantic_weight(raw)
}

/// BETA-39：从 settings.json live-read「图片语义索引」opt-in（启动期 purge / 语义 pass /
/// 段落级 explain 三处调）。读/解析失败 → false（安全侧 = 现状一刀切）。
pub(crate) fn read_enable_image_semantics(settings_path: &Option<std::path::PathBuf>) -> bool {
    settings_path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<AppSettings>(&s).ok())
        .is_some_and(|v| v.enable_image_semantics)
}

/// BETA-47：从 settings.json live-read「Everything 集成」开关（音频全盘发现 / 模型本地
/// 发现两处 live 调用点 + 启动期后端注册共用）。读 / 解析失败 → **true**（安全侧 =
/// 现状「装了就用、没装优雅降级」，不因配置损坏静默关掉加速）。
pub(crate) fn read_enable_everything(settings_path: &Option<std::path::PathBuf>) -> bool {
    settings_path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<AppSettings>(&s).ok())
        // 注：不用 `is_none_or`（1.82 稳定）——crate 声明 rust-version 1.80，
        // clippy `incompatible_msrv` 会拦。
        .map_or(true, |v| v.enable_everything)
}

/// 从 settings.json live-read 自动增量索引间隔。读取失败或旧配置缺字段时回退默认 30 分钟；
/// 显式 0 表示关闭自动索引。
pub(crate) fn read_auto_index_interval_minutes(settings_path: &Option<std::path::PathBuf>) -> u32 {
    settings_path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<AppSettings>(&s).ok())
        .map_or(DEFAULT_AUTO_INDEX_INTERVAL_MINUTES, |v| {
            v.auto_index_interval_minutes
        })
}

/// 把自动更新轮询间隔 clamp 到 [30, 1440] 分钟（半小时 ~ 24 小时）——UI
/// 侧固定几档可选值本就落在这个范围内，这里是防手改 settings.json 出界值。
pub(crate) fn resolve_auto_update_interval_minutes(raw: u32) -> u32 {
    raw.clamp(
        MIN_AUTO_UPDATE_INTERVAL_MINUTES,
        MAX_AUTO_UPDATE_INTERVAL_MINUTES,
    )
}

/// 从 settings.json live-read 自动更新总开关。读取失败或旧配置缺字段 → 默认开启
/// （安全侧：新装用户/老配置不因文件损坏静默失去更新提醒）。
pub(crate) fn read_auto_update_enabled(settings_path: &Option<PathBuf>) -> bool {
    settings_path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<AppSettings>(&s).ok())
        .map_or(true, |v| v.auto_update_enabled)
}

/// 从 settings.json live-read 自动更新轮询间隔（分钟），已 clamp 到允许范围。
pub(crate) fn read_auto_update_interval_minutes(settings_path: &Option<PathBuf>) -> u32 {
    let raw = settings_path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<AppSettings>(&s).ok())
        .map_or(DEFAULT_AUTO_UPDATE_INTERVAL_MINUTES, |v| {
            v.auto_update_interval_minutes
        });
    resolve_auto_update_interval_minutes(raw)
}

/// 从 settings.json live-read 复合条件匹配模式（每次查询调，`search_impl`/预览高亮共用）。
/// 读 / 解析失败 → **true**（安全侧 = 严格全部命中，不因配置损坏静默扩大召回）。
pub(crate) fn read_search_match_all_conditions(settings_path: &Option<PathBuf>) -> bool {
    settings_path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<AppSettings>(&s).ok())
        .map_or(true, |v| v.search_match_all_conditions)
}

/// 2026-07-26：从 settings.json live-read「关闭到托盘」开关——窗口 `CloseRequested`
/// 事件处理器每次关闭请求时读取，用户运行期切换设置无需重启生效。读 / 解析失败 →
/// false（安全侧 = 关窗即退出，不因配置损坏把用户"晾"在一个关不掉的窗口上）。
/// 仅 main.rs 的 Windows 关闭拦截调用，与 `tray` 模块同款 target_os 门禁、避免其他
/// 平台构建出现 dead_code 警告。
#[cfg(target_os = "windows")]
pub(crate) fn read_close_to_tray(settings_path: &Option<PathBuf>) -> bool {
    read_settings_or_default(settings_path).close_to_tray
}

/// 从 settings.json live-read 全局唤起快捷键（应用启动期注册用）。缺字段 / 空串 /
/// 读取或解析失败 → 默认 `Ctrl+Space`（不因配置损坏导致启动期注册直接失败）。
pub(crate) fn read_global_shortcut(settings_path: &Option<PathBuf>) -> String {
    let raw = settings_path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<AppSettings>(&s).ok())
        .map(|v| v.global_shortcut)
        .unwrap_or_default();
    if raw.trim().is_empty() {
        DEFAULT_GLOBAL_SHORTCUT.to_string()
    } else {
        raw
    }
}

/// BETA-53：best-effort 读取完整 `AppSettings`（`settings_path` 指向 settings.json）。
/// 读 / 解析失败 → `Default`（与其他 live-read 一致的安全侧；本机 MCP 服务模块用它
/// 读开关态 + token + 生效索引 roots，不经 `AppHandle`）。
pub(crate) fn read_settings_or_default(settings_path: &Option<PathBuf>) -> AppSettings {
    settings_path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<AppSettings>(&s).ok())
        .unwrap_or_default()
}

/// BETA-53：把 `AppSettings` 落盘到 settings.json（含父目录创建）。
/// 供本机 MCP 服务模块持久化开关态 / token（不经 `AppHandle` 的 `update_settings` 命令）。
///
/// # Errors
///
/// 创建目录 / 序列化 / 写文件任一失败时返回 `Err(描述)`。
pub(crate) fn write_settings(
    settings_path: &std::path::Path,
    settings: &AppSettings,
) -> Result<(), String> {
    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {e}"))?;
    }
    let content =
        serde_json::to_string_pretty(settings).map_err(|e| format!("序列化设置失败: {e}"))?;
    fs::write(settings_path, content).map_err(|e| format!("写入设置失败: {e}"))
}

/// 设置文件路径（BETA-21 隐私面板复用，展示「配置在哪」）。
/// 不创建目录，仅解析路径（best-effort）。2026-07-29 起改走 [`crate::scout_data_dir`]
/// （与 index.db / audit.jsonl 同目录，不再落在 Tauri bundle id 派生的 `ai.scout.desktop`
/// 目录——两个目录并存曾让隐私面板「数据存在哪」展示自相矛盾，见 `main.rs::scout_data_dir` 注释）。
pub(crate) fn settings_file_path(_app: &AppHandle) -> Option<std::path::PathBuf> {
    Some(crate::scout_data_dir().join("settings.json"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::field_reassign_with_default)]

    use super::*;

    /// 旧版 settings.json 缺少 model_path 字段时，仍能正常解析（向前兼容）。
    #[test]
    fn old_settings_without_model_path_parses_ok() {
        let json = r#"{"global_shortcut":"Option+Space","search_scope":["~"],"enable_model_fallback":true,"enable_tracing":false}"#;
        let s: AppSettings = serde_json::from_str(json).unwrap();
        assert!(s.model_path.is_none());
    }

    #[test]
    fn resolve_similarity_floor_clamps_and_defaults() {
        assert_eq!(resolve_similarity_floor(None), DEFAULT_SIMILARITY_FLOOR);
        assert_eq!(resolve_similarity_floor(Some(0.5)), 0.5);
        assert_eq!(resolve_similarity_floor(Some(-1.0)), 0.0);
        assert_eq!(resolve_similarity_floor(Some(2.0)), 1.0);
        assert_eq!(
            resolve_similarity_floor(Some(f32::NAN)),
            DEFAULT_SIMILARITY_FLOOR
        );
    }

    #[test]
    fn old_settings_without_similarity_floor_parses_ok() {
        let json = r#"{"global_shortcut":"Ctrl+Space","search_scope":["~"],"enable_model_fallback":true,"enable_tracing":false}"#;
        let s: AppSettings = serde_json::from_str(json).unwrap();
        assert!(s.semantic_similarity_floor.is_none());
    }

    // 旧 resolve_index_roots(raw) wrapper cycle 6 v4 已删（无外部 caller、tagged 版覆盖）；
    // 老测 resolve_index_roots_uses_config_when_nonempty 与其一并去掉，新 tagged 覆盖见下。

    /// 2026-07-06 新语义四分支（系统默认仅 include_defaults=true 时纳入、与 raw 空否解耦）：
    /// 空+false → **空（默认零索引）**；空+true → 系统三夹；
    /// 非空+false → 只 raw；非空+true → raw 在前 + 追加系统默认（不重复、标签正确）。
    #[test]
    fn resolve_index_roots_tagged_covers_four_branches() {
        // 空 raw + include=false：默认零索引（cycle 9 真机反馈：未经同意不索引任何目录）。
        let empty_false = resolve_index_roots_tagged(&[], false);
        assert!(
            empty_false.is_empty(),
            "空 raw + include_defaults=false 应为空（默认零索引），实得 {empty_false:?}"
        );
        // 空 raw + include=true：系统三夹（显式 opt-in）。
        let empty_true = resolve_index_roots_tagged(&[], true);
        assert!(
            empty_true.iter().all(|(_, is_def)| *is_def),
            "opt-in 系统默认时全部项 is_default=true"
        );

        // 非空 raw + include=false：只 raw、is_default 全 false（覆盖语义不变）。
        let raw = vec!["/tmp/custom1".to_string(), "/tmp/custom2".to_string()];
        let no_default = resolve_index_roots_tagged(&raw, false);
        assert_eq!(no_default.len(), 2);
        assert!(no_default.iter().all(|(_, is_def)| !is_def));

        // 非空 raw + include=true：raw 在前，之后追加系统默认；raw 项 is_default=false、
        // 系统默认项 is_default=true。
        let with_default = resolve_index_roots_tagged(&raw, true);
        assert!(with_default.len() >= 2, "至少含 2 个 raw 项");
        assert_eq!(with_default[0].0, PathBuf::from("/tmp/custom1"));
        assert!(!with_default[0].1);
        assert_eq!(with_default[1].0, PathBuf::from("/tmp/custom2"));
        assert!(!with_default[1].1);
        // 追加部分（若系统能给出默认目录）都应带 is_default=true。
        for (_, is_def) in &with_default[2..] {
            assert!(*is_def);
        }
    }

    /// cycle 6 v4：raw 与系统默认重叠时不重复入 tagged（去重按路径）。
    #[test]
    fn resolve_index_roots_tagged_dedups_overlap() {
        // 若能拿到系统 audio_dir，就把它塞到 raw 里模拟"用户手动加了 Music"的场景；
        // 拿不到就跳过（headless CI 可能返 None）。
        let Some(audio) = dirs::audio_dir() else {
            return;
        };
        let raw = vec![audio.to_string_lossy().into_owned()];
        let tagged = resolve_index_roots_tagged(&raw, true);
        let audio_count = tagged.iter().filter(|(p, _)| *p == audio).count();
        assert_eq!(audio_count, 1, "raw 与系统默认重叠时不重复");
        // 该项 is_default=false（raw 显式列出、按 raw 语义标非默认）。
        assert!(
            !tagged
                .iter()
                .find(|(p, _)| *p == audio)
                .expect("audio_dir 应在 tagged 结果里")
                .1
        );
    }

    /// cycle 6 v4：AppSettings::default().include_system_defaults 应为 false（旧语义、零回归）。
    #[test]
    fn default_settings_does_not_include_system_defaults() {
        let s = AppSettings::default();
        assert!(!s.include_system_defaults);
    }

    #[test]
    fn resolve_exclude_globs_empty_uses_defaults() {
        let d = resolve_exclude_globs(&[]);
        assert!(
            d.iter().any(|g| g == "node_modules"),
            "空 → 默认表含 node_modules"
        );
        let custom = resolve_exclude_globs(&["foo".to_string()]);
        assert_eq!(custom, vec!["foo".to_string()]);
    }

    #[test]
    fn old_settings_without_index_fields_parses_ok() {
        let json = r#"{"global_shortcut":"Ctrl+Space","search_scope":["~"],"enable_model_fallback":true,"enable_tracing":false}"#;
        let s: AppSettings = serde_json::from_str(json).unwrap();
        assert!(s.index_roots.is_empty());
        assert!(s.exclude_globs.is_empty());
    }

    /// cycle 7-b：向前兼容——旧 settings.json 无 root_excludes 字段仍能解析（serde default）。
    #[test]
    fn old_settings_without_root_excludes_parses_ok() {
        let json = r#"{"global_shortcut":"Ctrl+Space","index_roots":["/tmp"]}"#;
        let s: AppSettings = serde_json::from_str(json).unwrap();
        assert!(s.root_excludes.is_empty(), "缺字段 → serde default 空 Vec");
    }

    /// cycle 7-b：默认 AppSettings 的 root_excludes 应为空 Vec（旧行为零回归）。
    #[test]
    fn root_excludes_default_is_empty() {
        let s = AppSettings::default();
        assert!(s.root_excludes.is_empty());
    }

    /// cycle 7-b：RootExclude serde round-trip。
    #[test]
    fn root_exclude_serde_round_trip() {
        let re = RootExclude {
            root: "C:\\Users\\Alice\\Documents".to_string(),
            patterns: vec!["临时/**".to_string(), "**/backup/**".to_string()],
        };
        let json = serde_json::to_string(&re).unwrap();
        let back: RootExclude = serde_json::from_str(&json).unwrap();
        assert_eq!(back, re);
    }

    /// cycle 7-b · Codex SUGGEST 2：Windows 路径不同 display 归一化到同一 key。
    #[cfg(windows)]
    #[test]
    fn normalize_root_key_windows_equivalence() {
        let a = normalize_root_key("C:\\Users\\Alice\\Documents");
        let b = normalize_root_key("C:/Users/Alice/Documents");
        let c = normalize_root_key("c:\\users\\alice\\documents\\");
        let d = normalize_root_key("C:\\Users\\Alice\\Documents\\");
        assert_eq!(a, b, "Windows 反斜杠 vs 正斜杠等价");
        assert_eq!(a, c, "Windows 大小写不敏感");
        assert_eq!(a, d, "尾部分隔符等价");
        // 但根盘符 `C:\` 不该被裁成 `C:`
        assert_eq!(normalize_root_key("C:\\"), "c:\\");
    }

    /// cycle 7-b · Codex SUGGEST 2：Unix 路径大小写敏感、分隔符归一。
    #[cfg(not(windows))]
    #[test]
    fn normalize_root_key_unix_preserves_case() {
        assert_eq!(
            normalize_root_key("/home/User/Documents"),
            "/home/User/Documents",
            "Unix 保留大小写"
        );
        assert_eq!(
            normalize_root_key("/home/User/Documents/"),
            "/home/User/Documents",
            "Unix 尾部斜杠去掉"
        );
        assert_eq!(
            normalize_root_key("\\home\\User\\Documents"),
            "/home/User/Documents",
            "Unix 反斜杠转正斜杠"
        );
    }

    /// cycle 7-b：空 / 空白输入 → 空 key（防 UI 传空串时崩）。
    #[test]
    fn normalize_root_key_empty_returns_empty() {
        assert_eq!(normalize_root_key(""), "");
        assert_eq!(normalize_root_key("   "), "");
    }

    #[test]
    fn read_similarity_floor_reads_or_defaults() {
        assert_eq!(read_similarity_floor(&None), DEFAULT_SIMILARITY_FLOOR);
        let dir = std::env::temp_dir().join(format!("scout-floor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("settings.json");
        std::fs::write(&f, r#"{"semantic_similarity_floor":0.55}"#).unwrap();
        assert_eq!(read_similarity_floor(&Some(f)), 0.55);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_semantic_weight_clamps_and_defaults() {
        assert!(
            (resolve_semantic_weight(None) - scout_result_normalizer::DEFAULT_SEMANTIC_WEIGHT)
                .abs()
                < f64::EPSILON
        );
        assert!((resolve_semantic_weight(Some(3.0)) - 3.0).abs() < f64::EPSILON);
        // clamp 下界 0.5（< 0.5 拉到 0.5）
        assert!((resolve_semantic_weight(Some(0.1)) - 0.5).abs() < f64::EPSILON);
        // clamp 上界 50.0
        assert!((resolve_semantic_weight(Some(100.0)) - 50.0).abs() < f64::EPSILON);
        // NaN → 默认
        assert!(
            (resolve_semantic_weight(Some(f64::NAN))
                - scout_result_normalizer::DEFAULT_SEMANTIC_WEIGHT)
                .abs()
                < f64::EPSILON
        );
    }

    /// BETA-39：默认关 + 旧 settings.json 缺字段解析为 false + live-read 三态。
    #[test]
    fn enable_image_semantics_defaults_off_and_reads_ok() {
        assert!(!AppSettings::default().enable_image_semantics, "默认关");
        let json = r#"{"global_shortcut":"Ctrl+Space"}"#;
        let s: AppSettings = serde_json::from_str(json).unwrap();
        assert!(!s.enable_image_semantics, "旧配置缺字段 → false");
        // live-read：无路径 → false；有配置 → 读真值。
        assert!(!read_enable_image_semantics(&None));
        let dir = std::env::temp_dir().join(format!("scout-imgsem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("settings.json");
        std::fs::write(&f, r#"{"enable_image_semantics":true}"#).unwrap();
        assert!(read_enable_image_semantics(&Some(f.clone())));
        std::fs::write(&f, r#"{"enable_image_semantics":false}"#).unwrap();
        assert!(!read_enable_image_semantics(&Some(f)));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// BETA-47：Everything 开关默认开 + 旧 settings.json 缺字段解析为 true（零回归）+
    /// live-read 三态（无路径 → true；显式 false / true → 读真值）。
    #[test]
    fn enable_everything_defaults_on_and_reads_ok() {
        assert!(AppSettings::default().enable_everything, "默认开");
        let json = r#"{"global_shortcut":"Ctrl+Space"}"#;
        let s: AppSettings = serde_json::from_str(json).unwrap();
        assert!(s.enable_everything, "旧配置缺字段 → true（现状零回归）");
        // live-read：无路径 → true（安全侧 = 现状）；有配置 → 读真值。
        assert!(read_enable_everything(&None));
        let dir = std::env::temp_dir().join(format!("scout-everything-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("settings.json");
        std::fs::write(&f, r#"{"enable_everything":false}"#).unwrap();
        assert!(!read_enable_everything(&Some(f.clone())));
        std::fs::write(&f, r#"{"enable_everything":true}"#).unwrap();
        assert!(read_enable_everything(&Some(f.clone())));
        // 配置损坏 → true（不因坏文件静默关加速）。
        std::fs::write(&f, "not json").unwrap();
        assert!(read_enable_everything(&Some(f)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn search_match_all_conditions_defaults_on_and_reads_ok() {
        assert!(
            AppSettings::default().search_match_all_conditions,
            "默认严格全部条件命中"
        );
        let json = r#"{"global_shortcut":"Ctrl+Space"}"#;
        let s: AppSettings = serde_json::from_str(json).unwrap();
        assert!(
            s.search_match_all_conditions,
            "旧配置缺字段 → true（默认严格全部命中）"
        );
        // live-read：无路径 → true（安全侧）；有配置 → 读真值。
        assert!(read_search_match_all_conditions(&None));
        let dir = std::env::temp_dir().join(format!("scout-match-mode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("settings.json");
        std::fs::write(&f, r#"{"search_match_all_conditions":false}"#).unwrap();
        assert!(!read_search_match_all_conditions(&Some(f.clone())));
        std::fs::write(&f, r#"{"search_match_all_conditions":true}"#).unwrap();
        assert!(read_search_match_all_conditions(&Some(f.clone())));
        // 配置损坏 → true（不因坏文件静默放宽为任一命中）。
        std::fs::write(&f, "not json").unwrap();
        assert!(read_search_match_all_conditions(&Some(f)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn auto_index_interval_defaults_and_reads_ok() {
        assert_eq!(
            AppSettings::default().auto_index_interval_minutes,
            DEFAULT_AUTO_INDEX_INTERVAL_MINUTES
        );
        let json = r#"{"global_shortcut":"Ctrl+Space"}"#;
        let s: AppSettings = serde_json::from_str(json).unwrap();
        assert_eq!(
            s.auto_index_interval_minutes, DEFAULT_AUTO_INDEX_INTERVAL_MINUTES,
            "旧 settings.json 缺字段时应回退默认间隔"
        );

        assert_eq!(
            read_auto_index_interval_minutes(&None),
            DEFAULT_AUTO_INDEX_INTERVAL_MINUTES
        );
        let dir = std::env::temp_dir().join(format!("scout-auto-index-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("settings.json");
        std::fs::write(&f, r#"{"auto_index_interval_minutes":0}"#).unwrap();
        assert_eq!(read_auto_index_interval_minutes(&Some(f.clone())), 0);
        std::fs::write(&f, r#"{"auto_index_interval_minutes":15}"#).unwrap();
        assert_eq!(read_auto_index_interval_minutes(&Some(f.clone())), 15);
        std::fs::write(&f, r#"{"auto_index_interval_minutes":-1}"#).unwrap();
        assert_eq!(
            read_auto_index_interval_minutes(&Some(f)),
            DEFAULT_AUTO_INDEX_INTERVAL_MINUTES,
            "非法 JSON 值导致反序列化失败时回退默认"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn auto_update_defaults_on_with_4h_interval_and_old_settings_parse_ok() {
        assert!(
            AppSettings::default().auto_update_enabled,
            "默认自动更新开启"
        );
        assert_eq!(
            AppSettings::default().auto_update_interval_minutes,
            DEFAULT_AUTO_UPDATE_INTERVAL_MINUTES
        );
        // 旧配置缺这两个字段：默认开启 + 默认 4 小时间隔，不因新增字段解析失败。
        let json = r#"{"global_shortcut":"Ctrl+Space"}"#;
        let s: AppSettings = serde_json::from_str(json).unwrap();
        assert!(s.auto_update_enabled);
        assert_eq!(
            s.auto_update_interval_minutes,
            DEFAULT_AUTO_UPDATE_INTERVAL_MINUTES
        );

        assert!(read_auto_update_enabled(&None));
        assert_eq!(
            read_auto_update_interval_minutes(&None),
            DEFAULT_AUTO_UPDATE_INTERVAL_MINUTES
        );
    }

    #[test]
    fn auto_update_interval_clamped_to_30_1440_range() {
        assert_eq!(resolve_auto_update_interval_minutes(0), 30);
        assert_eq!(resolve_auto_update_interval_minutes(5), 30);
        assert_eq!(resolve_auto_update_interval_minutes(30), 30);
        assert_eq!(resolve_auto_update_interval_minutes(240), 240);
        assert_eq!(resolve_auto_update_interval_minutes(1440), 1440);
        assert_eq!(resolve_auto_update_interval_minutes(10_000), 1440);

        let dir = std::env::temp_dir().join(format!("scout-auto-update-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("settings.json");
        std::fs::write(
            &f,
            r#"{"auto_update_enabled":false,"auto_update_interval_minutes":5}"#,
        )
        .unwrap();
        assert!(!read_auto_update_enabled(&Some(f.clone())));
        assert_eq!(
            read_auto_update_interval_minutes(&Some(f.clone())),
            30,
            "手改 settings.json 出界小值应 clamp 到下限"
        );
        std::fs::write(
            &f,
            r#"{"auto_update_enabled":true,"auto_update_interval_minutes":999999}"#,
        )
        .unwrap();
        assert_eq!(
            read_auto_update_interval_minutes(&Some(f.clone())),
            1440,
            "出界大值应 clamp 到上限"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn old_settings_without_semantic_weight_parses_ok() {
        let json = r#"{"global_shortcut":"Ctrl+Space","search_scope":["~"],"enable_model_fallback":true,"enable_tracing":false}"#;
        let s: AppSettings = serde_json::from_str(json).unwrap();
        assert!(s.semantic_weight.is_none());
    }

    /// BETA-53 修复：设置表单全量覆写不得冲掉后端带外持久化的 MCP token / enabled。
    /// 复现分叉根因——前端旧快照携 token=null/enabled=false 回写，磁盘现值（后端刚写的
    /// 真 token + enabled=true）必须保留；其余字段仍按前端快照写入。
    #[test]
    fn update_settings_preserves_backend_managed_mcp_fields() {
        let dir = std::env::temp_dir().join(format!("scout-mcpmerge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("settings.json");

        // 磁盘现值：MCP 已由后端带外启用、持有真 token（enabled=true）。
        let mut on_disk = AppSettings::default();
        on_disk.mcp_service_enabled = true;
        on_disk.mcp_service_token = Some("a".repeat(64));
        write_settings(&f, &on_disk).unwrap();

        // 前端提交的旧快照：MCP 字段是挂载时的陈旧值（null / false），同时改了个无关字段。
        let mut incoming = AppSettings::default();
        incoming.mcp_service_enabled = false;
        incoming.mcp_service_token = None;
        incoming.enable_tracing = true; // 表单真正想改的字段
        update_settings_at(&f, incoming).unwrap();

        // 落盘后：MCP 两字段以磁盘现值为准（保留），无关字段按前端写入。
        let after = read_settings_or_default(&Some(f));
        assert!(
            after.mcp_service_enabled,
            "enabled 不得被表单快照冲成 false"
        );
        assert_eq!(
            after.mcp_service_token.as_deref(),
            Some("a".repeat(64).as_str()),
            "token 不得被表单快照冲成 null"
        );
        assert!(after.enable_tracing, "无关字段应按前端快照正常写入");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 全局快捷键同款保护：`shortcut::update_global_shortcut` 绕开设置表单直接改磁盘现值；
    /// 随后若表单又保存了任意其他字段，携带挂载期旧快捷键快照的全量覆写不得把它冲回去。
    #[test]
    fn update_settings_preserves_backend_managed_global_shortcut() {
        let dir = std::env::temp_dir().join(format!("scout-shortcutmerge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("settings.json");

        // 磁盘现值：用户刚通过快捷键录制器把它改成了自定义组合。
        let mut on_disk = AppSettings::default();
        on_disk.global_shortcut = "Ctrl+Alt+F".to_string();
        write_settings(&f, &on_disk).unwrap();

        // 前端旧快照：挂载时读到的仍是默认值，同时改了个无关字段。
        let mut incoming = AppSettings::default();
        incoming.global_shortcut = DEFAULT_GLOBAL_SHORTCUT.to_string();
        incoming.enable_tracing = true;
        update_settings_at(&f, incoming).unwrap();

        let after = read_settings_or_default(&Some(f));
        assert_eq!(
            after.global_shortcut, "Ctrl+Alt+F",
            "自定义快捷键不得被表单旧快照冲回默认值"
        );
        assert!(after.enable_tracing, "无关字段应按前端快照正常写入");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 磁盘无 settings.json（首次保存）时，`update_settings_at` 不因缺文件失败，
    /// 直接按前端快照写入（此时 MCP 字段本就是默认值、无可保留）。
    #[test]
    fn update_settings_at_writes_when_no_existing_file() {
        let dir = std::env::temp_dir().join(format!("scout-mcpmerge-new-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("settings.json");
        let mut incoming = AppSettings::default();
        incoming.enable_tracing = true;
        update_settings_at(&f, incoming).unwrap();
        let after = read_settings_or_default(&Some(f));
        assert!(after.enable_tracing);
        assert!(!after.mcp_service_enabled);
        assert!(after.mcp_service_token.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn global_shortcut_defaults_to_ctrl_space() {
        assert_eq!(AppSettings::default().global_shortcut, "Ctrl+Space");
        // 结构体级 `#[serde(default)]` 缺字段时取整个 `AppSettings::default()`，
        // 与其他字段（如 enable_everything 默认 true）同一套机制，故旧配置缺
        // global_shortcut 字段时也回退到 "Ctrl+Space" 而非 String 原生空串默认。
        let json = r#"{"search_scope":["~"]}"#;
        let s: AppSettings = serde_json::from_str(json).unwrap();
        assert_eq!(
            s.global_shortcut, "Ctrl+Space",
            "旧配置缺字段 → 默认 Ctrl+Space"
        );
    }

    #[test]
    fn read_global_shortcut_reads_or_defaults() {
        assert_eq!(read_global_shortcut(&None), DEFAULT_GLOBAL_SHORTCUT);
        let dir = std::env::temp_dir().join(format!("scout-shortcut-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("settings.json");
        std::fs::write(&f, r#"{"global_shortcut":"Ctrl+Alt+F"}"#).unwrap();
        assert_eq!(read_global_shortcut(&Some(f.clone())), "Ctrl+Alt+F");
        // 空串（理论上不该出现在磁盘上，防御一下）→ 回退默认。
        std::fs::write(&f, r#"{"global_shortcut":""}"#).unwrap();
        assert_eq!(
            read_global_shortcut(&Some(f.clone())),
            DEFAULT_GLOBAL_SHORTCUT
        );
        // 配置损坏 → 回退默认。
        std::fs::write(&f, "not json").unwrap();
        assert_eq!(read_global_shortcut(&Some(f)), DEFAULT_GLOBAL_SHORTCUT);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_semantic_weight_reads_or_defaults() {
        assert!(
            (read_semantic_weight(&None) - scout_result_normalizer::DEFAULT_SEMANTIC_WEIGHT).abs()
                < f64::EPSILON
        );
        let dir = std::env::temp_dir().join(format!("scout-weight-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("settings.json");
        std::fs::write(&f, r#"{"semantic_weight":3.5}"#).unwrap();
        assert!((read_semantic_weight(&Some(f)) - 3.5).abs() < f64::EPSILON);
        std::fs::remove_dir_all(&dir).ok();
    }
}

fn get_settings_path(_app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let path = crate::scout_data_dir();

    if !path.exists() {
        fs::create_dir_all(&path).map_err(|e| format!("Failed to create data dir: {}", e))?;
    }

    Ok(path.join("settings.json"))
}

#[tauri::command]
pub fn get_settings(app: AppHandle) -> Result<AppSettings, String> {
    let path = get_settings_path(&app)?;

    if !path.exists() {
        return Ok(AppSettings::default());
    }

    let content =
        fs::read_to_string(path).map_err(|e| format!("Failed to read settings: {}", e))?;
    serde_json::from_str(&content).map_err(|e| format!("Failed to parse settings: {}", e))
}

/// BETA-31-v3 cycle 6（v0.8.7）：返回**当前生效**的索引目录列表（绝对路径字符串）。
/// 设置页用它在 `index_roots` 为空时显示「系统默认（音频 / 文档 / 图片）」的具体路径、
/// 让用户清楚知道当前在索引哪些目录、便于决定是否要加自定义目录覆盖默认。
///
/// 与 `resolve_index_roots_tagged` 行为对齐（2026-07-06 新语义）：
/// - `include_system_defaults=false` → 只返 `index_roots`（为空即返空 = 默认零索引）
/// - `include_system_defaults=true` → `index_roots` 与系统三夹合并去重（roots 为空即纯三夹）
///
/// **cycle 6 v2（v0.8.8）**：入参 `index_roots` 可选；前端传当前 useState 值时直接用、
/// 不传时退到读 settings.json（保留旧调用方兼容）。修旧实现「数据源永远从文件读、
/// 与前端 state 不同步」的 3 个 UX bug：用户在 UI 移除目录未保存时显示错位、
/// 「系统默认」标签错误打在用户配置目录上、tab 切换不重 fetch。
///
/// **best-effort**：settings 读取失败 → 退到默认行为（系统三夹）；某 dirs 函数返 None →
/// 跳过该项（不阻塞其他项）。
#[tauri::command]
pub fn get_effective_index_roots(
    app: AppHandle,
    index_roots: Option<Vec<String>>,
    include_system_defaults: Option<bool>,
) -> Result<Vec<String>, String> {
    let (raw, include_defaults) = read_effective_inputs(&app, index_roots, include_system_defaults);
    Ok(resolve_index_roots_tagged(&raw, include_defaults)
        .into_iter()
        .map(|(p, _)| p.to_string_lossy().into_owned())
        .collect())
}

/// 收拢 get_effective_index_roots / get_index_overview 的入参解析逻辑：
/// 未传时退到读 settings.json；两者行为一致。
fn read_effective_inputs(
    app: &AppHandle,
    index_roots: Option<Vec<String>>,
    include_system_defaults: Option<bool>,
) -> (Vec<String>, bool) {
    let settings = || -> Option<AppSettings> {
        let path = get_settings_path(app).ok()?;
        if !path.exists() {
            return None;
        }
        let content = fs::read_to_string(&path).ok()?;
        serde_json::from_str::<AppSettings>(&content).ok()
    };
    match (index_roots, include_system_defaults) {
        (Some(r), Some(inc)) => (r, inc),
        (Some(r), None) => (
            r,
            settings()
                .map(|s| s.include_system_defaults)
                .unwrap_or(false),
        ),
        (None, Some(inc)) => (settings().map(|s| s.index_roots).unwrap_or_default(), inc),
        (None, None) => match settings() {
            Some(s) => (s.index_roots, s.include_system_defaults),
            None => (Vec::new(), false),
        },
    }
}

/// BETA-33 cycle 5（2026-07-01）：每个索引 root 的分类统计（doc / image / music）+
/// 是否系统默认 + 上次索引时间。桌面「选项 → 索引」pane 用它渲染目录概貌 + 目录管理。
#[derive(Debug, Clone, Serialize)]
pub struct RootIndexOverview {
    pub path: String,
    /// 是否是系统默认目录（`include_system_defaults=true` 时追加的三夹项）。
    pub is_default: bool,
    /// 非图片文档条数（docx / pdf / txt / md / xlsx / pptx / ...）。
    pub doc_count: u64,
    /// 图片 OCR 条数（png / jpg / bmp / ...）。
    pub image_count: u64,
    /// 音频条数（mp3 / flac / m4a / ...）。
    pub music_count: u64,
    /// 该 root 下最近一次 indexed_time，取 documents 表和 music 表 MAX 中较晚者；
    /// 转 rfc3339 字符串前端直接展示。无记录 → None。
    pub last_indexed_time: Option<String>,
}

/// BETA-33 cycle 5：返回**每个生效 root 的分类统计**（doc/image/music + 上次索引时间）。
///
/// 与 `get_effective_index_roots` 平行：入参 `index_roots` 可选，None 时退到读 settings.json。
/// cycle 6 v4：`resolve_index_roots_tagged` 返回每项自带 is_default 标签，追加模式下混合列表精准标。
///
/// **性能**：单 sqlite 文件、两表各 3 GLOB 前缀 OR、每 root 2 次 query_row。真机 3-5 root
/// 典型 <50ms。索引读并发不阻塞 reindex 写（BETA-04 busy_timeout=5s）。
///
/// **best-effort**：某 root 打开 DocumentIndex / MusicIndex 失败 → 该项返 0 计数、不整链 fail。
#[tauri::command]
pub fn get_index_overview(
    app: AppHandle,
    index_roots: Option<Vec<String>>,
    include_system_defaults: Option<bool>,
) -> Result<Vec<RootIndexOverview>, String> {
    let (raw, include_defaults) = read_effective_inputs(&app, index_roots, include_system_defaults);
    // cycle 6 v4：tagged 结果里每项自带 is_default 标签（追加模式下混合列表也能精准标）。
    let tagged = resolve_index_roots_tagged(&raw, include_defaults);
    let db_path = crate::local_index_db_path();

    // 打开一次 doc / music index 复用（同一 sqlite 文件、两个连接）。
    // 打开失败（首次未索引、文件不存在）→ 返所有 root 的 0 计数、不 fail。
    let doc_idx = scout_indexer::DocumentIndex::open(&db_path).ok();
    let music_idx = scout_indexer::MusicIndex::open(&db_path).ok();

    let mut out: Vec<RootIndexOverview> = Vec::with_capacity(tagged.len());
    for (path, is_default) in tagged {
        let path_str = path.to_string_lossy().into_owned();
        let doc_stats = doc_idx
            .as_ref()
            .and_then(|idx| idx.stats_under_root(&path_str).ok())
            .unwrap_or(scout_indexer::DocRootStats {
                total: 0,
                images: 0,
                last_indexed_time: None,
            });
        let music_stats = music_idx
            .as_ref()
            .and_then(|idx| idx.stats_under_root(&path_str).ok())
            .unwrap_or(scout_indexer::MusicRootStats {
                total: 0,
                last_indexed_time: None,
            });
        // doc_stats.total 含图片，拆开：文档 = total - images。
        let doc_count = doc_stats.total.saturating_sub(doc_stats.images);
        // 综合 last_indexed = max(doc_last, music_last)。
        let last_indexed_time =
            match (doc_stats.last_indexed_time, music_stats.last_indexed_time) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            }
            .and_then(|secs| {
                chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0).map(|d| d.to_rfc3339())
            });
        out.push(RootIndexOverview {
            path: path_str,
            is_default,
            doc_count,
            image_count: doc_stats.images,
            music_count: music_stats.total,
            last_indexed_time,
        });
    }
    Ok(out)
}

/// BETA-33 cycle 7-c：`purge_root_from_db` 的返回统计。
#[derive(Debug, Clone, Serialize)]
pub struct PurgeSummary {
    /// documents 表删除条数（含图片；document_vectors 随外键级联）。
    pub doc_deleted: u64,
    /// music 表删除条数。
    pub music_deleted: u64,
}

/// BETA-33 cycle 7-c（Codex SUGGEST 7/8）：清除某 root 子树下的索引记录（documents +
/// music，FTS 同步删、向量外键级联）。**只删 Scout 数据库缓存、绝不删磁盘文件**——
/// 前端「移除并清除索引记录」二次确认后调用。SQL 全部在 indexer 存储层
/// （`purge_under_root`，与 `stats_under_root` 共用边界谓词），此处只做薄封装。
#[tauri::command]
pub fn purge_root_from_db(root: String) -> Result<PurgeSummary, String> {
    let db_path = crate::local_index_db_path();
    let doc_deleted = scout_indexer::DocumentIndex::open(&db_path)
        .and_then(|idx| idx.purge_under_root(&root))
        .map_err(|e| format!("清除文档索引失败: {e}"))?;
    let music_deleted = scout_indexer::MusicIndex::open(&db_path)
        .and_then(|idx| idx.purge_under_root(&root))
        .map_err(|e| format!("清除音频索引失败: {e}"))?;
    Ok(PurgeSummary {
        doc_deleted,
        music_deleted,
    })
}

/// 「未能索引的文件」一条（BETA-40 `index_failures` 留痕的桌面消费）。
#[derive(Debug, Clone, Serialize)]
pub struct ExtractionFailureJson {
    /// 失败文件完整路径。
    pub path: String,
    /// 失败原因（提取器报错细节）。
    pub reason: String,
    /// 最近一次失败时间（rfc3339；时间戳异常时 None）。
    pub failed_time: Option<String>,
}

/// 桌面「选项 → 索引」查询文件级提取失败留痕（路径 + 原因 + 时间，按时间倒序）。
/// 成功重扫 / 文件从磁盘删除后由 indexer 自动清除对应条目。
/// 首次未索引（db 不存在）→ 空列表、不报错。
#[tauri::command]
pub fn get_extraction_failures() -> Result<Vec<ExtractionFailureJson>, String> {
    let db_path = crate::local_index_db_path();
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let idx = scout_indexer::DocumentIndex::open(&db_path)
        .map_err(|e| format!("打开索引数据库失败: {e}"))?;
    let rows = idx
        .extraction_failures()
        .map_err(|e| format!("读取失败留痕失败: {e}"))?;
    Ok(rows
        .into_iter()
        .map(|f| ExtractionFailureJson {
            path: f.path,
            reason: f.reason,
            failed_time: chrono::DateTime::<chrono::Utc>::from_timestamp(f.failed_time, 0)
                .map(|d| d.to_rfc3339()),
        })
        .collect())
}

/// BETA-53 修复：本机 MCP 服务的 `mcp_service_enabled` / `mcp_service_token` 属**后端带外
/// 持久化**字段——由 `mcp_service.rs`（start / stop / `reset_token` / 自动启动）直接读改写
/// settings.json，`McpPane` 面板**不经**此设置表单。
///
/// 而 `update_settings` 是**全量覆写**：前端 `AppSettings` 快照在弹窗挂载时经 `get_settings`
/// 一次读入、之后 `McpPane` 改动这两字段不会回灌该快照。用户随后保存任意设置时，旧快照里
/// 的 token / enabled 会把后端刚写的新值冲掉；此时运行中的 axum server 仍持内存里的旧
/// token → 磁盘 token 静默失效（401）、外部 client 无感退回 grep。
///
/// `global_shortcut` 同款风险：`shortcut::update_global_shortcut` 命令绕开设置表单、直接
/// 校验 + 重新注册 + 落盘（这样用户按下新组合的瞬间就知道是否与其他程序冲突，不必等重启才
/// 发现死快捷键）。若不在此处同款保护，用户改完快捷键后若又保存了表单里任意其他字段，
/// 表单挂载时的旧快照会把刚生效的新快捷键冲回旧值——下次启动时又变回去，而运行中的
/// 全局快捷键监听却仍是新值，磁盘与实际生效状态从此对不上。
///
/// 解决：以**磁盘现值为准**合并这些字段，让磁盘 settings.json 成为其唯一信源、设置表单
/// 永远不直接覆写它们。其余字段仍按前端快照全量写入（语义不变）。
fn merge_backend_managed_fields(incoming: &mut AppSettings, on_disk: &AppSettings) {
    incoming.mcp_service_enabled = on_disk.mcp_service_enabled;
    incoming.mcp_service_token = on_disk.mcp_service_token.clone();
    incoming.global_shortcut = on_disk.global_shortcut.clone();
}

/// `update_settings` 的路径化内核（便于单测，不依赖 `AppHandle`）。
/// 写盘前先读磁盘现值、合并回后端带外管理的字段（见 [`merge_backend_managed_fields`]）。
fn update_settings_at(path: &std::path::Path, mut settings: AppSettings) -> Result<(), String> {
    if let Some(on_disk) = fs::read_to_string(path)
        .ok()
        .and_then(|c| serde_json::from_str::<AppSettings>(&c).ok())
    {
        merge_backend_managed_fields(&mut settings, &on_disk);
    }
    let content = serde_json::to_string_pretty(&settings)
        .map_err(|e| format!("Failed to serialize settings: {}", e))?;
    fs::write(path, content).map_err(|e| format!("Failed to save settings: {}", e))
}

#[tauri::command]
pub fn update_settings(app: AppHandle, settings: AppSettings) -> Result<(), String> {
    let path = get_settings_path(&app)?;
    update_settings_at(&path, settings)
}
