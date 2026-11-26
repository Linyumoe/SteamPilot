//! 桌面集成：系统托盘、单实例唤起、开机自启、静默启动。
//! 只处理窗口生命周期，不涉及代理/优选逻辑。

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    App, AppHandle, Manager,
};

/// 是否以「静默启动」方式拉起（开机自启的注册表项带此参数）。
pub fn silent_launch() -> bool {
    std::env::args().any(|a| a == "--silent")
}

/// 关闭窗口时隐藏到托盘，而不是退出程序。
fn hide_on_close(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let w2 = w.clone();
        w.on_window_event(move |e| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = e {
                api.prevent_close();
                let _ = w2.hide();
            }
        });
    }
}

/// 托盘点击：可见则隐藏，隐藏则显示聚焦。
fn toggle_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            let _ = w.show();
            let _ = w.set_focus();
        }
    }
}

/// 初始化托盘图标与菜单。托盘对象交给 app.manage 持有，避免被回收。
pub fn setup(app: &App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示 / 隐藏", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出 SteamPilot", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    let icon = app
        .default_window_icon()
        .cloned()
        .expect("default window icon");

    let tray = TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("SteamPilot - 蒸汽领航")
        .on_menu_event(|app, e| match e.id.as_ref() {
            "show" => toggle_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, e| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                toggle_window(tray.app_handle());
            }
        })
        .build(app)?;

    app.manage(tray);
    hide_on_close(app.handle());
    Ok(())
}

/// 写入 / 删除当前用户的开机自启项。静默启动通过 --silent 参数实现。
#[cfg(windows)]
pub fn set_autostart(enable: bool) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

    let result = if enable {
        match std::env::current_exe() {
            Ok(exe) => {
                let value = format!("\"{}\" --silent", exe.display());
                std::process::Command::new("reg")
                    .args([
                        "add",
                        KEY,
                        "/v",
                        "SteamPilot",
                        "/t",
                        "REG_SZ",
                        "/d",
                        &value,
                        "/f",
                    ])
                    .creation_flags(CREATE_NO_WINDOW)
                    .status()
            }
            Err(_) => return,
        }
    } else {
        std::process::Command::new("reg")
            .args(["delete", KEY, "/v", "SteamPilot", "/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
    };
    let _ = result;
}

#[cfg(not(windows))]
pub fn set_autostart(_enable: bool) {}
