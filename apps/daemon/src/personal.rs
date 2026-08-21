//! BETA-78：个人模式自举——桌面安装器装机时调用，生成 scoutd 的默认单集合
//! 配置（复用 `DaemonConfigFile::personal_local` 同款语义：单 collection、
//! 全权 admin token）、随机 token、`connection.json`（desktop 客户端发现
//! service 用，见 `docs/`）；首次启动若本地无 embedding 模型，自动下载。
//!
//! **幂等**：`config.toml` 已存在即视为"已 bootstrap"，直接跳过（重装/升级
//! 场景不重复生成、不吞掉用户后续在设置页调整过的 roots）。
//!
//! TOML 手写序列化（不复用 `DaemonConfigFile` 的 `Deserialize`）：`TokenConfig.token`
//! 是 `secrecy::SecretString`，故意不 derive `Serialize`（防止意外序列化泄漏），
//! 本模块用局部镜像结构体显式生成明文 TOML——这是 bootstrap 唯一需要看见明文
//! token 的地方。

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rand::distributions::{Alphanumeric, DistString};
use serde::Serialize;
use tracing::{info, warn};

/// 个人模式默认监听地址：**loopback-only**——不同于团队模式默认的 `0.0.0.0`。
/// 个人数据默认不上网是 PROJECT.md 的核心原则，这里必须显式收紧，不能沿用
/// 团队模式的 LAN 默认值。直接构造（不走字符串 `parse()`）——常量地址没有
/// 运行时失败的可能，没必要为它引入一条 `expect()`/`unwrap()`。
#[must_use]
pub fn personal_bind_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8765))
}

/// embedding 模型文件名——与桌面端 `search::embedding_model::DEFAULT_EMBED_MODEL_FILE`
/// 保持一致（故意本地重声明一份，避免 daemon 反向依赖桌面 crate）。
const EMBED_MODEL_FILE: &str = "embeddinggemma-300m-q8_0.gguf";
const EMBEDDING_HF_URL: &str = "https://huggingface.co/ggml-org/embeddinggemma-300M-qat-q8_0-gguf/resolve/main/embeddinggemma-300m-qat-Q8_0.gguf?download=true";
/// HF 主源在部分网络（尤其中国大陆）直连挂起/超时/被拦——`hf-mirror.com` 是同路径
/// 结构的公开镜像，与桌面端 `model_download.rs::ModelKind::urls()` 同款兜底策略。
/// `LocalSystem` 服务进程的网络路径（代理/DNS）往往不同于交互用户会话（真机反馈：
/// 用户本人网络能连 HF，服务账户下却不通），主源必挂一次镜像更是刚需，非锦上添花。
fn embedding_urls() -> [&'static str; 2] {
    const MIRROR: &str = "https://hf-mirror.com/ggml-org/embeddinggemma-300M-qat-q8_0-gguf/resolve/main/embeddinggemma-300m-qat-Q8_0.gguf?download=true";
    [EMBEDDING_HF_URL, MIRROR]
}

/// 个人模式默认数据目录：`%ProgramData%\Scout\scoutd`（Windows）；非 Windows
/// 回退临时目录（个人模式 service 化本期只做 Windows，非 Windows 分支只为让
/// 单元测试能跑）。
#[must_use]
pub fn default_data_dir() -> PathBuf {
    if cfg!(windows) {
        let program_data =
            std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".to_string());
        PathBuf::from(program_data).join("Scout").join("scoutd")
    } else {
        std::env::temp_dir().join("scout-personal")
    }
}

/// 当前用户的系统默认索引目录（Desktop/Documents/Downloads/Pictures/Music），
/// 只保留实际存在的（避免喂给 indexer 不存在的 root——`preflight::check_root`
/// 会直接拒绝启动）。与桌面端 `settings::system_default_roots` 同源语义
/// （BETA-06 起的既有默认三/五夹约定）。
#[must_use]
pub fn default_roots() -> Vec<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    let Some(home) = home else {
        warn!("无法确定当前用户 home 目录（USERPROFILE/HOME 均未设置），个人模式默认 roots 为空");
        return Vec::new();
    };
    ["Desktop", "Documents", "Downloads", "Pictures", "Music"]
        .into_iter()
        .map(|d| home.join(d))
        .filter(|p| p.is_dir())
        .collect()
}

#[derive(Serialize)]
struct TomlCollection {
    id: String,
    display_name: String,
    subject_kind: &'static str,
    roots: Vec<String>,
    read_only: bool,
    audit_tags: Vec<String>,
    allow_full_read: bool,
}

#[derive(Serialize)]
struct TomlToken {
    token: String,
    subject: String,
    collections: Vec<String>,
    admin: bool,
}

#[derive(Serialize)]
struct TomlAudit {
    log_query: bool,
}

#[derive(Serialize)]
struct TomlConfig {
    collections: Vec<TomlCollection>,
    tokens: Vec<TomlToken>,
    audit: TomlAudit,
}

/// 生成个人模式随机 token（≥ `collections::MIN_TOKEN_LEN`，取 48 字符留余量）。
#[must_use]
pub fn generate_token() -> String {
    Alphanumeric.sample_string(&mut rand::thread_rng(), 48)
}

/// 幂等生成个人模式 `config.toml`：已存在则跳过（返回 `Ok(false)`）；否则生成
/// 单 collection（`id = "default"`，镜像 `DaemonConfigFile::personal_local`
/// 语义）+ 随机 token 写盘（返回 `Ok(true)`）。
pub fn bootstrap_config(data_dir: &Path, roots: &[PathBuf]) -> Result<bool> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("创建数据目录失败：{}", data_dir.display()))?;
    let config_path = data_dir.join("config.toml");
    if config_path.exists() {
        info!(path = %config_path.display(), "个人模式配置已存在，跳过 bootstrap");
        return Ok(false);
    }

    let roots: Vec<PathBuf> = if roots.is_empty() {
        default_roots()
    } else {
        roots.to_vec()
    };
    if roots.is_empty() {
        warn!("个人模式 bootstrap 未解析到任何默认索引目录——生成的配置 roots 为空，用户需在设置页手动添加");
    }

    let token = generate_token();
    let cfg = TomlConfig {
        collections: vec![TomlCollection {
            id: "default".to_string(),
            display_name: "本机文件".to_string(),
            subject_kind: "other",
            roots: roots
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            read_only: false,
            audit_tags: Vec::new(),
            allow_full_read: true,
        }],
        tokens: vec![TomlToken {
            token: token.clone(),
            subject: "local".to_string(),
            collections: vec!["*".to_string()],
            admin: true,
        }],
        audit: TomlAudit { log_query: true },
    };

    let toml_text = toml::to_string_pretty(&cfg).context("序列化个人模式配置为 TOML 失败")?;
    write_atomic(&config_path, toml_text.as_bytes())
        .with_context(|| format!("写入配置文件失败：{}", config_path.display()))?;
    info!(
        path = %config_path.display(),
        roots = roots.len(),
        "个人模式配置已生成"
    );
    Ok(true)
}

/// `connection.json`：desktop 客户端发现本机 service 用——bind 地址 + token +
/// pid + 启动时间。每次服务成功 bind 后覆写（bind 地址理论上不变，但覆写代价
/// 极低、能兼容用户手改 config.toml 换端口的场景）。
#[derive(Serialize)]
struct ConnectionFile<'a> {
    bind: &'a str,
    token: &'a str,
    pid: u32,
    started_at: String,
}

pub fn write_connection_file(data_dir: &Path, bind: SocketAddr, token: &str) -> Result<()> {
    let payload = ConnectionFile {
        bind: &bind.to_string(),
        token,
        pid: std::process::id(),
        started_at: chrono::Utc::now().to_rfc3339(),
    };
    let text = serde_json::to_string_pretty(&payload).context("序列化 connection.json 失败")?;
    let path = data_dir.join("connection.json");
    write_atomic(&path, text.as_bytes())
        .with_context(|| format!("写入 connection.json 失败：{}", path.display()))?;
    Ok(())
}

/// 原子写：先写临时文件再 rename，避免并发读到半截内容（`connection.json`
/// 会被 desktop 频繁轮询读取）。
fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let tmp_path = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// 确保 embedding 模型文件存在于 `<data_dir>/models/<file>.gguf`；不存在则
/// 同步阻塞下载（调用方已在 `spawn_blocking` 或专用线程里跑，避免占用
/// tokio worker）。主源 + `hf-mirror.com` 镜像依次重试（见 [`embedding_urls`]）。
///
/// **下载失败不阻塞服务启动**：两个源都失败只 warn，返回目标路径本身（文件可能
/// 不存在）——调用方（`build_personal_service`）据此继续走 FTS-only 降级路径，而
/// 不是让整个 Windows Service 因为一次网络请求失败而无法启动。这是本函数唯一
/// 会返回 `Err` 的分支收窄到"本地模型目录都建不出来"这类真正致命的情形。
pub async fn ensure_embedding_model(data_dir: &Path) -> Result<PathBuf> {
    let models_dir = data_dir.join("models");
    let target = models_dir.join(EMBED_MODEL_FILE);
    if target.is_file() {
        return Ok(target);
    }
    std::fs::create_dir_all(&models_dir)
        .with_context(|| format!("创建模型目录失败：{}", models_dir.display()))?;

    match download_embedding_model(&target).await {
        Ok(()) => info!(path = %target.display(), "embedding 模型下载完成"),
        Err(e) => warn!(
            error = %e,
            "embedding 模型下载失败（主源 + hf-mirror 均已尝试）；服务将以 FTS-only \
             模式启动（语义召回禁用），不影响关键词检索。可稍后重启服务自动重试下载，\
             或手动放置模型文件到：{}",
            target.display()
        ),
    }
    Ok(target)
}

/// 依次尝试 [`embedding_urls`] 的每个源，全部失败才返回 `Err`（聚合最后一个错误，
/// 调用方只 warn 不中断，具体哪个源失败对用户没有可操作价值、不逐个保留）。
async fn download_embedding_model(target: &Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .context("构造下载 HTTP client 失败")?;

    let mut last_err = None;
    for url in embedding_urls() {
        info!(url, "个人模式首启：本地无 embedding 模型，开始下载");
        match try_download(&client, url, target).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                warn!(url, error = %e, "该源下载失败，尝试下一个源（如有）");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("无可用下载源")))
}

async fn try_download(client: &reqwest::Client, url: &str, target: &Path) -> Result<()> {
    let resp = client
        .get(url)
        .send()
        .await
        .context("下载模型请求失败")?
        .error_for_status()
        .context("下载模型收到非 2xx 响应")?;

    let tmp_path = target.with_extension("gguf.partial");
    {
        use futures_util::StreamExt as _;
        use tokio::io::AsyncWriteExt as _;
        let mut file = tokio::fs::File::create(&tmp_path)
            .await
            .with_context(|| format!("创建临时下载文件失败：{}", tmp_path.display()))?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("下载流中断")?;
            file.write_all(&chunk).await.context("写入模型文件失败")?;
        }
        file.flush().await.context("flush 模型文件失败")?;
    }
    tokio::fs::rename(&tmp_path, target)
        .await
        .context("重命名下载完成的模型文件失败")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use tempfile::tempdir;

    #[test]
    fn bootstrap_config_is_idempotent() {
        let dir = tempdir().unwrap();
        let roots = vec![dir.path().to_path_buf()];
        assert!(
            bootstrap_config(dir.path(), &roots).unwrap(),
            "首次生成应返回 true"
        );
        assert!(
            !bootstrap_config(dir.path(), &roots).unwrap(),
            "已存在应跳过、返回 false"
        );
    }

    #[test]
    fn bootstrap_config_writes_parseable_toml() {
        let dir = tempdir().unwrap();
        let roots = vec![dir.path().to_path_buf()];
        bootstrap_config(dir.path(), &roots).unwrap();
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        let cfg = scout_server::collections::parse_config_toml(&text).unwrap();
        assert_eq!(cfg.collections.len(), 1);
        assert_eq!(cfg.collections[0].id, "default");
        assert!(cfg.collections[0].allow_full_read);
        assert_eq!(cfg.tokens.len(), 1);
        assert!(cfg.tokens[0].admin);
    }

    #[test]
    fn generate_token_meets_min_length() {
        let t = generate_token();
        assert!(t.len() >= 32, "token 长度必须 ≥ 32：{}", t.len());
    }

    #[test]
    fn write_connection_file_roundtrips() {
        let dir = tempdir().unwrap();
        write_connection_file(dir.path(), "127.0.0.1:8765".parse().unwrap(), "tok").unwrap();
        let text = std::fs::read_to_string(dir.path().join("connection.json")).unwrap();
        assert!(text.contains("127.0.0.1:8765"));
        assert!(text.contains("\"token\": \"tok\""));
    }
}
