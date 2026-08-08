use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

/// 注册全局快捷键。返回 boxed error 以便兼容 `tauri::Builder::setup` 闭包。
pub fn register_global_shortcut(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let shortcut_str = if cfg!(target_os = "macos") {
        "Option+Space"
    } else {
        "Ctrl+Alt+S"
    };

    let shortcut: Shortcut = shortcut_str
        .parse()
        .map_err(|e| format!("Invalid shortcut {shortcut_str}: {e:?}"))?;

    app.global_shortcut()
        .on_shortcut(shortcut, move |app, _shortcut, event| {
            if event.state() == ShortcutState::Pressed {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两个平台的默认快捷键字符串都要能解析成合法 `Shortcut`（不依赖 `cfg(target_os)`，
    /// 在任意开发机上跑都能测到——避免改默认值时手滑打错格式，运行时才炸。
    #[test]
    fn platform_default_shortcuts_parse() {
        for s in ["Option+Space", "Ctrl+Alt+S"] {
            let parsed: Result<Shortcut, _> = s.parse();
            assert!(parsed.is_ok(), "默认快捷键 {s} 应能解析: {parsed:?}");
        }
    }
}
