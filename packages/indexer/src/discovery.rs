//! 全盘路径发现层（BETA-01A；重构：Windows 侧移除对外部 Everything 的依赖）。
//!
//! 复用快速全盘枚举（仅路径，不读内容），交 [`MusicIndex::index_paths`]
//! （`crate::MusicIndex`）提取入库。Windows 用内置 [`scout_native_index`]
//! （MFT 枚举 + USN Journal，见该 crate 文档）、macOS 用 Spotlight `mdfind`。
//! 发现层是**可选加速**——工具/权限不可用时 `discover_audio` 返
//! [`DiscoveryError::Unavailable`]，调用方回退目录扫描（守 PROJECT「不强制依赖
//! 外部全盘索引工具」）。

use std::path::PathBuf;

#[cfg(target_os = "macos")]
use std::process::Command;

/// 发现层错误。
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// 发现工具不可用（未安装 / 不在 PATH）→ 调用方应回退目录扫描。
    #[error("发现器不可用: {detail}")]
    Unavailable {
        /// 详细原因。
        detail: String,
    },
    /// 工具存在但执行失败。
    #[error("发现失败: {detail}")]
    Failed {
        /// 详细原因。
        detail: String,
    },
}

/// 全盘音频路径发现（仅枚举路径，不读内容）。
pub trait AudioDiscovery: std::fmt::Debug + Send + Sync {
    /// 枚举系统内所有音频文件路径。工具不可用返回 [`DiscoveryError::Unavailable`]。
    fn discover_audio(&self) -> Result<Vec<PathBuf>, DiscoveryError>;
}

/// 平台默认发现器（Windows 内置原生索引 / macOS Spotlight）；不支持的平台返回 `None`。
/// 注：返回 `Some` 不代表实际可用——实际可用性在 [`AudioDiscovery::discover_audio`] 判定。
#[must_use]
pub fn default_audio_discovery() -> Option<Box<dyn AudioDiscovery>> {
    #[cfg(windows)]
    {
        Some(Box::new(NativeIndexAudioDiscovery))
    }
    #[cfg(target_os = "macos")]
    {
        Some(Box::new(SpotlightDiscovery))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        None
    }
}

/// BETA-64 T5：全盘路径发现（仅枚举路径，不读内容），泛化到任意扩展名/谓词集合——
/// 比照 [`AudioDiscovery`]，用于把「发现层」思路从音乐推广到文档 / 图片（ROADMAP
/// BETA-01A 背景注记已预留此扩展方向）。
pub trait PathDiscovery: std::fmt::Debug + Send + Sync {
    /// 枚举匹配的全盘路径。工具不可用返回 [`DiscoveryError::Unavailable`]。
    fn discover(&self) -> Result<Vec<PathBuf>, DiscoveryError>;
}

/// 平台默认「文档」发现器（按 [`crate::scan::DOC_EXTS`] 扩展名匹配）；
/// 不支持的平台返回 `None`。发现失败/不可用由调用方回退 `WalkDir`（同 `AudioDiscovery` 契约）。
#[must_use]
pub fn default_document_discovery() -> Option<Box<dyn PathDiscovery>> {
    #[cfg(windows)]
    {
        Some(Box::new(NativeIndexExtDiscovery::new(
            crate::scan::DOC_EXTS,
        )))
    }
    #[cfg(target_os = "macos")]
    {
        Some(Box::new(SpotlightExtDiscovery::by_extension(
            crate::scan::DOC_EXTS,
        )))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        None
    }
}

/// 平台默认「图片」发现器；Windows 按 [`crate::IMAGE_EXTS`] 扩展名匹配，macOS 走
/// `public.image` UTI 内容类型树（与 [`SpotlightDiscovery`] 对音频用 `public.audio`
/// 同款做法，比逐扩展名 OR 谓词更稳）。不支持的平台返回 `None`。
#[must_use]
pub fn default_image_discovery() -> Option<Box<dyn PathDiscovery>> {
    #[cfg(windows)]
    {
        Some(Box::new(NativeIndexExtDiscovery::new(
            crate::scan::IMAGE_EXTS,
        )))
    }
    #[cfg(target_os = "macos")]
    {
        Some(Box::new(SpotlightExtDiscovery::by_predicate(
            "kMDItemContentTypeTree == \"public.image\"",
        )))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        None
    }
}

/// 把发现工具的输出文本解析为路径列表：去 BOM、按行、trim、滤空。纯函数。
///
/// 重构后（Windows 侧改走内置原生索引、不再文本导出）调用点只剩
/// `#[cfg(target_os = "macos")]` SpotlightDiscovery + 同模块 `#[cfg(test)]` 单测两处；
/// Windows / Linux build 时 lib target 里函数变 dead。`cargo clippy ... -D warnings`
/// 在非 macOS runner 上会把 rustc 的 `dead_code` warn 升 error
/// （[CI workflow ci.yml](../../.github/workflows/ci.yml)）。这里显式 `allow(dead_code)`
/// 在非 macOS 平台上容忍——函数在所有平台仍编译以供 test 用，且 macOS 真实调用点行为不变。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn parse_paths_lines(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(|line| line.trim_start_matches('\u{feff}').trim())
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// 音频扩展名集合（原 Everything `ext:` 查询串的枚举形式，供
/// [`scout_native_index::search_by_extensions`] 复用）。
#[cfg(windows)]
const AUDIO_EXTS: &[&str] = &[
    "mp3", "flac", "m4a", "aac", "ogg", "opus", "wav", "wma", "aiff", "aif", "ape",
];

/// 全盘发现单次查询的结果上限。原 Everything 集成无显式上限（`es.exe` 全量导出）；
/// 内置索引服务同样是内存扫描，但给一个宽裕上限（远超真实个人/企业设备文件量的
/// 合理量级）防御极端场景下的无界内存占用。
#[cfg(windows)]
const DISCOVERY_LIMIT: usize = 2_000_000;

/// Windows：内置原生索引（MFT 枚举 + USN Journal）全盘枚举。
/// 替代原 Everything `es.exe` 集成——见 [`scout_native_index`] crate 文档。
#[cfg(windows)]
#[derive(Debug)]
pub struct NativeIndexAudioDiscovery;

#[cfg(windows)]
impl AudioDiscovery for NativeIndexAudioDiscovery {
    fn discover_audio(&self) -> Result<Vec<PathBuf>, DiscoveryError> {
        if !scout_native_index::native_index_available() {
            return Err(DiscoveryError::Unavailable {
                detail: "内置原生索引不可用（非 NTFS 卷，或进程无管理员权限）".to_owned(),
            });
        }
        Ok(scout_native_index::search_by_extensions(
            AUDIO_EXTS,
            DISCOVERY_LIMIT,
        ))
    }
}

/// BETA-64 T5（重构后改用内置原生索引）：按任意扩展名集合的全盘枚举
/// （文档/图片发现层共用）。逻辑与 [`NativeIndexAudioDiscovery`] 同构，仅扩展名集合
/// 参数化。
#[cfg(windows)]
#[derive(Debug)]
struct NativeIndexExtDiscovery {
    extensions: Vec<&'static str>,
}

#[cfg(windows)]
impl NativeIndexExtDiscovery {
    fn new(exts: &[&'static str]) -> Self {
        Self {
            extensions: exts.to_vec(),
        }
    }
}

#[cfg(windows)]
impl PathDiscovery for NativeIndexExtDiscovery {
    fn discover(&self) -> Result<Vec<PathBuf>, DiscoveryError> {
        if !scout_native_index::native_index_available() {
            return Err(DiscoveryError::Unavailable {
                detail: "内置原生索引不可用（非 NTFS 卷，或进程无管理员权限）".to_owned(),
            });
        }
        Ok(scout_native_index::search_by_extensions(
            &self.extensions,
            DISCOVERY_LIMIT,
        ))
    }
}

/// macOS：Spotlight（`mdfind`）全盘枚举。
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub struct SpotlightDiscovery;

#[cfg(target_os = "macos")]
impl AudioDiscovery for SpotlightDiscovery {
    fn discover_audio(&self) -> Result<Vec<PathBuf>, DiscoveryError> {
        let output = Command::new("mdfind")
            .arg("kMDItemContentTypeTree == \"public.audio\"")
            .output()
            .map_err(|e| DiscoveryError::Unavailable {
                detail: format!("mdfind 不可用: {e}"),
            })?;
        if !output.status.success() {
            return Err(DiscoveryError::Failed {
                detail: "mdfind 非零退出".to_owned(),
            });
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(parse_paths_lines(&text))
    }
}

/// BETA-64 T5：按任意 `mdfind` 谓词的 Spotlight 全盘枚举（文档/图片发现层共用）。
/// 逻辑与 [`SpotlightDiscovery`] 逐字节同构，仅谓词参数化。
#[cfg(target_os = "macos")]
#[derive(Debug)]
struct SpotlightExtDiscovery {
    predicate: String,
}

#[cfg(target_os = "macos")]
impl SpotlightExtDiscovery {
    /// 按扩展名集合构造「文件名匹配任一扩展名」谓词（文档没有单一 UTI 树覆盖
    /// docx/pdf/csv/eml 等混合格式，走扩展名 OR 列表；`cd` = 大小写/变音符不敏感）。
    fn by_extension(exts: &[&str]) -> Self {
        let predicate = exts
            .iter()
            .map(|e| format!("kMDItemFSName == \"*.{e}\"cd"))
            .collect::<Vec<_>>()
            .join(" || ");
        Self {
            predicate: format!("({predicate})"),
        }
    }

    /// 直接传入已构造好的 `mdfind` 谓词（如 `public.image` UTI 内容类型树）。
    fn by_predicate(predicate: &str) -> Self {
        Self {
            predicate: predicate.to_owned(),
        }
    }
}

#[cfg(target_os = "macos")]
impl PathDiscovery for SpotlightExtDiscovery {
    fn discover(&self) -> Result<Vec<PathBuf>, DiscoveryError> {
        let output = Command::new("mdfind")
            .arg(&self.predicate)
            .output()
            .map_err(|e| DiscoveryError::Unavailable {
                detail: format!("mdfind 不可用: {e}"),
            })?;
        if !output.status.success() {
            return Err(DiscoveryError::Failed {
                detail: "mdfind 非零退出".to_owned(),
            });
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(parse_paths_lines(&text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_strips_bom_blank_and_trims() {
        let text = "\u{feff}C:\\Music\\周华健-朋友.mp3\r\n\n  C:\\b.flac  \n";
        let paths = parse_paths_lines(text);
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], PathBuf::from("C:\\Music\\周华健-朋友.mp3"));
        assert_eq!(paths[1], PathBuf::from("C:\\b.flac"));
    }

    #[test]
    fn parse_empty_yields_empty() {
        assert!(parse_paths_lines("").is_empty());
        assert!(parse_paths_lines("\n  \n\u{feff}\n").is_empty());
    }

    #[test]
    fn default_discovery_does_not_panic() {
        // Windows/macOS 返回 Some，其它返回 None；本测只验不 panic。
        let _ = default_audio_discovery();
    }

    /// BETA-64 T5：文档/图片发现器构造与探测均不 panic（Windows/macOS 返回 `Some`，
    /// 其余平台 `None`；真机枚举行为由 CI Windows/macOS release 构建 + 真机验证覆盖，
    /// 本仓库沙盒无法起真 mdfind / 无管理员权限打开 NTFS 卷句柄）。
    #[test]
    fn default_document_and_image_discovery_do_not_panic() {
        let doc = default_document_discovery();
        let image = default_image_discovery();
        #[cfg(any(windows, target_os = "macos"))]
        {
            assert!(doc.is_some());
            assert!(image.is_some());
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            assert!(doc.is_none());
            assert!(image.is_none());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn spotlight_ext_discovery_by_extension_builds_or_predicate() {
        let disc = SpotlightExtDiscovery::by_extension(&["pdf", "docx"]);
        assert_eq!(
            disc.predicate,
            "(kMDItemFSName == \"*.pdf\"cd || kMDItemFSName == \"*.docx\"cd)"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn spotlight_ext_discovery_by_predicate_passes_through_verbatim() {
        let disc =
            SpotlightExtDiscovery::by_predicate("kMDItemContentTypeTree == \"public.image\"");
        assert_eq!(disc.predicate, "kMDItemContentTypeTree == \"public.image\"");
    }
}
