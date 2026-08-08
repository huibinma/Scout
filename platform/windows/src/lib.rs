//! Windows 平台适配。
//!
//! 提供位置 hint 解析器。原 v0.1 直接调用 Windows `SHGetKnownFolderPath`
//! （COM / `unsafe`），与 workspace `unsafe_code = "forbid"` 策略冲突且从未在
//! Windows target 上编译过。现改用 `dirs` crate（其内部的 `unsafe` FFI 收敛在依赖
//! 内部），在 Windows / macOS / Linux 上行为一致，本 crate 自身保持零 `unsafe`。

use std::path::PathBuf;

use scout_search_backend::{LocationResolveError, LocationResolver};

/// Windows 位置 hint 解析器。
#[derive(Debug, Clone, Default)]
pub struct WindowsLocationResolver;

impl WindowsLocationResolver {
    /// 创建新的 resolver。
    pub fn new() -> Result<Self, LocationResolveError> {
        Ok(Self)
    }

    /// 将 hint 映射到对应的 Known Folder 路径。
    ///
    /// 截屏 (screenshots) 特殊处理：返回 `Pictures/Screenshots`，与 Windows
    /// 默认截屏目录一致（由调用方决定是否使用）。
    fn resolve_known_folder(hint: &str) -> Result<PathBuf, LocationResolveError> {
        let normalized = hint.trim().to_ascii_lowercase();
        let (base, subdir): (Option<PathBuf>, Option<&str>) = match normalized.as_str() {
            "下载" | "downloads" | "download" => (dirs::download_dir(), None),
            "桌面" | "desktop" => (dirs::desktop_dir(), None),
            "文稿" | "文档" | "documents" | "document" => (dirs::document_dir(), None),
            "图片" | "照片" | "pictures" | "picture" | "photos" => (dirs::picture_dir(), None),
            "影片" | "视频" | "movies" | "movie" | "videos" | "video" => {
                (dirs::video_dir(), None)
            }
            "音乐" | "music" => (dirs::audio_dir(), None),
            "截屏" | "截图" | "screenshots" | "screenshot" => {
                (dirs::picture_dir(), Some("Screenshots"))
            }
            _ => {
                return Err(LocationResolveError::UnsupportedHint {
                    hint: hint.to_owned(),
                })
            }
        };

        let mut path = base.ok_or_else(|| LocationResolveError::Platform {
            detail: format!("known folder for hint '{hint}' is unavailable"),
        })?;
        if let Some(subdir) = subdir {
            path.push(subdir);
        }
        Ok(path)
    }
}

impl LocationResolver for WindowsLocationResolver {
    fn resolve_hint(&self, hint: &str) -> Result<Vec<PathBuf>, LocationResolveError> {
        Ok(vec![Self::resolve_known_folder(hint)?])
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn resolves_standard_user_folder_hints() {
        let resolver = WindowsLocationResolver::new().unwrap();
        // 只要不报 UnsupportedHint 且返回绝对路径即可（Windows 走真实 Known Folder，
        // macOS / Linux 走 dirs 的等价目录）。
        for hint in ["下载", "desktop", "文稿", "图片", "视频", "音乐"] {
            let paths = resolver.resolve_hint(hint).unwrap();
            assert!(!paths.is_empty());
            assert!(paths[0].is_absolute());
        }
    }

    #[test]
    fn screenshot_is_handled() {
        let resolver = WindowsLocationResolver::new().unwrap();
        let paths = resolver.resolve_hint("截屏").unwrap();
        assert!(!paths.is_empty());
        let path_str = paths[0].to_string_lossy();
        // 验证路径以 Screenshots 子目录结尾
        assert!(path_str.contains("Screenshots"));
    }

    #[test]
    fn unsupported_hint_is_reported() {
        let resolver = WindowsLocationResolver::new().unwrap();
        let error = resolver.resolve_hint("我的私密文件夹").unwrap_err();

        assert!(matches!(
            error,
            LocationResolveError::UnsupportedHint { .. }
        ));
    }
}
