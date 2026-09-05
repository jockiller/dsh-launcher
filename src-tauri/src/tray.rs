use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

use crate::config::LauncherConfig;

const FRAME_PERIOD: Duration = Duration::from_millis(300);

const PHASE_IDLE: u8 = 0;
const PHASE_HEALTHY: u8 = 1;
const PHASE_BUSY: u8 = 2;

static PHASE: AtomicU8 = AtomicU8::new(PHASE_IDLE);
static CYCLE_RUNNING: AtomicBool = AtomicBool::new(true);

const RAW_TRAY_PNG: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/icons/tray-icon.png"
));

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
    let open_item = MenuItem::with_id(app, "open_app", "打开", true, None::<&str>)?;
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
            &open_item,
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
            "open_app" => {
                // 打开：优先展示正在运行的 DSH 内嵌 WebView，否则唤起主窗口
                let app_handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    let maybe_url = if let Some(state) = app_handle.try_state::<crate::AppState>() {
                        if let Ok(service) = state.service.lock() {
                            service.authenticated_url().or_else(|| service.status().url)
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    if let Some(url) = maybe_url {
                        let _ = crate::service::open_content_view(&app_handle, &url, false);
                    } else {
                        show_main_window(&app_handle);
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

    // 启动托盘图标状态闪烁后台线程（有活跃会话时闪烁小绿点）
    let app_handle = app.clone();
    let _ = std::thread::Builder::new()
        .name("tray-blink".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                cycle_loop(app_handle.clone());
            }));
            if result.is_err() {
                crate::service::emit_log(&app_handle, "tray", "error", "托盘工作状态闪烁线程异常退出");
            }
        });

    Ok(())
}

/// 退出时停止托盘闪烁线程，避免退出后后台线程悬挂。
pub fn stop() {
    CYCLE_RUNNING.store(false, Ordering::Release);
}

/// 服务未启动或已停止时调用：恢复常态图标，不闪烁。
pub fn set_idle() {
    PHASE.store(PHASE_IDLE, Ordering::Release);
}

/// 服务正常运行且无活跃会话时调用：恢复常态图标，不闪烁。
pub fn set_running_healthy() {
    PHASE.store(PHASE_HEALTHY, Ordering::Release);
}

/// 会话监控线程状态同步：只在有活跃会话（active == true）时触发小绿点闪烁。
/// 附带 cancel 取消标志拦截，防止旧任务或已停止服务覆盖当前状态。
pub fn set_running_checked(active: bool, cancel: &AtomicBool) {
    if cancel.load(Ordering::Acquire) {
        return;
    }
    PHASE.store(
        if active { PHASE_BUSY } else { PHASE_HEALTHY },
        Ordering::Release,
    );
}

/// 运行时动态切换托盘图标显隐（由 save_config 触发）
pub fn apply_visibility(app: &AppHandle, visible: bool) {
    if let Some(tray) = app.tray_by_id("dsh-tray") {
        let _ = tray.set_visible(visible);
    }
}

/// 判断当前宿主是否处于深色外观模式（暗色菜单栏/任务栏）
pub(crate) fn is_system_dark(app: &AppHandle) -> bool {
    if let Some(window) = app.get_window("main") {
        if let Ok(theme) = window.theme() {
            return theme == tauri::Theme::Dark;
        }
    }
    #[cfg(target_os = "macos")]
    {
        use objc2::MainThreadMarker;
        use objc2_app_kit::NSApplication;
        use objc2_foundation::NSString;
        if let Some(mtm) = MainThreadMarker::new() {
            let ns_app = NSApplication::sharedApplication(mtm);
            let name = ns_app.effectiveAppearance().name();
            let dark_name = NSString::from_str("NSAppearanceNameDarkAqua");
            return name.isEqualToString(&dark_name);
        }
    }
    false
}

/// 为托盘图标生成特定状态与主题下的单帧像素（RGBA 格式）。
/// - `with_green_dot`: 是否在右下角叠加鲜亮小绿点（工作状态忙碌闪烁帧）。
/// - `is_dark`: 当前是否为深色背景（深色菜单栏时 base 图标置白，浅色置深灰）。
fn generate_tray_frame(raw_rgba: &[u8], width: u32, height: u32, is_dark: bool, with_green_dot: bool) -> Vec<u8> {
    let mut rgba = raw_rgba.to_vec();
    let base_rgb = if is_dark { [255, 255, 255] } else { [35, 35, 35] };

    // 为 base 图标非透明像素赋予当前主题前景色
    for chunk in rgba.chunks_exact_mut(4) {
        if chunk[3] > 0 {
            chunk[0] = base_rgb[0];
            chunk[1] = base_rgb[1];
            chunk[2] = base_rgb[2];
        }
    }

    if with_green_dot {
        // 在右下角绘制浑圆且带抗锯齿平滑边缘的高对比小绿点（自适应宽高比例定位）
        let cx = (width as f64) - 5.5_f64;
        let cy = (height as f64) - 5.5_f64;
        let r = 3.6_f64;
        let dot_rgb = [34_u8, 197_u8, 94_u8]; // 翠绿 #22c55e

        for y in 0..height {
            for x in 0..width {
                let dx = x as f64 - cx;
                let dy = y as f64 - cy;
                let d = (dx * dx + dy * dy).sqrt();
                let idx = ((y * width + x) * 4) as usize;
                if d <= r - 0.5 {
                    rgba[idx] = dot_rgb[0];
                    rgba[idx + 1] = dot_rgb[1];
                    rgba[idx + 2] = dot_rgb[2];
                    rgba[idx + 3] = 255;
                } else if d <= r + 0.5 {
                    let factor = (r + 0.5 - d).clamp(0.0, 1.0);
                    let current_a = rgba[idx + 3] as f64 / 255.0;
                    let dot_a = factor;
                    let out_a = dot_a + current_a * (1.0 - dot_a);
                    if out_a > 0.0 {
                        rgba[idx] = ((dot_rgb[0] as f64 * dot_a + rgba[idx] as f64 * current_a * (1.0 - dot_a)) / out_a).round() as u8;
                        rgba[idx + 1] = ((dot_rgb[1] as f64 * dot_a + rgba[idx + 1] as f64 * current_a * (1.0 - dot_a)) / out_a).round() as u8;
                        rgba[idx + 2] = ((dot_rgb[2] as f64 * dot_a + rgba[idx + 2] as f64 * current_a * (1.0 - dot_a)) / out_a).round() as u8;
                        rgba[idx + 3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
                    }
                }
            }
        }
    }

    rgba
}

/// 后台托盘图标帧循环：
/// - 仅在 PHASE_BUSY（有执行中的会话）时，按 300ms 周期在“亮绿点”与“灭绿点”之间交替闪烁。
/// - PHASE_IDLE 或 PHASE_HEALTHY 状态下恢复原始 template 图标，并仅在状态变化时提交一次，避免空转。
fn cycle_loop(app: AppHandle) {
    let Ok(base_image) = tauri::image::Image::from_bytes(RAW_TRAY_PNG) else {
        return;
    };
    let (width, height) = (base_image.width(), base_image.height());
    let raw_rgba = base_image.rgba().to_vec();

    let mut last_phase: Option<u8> = None;
    let mut dot_active = false;

    while CYCLE_RUNNING.load(Ordering::Acquire) {
        std::thread::sleep(FRAME_PERIOD);
        if !CYCLE_RUNNING.load(Ordering::Acquire) {
            break;
        }

        let phase = PHASE.load(Ordering::Acquire);
        let Some(tray) = app.tray_by_id("dsh-tray") else {
            continue;
        };

        if phase == PHASE_BUSY {
            dot_active = !dot_active;
            let is_dark = is_system_dark(&app);
            let frame_rgba = generate_tray_frame(&raw_rgba, width, height, is_dark, dot_active);
            let icon = tauri::image::Image::new_owned(frame_rgba, width, height);
            let _ = tray.set_icon_with_as_template(Some(icon), false);
            last_phase = Some(PHASE_BUSY);
        } else {
            // 从 BUSY 切回非 BUSY，或首次同步，恢复为原生的 template 单色图标
            if last_phase != Some(phase) {
                let _ = tray.set_icon_with_as_template(
                    Some(tauri::image::Image::new(&raw_rgba, width, height)),
                    true,
                );
                last_phase = Some(phase);
                dot_active = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_phase_constants_and_atomic_flow() {
        let cancel = AtomicBool::new(false);
        set_idle();
        assert_eq!(PHASE.load(Ordering::Acquire), PHASE_IDLE);

        set_running_healthy();
        assert_eq!(PHASE.load(Ordering::Acquire), PHASE_HEALTHY);

        set_running_checked(true, &cancel);
        assert_eq!(PHASE.load(Ordering::Acquire), PHASE_BUSY);

        set_running_checked(false, &cancel);
        assert_eq!(PHASE.load(Ordering::Acquire), PHASE_HEALTHY);

        cancel.store(true, Ordering::Release);
        set_running_checked(true, &cancel);
        // 被 cancel 拦截，不应变为 BUSY
        assert_eq!(PHASE.load(Ordering::Acquire), PHASE_HEALTHY);
    }

    #[test]
    fn tray_green_dot_generation_adds_bright_green_pixels() {
        let (w, h) = (34, 28);
        let raw = vec![0_u8; (w * h * 4) as usize];
        let frame_with_dot = generate_tray_frame(&raw, w, h, true, true);
        assert_eq!(frame_with_dot.len(), (w * h * 4) as usize);

        // 检查圆心附近像素应为鲜绿色
        let (dot_x, dot_y) = (28, 22);
        let idx = ((dot_y * w + dot_x) * 4) as usize;
        assert_eq!(frame_with_dot[idx], 34);     // R
        assert_eq!(frame_with_dot[idx + 1], 197); // G
        assert_eq!(frame_with_dot[idx + 2], 94);  // B
        assert_eq!(frame_with_dot[idx + 3], 255); // A

        // 检查无绿点帧：对应像素应为全透明
        let frame_no_dot = generate_tray_frame(&raw, w, h, true, false);
        assert_eq!(frame_no_dot[idx + 3], 0);
    }
}
