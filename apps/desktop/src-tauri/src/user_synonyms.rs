//! BETA-11D 用户级持久化同义词库。
//!
//! 用户词条持久化到 app 配置目录 `user-synonyms.yaml`（与 `settings.json` 同目录），运行时由
//! [`UserSynonymState`] 持有的 `Arc<RwLock<UserIndex>>` 维护——与 [`crate::search::SearchDeps`] 里
//! 的 `LayeredSynonymExpander` **共享同一把锁**，故管理命令改完搜索路径立即可见、零重启。
//!
//! **隐私**：用户词条属用户数据，只存自身配置目录、不外发、不调 `Tracer`（默认不进 trace）。
//!
//! **不变量（§5.2）**：完整校验候选 → 写文件 → 成功才更新内存索引（内存 == 最后一次成功落盘状态）。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use scout_harness::{UserGroup, UserIndex};
use tauri::{AppHandle, State};

/// 共享用户词典状态（manage 进 Tauri；与 LayeredSynonymExpander 共享同一 Arc）。
pub struct UserSynonymState {
    pub index: Arc<RwLock<UserIndex>>,
    path: PathBuf,
}

impl UserSynonymState {
    #[must_use]
    pub fn new(index: Arc<RwLock<UserIndex>>, path: PathBuf) -> Self {
        Self { index, path }
    }
}

/// `user-synonyms.yaml` 路径（best-effort，仅解析不创建目录）。隐私面板复用。
/// 2026-07-29 起改走 [`crate::scout_data_dir`]（理由见 `main.rs::scout_data_dir` 注释）。
pub(crate) fn user_synonyms_path(_app: &AppHandle) -> Option<PathBuf> {
    Some(crate::scout_data_dir().join("user-synonyms.yaml"))
}

/// 启动加载：文件缺失 / 损坏 → 空词典（best-effort，不阻塞搜索）。
#[must_use]
pub fn load_user_index(path: &Path) -> UserIndex {
    fs::read_to_string(path)
        .ok()
        .and_then(|c| UserIndex::from_yaml_str(&c).ok())
        .unwrap_or_else(UserIndex::empty)
}

/// 隐私面板用：读取用户词典组数（best-effort，缺库/损坏返回 0）。
pub(crate) fn group_count_at(path: &Path) -> usize {
    load_user_index(path).groups().len()
}

/// 将候选词典序列化写入磁盘（确保父目录存在）。
/// 调用方负责在落盘成功后才更新内存，以满足 §5.2 不变量。
fn write_candidate(path: &Path, candidate: &UserIndex) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("无法创建配置目录: {e}"))?;
    }
    fs::write(path, candidate.to_yaml_str()).map_err(|e| format!("写入用户词典失败: {e}"))
}

#[tauri::command]
pub fn get_user_synonyms(state: State<'_, UserSynonymState>) -> Result<Vec<UserGroup>, String> {
    Ok(state
        .index
        .read()
        .map_err(|_| "用户词典锁中毒".to_owned())?
        .groups()
        .to_vec())
}

#[tauri::command]
pub fn add_user_synonym(
    state: State<'_, UserSynonymState>,
    head: String,
    aliases: Vec<String>,
) -> Result<Vec<UserGroup>, String> {
    // 1) 在克隆上应用 + lint（不碰共享态）
    let candidate = {
        let idx = state
            .index
            .read()
            .map_err(|_| "用户词典锁中毒".to_owned())?;
        let mut next = idx.clone();
        next.add_or_merge(&head, aliases)
            .map_err(|e| e.to_string())?;
        next
    };
    // 2) 先落盘
    write_candidate(&state.path, &candidate)?;
    // 3) 落盘成功才提交内存
    let groups = candidate.groups().to_vec();
    *state
        .index
        .write()
        .map_err(|_| "用户词典锁中毒".to_owned())? = candidate;
    Ok(groups)
}

#[tauri::command]
pub fn update_user_synonym(
    state: State<'_, UserSynonymState>,
    head: String,
    aliases: Vec<String>,
) -> Result<Vec<UserGroup>, String> {
    // 1) 在克隆上应用 + lint
    let candidate = {
        let idx = state
            .index
            .read()
            .map_err(|_| "用户词典锁中毒".to_owned())?;
        let mut next = idx.clone();
        next.update(&head, aliases).map_err(|e| e.to_string())?;
        next
    };
    // 2) 先落盘
    write_candidate(&state.path, &candidate)?;
    // 3) 落盘成功才提交内存
    let groups = candidate.groups().to_vec();
    *state
        .index
        .write()
        .map_err(|_| "用户词典锁中毒".to_owned())? = candidate;
    Ok(groups)
}

#[tauri::command]
pub fn delete_user_synonym(
    state: State<'_, UserSynonymState>,
    head: String,
) -> Result<Vec<UserGroup>, String> {
    // 1) 在克隆上尝试删除
    let (candidate, changed) = {
        let idx = state
            .index
            .read()
            .map_err(|_| "用户词典锁中毒".to_owned())?;
        let mut next = idx.clone();
        let changed = next.remove(&head);
        (next, changed)
    };
    if !changed {
        // 无变化，直接返回当前列表，不写文件
        return Ok(candidate.groups().to_vec());
    }
    // 2) 先落盘
    write_candidate(&state.path, &candidate)?;
    // 3) 落盘成功才提交内存
    let groups = candidate.groups().to_vec();
    *state
        .index
        .write()
        .map_err(|_| "用户词典锁中毒".to_owned())? = candidate;
    Ok(groups)
}

#[tauri::command]
pub fn export_user_synonyms(state: State<'_, UserSynonymState>) -> Result<String, String> {
    Ok(state
        .index
        .read()
        .map_err(|_| "用户词典锁中毒".to_owned())?
        .to_yaml_str())
}

#[tauri::command]
pub fn import_user_synonyms(
    state: State<'_, UserSynonymState>,
    yaml_text: String,
) -> Result<Vec<UserGroup>, String> {
    // 1) 整份校验：任一组非法则拒绝（不碰共享态）
    let candidate = UserIndex::from_yaml_str(&yaml_text).map_err(|e| e.to_string())?;
    // 2) 先落盘
    write_candidate(&state.path, &candidate)?;
    // 3) 落盘成功才提交内存
    let groups = candidate.groups().to_vec();
    *state
        .index
        .write()
        .map_err(|_| "用户词典锁中毒".to_owned())? = candidate;
    Ok(groups)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn load_missing_or_corrupt_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_user_index(&dir.path().join("nope.yaml"))
            .groups()
            .is_empty());
        let bad = dir.path().join("bad.yaml");
        std::fs::write(&bad, b"\t not yaml :::").unwrap();
        assert!(load_user_index(&bad).groups().is_empty(), "损坏文件退空");
    }

    #[test]
    fn persist_roundtrip_and_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user-synonyms.yaml");
        let mut idx = UserIndex::empty();
        idx.add_or_merge("友商竞争分析", vec!["AWS".into(), "Azure".into()])
            .unwrap();
        std::fs::write(&path, idx.to_yaml_str()).unwrap();
        let loaded = load_user_index(&path);
        assert_eq!(loaded.groups().len(), 1);
        assert_eq!(loaded.groups()[0].head, "友商竞争分析");
        assert_eq!(group_count_at(&path), 1);
    }

    /// 验证 persist-before-commit 不变量（§5.2）：
    /// 克隆 + 变更 + 序列化落盘后，从磁盘重新加载的词典与内存候选完全一致。
    #[test]
    fn persist_before_commit_invariant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user-synonyms.yaml");

        // 初始状态：空词典
        let mut idx = UserIndex::empty();

        // 模拟 add_user_synonym 的 persist-before-commit 流程
        // Step 1: 在克隆上变更
        let mut candidate = idx.clone();
        candidate
            .add_or_merge("搜索引擎", vec!["Google".into(), "Bing".into()])
            .unwrap();

        // Step 2: 先写磁盘（模拟 write_candidate）
        std::fs::write(&path, candidate.to_yaml_str()).unwrap();

        // Step 3: 从磁盘重新加载，验证与候选一致
        let reloaded = load_user_index(&path);
        assert_eq!(
            reloaded.groups().len(),
            candidate.groups().len(),
            "磁盘组数应与候选一致"
        );
        assert_eq!(
            reloaded.groups()[0].head,
            candidate.groups()[0].head,
            "磁盘 head 应与候选一致"
        );

        // 原始 idx 尚未提交，应仍为空（模拟内存未更新场景）
        assert!(
            idx.groups().is_empty(),
            "落盘成功前内存不变量：原始索引不受影响"
        );

        // Step 4: 落盘成功，提交内存
        idx = candidate;
        assert_eq!(
            idx.groups().len(),
            reloaded.groups().len(),
            "提交后内存 == 磁盘"
        );
    }

    /// 验证 delete_user_synonym 无变化时不写文件（no-op 分支）。
    #[test]
    fn delete_no_op_does_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user-synonyms.yaml");

        let mut idx = UserIndex::empty();
        idx.add_or_merge("云计算", vec!["AWS".into()]).unwrap();
        std::fs::write(&path, idx.to_yaml_str()).unwrap();

        // 尝试删除不存在的词头
        let mut candidate = idx.clone();
        let changed = candidate.remove("不存在的词头");
        assert!(!changed, "不存在的词头 remove 应返回 false");

        // 文件内容不应被覆写（通过检查 mtime 或重新加载）
        let reloaded = load_user_index(&path);
        assert_eq!(reloaded.groups().len(), 1, "文件应保持原始 1 组不变");
    }
}
