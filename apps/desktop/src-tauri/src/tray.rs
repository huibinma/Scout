// 2026-07-26：Windows 专属系统托盘——配合 main.rs 里主窗口 `CloseRequested` 拦截
// （`settings::read_close_to_tray` 开关开着时 `hide()` 代替真关闭），让 Scout 能在
// 「点关闭按钮」后驻留后台：索引 / 全局快捷键继续跑，托盘图标常驻，左键或菜单
// 「显示 Scout」唤起、菜单「退出 Scout」才真正退出进程。
//
// 仅 target_os = "windows" 编译——macOS 已有原生「关窗不退出、Dock 图标常驻」心智，
// 不需要额外托盘；本模块整体不进其他平台的编译单元（main.rs 里 `mod tray` 同样按
// target_os 条件引入，见该处注释）。
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    AppHandle, Manager,
};

/// 在 app 启动期挂一个托盘图标 + 右键菜单（显示 / 退出）。
/// 失败不阻断启动——托盘只是「关闭到托盘」这个 opt-in 功能的载体，装不上不该拖垮主功能。
pub fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, "show", "显示 Scout", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "退出 Scout", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

    let mut builder = TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("Scout")
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                button_state: tauri::tray::MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });

    // 复用打包图标（bundle.icon 里的 icon.ico），不需要额外资源文件。
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }

    builder.build(app)?;
    Ok(())
}

/// 从托盘唤起主窗口：可能处于 hide() 或 minimize 两种状态，两个都兜。
pub fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}
