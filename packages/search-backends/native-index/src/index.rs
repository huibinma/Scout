//! 内存索引结构：`FRN -> 记录` 映射 + 父子链路径重建 + 子串搜索。
//!
//! 设计取舍（与 Everything 的实际实现思路一致，而非套用 trigram/倒排索引）：
//! 索引只保存"扁平的文件名 + 父子关系"，不做分词/N-gram；搜索时对全部记录名做
//! 一次大小写不敏感子串扫描。全盘文件数量级（十万到千万）下，紧凑内存布局 +
//! 线性扫描仍能做到毫秒级——瓶颈是内存带宽而非算法复杂度，这也是"极低资源占用"
//! 的来源：没有额外倒排索引结构的内存 / 维护开销，索引体积约等于"文件名字符串
//! 总量 + 定长元数据"，增删只需 O(1) 更新一条记录，不需要重建任何辅助结构。

use std::collections::HashMap;
use std::path::PathBuf;

/// NTFS 卷根目录的保留 FRN（`$MFT` 记录 5）。
const ROOT_FRN: u64 = 5;

/// 单条文件 / 目录记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    /// 文件引用号。
    pub frn: u64,
    /// 父目录文件引用号。
    pub parent_frn: u64,
    /// 文件 / 目录名（不含路径分隔符）。
    pub name: String,
    /// 是否为目录。
    pub is_directory: bool,
}

/// 单卷内存索引。
#[derive(Debug, Default)]
pub struct MemIndex {
    records: HashMap<u64, FileRecord>,
}

/// 路径重建时的最大回溯深度，防御环状 / 断链数据（畸形 USN 流、竞态下的悬空
/// `parent_frn`）导致无限循环——实际 NTFS 目录深度远小于此值。
const MAX_PATH_DEPTH: usize = 512;

impl MemIndex {
    /// 新建空索引。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 插入或覆盖一条记录（USN "create" / "modify" / "rename" 均落到这里——
    /// rename 的新记录直接覆盖旧记录，天然反映最新文件名，无需特判）。
    pub fn upsert(&mut self, record: FileRecord) {
        self.records.insert(record.frn, record);
    }

    /// 删除一条记录（USN "delete"）。
    pub fn remove(&mut self, frn: u64) {
        self.records.remove(&frn);
    }

    /// 当前索引记录数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// 索引是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// 沿 `parent_frn` 链回溯重建完整路径。断链（父记录缺失，如 USN 流里父目录
    /// 记录尚未到达）时退化为"从已知的最上层片段开始"的部分路径，而非返回
    /// `None`——调用方通常宁可拿到一个可能不完整但可用的路径，也不要整条结果消失。
    #[must_use]
    pub fn full_path(&self, frn: u64, drive_letter: char) -> Option<PathBuf> {
        let mut segments = Vec::new();
        let mut current = frn;
        let mut depth = 0;

        while let Some(record) = self.records.get(&current) {
            segments.push(record.name.clone());
            if current == ROOT_FRN || record.parent_frn == current {
                break;
            }
            current = record.parent_frn;
            depth += 1;
            if depth >= MAX_PATH_DEPTH {
                break;
            }
        }

        if segments.is_empty() {
            return None;
        }
        segments.reverse();
        let mut path = PathBuf::from(format!(r"{drive_letter}:\"));
        for segment in segments {
            path.push(segment);
        }
        Some(path)
    }

    /// 大小写不敏感的文件名子串搜索，返回匹配记录重建出的完整路径，最多 `limit` 条。
    #[must_use]
    pub fn search_substring(&self, query: &str, drive_letter: char, limit: usize) -> Vec<PathBuf> {
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }
        let needle = query.to_lowercase();
        let mut out = Vec::with_capacity(limit.min(64));
        for record in self.records.values() {
            if out.len() >= limit {
                break;
            }
            if record.name.to_lowercase().contains(&needle) {
                if let Some(path) = self.full_path(record.frn, drive_letter) {
                    out.push(path);
                }
            }
        }
        out
    }

    /// 按扩展名集合（不含点、大小写不敏感）过滤，返回匹配记录的完整路径，最多
    /// `limit` 条。用于替代原 `es.exe ext:` 查询（全盘发现文档 / 图片 / 音频 /
    /// 模型文件），`extensions` 为空返回空结果。
    #[must_use]
    pub fn search_by_extensions(
        &self,
        extensions: &[&str],
        drive_letter: char,
        limit: usize,
    ) -> Vec<PathBuf> {
        if extensions.is_empty() || limit == 0 {
            return Vec::new();
        }
        let wanted: Vec<String> = extensions.iter().map(|e| e.to_lowercase()).collect();
        let mut out = Vec::with_capacity(limit.min(64));
        for record in self.records.values() {
            if out.len() >= limit {
                break;
            }
            if record.is_directory {
                continue;
            }
            let Some(ext) = record.name.rsplit('.').next() else {
                continue;
            };
            if record.name.ends_with('.') || !record.name.contains('.') {
                continue;
            }
            if wanted.iter().any(|w| w == &ext.to_lowercase()) {
                if let Some(path) = self.full_path(record.frn, drive_letter) {
                    out.push(path);
                }
            }
        }
        out
    }

    /// 组合条件查询：关键词组间 AND、组内 OR（均大小写不敏感子串匹配文件名）+
    /// 扩展名白/黑名单，一次线性扫描内完成——供 [`crate::backend::NativeIndexBackend`]
    /// 翻译 `SearchIntent` 用。比逐条件多次独立 `search_*` 调用后再做路径级 `Vec`
    /// 交集更高效（也避免大小写/规范化在多轮之间不一致的风险），与本模块文档的
    /// "单次扫描"设计原则一致。
    #[must_use]
    pub fn search_query(
        &self,
        query: &NameQuery,
        drive_letter: char,
        limit: usize,
    ) -> Vec<PathBuf> {
        if limit == 0 {
            return Vec::new();
        }
        let groups: Vec<Vec<String>> = query
            .keyword_groups
            .iter()
            .map(|group| group.iter().map(|term| term.to_lowercase()).collect())
            .filter(|group: &Vec<String>| !group.is_empty())
            .collect();
        let allow: Vec<String> = query.extensions.iter().map(|e| e.to_lowercase()).collect();
        let deny: Vec<String> = query
            .exclude_extensions
            .iter()
            .map(|e| e.to_lowercase())
            .collect();

        let mut out = Vec::with_capacity(limit.min(64));
        for record in self.records.values() {
            if out.len() >= limit {
                break;
            }
            let name_lower = record.name.to_lowercase();
            if !groups
                .iter()
                .all(|group| group.iter().any(|term| name_lower.contains(term.as_str())))
            {
                continue;
            }
            if !allow.is_empty() || !deny.is_empty() {
                if record.is_directory {
                    continue;
                }
                let Some(ext) = file_extension_lower(&record.name) else {
                    continue;
                };
                if !allow.is_empty() && !allow.iter().any(|w| w == &ext) {
                    continue;
                }
                if deny.iter().any(|w| w == &ext) {
                    continue;
                }
            }
            if let Some(path) = self.full_path(record.frn, drive_letter) {
                out.push(path);
            }
        }
        out
    }
}

/// 组合过滤条件，见 [`MemIndex::search_query`]。
#[derive(Debug, Clone, Default)]
pub struct NameQuery {
    /// 组间 AND、组内 OR 的关键词（均大小写不敏感子串匹配文件名）；空 vec 表示不限制。
    pub keyword_groups: Vec<Vec<String>>,
    /// 扩展名白名单（不含点，大小写不敏感）；空表示不限制。
    pub extensions: Vec<String>,
    /// 扩展名黑名单（不含点，大小写不敏感）。
    pub exclude_extensions: Vec<String>,
}

/// 取文件名的小写扩展名（不含点）；无扩展名（含以 `.` 结尾、不含 `.`）返回 `None`。
fn file_extension_lower(name: &str) -> Option<String> {
    if name.ends_with('.') || !name.contains('.') {
        return None;
    }
    name.rsplit('.').next().map(str::to_lowercase)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn dir(frn: u64, parent: u64, name: &str) -> FileRecord {
        FileRecord {
            frn,
            parent_frn: parent,
            name: name.to_owned(),
            is_directory: true,
        }
    }

    fn file(frn: u64, parent: u64, name: &str) -> FileRecord {
        FileRecord {
            frn,
            parent_frn: parent,
            name: name.to_owned(),
            is_directory: false,
        }
    }

    fn sample_index() -> MemIndex {
        let mut idx = MemIndex::new();
        idx.upsert(dir(5, 5, "")); // 卷根，自环
        idx.upsert(dir(10, 5, "Users"));
        idx.upsert(dir(11, 10, "Alice"));
        idx.upsert(file(20, 11, "报告2024.docx"));
        idx.upsert(file(21, 11, "budget.xlsx"));
        idx
    }

    #[test]
    fn full_path_reconstructs_from_root() {
        let idx = sample_index();
        let path = idx.full_path(20, 'C').unwrap();
        assert_eq!(path, PathBuf::from(r"C:\Users\Alice\报告2024.docx"));
    }

    #[test]
    fn full_path_missing_frn_returns_none() {
        let idx = sample_index();
        assert!(idx.full_path(999, 'C').is_none());
    }

    #[test]
    fn full_path_breaks_cycle_defensively() {
        let mut idx = MemIndex::new();
        // 人为构造一个环（不应出现在真实数据里，但要防御）。
        idx.upsert(FileRecord {
            frn: 1,
            parent_frn: 2,
            name: "a".into(),
            is_directory: true,
        });
        idx.upsert(FileRecord {
            frn: 2,
            parent_frn: 1,
            name: "b".into(),
            is_directory: true,
        });
        let path = idx.full_path(1, 'C');
        assert!(
            path.is_some(),
            "环状数据应退化返回部分路径而非 panic/死循环"
        );
    }

    #[test]
    fn search_substring_is_case_insensitive_and_limited() {
        let idx = sample_index();
        let hits = idx.search_substring("报告", 'C', 10);
        assert_eq!(hits, vec![PathBuf::from(r"C:\Users\Alice\报告2024.docx")]);

        let hits = idx.search_substring("BUDGET", 'C', 10);
        assert_eq!(hits, vec![PathBuf::from(r"C:\Users\Alice\budget.xlsx")]);

        let hits = idx.search_substring("x", 'C', 1);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn search_substring_empty_query_returns_empty() {
        let idx = sample_index();
        assert!(idx.search_substring("", 'C', 10).is_empty());
    }

    #[test]
    fn search_by_extensions_matches_case_insensitive() {
        let idx = sample_index();
        let hits = idx.search_by_extensions(&["DOCX"], 'C', 10);
        assert_eq!(hits, vec![PathBuf::from(r"C:\Users\Alice\报告2024.docx")]);
    }

    #[test]
    fn search_by_extensions_skips_directories_and_extensionless() {
        let mut idx = sample_index();
        idx.upsert(file(30, 11, "noext"));
        let hits = idx.search_by_extensions(&["xlsx", "docx"], 'C', 10);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn upsert_overwrites_on_rename() {
        let mut idx = sample_index();
        idx.upsert(file(20, 11, "renamed.docx"));
        assert_eq!(idx.len(), 5);
        let path = idx.full_path(20, 'C').unwrap();
        assert_eq!(path, PathBuf::from(r"C:\Users\Alice\renamed.docx"));
    }

    #[test]
    fn remove_deletes_record() {
        let mut idx = sample_index();
        idx.remove(20);
        assert!(idx.full_path(20, 'C').is_none());
        assert_eq!(idx.len(), 4);
    }

    #[test]
    fn search_query_ands_groups_ors_within_group() {
        let idx = sample_index();
        // 组1："报告" OR "budget"；组2："2024"——两组 AND，命中 报告2024.docx。
        let query = NameQuery {
            keyword_groups: vec![
                vec!["报告".to_owned(), "budget".to_owned()],
                vec!["2024".to_owned()],
            ],
            ..NameQuery::default()
        };
        let hits = idx.search_query(&query, 'C', 10);
        assert_eq!(hits, vec![PathBuf::from(r"C:\Users\Alice\报告2024.docx")]);
    }

    #[test]
    fn search_query_no_groups_matches_all_then_extension_filters() {
        let idx = sample_index();
        let query = NameQuery {
            extensions: vec!["XLSX".to_owned()],
            ..NameQuery::default()
        };
        let hits = idx.search_query(&query, 'C', 10);
        assert_eq!(hits, vec![PathBuf::from(r"C:\Users\Alice\budget.xlsx")]);
    }

    #[test]
    fn search_query_exclude_extensions_filters_out() {
        let idx = sample_index();
        let query = NameQuery {
            exclude_extensions: vec!["docx".to_owned()],
            ..NameQuery::default()
        };
        let hits = idx.search_query(&query, 'C', 10);
        assert_eq!(hits, vec![PathBuf::from(r"C:\Users\Alice\budget.xlsx")]);
    }

    #[test]
    fn search_query_empty_returns_nothing_when_limit_zero() {
        let idx = sample_index();
        assert!(idx.search_query(&NameQuery::default(), 'C', 0).is_empty());
    }
}
