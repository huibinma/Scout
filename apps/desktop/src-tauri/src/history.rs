//! BETA-22 搜索历史 + 保存的搜索（本地持久化）。
//!
//! 两类数据持久化到 app 配置目录下的 `search_history.json`（与 `settings.json` 同目录）：
//! - **搜索历史**（`recent`）：自动记录最近执行的查询，去重 + 上限 [`MAX_RECENT`]、最近优先，
//!   可一键重跑 / 清空。
//! - **保存的搜索 / 智能文件夹 v1**（`saved`）：用户显式命名置顶的查询，可一键重跑 / 单条删除。
//!
//! **隐私**：查询文本属用户数据，故存在 Scout 自身配置目录、可在 BETA-21 隐私面板
//! 看到位置/条数并一键清除（[`crate::privacy`] 复用 [`search_history_path`] / [`recent_count_at`]）。
//! 本模块只读写自身数据文件、不外发任何数据、不调用 `Tracer`（不进 trace，按构造保证）。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

/// 搜索历史上限（最近优先，超出截断）。保存的搜索不受此限。
const MAX_RECENT: usize = 50;

/// 单条搜索历史。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// 查询文本（已 trim）。
    pub query: String,
    /// 最近一次执行时间（rfc3339）。
    pub last_run: String,
    /// 累计执行次数。
    pub run_count: u32,
}

/// 单条保存的搜索（智能文件夹 v1：命名 + 查询）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSearch {
    /// 稳定 id（创建时间毫秒，冲突时追加序号），删除 / 重跑用。
    pub id: String,
    /// 用户给定的名称（已 trim）。
    pub name: String,
    /// 查询文本（已 trim）。
    pub query: String,
    /// 创建时间（rfc3339）。
    pub created: String,
    /// BETA-29 v2：可选的意图草稿（schema wire 格式 Search Intent JSON）。
    /// 存在时重跑走 `search_with_intent`（保留用户在草稿面板的修正），缺省走普通搜索。
    /// 旧文件无此字段 → None（serde default，向后兼容）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<serde_json::Value>,
}

/// 持久化的搜索历史存储（`recent` + `saved` 同文件）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchHistoryStore {
    /// 自动记录的最近查询（最近优先）。
    #[serde(default)]
    pub recent: Vec<HistoryEntry>,
    /// 用户保存的搜索（最近创建优先）。
    #[serde(default)]
    pub saved: Vec<SavedSearch>,
}

/// `search_history.json` 路径（best-effort，仅解析不创建目录）。
/// BETA-21 隐私面板复用此函数展示「历史在哪」（单一信源）。2026-07-29 起改走
/// [`crate::scout_data_dir`]（与 index.db / audit.jsonl 同目录，理由见 `main.rs::scout_data_dir`）。
pub(crate) fn search_history_path(_app: &AppHandle) -> Option<PathBuf> {
    Some(crate::scout_data_dir().join("search_history.json"))
}

/// 写入用路径：确保数据目录存在（与 `settings.rs::get_settings_path` 同款）。
fn store_path(_app: &AppHandle) -> Result<PathBuf, String> {
    let path = crate::scout_data_dir();
    if !path.exists() {
        fs::create_dir_all(&path).map_err(|e| format!("无法创建数据目录: {e}"))?;
    }
    Ok(path.join("search_history.json"))
}

/// 从磁盘读取存储。文件缺失 / 解析失败 → 返回默认空存储（不报错，best-effort）。
fn load(path: &Path) -> SearchHistoryStore {
    fs::read_to_string(path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

/// 写回存储（pretty JSON）。
fn save(path: &Path, store: &SearchHistoryStore) -> Result<(), String> {
    let content =
        serde_json::to_string_pretty(store).map_err(|e| format!("序列化搜索历史失败: {e}"))?;
    fs::write(path, content).map_err(|e| format!("写入搜索历史失败: {e}"))
}

/// 隐私面板用：读取 `recent` 条数（best-effort，缺库返回 0）。
pub(crate) fn recent_count_at(path: &Path) -> usize {
    load(path).recent.len()
}

/// 把一次查询并入历史（纯逻辑，便于单测）：
/// 已存在则提到最前 + 次数 +1 + 更新时间；否则插入到最前；最后按 [`MAX_RECENT`] 截断。
/// 空白查询忽略。
fn push_recent(store: &mut SearchHistoryStore, query: &str, now: &str) {
    let q = query.trim();
    if q.is_empty() {
        return;
    }
    if let Some(pos) = store.recent.iter().position(|e| e.query == q) {
        let mut entry = store.recent.remove(pos);
        entry.run_count = entry.run_count.saturating_add(1);
        entry.last_run = now.to_owned();
        store.recent.insert(0, entry);
    } else {
        store.recent.insert(
            0,
            HistoryEntry {
                query: q.to_owned(),
                last_run: now.to_owned(),
                run_count: 1,
            },
        );
    }
    store.recent.truncate(MAX_RECENT);
}

/// 由种子生成不与现有 id 冲突的稳定 id（纯逻辑，便于单测）。
fn unique_id(seed: i64, existing: &[SavedSearch]) -> String {
    let mut id = seed.to_string();
    let mut n = 0u32;
    while existing.iter().any(|s| s.id == id) {
        n += 1;
        id = format!("{seed}-{n}");
    }
    id
}

/// 当前 rfc3339 时间戳（与 BETA-07 `last_indexed` 同款口径）。
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// BETA-22：记录一次查询到搜索历史（搜索执行时由前端 fire-and-forget 调用）。
#[tauri::command]
pub fn record_search(app: AppHandle, query: String) -> Result<(), String> {
    if query.trim().is_empty() {
        return Ok(());
    }
    let path = store_path(&app)?;
    let mut store = load(&path);
    push_recent(&mut store, &query, &now_rfc3339());
    save(&path, &store)
}

/// BETA-22：返回搜索历史 + 保存的搜索（前端启动 / 变更后拉取）。
#[tauri::command]
pub fn get_search_history(app: AppHandle) -> Result<SearchHistoryStore, String> {
    match search_history_path(&app) {
        Some(p) => Ok(load(&p)),
        None => Ok(SearchHistoryStore::default()),
    }
}

/// BETA-22：清空搜索历史（仅 `recent`；保存的搜索是用户显式数据，不在此清除）。
/// 隐私面板「清除搜索历史」与历史下拉「清空」共用。
#[tauri::command]
pub fn clear_search_history(app: AppHandle) -> Result<(), String> {
    let path = store_path(&app)?;
    let mut store = load(&path);
    if store.recent.is_empty() {
        return Ok(());
    }
    store.recent.clear();
    save(&path, &store)
}

/// BETA-22：保存一条命名搜索，返回新建条目（含生成的 id）。最近创建优先（插入最前）。
///
/// BETA-29 v2：`intent` 可选携带意图草稿（草稿面板「保存草稿」入口）。与
/// `search_with_intent` 同款闸门：必须能反序列化为合法 Search Intent 且仅收
/// file_search / media_search——坏草稿在保存时就拒绝，不落盘等到重跑才炸。
#[tauri::command]
pub fn save_search(
    app: AppHandle,
    name: String,
    query: String,
    intent: Option<serde_json::Value>,
) -> Result<SavedSearch, String> {
    let name = name.trim().to_owned();
    let query = query.trim().to_owned();
    if name.is_empty() {
        return Err("名称不能为空".to_owned());
    }
    if query.is_empty() {
        return Err("查询不能为空".to_owned());
    }
    if let Some(v) = &intent {
        validate_draft_intent(v)?;
    }
    let path = store_path(&app)?;
    let mut store = load(&path);
    let saved = SavedSearch {
        id: unique_id(chrono::Utc::now().timestamp_millis(), &store.saved),
        name,
        query,
        created: now_rfc3339(),
        intent,
    };
    store.saved.insert(0, saved.clone());
    save(&path, &store)?;
    Ok(saved)
}

/// BETA-29 v2：意图草稿保存前校验（纯逻辑，便于单测）。
/// serde 强校验（schema 类型化枚举 + deny_unknown_fields）+ 仅收 file_search / media_search，
/// 与 `search_with_intent` 的准入口径一致。
fn validate_draft_intent(v: &serde_json::Value) -> Result<(), String> {
    use scout_search_backend::SearchIntent;
    let parsed: SearchIntent =
        serde_json::from_value(v.clone()).map_err(|e| format!("意图草稿不合法: {e}"))?;
    if !matches!(
        parsed,
        SearchIntent::FileSearch(_) | SearchIntent::MediaSearch(_)
    ) {
        return Err("意图草稿仅支持 file_search / media_search".to_owned());
    }
    Ok(())
}

/// BETA-22：删除一条保存的搜索（按 id）。
#[tauri::command]
pub fn delete_saved_search(app: AppHandle, id: String) -> Result<(), String> {
    let path = store_path(&app)?;
    let mut store = load(&path);
    let before = store.saved.len();
    store.saved.retain(|s| s.id != id);
    if store.saved.len() != before {
        save(&path, &store)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn push_recent_inserts_newest_first() {
        let mut store = SearchHistoryStore::default();
        push_recent(&mut store, "第一个", "2026-06-03T00:00:00Z");
        push_recent(&mut store, "第二个", "2026-06-03T00:00:01Z");
        assert_eq!(store.recent.len(), 2);
        assert_eq!(store.recent[0].query, "第二个");
        assert_eq!(store.recent[1].query, "第一个");
        assert_eq!(store.recent[0].run_count, 1);
    }

    #[test]
    fn push_recent_dedupes_and_bumps_to_front() {
        let mut store = SearchHistoryStore::default();
        push_recent(&mut store, "重复", "2026-06-03T00:00:00Z");
        push_recent(&mut store, "其它", "2026-06-03T00:00:01Z");
        push_recent(&mut store, "重复", "2026-06-03T00:00:02Z");
        assert_eq!(store.recent.len(), 2, "去重后只两条");
        assert_eq!(store.recent[0].query, "重复");
        assert_eq!(store.recent[0].run_count, 2, "命中次数累加");
        assert_eq!(store.recent[0].last_run, "2026-06-03T00:00:02Z", "时间更新");
    }

    #[test]
    fn push_recent_trims_and_ignores_blank() {
        let mut store = SearchHistoryStore::default();
        push_recent(&mut store, "  含空白  ", "t");
        push_recent(&mut store, "   ", "t");
        assert_eq!(store.recent.len(), 1);
        assert_eq!(store.recent[0].query, "含空白", "前后空白被裁剪");
    }

    #[test]
    fn push_recent_caps_at_max() {
        let mut store = SearchHistoryStore::default();
        for i in 0..(MAX_RECENT + 10) {
            push_recent(&mut store, &format!("q{i}"), "t");
        }
        assert_eq!(store.recent.len(), MAX_RECENT, "超出上限被截断");
        // 最近的（最后插入的）在最前。
        assert_eq!(store.recent[0].query, format!("q{}", MAX_RECENT + 9));
    }

    #[test]
    fn unique_id_avoids_collision() {
        let existing = vec![
            SavedSearch {
                id: "100".to_owned(),
                name: "a".to_owned(),
                query: "qa".to_owned(),
                created: "t".to_owned(),
                intent: None,
            },
            SavedSearch {
                id: "100-1".to_owned(),
                name: "b".to_owned(),
                query: "qb".to_owned(),
                created: "t".to_owned(),
                intent: None,
            },
        ];
        assert_eq!(unique_id(100, &existing), "100-2", "应跳过已占用 id");
        assert_eq!(unique_id(200, &existing), "200", "无冲突原样返回");
    }

    #[test]
    fn load_missing_or_corrupt_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        assert!(load(&missing).recent.is_empty());

        let corrupt = dir.path().join("bad.json");
        std::fs::write(&corrupt, b"not json {{{").unwrap();
        let store = load(&corrupt);
        assert!(store.recent.is_empty(), "损坏文件退回空存储");
        assert!(store.saved.is_empty());
    }

    #[test]
    fn save_and_load_roundtrip_and_recent_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search_history.json");
        let mut store = SearchHistoryStore::default();
        push_recent(&mut store, "查询一", "t1");
        push_recent(&mut store, "查询二", "t2");
        store.saved.push(SavedSearch {
            id: "1".to_owned(),
            name: "我的发票".to_owned(),
            query: "本月的发票 pdf".to_owned(),
            created: "t".to_owned(),
            intent: None,
        });
        save(&path, &store).unwrap();

        let loaded = load(&path);
        assert_eq!(loaded.recent.len(), 2);
        assert_eq!(loaded.saved.len(), 1);
        assert_eq!(loaded.saved[0].name, "我的发票");
        // 隐私面板计数复用同一存储。
        assert_eq!(recent_count_at(&path), 2);
    }

    #[test]
    fn saved_search_intent_roundtrip_and_backcompat() {
        // BETA-29 v2：带 intent 的保存条目 round-trip；旧文件（无 intent 字段）向后兼容。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search_history.json");
        let intent = serde_json::json!({
            "intent": "file_search",
            "schema_version": "1.0",
            "keywords": ["发票"],
            "sort": "modified_desc"
        });
        let mut store = SearchHistoryStore::default();
        store.saved.push(SavedSearch {
            id: "1".to_owned(),
            name: "草稿".to_owned(),
            query: "发票".to_owned(),
            created: "t".to_owned(),
            intent: Some(intent.clone()),
        });
        save(&path, &store).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.saved[0].intent, Some(intent));

        // 旧格式文件（saved 条目无 intent 字段）→ 解析为 None 而非失败。
        let legacy = r#"{"recent":[],"saved":[{"id":"9","name":"旧","query":"q","created":"t"}]}"#;
        std::fs::write(&path, legacy).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.saved.len(), 1, "旧文件应正常解析");
        assert_eq!(loaded.saved[0].intent, None);
    }

    #[test]
    fn validate_draft_intent_gates_variant_and_schema() {
        // 与 search_with_intent 同款闸门：合法 file_search 过；action 拒；未知字段拒。
        let ok = serde_json::json!({
            "intent": "file_search",
            "schema_version": "1.0",
            "keywords": ["合同"]
        });
        assert!(validate_draft_intent(&ok).is_ok());
        let action = serde_json::json!({
            "schema_version": "1.0",
            "intent": "file_action",
            "action": "open",
            "target_ref": { "source": "last_results", "selector": { "type": "index", "value": 1 } },
            "requires_confirmation": false
        });
        assert!(
            validate_draft_intent(&action).is_err(),
            "action 类不应可存为草稿"
        );
        let unknown = serde_json::json!({
            "intent": "file_search",
            "schema_version": "1.0",
            "not_a_field": 1
        });
        assert!(
            validate_draft_intent(&unknown).is_err(),
            "未知字段应被 deny_unknown_fields 拒绝"
        );
    }
}
