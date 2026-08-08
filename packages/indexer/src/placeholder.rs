//! 云占位符（"仅在线"文件）检测（BETA-01A）。
//!
//! Windows：查文件属性 `FILE_ATTRIBUTE_OFFLINE` / `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`
//! ——**只读属性、不触发内容水合**，经 `std::os::windows::fs::MetadataExt::file_attributes`，
//! **无 unsafe**（守 workspace `unsafe_code = forbid`）。其它平台 best-effort 返 `false`
//! （macOS iCloud dataless 无安全 std API，留后续）。
//!
//! 动机：spike 实测 OneDrive "仅在线"文件占 24%，lofty 读内容即被拒（`os error 395`）
//! 且会触发水合下载。占位符应跳过标签读取、只存文件名。

use std::path::Path;

/// `FILE_ATTRIBUTE_OFFLINE`：文件数据不在本地。
#[cfg(windows)]
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`：访问数据时按需召回（OneDrive "仅在线"占位符）。
#[cfg(windows)]
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// 文件属性位是否表示"仅在线"占位符（纯函数）。
#[cfg(windows)]
pub(crate) fn attrs_indicate_online_only(attrs: u32) -> bool {
    attrs & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) != 0
}

/// 文件是否为"仅在线"云占位符（读其内容会触发下载/被拒）。
#[cfg(windows)]
pub(crate) fn is_online_only(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    std::fs::metadata(path).is_ok_and(|m| attrs_indicate_online_only(m.file_attributes()))
}

/// 非 Windows：best-effort 返 false（macOS dataless 无安全 std API）。
#[cfg(not(windows))]
pub(crate) fn is_online_only(_path: &Path) -> bool {
    false
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn attrs_detect_online_only() {
        assert!(attrs_indicate_online_only(FILE_ATTRIBUTE_OFFLINE));
        assert!(attrs_indicate_online_only(
            FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
        ));
        // 与普通属性（如 ARCHIVE 0x20）组合仍命中。
        assert!(attrs_indicate_online_only(0x20 | FILE_ATTRIBUTE_OFFLINE));
        // 普通文件（无占位符位）不命中。
        assert!(!attrs_indicate_online_only(0x20)); // ARCHIVE
        assert!(!attrs_indicate_online_only(0x80)); // NORMAL
        assert!(!attrs_indicate_online_only(0));
    }
}
