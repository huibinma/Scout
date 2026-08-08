//! BETA-36：归档集合（collection）与 per-collection token 权限的配置模型。
//!
//! TOML 声明式 `[[collections]]` + `[[tokens]]` + `[audit]`；legacy `--root`/`--token`
//! 启动经 [`DaemonConfigFile::legacy_single_root`] 合成 default collection + 全权 admin
//! token，现有部署零迁移。
//!
//! **collection id 即路径段**（`data_dir/collections/<id>/index.db`），字符集严格限
//! `[a-z0-9-]` 且不以 `-` 开头，杜绝路径注入。

use std::collections::BTreeSet;
use std::path::PathBuf;

use secrecy::SecretString;
use serde::Deserialize;

/// legacy 单根模式合成的 collection id / token subject。
pub const LEGACY_COLLECTION_ID: &str = "default";
/// token 最短长度（与 daemon preflight `check_token` 一致，spec §6.2 安全硬规则）。
pub const MIN_TOKEN_LEN: usize = 32;

/// 归档主体类型（验收 ②：案件 / 员工 / 审计项目边界）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    /// 律所案件。
    Case,
    /// 离职员工归档。
    Employee,
    /// 审计项目。
    AuditProject,
    /// 其他 / 未分类。
    #[default]
    Other,
}

/// 一个归档集合：root 分组 + 归档主体边界 + 显示名 + 只读态 + 审计标签。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionConfig {
    /// 唯一 slug（`[a-z0-9-]`，不以 `-` 开头）；同时是索引目录路径段。
    pub id: String,
    /// 人类可读显示名；缺省用 id。
    #[serde(default)]
    pub display_name: Option<String>,
    /// 归档主体类型。
    #[serde(default)]
    pub subject_kind: SubjectKind,
    /// 集合包含的根目录（≥1）。
    pub roots: Vec<PathBuf>,
    /// 只读态（冷冻归档）：true 时 admin reindex 拒绝（409）。
    #[serde(default)]
    pub read_only: bool,
    /// 审计标签（进 audit 记录与 `list_collections` 输出）。
    #[serde(default)]
    pub audit_tags: Vec<String>,
    /// 是否允许读取类工具（`read_document`）返回全文（BETA-43 验收 ②）。
    /// TOML 缺省 **false**（企业信息墙姿态：禁全文时读取类工具仅返回命中片段 +
    /// 有限上下文窗口）；legacy 单根模式合成 true（单钥全权 admin 的个人/小队形态）。
    #[serde(default)]
    pub allow_full_read: bool,
}

impl CollectionConfig {
    /// 显示名：配置缺省时回退 id。
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.id)
    }
}

/// 一条 token 声明：token → subject（audit 留痕主体）+ 授权 collection + admin 标志。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenConfig {
    /// bearer token 明文（`SecretString`：Debug 输出 REDACTED、不泄漏进日志）。
    pub token: SecretString,
    /// audit 留痕主体（谁在检索），必填非空。
    pub subject: String,
    /// 授权 collection id 列表；含 `"*"` 表示全权。
    pub collections: Vec<String>,
    /// admin=true 才能调 `/admin/*`（reindex / audit 导出）。
    #[serde(default)]
    pub admin: bool,
}

/// token 的 collection 授权范围。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectionGrant {
    /// 全权（配置里写 `"*"`）。
    All,
    /// 指定 collection id 集合。
    Listed(BTreeSet<String>),
}

impl CollectionGrant {
    /// 从配置 patterns 构造：任一项为 `"*"` → 全权。
    #[must_use]
    pub fn from_patterns(patterns: &[String]) -> Self {
        if patterns.iter().any(|p| p == "*") {
            Self::All
        } else {
            Self::Listed(patterns.iter().cloned().collect())
        }
    }

    /// 是否授权访问指定 collection。
    #[must_use]
    pub fn allows(&self, collection_id: &str) -> bool {
        match self {
            Self::All => true,
            Self::Listed(ids) => ids.contains(collection_id),
        }
    }
}

/// `[audit]` 配置段。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditConfig {
    /// audit 记录是否含 query 明文（spec §7：默认 true——审计取证的核心诉求；
    /// false 时降级只记 query 长度）。
    #[serde(default = "default_true")]
    pub log_query: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self { log_query: true }
    }
}

/// daemon TOML 配置文件全貌（`--config` 消费；与 `--root`/`--token` 互斥由 daemon CLI 层把守）。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfigFile {
    /// 归档集合声明（≥1）。
    #[serde(default)]
    pub collections: Vec<CollectionConfig>,
    /// token 声明（≥1）。
    #[serde(default)]
    pub tokens: Vec<TokenConfig>,
    /// audit 配置。
    #[serde(default)]
    pub audit: AuditConfig,
}

/// 归一 root 路径分隔符到原生形态（Windows：`/` → `\`）。
///
/// 动机：`--root C:/foo`（或 TOML `roots = ['C:/foo']`）这类正斜杠输入，经
/// indexer `root.join(file_name)` 会拼出**混合分隔符** `C:/foo\bar.png` 存进
/// `documents.path`；而 `search` 命中 path 走 `canonicalize` 产出全反斜杠
/// `\\?\C:\foo\bar.png`，[`crate::provenance::lookup_candidates`] 只 strip `\\?\`、
/// 不改分隔符 → `read_document` 精确查库落空（"document not found"）。在 root 入口
/// 归一为原生分隔符即让 `documents.path` 一致、round-trip 成立。`components()`
/// 同时会正规化冗余分隔符与 `.`；空路径兜底返回原值。
#[must_use]
fn normalize_root(root: PathBuf) -> PathBuf {
    let normalized: PathBuf = root.components().collect();
    if normalized.as_os_str().is_empty() {
        root
    } else {
        normalized
    }
}

impl DaemonConfigFile {
    /// legacy 单根模式合成：`--root`/`--token` → default collection（读写）+
    /// subject=`default` 全权 admin token。现有部署零迁移。
    #[must_use]
    pub fn legacy_single_root(root: PathBuf, token: SecretString) -> Self {
        Self {
            collections: vec![CollectionConfig {
                id: LEGACY_COLLECTION_ID.to_string(),
                display_name: None,
                subject_kind: SubjectKind::Other,
                roots: vec![normalize_root(root)],
                read_only: false,
                audit_tags: Vec::new(),
                allow_full_read: true,
            }],
            tokens: vec![TokenConfig {
                token,
                subject: LEGACY_COLLECTION_ID.to_string(),
                collections: vec!["*".to_string()],
                admin: true,
            }],
            audit: AuditConfig::default(),
        }
    }

    /// 桌面「本机 MCP 服务」变体（BETA-53）：把桌面已建的本机索引经 MCP 暴露给
    /// 本机 LLM 客户端。与 [`Self::legacy_single_root`] 同为单 `default` 集合 + 全权
    /// admin token，区别是**支持多 root**——桌面通常索引多个目录（自定义 + 系统默认
    /// 三夹），逐一归一分隔符后放进同一集合的 roots，让 `list_collections` 如实反映
    /// 索引范围。`allow_full_read=true`：自己的文件、自己的 agent，默认放开全文
    /// （设计见当时的评审记录 §5.4）。
    ///
    /// roots 仅供 `list_collections` 展示与 audit 留痕——读取面由索引 db 边界天然约束
    /// （`read_document` 内容取自索引、不触磁盘，roots 之外的文件不在库里即不可达）。
    #[must_use]
    pub fn personal_local(roots: Vec<PathBuf>, token: SecretString) -> Self {
        Self {
            collections: vec![CollectionConfig {
                id: LEGACY_COLLECTION_ID.to_string(),
                display_name: Some("本机文件".to_string()),
                subject_kind: SubjectKind::Other,
                roots: roots.into_iter().map(normalize_root).collect(),
                read_only: false,
                audit_tags: Vec::new(),
                allow_full_read: true,
            }],
            tokens: vec![TokenConfig {
                token,
                subject: "local".to_string(),
                collections: vec!["*".to_string()],
                admin: true,
            }],
            audit: AuditConfig::default(),
        }
    }

    /// 按 id 查 collection。
    #[must_use]
    pub fn collection(&self, id: &str) -> Option<&CollectionConfig> {
        self.collections.iter().find(|c| c.id == id)
    }
}

/// 配置解析 / 校验错误。**错误信息不含 token 内容**（可进日志与启动 stderr）。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// TOML 语法 / 结构错误。
    #[error("TOML 解析失败：{0}")]
    Toml(#[from] toml::de::Error),
    /// 语义校验失败（id 非法 / 引用悬空 / 空列表等）。
    #[error("配置校验失败：{0}")]
    Invalid(String),
}

/// collection id 字符集校验：`[a-z0-9-]+` 且不以 `-` 开头（id 会拼进索引目录路径）。
#[must_use]
pub fn is_valid_collection_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// 解析 + 校验 daemon TOML 配置。
///
/// 校验规则：
/// - `collections` 非空；id 合法（[`is_valid_collection_id`]）且互不重复；每个 `roots` 非空
/// - `tokens` 非空；token 长度 ≥ [`MIN_TOKEN_LEN`]；`subject` 非空；`collections`
///   授权列表非空且（`"*"` 之外）全部指向已声明的 collection id
///
/// # Errors
///
/// TOML 语法错误返 [`ConfigError::Toml`]；语义校验失败返 [`ConfigError::Invalid`]
/// （信息不含 token 内容）。
pub fn parse_config_toml(text: &str) -> Result<DaemonConfigFile, ConfigError> {
    let mut cfg: DaemonConfigFile = toml::from_str(text)?;
    // root 分隔符归一（详见 [`normalize_root`]）：正斜杠 TOML root 在 Windows 上
    // 否则会让 search→read_document round-trip 落空。
    for c in &mut cfg.collections {
        c.roots = c.roots.drain(..).map(normalize_root).collect();
    }
    validate(&cfg)?;
    Ok(cfg)
}

fn validate(cfg: &DaemonConfigFile) -> Result<(), ConfigError> {
    use secrecy::ExposeSecret;

    if cfg.collections.is_empty() {
        return Err(ConfigError::Invalid(
            "至少声明一个 [[collections]]".to_string(),
        ));
    }
    let mut ids: BTreeSet<&str> = BTreeSet::new();
    for c in &cfg.collections {
        if !is_valid_collection_id(&c.id) {
            return Err(ConfigError::Invalid(format!(
                "collection id 非法（仅 [a-z0-9-]、不以 - 开头）：{}",
                c.id
            )));
        }
        if !ids.insert(&c.id) {
            return Err(ConfigError::Invalid(format!(
                "collection id 重复：{}",
                c.id
            )));
        }
        if c.roots.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "collection {} 的 roots 不能为空",
                c.id
            )));
        }
    }

    if cfg.tokens.is_empty() {
        return Err(ConfigError::Invalid("至少声明一个 [[tokens]]".to_string()));
    }
    for (i, t) in cfg.tokens.iter().enumerate() {
        if t.token.expose_secret().len() < MIN_TOKEN_LEN {
            return Err(ConfigError::Invalid(format!(
                "第 {} 条 token 长度必须 ≥ {MIN_TOKEN_LEN} 字符（subject={}）",
                i + 1,
                t.subject
            )));
        }
        if t.subject.trim().is_empty() {
            return Err(ConfigError::Invalid(format!(
                "第 {} 条 token 的 subject 不能为空",
                i + 1
            )));
        }
        if t.collections.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "token（subject={}）的 collections 授权列表不能为空",
                t.subject
            )));
        }
        for cid in &t.collections {
            if cid != "*" && !ids.contains(cid.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "token（subject={}）授权了未声明的 collection：{cid}",
                    t.subject
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use secrecy::ExposeSecret;

    const TOKEN_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"; // 32 chars

    fn two_collection_toml() -> String {
        format!(
            r#"
[[collections]]
id = "case-blueharbor"
display_name = "蓝湾贸易合同纠纷案"
subject_kind = "case"
roots = ["/archive/cases/blueharbor"]
read_only = true
audit_tags = ["lawfirm"]
allow_full_read = true

[[collections]]
id = "offboarding-lishili"
subject_kind = "employee"
roots = ["/archive/offboarding/lishili", "/archive/offboarding/lishili-mail"]

[[tokens]]
token = "{TOKEN_A}"
subject = "zhang.san"
collections = ["case-blueharbor"]

[[tokens]]
token = "{TOKEN_A}bbbb"
subject = "ops"
collections = ["*"]
admin = true

[audit]
log_query = false
"#
        )
    }

    #[test]
    fn parse_full_config_happy_path() {
        let cfg = parse_config_toml(&two_collection_toml()).unwrap();
        assert_eq!(cfg.collections.len(), 2);
        let case = cfg.collection("case-blueharbor").unwrap();
        assert_eq!(case.display_name(), "蓝湾贸易合同纠纷案");
        assert_eq!(case.subject_kind, SubjectKind::Case);
        assert!(case.read_only);
        assert_eq!(case.audit_tags, vec!["lawfirm"]);
        assert!(case.allow_full_read);
        let off = cfg.collection("offboarding-lishili").unwrap();
        assert!(
            !off.allow_full_read,
            "TOML 缺省 allow_full_read 应为 false（禁全文姿态）"
        );
        assert_eq!(
            off.display_name(),
            "offboarding-lishili",
            "缺省显示名回退 id"
        );
        assert_eq!(off.subject_kind, SubjectKind::Employee);
        assert!(!off.read_only);
        assert_eq!(off.roots.len(), 2, "roots 支持多目录归组");
        assert_eq!(cfg.tokens.len(), 2);
        assert_eq!(cfg.tokens[0].subject, "zhang.san");
        assert!(!cfg.tokens[0].admin);
        assert!(cfg.tokens[1].admin);
        assert!(!cfg.audit.log_query);
    }

    #[test]
    fn grant_from_patterns_and_allows() {
        let g = CollectionGrant::from_patterns(&["a".to_string(), "b".to_string()]);
        assert!(g.allows("a"));
        assert!(g.allows("b"));
        assert!(!g.allows("c"));
        let all = CollectionGrant::from_patterns(&["a".to_string(), "*".to_string()]);
        assert_eq!(all, CollectionGrant::All);
        assert!(all.allows("anything"));
    }

    #[test]
    fn legacy_single_root_synthesizes_default_admin() {
        let cfg = DaemonConfigFile::legacy_single_root(
            PathBuf::from("/archive"),
            SecretString::from(TOKEN_A.to_string()),
        );
        assert_eq!(cfg.collections.len(), 1);
        assert_eq!(cfg.collections[0].id, LEGACY_COLLECTION_ID);
        assert!(!cfg.collections[0].read_only);
        assert!(
            cfg.collections[0].allow_full_read,
            "legacy 单根合成集合应允许全文（单钥全权 admin 形态）"
        );
        assert_eq!(cfg.tokens.len(), 1);
        assert!(cfg.tokens[0].admin);
        assert_eq!(
            CollectionGrant::from_patterns(&cfg.tokens[0].collections),
            CollectionGrant::All
        );
        assert_eq!(cfg.tokens[0].token.expose_secret(), TOKEN_A);
        assert!(cfg.audit.log_query, "legacy 模式 audit 缺省开");
    }

    #[test]
    fn normalize_root_yields_native_separators() {
        // 正斜杠 root 输入应归一为原生分隔符，避免 documents.path 混合分隔符导致
        // search→read_document round-trip 落空（bug：读取报 document not found）。
        let n = normalize_root(PathBuf::from("C:/Users/x/Downloads"));
        #[cfg(windows)]
        {
            assert_eq!(n.to_string_lossy(), r"C:\Users\x\Downloads");
            assert!(
                !n.to_string_lossy().contains('/'),
                "Windows 归一后不应残留正斜杠"
            );
        }
        #[cfg(not(windows))]
        {
            // Unix 上正斜杠即原生；components 正规化后语义不变。
            assert_eq!(n.to_string_lossy(), "C:/Users/x/Downloads");
        }
        assert_eq!(normalize_root(n.clone()), n, "归一应幂等");
    }

    #[test]
    fn legacy_single_root_normalizes_forward_slash_root() {
        let cfg = DaemonConfigFile::legacy_single_root(
            PathBuf::from("C:/data/archive"),
            SecretString::from(TOKEN_A.to_string()),
        );
        let stored = cfg.collections[0].roots[0].to_string_lossy().into_owned();
        #[cfg(windows)]
        assert_eq!(stored, r"C:\data\archive");
        #[cfg(not(windows))]
        assert_eq!(stored, "C:/data/archive");
    }

    /// BETA-53 桌面变体：多 root 全部逐一归一分隔符入同一 default 集合、
    /// 全权 admin token、`allow_full_read=true`（个人单库姿态）。
    #[test]
    fn personal_local_multi_root_normalized_and_full_read() {
        let cfg = DaemonConfigFile::personal_local(
            vec![
                PathBuf::from("C:/Users/x/Documents"),
                PathBuf::from("D:/media/photos"),
            ],
            SecretString::from(TOKEN_A.to_string()),
        );
        assert_eq!(cfg.collections.len(), 1);
        let c = &cfg.collections[0];
        assert_eq!(c.id, LEGACY_COLLECTION_ID);
        assert_eq!(c.roots.len(), 2, "两个 root 都应保留");
        assert!(c.allow_full_read, "个人单库默认放开全文");
        assert!(!c.read_only);
        #[cfg(windows)]
        {
            assert_eq!(c.roots[0].to_string_lossy(), r"C:\Users\x\Documents");
            assert_eq!(c.roots[1].to_string_lossy(), r"D:\media\photos");
        }
        assert_eq!(cfg.tokens.len(), 1);
        assert!(cfg.tokens[0].admin, "个人变体单钥全权");
        assert_eq!(
            CollectionGrant::from_patterns(&cfg.tokens[0].collections),
            CollectionGrant::All
        );
        assert!(cfg.audit.log_query, "个人变体 audit 缺省开");
    }

    #[test]
    fn collection_id_charset_enforced() {
        assert!(is_valid_collection_id("case-2026-a1"));
        assert!(!is_valid_collection_id(""));
        assert!(!is_valid_collection_id("-leading"));
        assert!(!is_valid_collection_id("Upper"));
        assert!(!is_valid_collection_id("has space"));
        assert!(!is_valid_collection_id("dot./slash"));
        assert!(!is_valid_collection_id("../escape"));
    }

    #[test]
    fn invalid_id_rejected() {
        let toml = format!(
            r#"
[[collections]]
id = "../evil"
roots = ["/x"]
[[tokens]]
token = "{TOKEN_A}"
subject = "s"
collections = ["*"]
"#
        );
        let err = parse_config_toml(&toml).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)), "{err}");
    }

    #[test]
    fn duplicate_id_rejected() {
        let toml = format!(
            r#"
[[collections]]
id = "a"
roots = ["/x"]
[[collections]]
id = "a"
roots = ["/y"]
[[tokens]]
token = "{TOKEN_A}"
subject = "s"
collections = ["*"]
"#
        );
        assert!(matches!(
            parse_config_toml(&toml).unwrap_err(),
            ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn dangling_grant_rejected() {
        let toml = format!(
            r#"
[[collections]]
id = "a"
roots = ["/x"]
[[tokens]]
token = "{TOKEN_A}"
subject = "s"
collections = ["nonexistent"]
"#
        );
        let err = parse_config_toml(&toml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nonexistent"), "{msg}");
    }

    #[test]
    fn short_token_rejected_without_leaking_token() {
        let toml = r#"
[[collections]]
id = "a"
roots = ["/x"]
[[tokens]]
token = "short-secret"
subject = "s"
collections = ["*"]
"#;
        let err = parse_config_toml(toml).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, ConfigError::Invalid(_)));
        assert!(
            !msg.contains("short-secret"),
            "错误信息不得回显 token 内容：{msg}"
        );
    }

    #[test]
    fn empty_collections_or_tokens_rejected() {
        assert!(matches!(
            parse_config_toml("").unwrap_err(),
            ConfigError::Invalid(_)
        ));
        let no_tokens = r#"
[[collections]]
id = "a"
roots = ["/x"]
"#;
        assert!(matches!(
            parse_config_toml(no_tokens).unwrap_err(),
            ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn empty_roots_rejected() {
        let toml = format!(
            r#"
[[collections]]
id = "a"
roots = []
[[tokens]]
token = "{TOKEN_A}"
subject = "s"
collections = ["*"]
"#
        );
        assert!(matches!(
            parse_config_toml(&toml).unwrap_err(),
            ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn unknown_fields_rejected() {
        // deny_unknown_fields：typo（如 read_olny）应报 TOML 结构错而非静默忽略。
        let toml = format!(
            r#"
[[collections]]
id = "a"
roots = ["/x"]
read_olny = true
[[tokens]]
token = "{TOKEN_A}"
subject = "s"
collections = ["*"]
"#
        );
        assert!(matches!(
            parse_config_toml(&toml).unwrap_err(),
            ConfigError::Toml(_)
        ));
    }

    #[test]
    fn token_debug_redacted() {
        let cfg = parse_config_toml(&two_collection_toml()).unwrap();
        let dbg = format!("{:?}", cfg.tokens[0]);
        assert!(
            !dbg.contains(TOKEN_A),
            "TokenConfig Debug 不得泄漏 token 明文：{dbg}"
        );
    }
}
