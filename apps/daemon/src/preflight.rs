//! 启动 fail-fast 检查（spec §6.3）。
//!
//! 6 条 check：
//! 1. [`check_root`]：`--root` 存在且是目录、可 `read_dir`。
//! 2. [`check_data_dir`]：`--data-dir` 父目录可写（创建并删 probe 文件）。
//! 3. [`check_token`]：`--token` 长度 ≥ 32（spec 安全硬规则）。
//! 4. [`check_model`]：embedder GGUF 文件存在且是文件。
//! 5. [`check_rebuild_leftover`]：`<data-dir>/index.db.rebuild` 或 `index.db.old`
//!    残留 → 默认拒绝启动，要求 `--allow-rebuild-schema` 显式清理（spec §5.3
//!    atomic swap 中断恢复）。
//! 6. **bind 端口** 留 [`lifecycle::serve`] 真 `TcpListener::bind` 时报错——
//!    `try_bind + drop` 双绑会有 TOCTOU 风险，且 axum 会直接告诉我们错误码，
//!    不在此重复 try。
//!
//! **`SQLite` schema 版本一致性**：daemon 首次启动 db 不存在，[`MusicIndex::open`]
//! 与 [`DocumentIndex::open`] 会自动 [`ensure_schema_version`] 写入；
//! 老 db 打开时同函数 `INSERT OR IGNORE` 也不会覆盖。真正的"版本不匹配 → 退出"
//! 校验在 schema bump 后（[`INDEXER_SCHEMA_VERSION`] 升级）才有意义，目前仅 `"1"`、
//! [`check_rebuild_leftover`] 就是 spec §6.3 列表里的实际可生效"残留检查"。
//!
//! [`lifecycle::serve`]: crate::lifecycle::serve
//! [`MusicIndex::open`]: scout_indexer::MusicIndex::open
//! [`DocumentIndex::open`]: scout_indexer::DocumentIndex::open
//! [`ensure_schema_version`]: scout_indexer::ensure_schema_version
//! [`INDEXER_SCHEMA_VERSION`]: scout_indexer::INDEXER_SCHEMA_VERSION

use std::path::Path;

use anyhow::{anyhow, Context, Result};

/// 校验 `--root`：存在 + 是目录 + 可 `read_dir`。
pub fn check_root(root: &Path) -> Result<()> {
    if !root.exists() {
        return Err(anyhow!("root 目录不存在：{}", root.display()));
    }
    if !root.is_dir() {
        return Err(anyhow!("root 不是目录：{}", root.display()));
    }
    // 可读性探针：read_dir 失败说明权限 / mount 异常。
    std::fs::read_dir(root).with_context(|| format!("root 不可读：{}", root.display()))?;
    Ok(())
}

/// 校验 `--data-dir` 父目录可写（写入 + 删除 probe）。
///
/// `data_dir` 本身可以不存在（首次启动会由 indexer `open` 自建）；父目录必须存在
/// 或可创建、且可写。
pub fn check_data_dir(data_dir: &Path) -> Result<()> {
    let parent = data_dir
        .parent()
        .ok_or_else(|| anyhow!("data_dir 无父目录：{}", data_dir.display()))?;
    if !parent.as_os_str().is_empty() && !parent.exists() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("data_dir 父目录创建失败：{}", parent.display()))?;
    }
    // 选定 probe 落点：data_dir 已存在就在它里面写，否则落在父目录。
    let probe_dir = if data_dir.exists() { data_dir } else { parent };
    let probe = probe_dir.join(".scoutd-write-probe");
    std::fs::write(&probe, b"x")
        .with_context(|| format!("data_dir 父目录不可写：{}", probe_dir.display()))?;
    std::fs::remove_file(&probe)
        .with_context(|| format!("data_dir probe 清理失败：{}", probe.display()))?;
    Ok(())
}

/// 校验 `--token` 长度 ≥ 32（spec §6.2 安全硬规则）。
pub fn check_token(token: &str) -> Result<()> {
    if token.len() < 32 {
        return Err(anyhow!("token 长度必须 ≥ 32 字符（当前 {}）", token.len()));
    }
    Ok(())
}

/// 校验 embedder GGUF 文件存在且是文件。
pub fn check_model(model_path: &Path) -> Result<()> {
    if !model_path.exists() {
        return Err(anyhow!(
            "embedder model 文件不存在：{}",
            model_path.display()
        ));
    }
    if !model_path.is_file() {
        return Err(anyhow!("embedder model 不是文件：{}", model_path.display()));
    }
    Ok(())
}

/// 校验 reindex 中断残留（spec §5.3 atomic swap 恢复）。
///
/// `index.db.rebuild` 或 `index.db.old` 存在 →
/// - `allow=false`（默认）：拒绝启动，提示加 `--allow-rebuild-schema` 重启清理；
/// - `allow=true`：删 leftover 后继续。
///
/// 不存在 → no-op。
pub fn check_rebuild_leftover(data_dir: &Path, allow: bool) -> Result<()> {
    let rebuild = data_dir.join("index.db.rebuild");
    let old = data_dir.join("index.db.old");
    let has_rebuild = rebuild.exists();
    let has_old = old.exists();
    if !has_rebuild && !has_old {
        return Ok(());
    }
    if !allow {
        return Err(anyhow!(
            "检测到 reindex 中断残留（{} / {}），重启加 --allow-rebuild-schema 清理",
            rebuild.display(),
            old.display()
        ));
    }
    if has_rebuild {
        std::fs::remove_file(&rebuild)
            .with_context(|| format!("清理 rebuild 残留失败：{}", rebuild.display()))?;
    }
    if has_old {
        std::fs::remove_file(&old)
            .with_context(|| format!("清理 old 残留失败：{}", old.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn check_root_ok() {
        let d = tempdir().unwrap();
        check_root(d.path()).unwrap();
    }

    #[test]
    fn check_root_missing_fails() {
        let p = Path::new("/nonexistent/zzzzz-scoutd-preflight");
        assert!(check_root(p).is_err());
    }

    #[test]
    fn check_token_min_length() {
        assert!(check_token("short").is_err());
        let long = "a".repeat(32);
        assert!(check_token(&long).is_ok());
    }

    #[test]
    fn check_rebuild_leftover_blocks_without_flag() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("index.db.rebuild"), b"x").unwrap();
        // 默认 allow=false：报错。
        assert!(check_rebuild_leftover(d.path(), false).is_err());
        // allow=true：清理 + 通过。
        assert!(check_rebuild_leftover(d.path(), true).is_ok());
        assert!(!d.path().join("index.db.rebuild").exists());
    }
}
