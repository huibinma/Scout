use serde::{Deserialize, Serialize};
use std::fs;
use std::process::Command;
use tauri::AppHandle;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MacOsPermissionStatus {
    Granted,
    NotGranted,
    Unknown,
    NotApplicable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WindowsIndexStatus {
    Indexed,
    NotIndexed,
    Unknown,
    NotApplicable,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OnboardingState {
    #[serde(default)]
    pub macos_fda_shown: bool,
    #[serde(default)]
    pub windows_indexing_shown: bool,
    /// BETA-31：模型下载步骤是否完成（无论下载成功还是「跳过」都 = true）。
    #[serde(default)]
    pub model_download_shown: bool,
}

fn get_onboarding_path(_app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let path = crate::scout_data_dir();

    if !path.exists() {
        fs::create_dir_all(&path).map_err(|e| format!("Failed to create data dir: {}", e))?;
    }

    Ok(path.join("onboarding.json"))
}

#[tauri::command]
pub fn check_macos_full_disk_access() -> Result<MacOsPermissionStatus, String> {
    #[cfg(target_os = "macos")]
    {
        // Heuristic: Try to list a directory that requires FDA
        let home = std::env::var("HOME").map_err(|e| e.to_string())?;

        // TCC database directory is the best test
        let tcc_dir = format!("{}/Library/Application Support/com.apple.TCC", home);
        match fs::read_dir(&tcc_dir) {
            Ok(_) => Ok(MacOsPermissionStatus::Granted),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                Ok(MacOsPermissionStatus::NotGranted)
            }
            Err(_) => {
                // Fallback to Mail directory
                let mail_dir = format!("{}/Library/Mail", home);
                match fs::read_dir(&mail_dir) {
                    Ok(_) => Ok(MacOsPermissionStatus::Granted),
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                        Ok(MacOsPermissionStatus::NotGranted)
                    }
                    Err(_) => Ok(MacOsPermissionStatus::Unknown),
                }
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(MacOsPermissionStatus::NotApplicable)
    }
}

#[tauri::command]
pub fn open_macos_fda_settings() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
            .spawn()
            .map_err(|e| format!("Failed to open system settings: {}", e))?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("Not supported on this platform".to_string())
    }
}

/// BETA-33 cycle 9：`sc query WSearch` 输出 → 服务状态判定（纯函数，供单测）。
///
/// 解析 `STATE : <n> <NAME>` 行的**数字状态码**（locale 无关：zh-CN 真机实测
/// 字段名 `STATE` 与数字码不本地化，只有错误消息本地化）：`4`=RUNNING →
/// `Indexed`（Windows 搜索服务运行中、SystemIndex 可查）；其余（1=STOPPED、
/// 7=PAUSED、各 pending 态）→ `NotIndexed`；整个输出无 STATE 行（如服务未安装
/// 时 sc 的 1060 错误输出）→ `NotIndexed`（服务不可用同样意味着系统搜索臂不可用）。
///
/// 语义注：探测的是「Windows 搜索**服务**是否运行」——服务停了 SystemIndex 必不可查；
/// 至于具体目录是否入索引范围，需 `ISearchCrawlScopeManager` COM interop（unsafe，
/// 与 workspace `unsafe_code = "forbid"` 冲突），不做；目录级引导由快速入门第 1 步
/// 「打开索引选项…」交给用户在系统 UI 里确认。
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn parse_sc_query_state(output: &str) -> WindowsIndexStatus {
    for line in output.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("STATE") {
            if let Some(colon) = rest.find(':') {
                return match rest[colon + 1..].split_whitespace().next() {
                    Some("4") => WindowsIndexStatus::Indexed,
                    Some(_) => WindowsIndexStatus::NotIndexed,
                    None => WindowsIndexStatus::Unknown,
                };
            }
        }
    }
    WindowsIndexStatus::NotIndexed
}

/// BETA-33 cycle 9 真做（原为恒返 `Unknown` 的 stub）：探测 Windows 搜索服务
/// （WSearch）是否运行。消费方：快速入门第 1 步状态条（`WindowsSearchCheckStep`）
/// + `useShouldShowOnboarding`（仅作平台探测、只比对 `NotApplicable`）。
#[tauri::command]
pub fn check_windows_search_indexed() -> Result<WindowsIndexStatus, String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：GUI app spawn sc.exe 不闪控制台黑框（与 windows-search
        // 后端同款惯例）。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        match Command::new("sc")
            .args(["query", "WSearch"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            // sc 自身跑起来了：按输出判(含服务不存在的错误输出 → NotIndexed)。
            Ok(out) => Ok(parse_sc_query_state(&String::from_utf8_lossy(&out.stdout))),
            // sc 都 spawn 不了（异常环境）→ 无法判定。
            Err(_) => Ok(WindowsIndexStatus::Unknown),
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(WindowsIndexStatus::NotApplicable)
    }
}

/// 重构（原 BETA-47 `check_everything_available`）：探测内置原生索引（MFT 枚举 +
/// USN Journal）当前是否可用（选项页「内置原生索引」tab 检测行）。与
/// `enable_native_file_index` 设置**无关**——集成关闭时也要能告知「能不能用」，
/// 供用户决定开关。检测会真的尝试为本机每个已知盘符启动一次索引服务（打开卷句柄 +
/// 查询 USN Journal），最常见的失败原因是**进程未以管理员权限运行**——这是 Win32
/// 打开卷句柄的硬性要求，不是本实现的选择。`scout-native-index` 是 Windows
/// target-gated 依赖，非 Windows 恒 false（v0.9.16 macOS CI E0433 踩坑口径，同
/// model_download.rs shim）。
#[tauri::command]
pub fn check_native_file_index_available() -> bool {
    #[cfg(target_os = "windows")]
    {
        scout_native_index::native_index_available()
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// BETA-35 cycle 6：探测本机是否有可用的 `pdftoppm`（poppler-utils）——扫描版 PDF
/// OCR 管线的页渲染依赖。**跨平台**：Windows 走 poppler-windows / winget，
/// macOS 走 `brew install poppler`。检测语义等价于 [`scout_indexer::PopplerPdfRasterizer::detect`]
/// （`pdftoppm -v` 可 spawn 即认可用），无副作用。
#[tauri::command]
pub fn check_pdftoppm_available() -> bool {
    scout_indexer::PopplerPdfRasterizer::detect()
}

#[tauri::command]
pub fn open_windows_indexing_options() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        Command::new("control.exe")
            .arg("/name")
            .arg("Microsoft.IndexingOptions")
            .spawn()
            .map_err(|e| format!("Failed to open indexing options: {}", e))?;
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("Not supported on this platform".to_string())
    }
}

#[tauri::command]
pub fn get_onboarding_state(app: AppHandle) -> Result<OnboardingState, String> {
    let path = get_onboarding_path(&app)?;
    if !path.exists() {
        return Ok(OnboardingState::default());
    }
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&content).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn complete_onboarding(app: AppHandle, feature: String) -> Result<(), String> {
    let path = get_onboarding_path(&app)?;
    let mut state = if path.exists() {
        let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        serde_json::from_str(&content).map_err(|e| e.to_string())?
    } else {
        OnboardingState::default()
    };

    match feature.as_str() {
        "macos_fda" => state.macos_fda_shown = true,
        "windows_indexing" => state.windows_indexing_shown = true,
        "model_download" => state.model_download_shown = true,
        _ => return Err(format!("Unknown feature: {}", feature)),
    }

    let content = serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?;
    fs::write(path, content).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// zh-CN Windows 真机 `sc query WSearch` 实测输出样本（2026-07-03）：
    /// 字段名 `STATE` 与数字码不本地化，解析 locale 无关。
    const SC_RUNNING: &str = "SERVICE_NAME: WSearch \n        TYPE               : 10  WIN32_OWN_PROCESS  \n        STATE              : 4  RUNNING \n                                (STOPPABLE, NOT_PAUSABLE, ACCEPTS_SHUTDOWN)\n        WIN32_EXIT_CODE    : 0  (0x0)\n";

    #[test]
    fn sc_state_running_is_indexed() {
        assert!(matches!(
            parse_sc_query_state(SC_RUNNING),
            WindowsIndexStatus::Indexed
        ));
    }

    #[test]
    fn sc_state_stopped_is_not_indexed() {
        let out = SC_RUNNING.replace(": 4  RUNNING", ": 1  STOPPED");
        assert!(matches!(
            parse_sc_query_state(&out),
            WindowsIndexStatus::NotIndexed
        ));
    }

    /// 服务不存在时 sc 输出（1060 错误，消息本地化、无 STATE 行）→ NotIndexed。
    #[test]
    fn sc_service_missing_is_not_indexed() {
        let out = "[SC] EnumQueryServicesStatus:OpenService 失败 1060:\n\n指定的服务未安装。\n";
        assert!(matches!(
            parse_sc_query_state(out),
            WindowsIndexStatus::NotIndexed
        ));
    }

    /// STATE 行存在但冒号后为空（畸形输出）→ Unknown（不臆断）。
    #[test]
    fn sc_state_malformed_is_unknown() {
        assert!(matches!(
            parse_sc_query_state("        STATE              : \n"),
            WindowsIndexStatus::Unknown
        ));
    }
}
