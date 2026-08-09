//! 桌面自动更新：默认每 4 小时轮询 GitHub Releases API，发现新 tag 就提醒用户，
//! 用户点「更新」后台下载对应平台安装包并静默安装、装完自动重启。是否自动检查 /
//! 轮询间隔（[30, 1440] 分钟）经「设置 → 常规」配置，见 `settings.rs` 的
//! `auto_update_enabled` / `auto_update_interval_minutes`。
//!
//! **轻量自研，不接 `tauri-plugin-updater`**（2026-08-09 拍板）：官方插件需要生成新的
//! ed25519 签名密钥对存成 GitHub Actions secret，还要改 `release-macos.yml` /
//! `release-windows.yml` 生成合并 `latest.json`——两个 workflow 都标 `prerelease: true`，
//! GitHub `/releases/latest` 别名不可用，得另建固定 tag 专门托管 manifest。本方案直接调
//! GitHub Releases API 找最新 tag、下载既有的 `Scout_{version}_aarch64.dmg` /
//! `Scout_{version}_x64-setup.exe` 资产，不改任何 CI/workflow、不引入新 secret；信任链
//! 与用户今天手动下载安装的信任级别一致（Windows 安装包本就经 SignPath 签名，macOS
//! 本就未签名/未公证）。
//!
//! **数据保留**：Windows 一侧完全依赖 [`crate`] 同目录的
//! `nsis/uninstall-hooks.nsh` 里已有的 `$UpdateMode` 守卫——NSIS 检测到"覆盖安装同一
//! 产品"会跳过所有数据清理分支，静默重跑新版 `*-setup.exe` 本来就是官方支持的原地升级
//! 路径，settings.json / index.db / models / MCP token 全部保留，本模块不需要另写保留
//! 逻辑。macOS 一侧用户数据在 [`crate::scout_data_dir`]（`~/Library/Application
//! Support/Scout/`），与 `Scout.app` 程序包本就分离，替换程序包天然不碰它们。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::settings;

const GITHUB_RELEASES_API: &str = "https://api.github.com/repos/huibinma/Scout/releases?per_page=5";
const USER_AGENT: &str = "Scout-Desktop-Updater";
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(30);
/// 自动更新关闭时，多久重读一次设置（用户运行期打开开关后无需重启即生效）。
const DISABLED_RECHECK_INTERVAL: Duration = Duration::from_secs(60);
const PROGRESS_EMIT_BYTES: u64 = 256 * 1024; // 安装包比模型小得多，256 KB 一跳足够顺滑

#[derive(Debug, Clone, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// 提醒事件 `update://available` 的 payload，同时也是 `install_update` 的入参来源
/// （前端原样把收到的这几个字段传回）。
#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub notes: String,
    pub asset_name: String,
    pub asset_url: String,
    pub asset_size: u64,
}

#[derive(Clone, Serialize)]
struct DownloadProgressPayload {
    downloaded: u64,
    total: Option<u64>,
}

/// 平台安装包资产的文件名后缀（与 `release-macos.yml` / `release-windows.yml` /
/// `.signpath/artifact-configuration.xml` 实际产出的命名一致，见 `docs/install.md`）。
#[cfg(target_os = "macos")]
const ASSET_SUFFIX: &str = ".dmg";
#[cfg(target_os = "windows")]
const ASSET_SUFFIX: &str = "-setup.exe";

/// 手写三段版本号解析（`v0.9.49` → `(0,9,49)`），不引入 `semver` crate——
/// 项目版本号格式简单固定，没必要为此加依赖。patch 段允许带非数字后缀（如 CI 场景
/// 误打 `0.9.49-beta`），只取数字前缀，解析不出三段视为不可比、返回 None。
fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let mut parts = v.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch_raw = parts.next()?;
    let patch_digits: String = patch_raw.chars().take_while(char::is_ascii_digit).collect();
    let patch = patch_digits.parse().ok()?;
    Some((major, minor, patch))
}

fn pick_asset(release: &GhRelease) -> Option<&GhAsset> {
    release
        .assets
        .iter()
        .find(|a| a.name.ends_with(ASSET_SUFFIX))
}

fn build_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("reqwest client build 失败: {e}"))
}

/// 取 GitHub 上最新一条非草稿 Release（含 prerelease——本项目所有正式发布都标
/// `prerelease: true`，见 STATUS.md，不能只认非 prerelease）。releases 列表 API
/// 默认按创建时间倒序，第一条非 draft 即最新。
async fn fetch_latest_release(client: &reqwest::Client) -> Result<Option<GhRelease>, String> {
    let resp = client
        .get(GITHUB_RELEASES_API)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("GitHub Releases API 请求失败: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("GitHub Releases API HTTP {}", resp.status()));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("读取 GitHub Releases 响应失败: {e}"))?;
    let releases: Vec<GhRelease> =
        serde_json::from_str(&body).map_err(|e| format!("解析 GitHub Releases 响应失败: {e}"))?;

    Ok(releases.into_iter().find(|r| !r.draft))
}

/// 纯逻辑：给定「远端最新 release」判断是否该提醒更新（无网络依赖，便于单测）。
fn evaluate_release(release: &GhRelease, current_version: &str) -> Option<UpdateInfo> {
    let remote = parse_semver(&release.tag_name)?;
    let current = parse_semver(current_version).unwrap_or((0, 0, 0));
    if remote <= current {
        return None;
    }
    let asset = pick_asset(release)?;
    Some(UpdateInfo {
        version: release.tag_name.trim_start_matches('v').to_string(),
        notes: release.body.clone().unwrap_or_default(),
        asset_name: asset.name.clone(),
        asset_url: asset.browser_download_url.clone(),
        asset_size: asset.size,
    })
}

async fn check_for_newer_release(client: &reqwest::Client) -> Result<Option<UpdateInfo>, String> {
    let Some(release) = fetch_latest_release(client).await? else {
        return Ok(None);
    };
    Ok(evaluate_release(&release, env!("CARGO_PKG_VERSION")))
}

/// 常驻后台任务：启动 30 秒后首检，随后按「设置 → 常规」配置的间隔轮询（默认 4
/// 小时，范围 [30, 1440] 分钟）。开关 / 间隔每轮 tick 前都 live-read settings.json，
/// 用户运行期改设置无需重启即生效（与 `auto_index_interval_minutes` 的既有约定一致）。
/// **不做任何持久化**（不记「已跳过版本」/「上次检查时间」）——每次进程重启都会在
/// 30 秒后重新检查一轮，足够满足「定期检查」的字面需求。
pub async fn run_update_check_loop(app: AppHandle, settings_path: Option<PathBuf>) {
    tokio::time::sleep(FIRST_CHECK_DELAY).await;

    let client = match build_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "自动更新检查：构建 HTTP client 失败，本次会话跳过");
            return;
        }
    };

    loop {
        if !settings::read_auto_update_enabled(&settings_path) {
            tracing::debug!("自动更新检查已在设置中关闭，60 秒后重读设置");
            tokio::time::sleep(DISABLED_RECHECK_INTERVAL).await;
            continue;
        }

        match check_for_newer_release(&client).await {
            Ok(Some(info)) => {
                tracing::info!(version = %info.version, "检测到新版本，提醒用户更新");
                let _ = app.emit("update://available", info);
            }
            Ok(None) => {
                tracing::debug!("自动更新检查：当前已是最新版本");
            }
            Err(reason) => {
                tracing::warn!(%reason, "自动更新检查失败，将在下一轮重试");
            }
        }

        let interval_minutes = settings::read_auto_update_interval_minutes(&settings_path);
        tokio::time::sleep(Duration::from_secs(u64::from(interval_minutes) * 60)).await;
    }
}

static INSTALL_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

struct InstallGuard;
impl Drop for InstallGuard {
    fn drop(&mut self) {
        INSTALL_IN_FLIGHT.store(false, Ordering::SeqCst);
    }
}

async fn download_asset(
    client: &reqwest::Client,
    app: &AppHandle,
    url: &str,
    target: &Path,
) -> Result<(), String> {
    let resp = client
        .get(url)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|e| format!("下载安装包失败: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("下载安装包 HTTP {}", resp.status()));
    }

    let total = resp.content_length();
    let mut file = fs::File::create(target)
        .await
        .map_err(|e| format!("创建安装包临时文件失败: {e}"))?;

    let mut stream = resp.bytes_stream();
    let mut downloaded: u64 = 0;
    let mut next_emit: u64 = 0;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("下载数据块失败: {e}"))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("写入安装包失败: {e}"))?;
        downloaded += chunk.len() as u64;
        if downloaded >= next_emit {
            let _ = app.emit(
                "update://download-progress",
                DownloadProgressPayload { downloaded, total },
            );
            next_emit = downloaded + PROGRESS_EMIT_BYTES;
        }
    }

    file.flush()
        .await
        .map_err(|e| format!("安装包写入 flush 失败: {e}"))?;
    Ok(())
}

/// 前端点「更新」触发：下载 + 静默安装 + 装完自动重启。成功路径上进程会自行退出，
/// 不需要单独的「完成」事件；失败时保留下载文件供排查、返回 Err 让前端展示错误态。
#[tauri::command]
pub async fn install_update(
    app: AppHandle,
    version: String,
    asset_name: String,
    asset_url: String,
) -> Result<(), String> {
    if INSTALL_IN_FLIGHT
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("更新已在进行中，请稍候".to_string());
    }
    let _guard = InstallGuard;

    let client = build_client()?;
    let tmp_dir = std::env::temp_dir().join(format!("scout-update-{version}"));
    fs::create_dir_all(&tmp_dir)
        .await
        .map_err(|e| format!("创建临时目录失败: {e}"))?;
    let asset_path = tmp_dir.join(&asset_name);

    if let Err(e) = download_asset(&client, &app, &asset_url, &asset_path).await {
        let _ = fs::remove_file(&asset_path).await;
        return Err(e);
    }

    let _ = app.emit("update://installing", ());

    install_and_relaunch(app, asset_path).await
}

#[cfg(target_os = "macos")]
async fn install_and_relaunch(app: AppHandle, dmg_path: PathBuf) -> Result<(), String> {
    let result = tauri::async_runtime::spawn_blocking(move || install_macos_blocking(&dmg_path))
        .await
        .map_err(|e| format!("安装任务异常退出: {e}"))?;
    result?;
    app.exit(0);
    Ok(())
}

/// macOS 安装：挂载 DMG → 拷贝新 `Scout.app` 到安装目录 → 原地替换 → relaunch。
/// **拷贝失败直接返回 Err、旧 app 完全未动**（先拷到 `.new`、拷贝成功才做 rename 替换，
/// 不做半步替换）。不做提权重试——`/Applications` 对个人电脑单一用户通常可写，写入失败
/// 就让用户走手动下载兜底（`docs/install.md`），保持与"轻量自研"选择一致的实现体量。
#[cfg(target_os = "macos")]
fn install_macos_blocking(dmg_path: &Path) -> Result<(), String> {
    use std::process::Command;

    let current_exe =
        std::env::current_exe().map_err(|e| format!("定位当前可执行文件失败: {e}"))?;
    // .../Scout.app/Contents/MacOS/Scout -> Scout.app
    let app_bundle = current_exe
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or_else(|| "无法从可执行文件路径定位 App Bundle".to_string())?
        .to_path_buf();
    let apps_dir = app_bundle
        .parent()
        .ok_or_else(|| "无法定位安装目录".to_string())?
        .to_path_buf();

    let mount_point =
        std::env::temp_dir().join(format!("scout-update-mount-{}", std::process::id()));
    std::fs::create_dir_all(&mount_point).map_err(|e| format!("创建 DMG 挂载点失败: {e}"))?;

    let attach_ok = Command::new("hdiutil")
        .arg("attach")
        .arg(dmg_path)
        .args(["-nobrowse", "-quiet", "-mountpoint"])
        .arg(&mount_point)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !attach_ok {
        let _ = std::fs::remove_dir_all(&mount_point);
        return Err("挂载安装包（hdiutil attach）失败，安装包可能已损坏".to_string());
    }

    let mounted_app = mount_point.join("Scout.app");
    let staged_new = apps_dir.join("Scout.app.new");
    let _ = std::fs::remove_dir_all(&staged_new); // 清理上次失败残留

    let copy_ok = Command::new("cp")
        .arg("-R")
        .arg(&mounted_app)
        .arg(&staged_new)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let _ = Command::new("hdiutil")
        .args(["detach", "-quiet"])
        .arg(&mount_point)
        .status();
    let _ = std::fs::remove_dir_all(&mount_point);

    if !copy_ok {
        let _ = std::fs::remove_dir_all(&staged_new);
        return Err("拷贝新版本到安装目录失败（可能是权限不足）".to_string());
    }

    let backup = apps_dir.join(format!("Scout.app.bak-{}", chrono::Utc::now().timestamp()));
    std::fs::rename(&app_bundle, &backup).map_err(|e| format!("备份旧版本失败: {e}"))?;
    if let Err(e) = std::fs::rename(&staged_new, &app_bundle) {
        let _ = std::fs::rename(&backup, &app_bundle); // 尽力回滚，保证旧版本仍可用
        return Err(format!("替换为新版本失败: {e}"));
    }

    let _ = Command::new("open").arg("-n").arg(&app_bundle).spawn();
    let _ = std::fs::remove_dir_all(&backup);

    Ok(())
}

/// Windows 安装：新安装包不能覆盖正在运行的 exe/dll，写一个临时 `.bat` 串起
/// 「等自身退出 → 静默安装 → 拉起新进程 → 自删」，spawn 后立即 `app.exit(0)`
/// 释放文件锁。静默参数 `/S` 是 NSIS 标准写法；数据保留完全交给
/// `nsis/uninstall-hooks.nsh` 里已有的 `$UpdateMode` 守卫（见本文件顶部说明），
/// 这里不需要也不做额外的保留逻辑。
#[cfg(target_os = "windows")]
async fn install_and_relaunch(app: AppHandle, installer_path: PathBuf) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let current_exe =
        std::env::current_exe().map_err(|e| format!("定位当前可执行文件失败: {e}"))?;
    let bat_path = std::env::temp_dir().join(format!("scout-update-{}.bat", std::process::id()));
    let script = format!(
        "@echo off\r\ntimeout /t 2 /nobreak >nul\r\nstart \"\" /wait \"{installer}\" /S\r\nstart \"\" \"{exe}\"\r\ndel \"%~f0\"\r\n",
        installer = installer_path.display(),
        exe = current_exe.display(),
    );
    fs::write(&bat_path, script)
        .await
        .map_err(|e| format!("写入更新脚本失败: {e}"))?;

    Command::new("cmd")
        .args(["/C", &bat_path.display().to_string()])
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()
        .map_err(|e| format!("启动安装脚本失败: {e}"))?;

    app.exit(0);
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
async fn install_and_relaunch(_app: AppHandle, _installer_path: PathBuf) -> Result<(), String> {
    Err("当前平台暂不支持自动更新，请到 GitHub Releases 页手动下载".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> GhAsset {
        GhAsset {
            name: name.to_string(),
            browser_download_url: format!("https://example.com/{name}"),
            size: 1024,
        }
    }

    fn release(tag: &str, assets: Vec<GhAsset>) -> GhRelease {
        GhRelease {
            tag_name: tag.to_string(),
            body: Some("变更说明".to_string()),
            draft: false,
            assets,
        }
    }

    #[test]
    fn parse_semver_handles_v_prefix_and_plain() {
        assert_eq!(parse_semver("v0.9.49"), Some((0, 9, 49)));
        assert_eq!(parse_semver("0.9.49"), Some((0, 9, 49)));
        assert_eq!(parse_semver("v1.0.0"), Some((1, 0, 0)));
    }

    #[test]
    fn parse_semver_tolerates_non_numeric_patch_suffix() {
        assert_eq!(parse_semver("v0.9.49-beta"), Some((0, 9, 49)));
    }

    #[test]
    fn parse_semver_rejects_malformed_input() {
        assert_eq!(parse_semver("not-a-version"), None);
        assert_eq!(parse_semver("v0.9"), None);
        assert_eq!(parse_semver(""), None);
    }

    #[test]
    fn semver_tuples_compare_numerically_not_lexically() {
        // (0,9,9) < (0,9,10)：字符串比较会算错，必须是数值 tuple 比较。
        assert!(parse_semver("v0.9.9") < parse_semver("v0.9.10"));
    }

    #[test]
    fn pick_asset_matches_platform_suffix() {
        let r = release(
            "v0.9.50",
            vec![
                asset("Scout_0.9.50_aarch64.dmg"),
                asset("Scout_0.9.50_x64-setup.exe"),
            ],
        );
        let picked = pick_asset(&r).expect("应命中当前平台资产");
        assert!(picked.name.ends_with(ASSET_SUFFIX));
    }

    #[test]
    fn pick_asset_returns_none_without_matching_platform_asset() {
        let r = release("v0.9.50", vec![asset("random-file.txt")]);
        assert!(pick_asset(&r).is_none());
    }

    #[test]
    fn evaluate_release_none_when_remote_not_newer() {
        let r = release(
            "v0.9.49",
            vec![asset(&format!("Scout_0.9.49_x{ASSET_SUFFIX}"))],
        );
        assert!(evaluate_release(&r, "0.9.49").is_none());
        assert!(evaluate_release(&r, "0.9.50").is_none());
    }

    #[test]
    fn evaluate_release_some_when_remote_newer_and_asset_present() {
        let r = release(
            "v0.9.50",
            vec![asset(&format!("Scout_0.9.50_x{ASSET_SUFFIX}"))],
        );
        let info = evaluate_release(&r, "0.9.49").expect("应判定为需要更新");
        assert_eq!(info.version, "0.9.50");
        assert!(info.asset_url.contains(&info.asset_name));
    }

    #[test]
    fn evaluate_release_none_when_newer_but_no_matching_asset() {
        let r = release("v0.9.50", vec![asset("unrelated.zip")]);
        assert!(evaluate_release(&r, "0.9.49").is_none());
    }

    #[tokio::test]
    async fn fetch_latest_release_skips_drafts_and_parses_mock_response() {
        use httptest::matchers::request;
        use httptest::responders::status_code;
        use httptest::{Expectation, Server};

        let server = Server::run();
        let body = serde_json::json!([
            {
                "tag_name": "v0.9.99",
                "draft": true,
                "prerelease": true,
                "body": "草稿，不应被选中",
                "assets": []
            },
            {
                "tag_name": "v0.9.50",
                "draft": false,
                "prerelease": true,
                "body": "正式发布说明",
                "assets": [
                    {
                        "name": format!("Scout_0.9.50_x{ASSET_SUFFIX}"),
                        "browser_download_url": "https://example.com/asset",
                        "size": 12345
                    }
                ]
            }
        ])
        .to_string();

        server.expect(
            Expectation::matching(request::method_path("GET", "/releases"))
                .respond_with(status_code(200).body(body)),
        );

        let client = reqwest::Client::new();
        let url = server.url("/releases").to_string();
        let resp = client
            .get(&url)
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .expect("请求 mock server 失败");
        let text = resp.text().await.expect("读取响应失败");
        let releases: Vec<GhRelease> = serde_json::from_str(&text).expect("解析响应失败");
        let picked = releases
            .into_iter()
            .find(|r| !r.draft)
            .expect("应跳过 draft 取到下一条");
        assert_eq!(picked.tag_name, "v0.9.50");

        let info = evaluate_release(&picked, "0.9.49").expect("应判定为需要更新");
        assert_eq!(info.version, "0.9.50");
    }
}
