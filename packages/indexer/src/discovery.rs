//! 全盘音频路径发现层（BETA-01A）。
//!
//! 复用系统索引快速枚举**全盘**音频路径（仅路径，不读内容），交 [`MusicIndex::index_paths`]
//! （`crate::MusicIndex`）提取入库。Windows 用 Everything `es.exe`、macOS 用 Spotlight `mdfind`。
//! 发现层是**可选加速**——工具不可用时 `discover_audio` 返 [`DiscoveryError::Unavailable`]，
//! 调用方回退目录扫描（守 PROJECT「不强制依赖 Everything」）。

use std::path::PathBuf;

#[cfg(any(windows, target_os = "macos"))]
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

/// 平台默认发现器（Windows Everything / macOS Spotlight）；不支持的平台返回 `None`。
/// 注：返回 `Some` 不代表工具已安装——实际可用性在 [`AudioDiscovery::discover_audio`] 判定。
#[must_use]
pub fn default_audio_discovery() -> Option<Box<dyn AudioDiscovery>> {
    #[cfg(windows)]
    {
        Some(Box::new(EverythingDiscovery))
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
        Some(Box::new(EverythingExtDiscovery::new(
            crate::scan::DOC_EXTS,
            "doc",
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
        Some(Box::new(EverythingExtDiscovery::new(
            crate::scan::IMAGE_EXTS,
            "image",
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
/// 调用点在 `#[cfg(windows)]` EverythingDiscovery + `#[cfg(target_os = "macos")]`
/// SpotlightDiscovery + 同模块 `#[cfg(test)]` 单测三处；Linux build 时 lib target
/// 下两个 cfg 块都不编译，函数变 dead。`cargo clippy ... -D warnings` 在 ubuntu runner
/// 上会把 rustc 的 `dead_code` warn 升 error（[CI workflow ci.yml](../../.github/workflows/ci.yml)
/// 在 ubuntu-22.04 上首跑发现）。这里显式 `allow(dead_code)` 在非 windows/macos 平台
/// 上容忍——函数在所有平台仍编译以供 test 用，且 Mac/Win 真实调用点行为不变。
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub(crate) fn parse_paths_lines(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(|line| line.trim_start_matches('\u{feff}').trim())
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// 音频扩展名查询（Everything `ext:` 语法）。
#[cfg(windows)]
const AUDIO_EXT_QUERY: &str = "ext:mp3;flac;m4a;aac;ogg;opus;wav;wma;aiff;aif;ape";

/// Windows：Everything CLI（`es.exe`）全盘枚举。
#[cfg(windows)]
#[derive(Debug)]
pub struct EverythingDiscovery;

#[cfg(windows)]
impl AudioDiscovery for EverythingDiscovery {
    fn discover_audio(&self) -> Result<Vec<PathBuf>, DiscoveryError> {
        // 经 -export-txt -utf8-bom 导出（规避中文 Windows GBK stdout 破坏 CJK 路径）。
        let export = std::env::temp_dir().join("scout_audio_discovery.txt");
        let ran = es_candidates()
            .into_iter()
            .any(|es| run_es_export(&es, AUDIO_EXT_QUERY, &export));
        if !ran {
            return Err(DiscoveryError::Unavailable {
                detail: "es.exe（Everything CLI）不可用".to_owned(),
            });
        }
        let bytes = std::fs::read(&export).map_err(|e| DiscoveryError::Failed {
            detail: format!("读导出文件失败: {e}"),
        })?;
        let text =
            String::from_utf8_lossy(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes));
        Ok(parse_paths_lines(&text))
    }
}

/// BETA-64 T5：按任意扩展名集合的 Everything CLI 全盘枚举（文档/图片发现层共用）。
/// 逻辑与 [`EverythingDiscovery`] 逐字节同构，仅查询串与导出文件名参数化。
#[cfg(windows)]
#[derive(Debug)]
struct EverythingExtDiscovery {
    ext_query: String,
    /// 导出临时文件名区分标签（避免与音频发现 / 并发的另一发现器互相覆盖导出文件）。
    export_tag: &'static str,
}

#[cfg(windows)]
impl EverythingExtDiscovery {
    fn new(exts: &[&str], export_tag: &'static str) -> Self {
        Self {
            ext_query: format!("ext:{}", exts.join(";")),
            export_tag,
        }
    }
}

#[cfg(windows)]
impl PathDiscovery for EverythingExtDiscovery {
    fn discover(&self) -> Result<Vec<PathBuf>, DiscoveryError> {
        let export = std::env::temp_dir().join(format!("scout_{}_discovery.txt", self.export_tag));
        let ran = es_candidates()
            .into_iter()
            .any(|es| run_es_export(&es, &self.ext_query, &export));
        if !ran {
            return Err(DiscoveryError::Unavailable {
                detail: "es.exe（Everything CLI）不可用".to_owned(),
            });
        }
        let bytes = std::fs::read(&export).map_err(|e| DiscoveryError::Failed {
            detail: format!("读导出文件失败: {e}"),
        })?;
        let text =
            String::from_utf8_lossy(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes));
        Ok(parse_paths_lines(&text))
    }
}

/// 候选 es.exe：PATH（`es.exe`）+ winget 安装路径（经 `LOCALAPPDATA`）。
#[cfg(windows)]
fn es_candidates() -> Vec<String> {
    let mut v = vec!["es.exe".to_owned()];
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let p = std::path::Path::new(&local)
            .join("Microsoft")
            .join("WinGet")
            .join("Packages")
            .join("voidtools.Everything.Cli_Microsoft.Winget.Source_8wekyb3d8bbwe")
            .join("es.exe");
        v.push(p.to_string_lossy().into_owned());
    }
    v
}

/// 调 es.exe 导出全盘匹配路径（`query` 为 es.exe 查询串，如 `ext:mp3;flac;...`）；
/// spawn 成功且退出码 0 返 true。`CREATE_NO_WINDOW`：索引枚举时 spawn es.exe 不闪现
/// 控制台黑框（与 everything 搜索路径一致）。BETA-64 T5：`query` 参数化以供文档/图片
/// 发现层复用（此前硬编码 `AUDIO_EXT_QUERY`，仅供音频发现调用）。
#[cfg(windows)]
fn run_es_export(es: &str, query: &str, export: &std::path::Path) -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new(es)
        .args([query, "-export-txt", &export.to_string_lossy(), "-utf8-bom"])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .is_ok_and(|s| s.success())
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
    /// 其余平台 `None`；真机枚举行为由 CI Windows/macOS release 构建 + 明日真机验证覆盖，
    /// 本仓库沙盒无法起真 mdfind/es.exe 环境）。
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
