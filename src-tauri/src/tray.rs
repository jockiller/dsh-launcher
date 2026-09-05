use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

use crate::config::LauncherConfig;

pub fn show_main_window(app: &AppHandle) {
    // 多 WebView 模式下 main 不再是 WebviewWindow（is_webview_window=false），
    // get_webview_window 会返回 None，必须用 get_window
    if let Some(window) = app.get_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

pub fn init(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let title_item = MenuItem::with_id(app, "title", "DSH Launcher", false, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let show_item = MenuItem::with_id(app, "show_main", "打开主界面", true, None::<&str>)?;
    let web_item = MenuItem::with_id(app, "open_web", "打开 Web GUI", true, None::<&str>)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let start_item = MenuItem::with_id(app, "start_service", "启动服务", true, None::<&str>)?;
    let stop_item = MenuItem::with_id(app, "stop_service", "停止服务", true, None::<&str>)?;
    let restart_item = MenuItem::with_id(app, "restart_service", "重启服务", true, None::<&str>)?;
    let sep3 = PredefinedMenuItem::separator(app)?;
    let quit_item = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &title_item,
            &sep1,
            &show_item,
            &web_item,
            &sep2,
            &start_item,
            &stop_item,
            &restart_item,
            &sep3,
            &quit_item,
        ],
    )?;

    let tray_icon = tauri::image::Image::from_bytes(include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/icons/tray-icon.png"
    )))?;

    let builder = TrayIconBuilder::with_id("dsh-tray")
        .tooltip("DSH Launcher")
        .icon(tray_icon)
        .icon_as_template(true)
        .menu(&menu)
        .show_menu_on_left_click(false);

    builder
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "show_main" => {
                show_main_window(app);
            }
            "open_web" => {
                // 打开内置 DSH 视图（可能创建子 WebView，add_child 需派发主线程），
                // 放到异步运行时执行，避免在菜单事件（主线程）里同步阻塞。
                let app_handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(state) = app_handle.try_state::<crate::AppState>()
                        && let Ok(service) = state.service.lock()
                        && let Some(url) = service.authenticated_url().or_else(|| service.status().url)
                    {
                        let _ = crate::service::open_content_view(&app_handle, &url, false);
                    }
                });
            }
            "start_service" => {
                let app_handle = app.clone();
                if let Some(state) = app.try_state::<crate::AppState>()
                    && !state.maintenance.load(std::sync::atomic::Ordering::Acquire)
                {
                    let config = LauncherConfig::load();
                    if let Ok(mut service) = state.service.lock() {
                        let phase = service.status().phase;
                        if phase == "stopped" || phase == "failed" || phase == "external" {
                            if let Ok(status) = service.start(app_handle.clone(), config, false) {
                                if status.phase == "external" {
                                    show_main_window(&app_handle);
                                }
                            }
                        }
                    }
                }
            }
            "stop_service" => {
                let app_handle = app.clone();
                if let Some(state) = app.try_state::<crate::AppState>()
                    && let Ok(mut service) = state.service.lock()
                {
                    let _ = service.stop(Some(&app_handle));
                }
            }
            "restart_service" => {
                let app_handle = app.clone();
                if let Some(state) = app.try_state::<crate::AppState>()
                    && !state.maintenance.load(std::sync::atomic::Ordering::Acquire)
                {
                    let config = LauncherConfig::load();
                    if let Ok(mut service) = state.service.lock() {
                        let _ = service.stop(Some(&app_handle));
                        if let Ok(status) = service.start(app_handle.clone(), config, true) {
                            if status.phase == "external" {
                                show_main_window(&app_handle);
                            }
                        }
                    }
                }
            }
            "quit" => {
                let app = app.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    crate::shutdown_service(&app);
                    std::process::exit(0);
                });
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    let visible = LauncherConfig::load().show_tray_icon;
    apply_visibility(app, visible);

    Ok(())
}

/// 运行时动态切换托盘图标显隐（由 save_config 触发）
pub fn apply_visibility(app: &AppHandle, visible: bool) {
    if let Some(tray) = app.tray_by_id("dsh-tray") {
        let _ = tray.set_visible(visible);
    }
}
