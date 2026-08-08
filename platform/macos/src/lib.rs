//! macOS 平台适配。

use std::env;
use std::path::PathBuf;
use std::process::Command;

use scout_search_backend::{LocationResolveError, LocationResolver};

/// macOS v0.1 位置 hint 解析器。
#[derive(Debug, Clone)]
pub struct MacOsLocationResolver {
    home_dir: PathBuf,
    screenshot_location: Option<PathBuf>,
    read_screenshot_defaults: bool,
}

impl MacOsLocationResolver {
    /// 从当前进程环境创建 resolver。
    pub fn new() -> Result<Self, LocationResolveError> {
        let home_dir = env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or(LocationResolveError::HomeDirUnavailable)?;

        Ok(Self {
            home_dir,
            screenshot_location: None,
            read_screenshot_defaults: true,
        })
    }

    /// 为测试注入 home 目录与截屏偏好。
    #[must_use]
    pub fn with_home_and_screenshot_location(
        home_dir: PathBuf,
        screenshot_location: Option<PathBuf>,
    ) -> Self {
        Self {
            home_dir,
            screenshot_location,
            read_screenshot_defaults: false,
        }
    }

    fn home_child(&self, child: &str) -> PathBuf {
        let mut path = self.home_dir.clone();
        path.push(child);
        path
    }

    fn home_grandchild(&self, parent: &str, child: &str) -> PathBuf {
        let mut path = self.home_child(parent);
        path.push(child);
        path
    }

    fn screenshot_paths(&self) -> Vec<PathBuf> {
        if let Some(path) = self
            .screenshot_location
            .as_ref()
            .filter(|path| path.is_absolute())
        {
            return vec![path.clone()];
        }

        if self.read_screenshot_defaults {
            if let Some(path) = read_screenshot_location_from_defaults() {
                return vec![path];
            }
        }

        vec![
            self.home_child("Desktop"),
            self.home_grandchild("Pictures", "Screenshots"),
        ]
    }
}

impl LocationResolver for MacOsLocationResolver {
    fn resolve_hint(&self, hint: &str) -> Result<Vec<PathBuf>, LocationResolveError> {
        let normalized = hint.trim().to_ascii_lowercase();
        let paths = match normalized.as_str() {
            "下载" | "downloads" | "download" => vec![self.home_child("Downloads")],
            "桌面" | "desktop" => vec![self.home_child("Desktop")],
            "文稿" | "文档" | "documents" | "document" => vec![self.home_child("Documents")],
            "图片" | "照片" | "pictures" | "picture" | "photos" => {
                vec![self.home_child("Pictures")]
            }
            "影片" | "视频" | "movies" | "movie" | "videos" | "video" => {
                vec![self.home_child("Movies")]
            }
            "音乐" | "music" => vec![self.home_child("Music")],
            "截屏" | "截图" | "screenshots" | "screenshot" => self.screenshot_paths(),
            _ => {
                return Err(LocationResolveError::UnsupportedHint {
                    hint: hint.to_owned(),
                });
            }
        };

        Ok(paths)
    }
}

fn read_screenshot_location_from_defaults() -> Option<PathBuf> {
    let output = Command::new("defaults")
        .args(["read", "com.apple.screencapture", "location"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8(output.stdout).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    let path = PathBuf::from(trimmed);
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn resolver() -> MacOsLocationResolver {
        MacOsLocationResolver::with_home_and_screenshot_location(
            PathBuf::from("/Users/tester"),
            None,
        )
    }

    #[test]
    fn resolves_standard_user_folder_hints() {
        let resolver = resolver();

        for (hint, expected) in [
            ("下载", "/Users/tester/Downloads"),
            ("desktop", "/Users/tester/Desktop"),
            ("文稿", "/Users/tester/Documents"),
            ("图片", "/Users/tester/Pictures"),
            ("影片", "/Users/tester/Movies"),
            ("音乐", "/Users/tester/Music"),
        ] {
            let paths = resolver.resolve_hint(hint).unwrap();
            assert_eq!(paths, vec![PathBuf::from(expected)]);
            assert!(paths[0].is_absolute());
        }
    }

    #[test]
    fn screenshot_defaults_location_wins_when_available() {
        let resolver = MacOsLocationResolver::with_home_and_screenshot_location(
            PathBuf::from("/Users/tester"),
            Some(PathBuf::from("/Volumes/Media/Shots")),
        );

        assert_eq!(
            resolver.resolve_hint("截屏").unwrap(),
            vec![PathBuf::from("/Volumes/Media/Shots")]
        );
    }

    #[test]
    fn screenshot_falls_back_to_desktop_and_pictures_screenshots() {
        let paths = resolver().resolve_hint("screenshots").unwrap();

        assert_eq!(
            paths,
            vec![
                PathBuf::from("/Users/tester/Desktop"),
                PathBuf::from("/Users/tester/Pictures/Screenshots"),
            ]
        );
    }

    #[test]
    fn unsupported_hint_is_reported() {
        let error = resolver().resolve_hint("项目归档").unwrap_err();

        assert!(matches!(
            error,
            LocationResolveError::UnsupportedHint { .. }
        ));
    }
}
