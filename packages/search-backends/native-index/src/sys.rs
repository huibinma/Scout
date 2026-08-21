//! Win32 卷 I/O / USN Journal 原始调用层。
//!
//! 全 crate 唯一含 `unsafe` 的模块：打开卷句柄（`CreateFileW \\.\C:`）、
//! `DeviceIoControl` 枚举 MFT（`FSCTL_ENUM_USN_DATA`）、查询 / 读取 USN Journal
//! （`FSCTL_QUERY_USN_JOURNAL` / `FSCTL_READ_USN_JOURNAL`）。上层 [`crate::journal`]
//! 只经这里的安全函数交互，不直接碰 FFI。
//!
//! 打开卷句柄通常需要管理员权限（Windows 官方要求）；权限不足时 `open_volume`
//! 返回 `Err`，调用方（[`crate::service`]）据此优雅降级。

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_HANDLE_EOF, ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED,
    ERROR_JOURNAL_NOT_ACTIVE, ERROR_MORE_DATA, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0,
    READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0,
};
use windows::Win32::System::IO::DeviceIoControl;

use crate::error::NativeIndexError;

/// 打开的卷句柄。`Drop` 时自动 `CloseHandle`。
#[derive(Debug)]
pub(crate) struct VolumeHandle(HANDLE);

// `HANDLE` 是不透明句柄值，跨线程共享读操作是安全的（`DeviceIoControl` 本身线程安全）。
unsafe impl Send for VolumeHandle {}
unsafe impl Sync for VolumeHandle {}

impl Drop for VolumeHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // 尽力关闭；失败无可挽回操作，不 panic。
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// 打开形如 `\\.\C:` 的卷根句柄（只读 + 共享读写，不独占卷，避免阻塞用户正常读写）。
pub(crate) fn open_volume(drive_letter: char) -> Result<VolumeHandle, NativeIndexError> {
    let path = format!(r"\\.\{drive_letter}:");
    let wide: Vec<u16> = std::ffi::OsStr::new(&path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY：`wide` 是以 NUL 结尾、生命周期覆盖调用本身的合法 UTF-16 缓冲区；
    // 其余参数均为按 Win32 文档的字面常量/空指针，无别名/悬垂风险。
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|e| NativeIndexError::VolumeOpen {
        drive_letter,
        detail: e.to_string(),
    })?;

    if handle.is_invalid() {
        return Err(NativeIndexError::VolumeOpen {
            drive_letter,
            detail: "CreateFileW returned an invalid handle".to_owned(),
        });
    }
    Ok(VolumeHandle(handle))
}

/// 一次 `DeviceIoControl` 调用的输出缓冲区大小。64KiB 是微软示例代码的常用取值，
/// 在“单次系统调用开销”与“单次缓冲区内存占用”之间取得平衡。
pub(crate) const IO_BUFFER_SIZE: usize = 64 * 1024;

/// `FSCTL_QUERY_USN_JOURNAL`：查询卷当前 USN Journal 的 ID 与游标范围。
/// Journal 不存在（卷从未启用变更日志）时返回 `Err`——调用方据此决定是否
/// 尝试用 `FSCTL_CREATE_USN_JOURNAL` 创建（本 crate 不主动创建，见 crate 文档）。
pub(crate) fn query_usn_journal(
    volume: &VolumeHandle,
) -> Result<USN_JOURNAL_DATA_V0, NativeIndexError> {
    let mut out = USN_JOURNAL_DATA_V0::default();
    let mut returned = 0u32;

    // SAFETY：`out` 是合法对齐的本地变量，容量与 `lpOutBuffer`/`nOutBufferSize` 一致；
    // 无输入缓冲区（`FSCTL_QUERY_USN_JOURNAL` 不需要）。
    let ok = unsafe {
        DeviceIoControl(
            volume.0,
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some(std::ptr::addr_of_mut!(out).cast::<c_void>()),
            u32::try_from(std::mem::size_of::<USN_JOURNAL_DATA_V0>()).unwrap_or(u32::MAX),
            Some(&mut returned),
            None,
        )
    };
    ok.map_err(|e| NativeIndexError::Ioctl {
        operation: "FSCTL_QUERY_USN_JOURNAL",
        detail: e.to_string(),
    })?;
    Ok(out)
}

/// `FSCTL_ENUM_USN_DATA` 单次调用：从 `start_frn` 开始批量枚举 MFT 记录到 `buf`。
/// 返回 `(下一次 start_frn, 原始记录字节切片)`；`Ok(None)` 表示已枚举到卷末尾
/// （`ERROR_HANDLE_EOF`，正常终止条件，不是错误）。
///
/// 这正是 Everything 等工具用来"解析 MFT"的官方、受支持接口——直接读取 NTFS 卷的
/// 原始簇数据需要自行解析 `$MFT` 记录格式，权限要求相同（管理员）但正确性风险高得多
/// （簇碎片、压缩/稀疏文件、NTFS 版本差异均需自行处理）；`FSCTL_ENUM_USN_DATA`
/// 由文件系统驱动本身保证遍历正确性与顺序，语义上就是"批量枚举 MFT 记录"。
pub(crate) fn enum_usn_data<'buf>(
    volume: &VolumeHandle,
    start_frn: u64,
    buf: &'buf mut [u8],
) -> Result<Option<(u64, &'buf [u8])>, NativeIndexError> {
    let input = MFT_ENUM_DATA_V0 {
        StartFileReferenceNumber: start_frn,
        LowUsn: 0,
        HighUsn: i64::MAX,
    };
    let mut returned = 0u32;

    // SAFETY：`input` 只读、生命周期覆盖调用；`buf` 是调用方持有的、大小已知的可写缓冲区。
    let result = unsafe {
        DeviceIoControl(
            volume.0,
            FSCTL_ENUM_USN_DATA,
            Some(std::ptr::addr_of!(input).cast::<c_void>()),
            u32::try_from(std::mem::size_of::<MFT_ENUM_DATA_V0>()).unwrap_or(u32::MAX),
            Some(buf.as_mut_ptr().cast::<c_void>()),
            u32::try_from(buf.len()).unwrap_or(u32::MAX),
            Some(&mut returned),
            None,
        )
    };

    if let Err(e) = result {
        if e.code() == ERROR_HANDLE_EOF.to_hresult() {
            return Ok(None);
        }
        return Err(NativeIndexError::Ioctl {
            operation: "FSCTL_ENUM_USN_DATA",
            detail: e.to_string(),
        });
    }

    if returned < 8 {
        // 输出至少含 8 字节的"下一个 start_frn"；不足视为空批次结束。
        return Ok(None);
    }
    let next_frn = u64::from_ne_bytes(buf[0..8].try_into().unwrap_or([0; 8]));
    let records = &buf[8..returned as usize];
    if records.is_empty() {
        return Ok(None);
    }
    Ok(Some((next_frn, records)))
}

/// `FSCTL_READ_USN_JOURNAL` 单次调用：从 `start_usn` 读取增量变更记录到 `buf`。
/// 返回 `(下一次 start_usn, 原始记录字节切片)`。`journal_id` 必须与
/// [`query_usn_journal`] 拿到的一致——journal 一旦被删除重建，`id` 会变化，
/// 旧 `id` 读取会失败（[`NativeIndexError::Ioctl`]），调用方需重新
/// `query_usn_journal` 并触发一次全量重建（见 [`crate::service`]）。
pub(crate) fn read_usn_journal<'buf>(
    volume: &VolumeHandle,
    journal_id: u64,
    start_usn: i64,
    buf: &'buf mut [u8],
) -> Result<(i64, &'buf [u8]), NativeIndexError> {
    let input = READ_USN_JOURNAL_DATA_V0 {
        StartUsn: start_usn,
        ReasonMask: 0xFFFF_FFFF,
        ReturnOnlyOnClose: 0,
        Timeout: 0,
        BytesToWaitFor: 0,
        UsnJournalID: journal_id,
    };
    let mut returned = 0u32;

    // SAFETY：同 `enum_usn_data`——`input` 只读本地值，`buf` 是调用方持有的可写缓冲区。
    let result = unsafe {
        DeviceIoControl(
            volume.0,
            FSCTL_READ_USN_JOURNAL,
            Some(std::ptr::addr_of!(input).cast::<c_void>()),
            u32::try_from(std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>()).unwrap_or(u32::MAX),
            Some(buf.as_mut_ptr().cast::<c_void>()),
            u32::try_from(buf.len()).unwrap_or(u32::MAX),
            Some(&mut returned),
            None,
        )
    };

    if let Err(e) = result {
        // ERROR_MORE_DATA：缓冲区被填满但仍有更多数据——调用方按返回的记录处理后
        // 用新 start_usn 继续读即可，不是错误（与 EOF 语义不同，这里仍有有效负载）。
        if e.code() != ERROR_MORE_DATA.to_hresult() {
            if is_journal_invalidated(&e) {
                return Err(NativeIndexError::JournalInvalidated {
                    detail: e.to_string(),
                });
            }
            return Err(NativeIndexError::Ioctl {
                operation: "FSCTL_READ_USN_JOURNAL",
                detail: e.to_string(),
            });
        }
    }

    if returned < 8 {
        return Ok((start_usn, &buf[0..0]));
    }
    let next_usn = i64::from_ne_bytes(buf[0..8].try_into().unwrap_or([0; 8]));
    let records = &buf[8..returned as usize];
    Ok((next_usn, records))
}

/// 判断 `FSCTL_READ_USN_JOURNAL` 失败是否属于"journal 已失效"——被删除、
/// 正在删除、或其记录已被裁剪，均意味着旧 `journal_id`/游标永久不可用，
/// 与瞬时 I/O 错误（网络卷抖动、句柄暂时繁忙等，重试通常能恢复）性质不同。
/// 调用方（[`crate::service`] 的 tail 线程）据此决定"直接停止待全量重建"还是
/// "退避重试几次"。
fn is_journal_invalidated(e: &windows::core::Error) -> bool {
    let code = e.code();
    code == ERROR_JOURNAL_DELETE_IN_PROGRESS.to_hresult()
        || code == ERROR_JOURNAL_NOT_ACTIVE.to_hresult()
        || code == ERROR_JOURNAL_ENTRY_DELETED.to_hresult()
}

/// 从 `\\?\C:\...` / `C:\...` 形式的绝对路径取盘符（大写）。非绝对路径 /
/// 无盘符（如 UNC 网络路径）返回 `None`——USN Journal 只对本地 NTFS 卷有效。
pub(crate) fn drive_letter_of(path: &Path) -> Option<char> {
    let s = path.to_str()?;
    let s = s.strip_prefix(r"\\?\").unwrap_or(s);
    let mut chars = s.chars();
    let letter = chars.next()?.to_ascii_uppercase();
    if letter.is_ascii_alphabetic() && chars.next() == Some(':') {
        Some(letter)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_letter_extracts_from_plain_and_extended_paths() {
        assert_eq!(drive_letter_of(Path::new(r"C:\Users\a")), Some('C'));
        assert_eq!(drive_letter_of(Path::new(r"\\?\D:\x")), Some('D'));
        assert_eq!(drive_letter_of(Path::new(r"\\server\share\x")), None);
        assert_eq!(drive_letter_of(Path::new("relative")), None);
    }

    #[test]
    fn journal_invalidated_recognizes_all_three_terminal_codes() {
        for code in [
            ERROR_JOURNAL_DELETE_IN_PROGRESS,
            ERROR_JOURNAL_NOT_ACTIVE,
            ERROR_JOURNAL_ENTRY_DELETED,
        ] {
            let err = windows::core::Error::from_hresult(code.to_hresult());
            assert!(
                is_journal_invalidated(&err),
                "{code:?} 应被判定为 journal 已失效"
            );
        }
    }

    #[test]
    fn journal_invalidated_rejects_transient_io_errors() {
        // 任取几个与 journal 失效无关的错误码，代表典型瞬时 I/O 故障。
        for code in [ERROR_HANDLE_EOF, ERROR_MORE_DATA] {
            let err = windows::core::Error::from_hresult(code.to_hresult());
            assert!(
                !is_journal_invalidated(&err),
                "{code:?} 不应被误判为 journal 已失效"
            );
        }
    }
}
