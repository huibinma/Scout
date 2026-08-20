//! 本 crate 的错误类型。

/// 原生索引服务错误。
#[derive(Debug, thiserror::Error)]
pub enum NativeIndexError {
    /// 当前平台不是 Windows（本服务只支持 NTFS + USN Journal）。
    #[error("native index service is only available on Windows")]
    UnsupportedPlatform,

    /// 打开卷句柄失败——最常见原因是进程未以管理员权限运行（Win32 要求打开
    /// `\\.\<drive>:` 卷句柄需要管理员权限），其次是盘符不存在 / 非 NTFS 卷。
    #[error("打开卷 {drive_letter}: 失败（可能需要管理员权限）: {detail}")]
    VolumeOpen {
        /// 目标盘符。
        drive_letter: char,
        /// 底层 Win32 错误描述。
        detail: String,
    },

    /// 卷未启用 USN Journal，或 `DeviceIoControl` 调用失败。
    #[error("{operation} 失败: {detail}")]
    Ioctl {
        /// 失败的 FSCTL 操作名（用于日志/调试）。
        operation: &'static str,
        /// 底层 Win32 错误描述。
        detail: String,
    },

    /// 路径不含合法本地盘符（如 UNC 网络路径），无法定位所属卷。
    #[error("路径 {0:?} 不含本地盘符，无法定位所属卷")]
    NoLocalDrive(std::path::PathBuf),
}
