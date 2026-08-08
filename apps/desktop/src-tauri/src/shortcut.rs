use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

/// 唤起主窗口的公共处理逻辑，启动期注册与运行期改注册（`update_global_shortcut`）共用。
fn show_and_focus_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// 校验并解析快捷键字符串。**要求至少一个修饰键（Ctrl/Alt/Shift/Cmd）+ 一个主键**——
/// 底层 `global-hotkey` 语法本身允许纯单键（如 `"A"`）注册为全局快捷键，那会让系统级
/// 拦截掉这个键的所有正常输入（用户再也打不出字母 A），必须在这一层就拦掉、不能指望
/// OS 替用户兜底。
fn parse_shortcut(shortcut_str: &str) -> Result<Shortcut, String> {
    let trimmed = shortcut_str.trim();
    if !trimmed.contains('+') {
        return Err(format!(
            "快捷键「{trimmed}」缺少修饰键——必须是「修饰键+按键」组合（如 Ctrl+Space），\
             否则会占用该按键的全部正常输入"
        ));
    }
    trimmed
        .parse::<Shortcut>()
        .map_err(|e| format!("无法识别的快捷键「{trimmed}」：{e:?}"))
}

/// 应用启动时注册全局快捷键：从 settings.json live-read 用户自定义值（缺失 / 解析失败时
/// 回退默认 `Ctrl+Space`，见 [`crate::settings::DEFAULT_GLOBAL_SHORTCUT`]）。
pub fn register_global_shortcut(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let settings_path = crate::settings::settings_file_path(app);
    let configured = crate::settings::read_global_shortcut(&settings_path);
    let shortcut = parse_shortcut(&configured)
        .or_else(|_| parse_shortcut(crate::settings::DEFAULT_GLOBAL_SHORTCUT))?;

    app.global_shortcut()
        .on_shortcut(shortcut, |app, _shortcut, event| {
            if event.state() == ShortcutState::Pressed {
                show_and_focus_main_window(app);
            }
        })?;

    Ok(())
}

/// 「常规」设置面板的快捷键录制器保存时调用：校验格式 → 取消旧注册 → 注册新值 → 落盘。
///
/// 注册失败（含被其他程序占用等系统级冲突）直接把错误原文返回前端、**不落盘**——用户
/// 录完新组合的瞬间就知道行不行，不必等重启后发现快捷键唤不起来才回头排查。成功后写回
/// settings.json，与 `update_settings` 之间的覆写保护见 `settings::merge_backend_managed_fields`。
#[tauri::command]
pub fn update_global_shortcut(app: AppHandle, shortcut: String) -> Result<(), String> {
    let parsed = parse_shortcut(&shortcut)?;

    let manager = app.global_shortcut();
    // 先清场：旧快捷键可能还占着监听。清场本身失败（例如启动期就没注册成功过）不视为
    // 致命错误——真正的成败判定交给紧随其后的注册调用。
    let _ = manager.unregister_all();

    manager
        .on_shortcut(parsed, |app, _shortcut, event| {
            if event.state() == ShortcutState::Pressed {
                show_and_focus_main_window(app);
            }
        })
        .map_err(|e| format!("注册快捷键失败（可能与其他程序冲突）：{e}"))?;

    let settings_path = crate::settings::settings_file_path(&app)
        .ok_or_else(|| "无法解析设置文件路径".to_string())?;
    let mut settings = crate::settings::read_settings_or_default(&Some(settings_path.clone()));
    settings.global_shortcut = shortcut;
    crate::settings::write_settings(&settings_path, &settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认快捷键必须能解析（改默认值时手滑打错格式，编译期就该炸，不要等运行时）。
    #[test]
    fn default_shortcut_parses() {
        assert!(parse_shortcut(crate::settings::DEFAULT_GLOBAL_SHORTCUT).is_ok());
    }

    /// 旧版本写死过的两个平台默认值仍要能解析——用户从旧版本升级后，启动期
    /// live-read 到 settings.json 里残留的这些旧值时不该注册失败。
    #[test]
    fn legacy_platform_defaults_still_parse() {
        for s in ["Option+Space", "Ctrl+Alt+S"] {
            assert!(parse_shortcut(s).is_ok(), "旧默认值 {s} 应仍可解析: {s}");
        }
    }

    /// 核心安全约束：拒绝没有修饰键的组合，防止全局吞掉某个普通按键的输入。
    #[test]
    fn bare_key_without_modifier_rejected() {
        for s in ["A", "Space", "Escape", "1"] {
            assert!(parse_shortcut(s).is_err(), "无修饰键的 {s} 应被拒绝");
        }
    }

    #[test]
    fn empty_or_garbage_shortcut_rejected() {
        assert!(parse_shortcut("").is_err());
        assert!(parse_shortcut("Ctrl+NotAKey").is_err());
    }

    #[test]
    fn multi_modifier_combo_parses() {
        assert!(parse_shortcut("Ctrl+Shift+Space").is_ok());
        assert!(parse_shortcut("Ctrl+Alt+F").is_ok());
    }
}
