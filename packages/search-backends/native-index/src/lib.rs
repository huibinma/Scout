//! Scout 内置文件元数据索引服务（`scout-native-index`）。
//!
//! 替代对外部 Everything（`es.exe`）的依赖：用 NTFS 原生的 MFT 批量枚举
//! （`FSCTL_ENUM_USN_DATA`）做一次性全盘发现，配合 USN Journal 增量监控
//! （`FSCTL_READ_USN_JOURNAL`）常驻内存索引，服务方对 Everything 的两类真实用法——
//! 索引构建期的"全盘路径发现"（[`packages/indexer/src/discovery.rs`](../../../indexer/src/discovery.rs)）
//! 与桌面端"按文件名/扩展名找本机文件"（模型文件发现）。
//!
//! ## 三大核心技术（对应本次重构目标）
//!
//! 1. **MFT 枚举**（[`sys::enum_usn_data`]）：`FSCTL_ENUM_USN_DATA` 是 NTFS
//!    驱动提供的官方批量枚举接口，效果等价于"解析主文件表"——比自行读取卷原始
//!    簇数据解析 `$MFT` 记录格式更安全（不必自行处理压缩/稀疏文件、NTFS 版本
//!    差异），由文件系统驱动保证遍历正确性。
//! 2. **内存索引结构**（[`index::MemIndex`]）：`FRN -> 记录` 扁平映射 + 父子链
//!    路径重建，无倒排索引/分词开销，见该模块文档的设计取舍说明。
//! 3. **USN Journal 实时监控**（[`service::NativeIndexService`]）：后台线程持续
//!    `FSCTL_READ_USN_JOURNAL` 增量 tail，创建/重命名/删除毫秒级反映到内存索引，
//!    不需要重新全盘枚举。
//!
//! ## 已知限制（如实记录，不夸大能力边界）
//!
//! - **仅 Windows + NTFS**：非 Windows 平台、非 NTFS 卷（FAT32/exFAT/ReFS）一律
//!   返回"不可用"，调用方须回退目录扫描（[`native_index_available`]/各查询函数
//!   均遵循此契约，与原 Everything 发现层同构）。
//! - **需要管理员权限**：Win32 打开卷句柄（`\\.\C:`）本身要求管理员权限，这是
//!   系统限制、非本实现选择。非管理员进程下 [`NativeIndexService::start`] 会
//!   失败，本 crate 的做法与原 Everything 集成一致——优雅降级，不强制提权。
//! - **USN_RECORD_V3（128-bit FileId）不支持**：`ReFS` 或未来 NTFS 大卷可能用
//!   V3 记录格式，本实现只解析 V2（NTFS 卷现状恒为 V2），V3 记录被跳过。

mod error;
mod index;
mod record;
mod service;
#[cfg(windows)]
mod sys;
#[cfg(not(windows))]
mod sys {
    pub(crate) fn drive_letter_of(path: &std::path::Path) -> Option<char> {
        let s = path.to_str()?;
        let mut chars = s.chars();
        let letter = chars.next()?.to_ascii_uppercase();
        (letter.is_ascii_alphabetic() && chars.next() == Some(':')).then_some(letter)
    }
}

pub mod backend;

pub use error::NativeIndexError;
pub use index::{FileRecord, MemIndex, NameQuery};
pub use service::NativeIndexService;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

/// 进程内跨调用共享的多卷索引管理器：按需为出现过的盘符启动
/// [`NativeIndexService`]，启动失败的盘符记为"不可用"并缓存，避免每次查询都
/// 重新尝试打开卷句柄（管理员权限判定这类失败通常在进程生命周期内不会变化）。
#[derive(Debug, Default)]
struct Manager {
    services: RwLock<HashMap<char, Option<Arc<NativeIndexService>>>>,
}

impl Manager {
    fn global() -> &'static Self {
        static INSTANCE: OnceLock<Manager> = OnceLock::new();
        INSTANCE.get_or_init(Manager::default)
    }

    fn service_for(&self, drive_letter: char) -> Option<Arc<NativeIndexService>> {
        if let Some(existing) = self.services.read().ok()?.get(&drive_letter) {
            return existing.clone();
        }
        let started = NativeIndexService::start(drive_letter).ok().map(Arc::new);
        if let Ok(mut map) = self.services.write() {
            map.insert(drive_letter, started.clone());
        }
        started
    }

    /// 已知本机存在的固定盘符（`A:`..`Z:` 中路径存在者）。用 `std::fs` 探测而非
    /// 额外的 `GetLogicalDrives` Win32 调用——足够准确，且不给 [`sys`] 模块新增
    /// `unsafe` 面。
    fn known_drives() -> Vec<char> {
        (b'A'..=b'Z')
            .map(char::from)
            .filter(|&letter| Path::new(&format!(r"{letter}:\")).exists())
            .collect()
    }

    fn search_all(&self, query: &str, limit: usize) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for letter in Self::known_drives() {
            if out.len() >= limit {
                break;
            }
            if let Some(service) = self.service_for(letter) {
                out.extend(service.search(query, limit - out.len()));
            }
        }
        out
    }

    fn search_by_extensions_all(&self, extensions: &[&str], limit: usize) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for letter in Self::known_drives() {
            if out.len() >= limit {
                break;
            }
            if let Some(service) = self.service_for(letter) {
                out.extend(service.search_by_extensions(extensions, limit - out.len()));
            }
        }
        out
    }

    fn search_query_all(&self, query: &NameQuery, limit: usize) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for letter in Self::known_drives() {
            if out.len() >= limit {
                break;
            }
            if let Some(service) = self.service_for(letter) {
                out.extend(service.search_query(query, limit - out.len()));
            }
        }
        out
    }

    fn any_available(&self) -> bool {
        Self::known_drives()
            .into_iter()
            .any(|letter| self.service_for(letter).is_some())
    }
}

/// 全盘（本机全部本地固定盘符）按**完整文件名**（大小写不敏感）查找文件。
///
/// 对齐原 `scout-search-backend-everything::find_files_named` 的调用契约——
/// 桌面端「本地已有 .gguf 模型发现」等场景零改动接入。原生索引不可用
/// （非 Windows / 无管理员权限 / 无 NTFS 卷）时返回空，调用方按"未发现"降级。
#[must_use]
pub fn find_files_named(filename: &str, limit: usize) -> Vec<PathBuf> {
    let needle = filename.to_lowercase();
    Manager::global()
        .search_all(filename, limit.saturating_mul(4).max(limit))
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.to_lowercase() == needle)
        })
        .take(limit)
        .collect()
}

/// 全盘按**扩展名**（不含点、大小写不敏感）查找文件。对齐原
/// `find_files_by_extension` 的调用契约。
#[must_use]
pub fn find_files_by_extension(ext: &str, limit: usize) -> Vec<PathBuf> {
    Manager::global().search_by_extensions_all(&[ext], limit)
}

/// 全盘文件名子串搜索（大小写不敏感）。供发现层（[`FileNameDiscovery`]）与
/// 直接查询场景共用的最小 API。
#[must_use]
pub fn search_filename_substring(query: &str, limit: usize) -> Vec<PathBuf> {
    Manager::global().search_all(query, limit)
}

/// 按扩展名集合批量查找（发现层用，一次覆盖多个扩展名，避免逐扩展名多次全盘扫）。
#[must_use]
pub fn search_by_extensions(extensions: &[&str], limit: usize) -> Vec<PathBuf> {
    Manager::global().search_by_extensions_all(extensions, limit)
}

/// 组合条件查询（[`backend::NativeIndexBackend`] 翻译 `SearchIntent` 后调用），
/// 见 [`index::NameQuery`] / [`MemIndex::search_query`]。
#[must_use]
pub fn search_query(query: &NameQuery, limit: usize) -> Vec<PathBuf> {
    Manager::global().search_query_all(query, limit)
}

/// 原生索引是否至少在一个本地卷上可用（用于替代原 `es_cli_available`，
/// 供设置页"检测"提示使用）。
#[must_use]
pub fn native_index_available() -> bool {
    Manager::global().any_available()
}

/// 取路径所属盘符（大写字符），非本地路径（如 UNC）返回 `None`。
#[must_use]
pub fn drive_letter_of(path: &Path) -> Option<char> {
    sys::drive_letter_of(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_letter_of_extracts_plain_path() {
        assert_eq!(drive_letter_of(Path::new(r"C:\a\b")), Some('C'));
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_queries_return_empty_and_unavailable() {
        assert!(find_files_named("x.txt", 10).is_empty());
        assert!(find_files_by_extension("gguf", 10).is_empty());
        assert!(!native_index_available());
    }
}
