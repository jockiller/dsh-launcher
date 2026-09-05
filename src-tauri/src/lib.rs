mod app_update;
mod config;
mod managed;
mod service;
mod session_monitor;
mod tray;
mod update;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use config::LauncherConfig;
use serde::Serialize;
use service::{ServiceManager, ServiceStatus};
use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, RunEvent, State, WindowEvent};

struct AppState {
    service: Mutex<ServiceManager>,
    maintenance: AtomicBool,
    /// 内容子 WebView 是否应处于隐藏状态（标题栏弹层/模态/过渡激活期间为 true）。
    /// 记忆在此处，创建内容 WebView 时可立即应用，避免闪现盖住弹层。
    content_hidden: AtomicBool,
    /// 启动器推送给 DSH 内容页的生效主题（None = 尚未接管，页面跟随系统）。
    content_theme: Mutex<Option<String>>,
}

/// CloseRequested 回调帧内不直接 hide（与多 WebView 关闭流程互锁），
/// 标记后延迟到 MainEventsCleared 帧执行。
static PENDING_MAIN_HIDE: AtomicBool = AtomicBool::new(false);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Bootstrap {
    app_version: &'static str,
    platform: &'static str,
    config: LauncherConfig,
    detected_dsh: Option<String>,
    dsh_version: Option<String>,
    dsh_theme_preference: String,
    profiles: Vec<String>,
    status: ServiceStatus,
}

#[tauri::command]
fn bootstrap(app: AppHandle, state: State<'_, AppState>) -> Result<Bootstrap, String> {
    let mut config = LauncherConfig::load();
    // 托管目录旧布局迁移：入口从 dsh-managed 改名为根目录下的 dsh（详见 managed::migrate_layout）。
    if !config.managed_runtime_dir.trim().is_empty() {
        match managed::migrate_layout(Path::new(&config.managed_runtime_dir)) {
            Ok(wrapper) => {
                let wrapper = wrapper.to_string_lossy().into_owned();
                if config.dsh_path.trim().is_empty()
                    || config.dsh_path
                        == managed::legacy_managed_dsh_path(Path::new(
                            &config.managed_runtime_dir,
                        ))
                        .to_string_lossy()
                {
                    config.dsh_path = wrapper;
                    let _ = config.save();
                }
            }
            Err(error) => {
                service::emit_log(
                    &app,
                    "launcher",
                    "error",
                    &format!("Failed to migrate managed DSH layout: {error}"),
                );
            }
        }
    }
    let detected = service::resolve_dsh(&config.dsh_path);
    let version = detected
        .as_ref()
        .and_then(|path| service::dsh_version(path).ok());
    let profiles = service::discover_profiles();
    let status = state.service.lock().map_err(|e| e.to_string())?.status();
    let dsh_theme_preference = service::read_dsh_theme_preference();

    Ok(Bootstrap {
        app_version: env!("CARGO_PKG_VERSION"),
        platform: std::env::consts::OS,
        config,
        detected_dsh: detected.map(|path| path.to_string_lossy().into_owned()),
        dsh_version: version,
        dsh_theme_preference,
        profiles,
        status,
    })
}

#[tauri::command]
fn save_config(app: AppHandle, config: LauncherConfig) -> Result<(), String> {
    config.validate()?;
    let show_tray = config.show_tray_icon;
    config.save().map_err(|e| e.to_string())?;
    tray::apply_visibility(&app, show_tray);
    Ok(())
}

#[tauri::command]
fn detect_dsh() -> Result<(String, String), String> {
    let path =
        service::resolve_dsh("").ok_or_else(|| "未找到 dsh，请手动指定可执行文件".to_string())?;
    let version = service::dsh_version(&path)?;
    Ok((path.to_string_lossy().into_owned(), version))
}

#[tauri::command]
fn validate_dsh(path: String) -> Result<String, String> {
    service::dsh_version(&PathBuf::from(path))
}

#[tauri::command]
fn start_service(
    app: AppHandle,
    state: State<'_, AppState>,
    config: LauncherConfig,
) -> Result<ServiceStatus, String> {
    if state.maintenance.load(Ordering::Acquire) {
        return Err("DSH 正在维护升级，暂时不能启动".into());
    }
    config.validate()?;
    config.save().map_err(|e| e.to_string())?;
    state
        .service
        .lock()
        .map_err(|e| e.to_string())?
        .start(app, config, false)
}

#[tauri::command]
fn stop_service(app: AppHandle, state: State<'_, AppState>) -> Result<ServiceStatus, String> {
    state
        .service
        .lock()
        .map_err(|e| e.to_string())?
        .stop(Some(&app))
}

#[tauri::command]
async fn force_stop_external_service(
    app: AppHandle,
    config: LauncherConfig,
) -> Result<ServiceStatus, String> {
    config.validate()?;
    // 终止流程包含多次子进程调用与最长数秒的等待，放到阻塞线程池执行，
    // 避免长时间占用 IPC 运行时线程（沿用 install_managed_runtime 的做法）。
    let task_app = app.clone();
    let task = tauri::async_runtime::spawn_blocking(move || {
        let state = task_app.state::<AppState>();
        state
            .service
            .lock()
            .map_err(|error| error.to_string())?
            .force_stop_external(&task_app, &config)
    })
    .await;
    match task {
        Ok(result) => result,
        Err(error) => Err(format!("强制停止任务异常结束：{error}")),
    }
}

#[tauri::command]
fn restart_service(
    app: AppHandle,
    state: State<'_, AppState>,
    config: LauncherConfig,
) -> Result<ServiceStatus, String> {
    if state.maintenance.load(Ordering::Acquire) {
        return Err("DSH 正在维护升级，暂时不能重启".into());
    }
    config.validate()?;
    config.save().map_err(|e| e.to_string())?;
    let mut service = state.service.lock().map_err(|e| e.to_string())?;
    service.stop(Some(&app))?;
    service.start(app, config, true)
}

#[tauri::command]
fn service_status(state: State<'_, AppState>) -> Result<ServiceStatus, String> {
    Ok(state.service.lock().map_err(|e| e.to_string())?.status())
}

#[tauri::command]
fn embedded_webview_open(app: AppHandle) -> bool {
    service::embedded_view_open(&app)
}

/// 模态框/标题栏弹层打开期间隐藏 DSH 内容 WebView，让标题栏层的真实弹窗可见；
/// 关闭后恢复显示。内容页状态保留（隐藏而非销毁）。
#[tauri::command]
fn set_content_hidden(app: AppHandle, state: State<'_, AppState>, hidden: bool) -> Result<(), String> {
    state.content_hidden.store(hidden, Ordering::Release);
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if let Some(content) = app.get_webview("content") {
        let _ = if hidden { content.hide() } else { content.show() };
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = app;
    Ok(())
}

/// 把标题栏当前生效的主题推送给 DSH 内容页，保持两端一致。
/// 主题同时记忆在 AppState 与 DSH 页面 localStorage：页面导航/刷新后由
/// on_page_load 钩子与初始化脚本恢复，避免跟随系统脚本把覆盖值冲掉。
#[tauri::command]
fn set_content_theme(app: AppHandle, state: State<'_, AppState>, theme: String) -> Result<(), String> {
    let theme = if theme == "dark" { "dark" } else { "light" }.to_string();
    *state.content_theme.lock().map_err(|error| error.to_string())? = Some(theme.clone());
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if let Some(content) = app.get_webview("content") {
        let _ = content.eval(service::theme_apply_script(&theme));
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = app;
    Ok(())
}

/// 读取当前 Profile 已安装插件列表（package.json dependencies）。
#[tauri::command]
async fn list_profile_plugins(profile: Option<String>) -> Result<Vec<service::ProfilePlugin>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        service::read_profile_plugins(profile.as_deref().unwrap_or("web"))
    })
    .await
    .map_err(|error| format!("读取插件列表任务异常结束：{error}"))?
}

/// 卸载 Profile 插件：优先 `pnpm remove`，失败回退手动清理；过程写入服务日志。
#[tauri::command]
async fn uninstall_profile_plugin(
    app: AppHandle,
    profile: Option<String>,
    name: String,
) -> Result<(), String> {
    let task = tauri::async_runtime::spawn_blocking(move || {
        service::uninstall_profile_plugin(&app, profile.as_deref().unwrap_or("web"), &name)
    })
    .await;
    match task {
        Ok(result) => result,
        Err(error) => Err(format!("卸载插件任务异常结束：{error}")),
    }
}

/// 在 Profile 目录执行 `pnpm clean --lockfile`，输出写入服务日志。
#[tauri::command]
async fn run_profile_clean(app: AppHandle, profile: Option<String>) -> Result<(), String> {
    let task = tauri::async_runtime::spawn_blocking(move || {
        service::run_profile_clean(&app, profile.as_deref().unwrap_or("web"))
    })
    .await;
    match task {
        Ok(result) => result,
        Err(error) => Err(format!("清理任务异常结束：{error}")),
    }
}

/// 显式以内嵌视图打开/揭示 DSH。
#[tauri::command]
async fn open_embedded_view(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    // 显式打开：清除隐藏标记，确保切回 webview
    state.content_hidden.store(false, Ordering::Release);
    // add_child 需要派发到主线程，放在异步命令/独立线程上执行，避免阻塞 IPC。
    let url = {
        let service = state.service.lock().map_err(|error| error.to_string())?;
        service
            .authenticated_url()
            .or_else(|| service.status().url)
            .ok_or_else(|| "DSH 服务尚未运行".to_string())?
    };
    let task_app = app.clone();
    // 揭示语义：不重新导航，已加载的页面原样展示，避免闪烁
    tauri::async_runtime::spawn_blocking(move || service::open_content_view(&task_app, &url, false))
        .await
        .map_err(|error| format!("打开内嵌视图任务异常结束：{error}"))?
}

#[tauri::command]
fn open_project_page() -> Result<(), String> {
    service::open_default("https://github.com/jockiller/dsh-desktop")
}

#[tauri::command]
fn open_dsh_github_page() -> Result<(), String> {
    service::open_default("https://github.com/deepseek-ai/deepseek-harness")
}

/// 使用系统默认浏览器打开 DSH（带认证 token 的地址）。独立于内置 WebView 的入口。
#[tauri::command]
async fn open_service_url(state: State<'_, AppState>) -> Result<(), String> {
    let url = {
        let service = state.service.lock().map_err(|e| e.to_string())?;
        // 优先使用 dsh 启动日志捕获的带 token URL；回退到状态里的固定地址
        service
            .authenticated_url()
            .or_else(|| service.status().url)
            .ok_or_else(|| "DSH 服务尚未运行".to_string())?
    };
    // open_default 内部等待子进程退出，放到阻塞线程池执行
    tauri::async_runtime::spawn_blocking(move || service::open_default(&url))
        .await
        .map_err(|error| format!("打开浏览器任务异常结束：{error}"))?
}

#[tauri::command]
fn open_profile_dir(profile: Option<String>) -> Result<(), String> {
    let dir = service::profile_directory(profile.as_deref().unwrap_or("web"))
        .ok_or_else(|| "无法定位 DSH Profile 目录".to_string())?;
    let _ = std::fs::create_dir_all(&dir);
    service::open_default(&dir.to_string_lossy())
}

/// Launcher 更新检测：启动时自动检查一次；`force` 为 true 时忽略缓存重新请求。
/// 网络请求在阻塞线程池执行。网络失败返回 `None`。
#[tauri::command]
async fn check_launcher_update(force: Option<bool>) -> Option<update::ReleaseUpdate> {
    update::release_update(force.unwrap_or(false)).await
}

/// 打开版本按钮对应的 Release 页面：只放行本项目 GitHub Release 相关 URL，
/// 其余一律拒绝，避免把任意 URL 交给系统默认浏览器。
#[tauri::command]
fn open_release_page(url: String) -> Result<(), String> {
    if update::validate_release_url(&url) {
        service::open_default(url.trim())
    } else {
        Err("仅允许打开 DSH Desktop 的 GitHub Release 页面".into())
    }
}

#[tauri::command]
async fn install_managed_runtime(
    app: AppHandle,
    root: String,
    use_mirror: bool,
) -> Result<managed::ManagedStatus, String> {
    let log_app = app.clone();
    let task = tauri::async_runtime::spawn_blocking(move || {
        managed::install_managed(app, PathBuf::from(root), use_mirror)
    })
    .await;
    match task {
        Ok(result) => {
            if let Err(error) = &result {
                service::emit_log(&log_app, "installer", "error", error);
            }
            result
        }
        Err(error) => {
            let message = format!("安装任务异常结束：{error}");
            service::emit_log(&log_app, "installer", "error", &message);
            Err(message)
        }
    }
}

#[tauri::command]
async fn upgrade_managed_dsh(
    app: AppHandle,
    state: State<'_, AppState>,
    root: String,
) -> Result<managed::ManagedStatus, String> {
    if let Err(error) = managed::managed_status(PathBuf::from(&root).as_path()) {
        service::emit_log(&app, "installer", "error", &error);
        return Err(error);
    }
    state
        .maintenance
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| "已有维护升级任务正在进行".to_string())?;
    service::emit_log(&app, "installer", "info", "Stopping DSH before upgrade");
    let stop_result = state
        .service
        .lock()
        .map_err(|error| error.to_string())
        .and_then(|mut service| {
            if service.status().phase == "external" {
                return Err("检测到 Launcher 无法停止的外部 DSH 服务，请先手动停止后再升级".into());
            }
            service.stop(Some(&app))
        });
    if let Err(error) = stop_result {
        service::emit_log(&app, "installer", "error", &error);
        state.maintenance.store(false, Ordering::Release);
        return Err(error);
    }
    let log_app = app.clone();
    let task = tauri::async_runtime::spawn_blocking(move || {
        managed::upgrade_managed(app, PathBuf::from(root))
    })
    .await;
    state.maintenance.store(false, Ordering::Release);
    match task {
        Ok(result) => {
            if let Err(error) = &result {
                service::emit_log(&log_app, "installer", "error", error);
            }
            result
        }
        Err(error) => {
            let message = format!("升级任务异常结束：{error}");
            service::emit_log(&log_app, "installer", "error", &message);
            Err(message)
        }
    }
}

#[tauri::command]
async fn managed_runtime_status(root: String) -> Result<managed::ManagedStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        managed::managed_status(PathBuf::from(root).as_path())
    })
    .await
    .map_err(|error| format!("检查托管环境异常结束：{error}"))?
}

#[tauri::command]
async fn check_latest_dsh(root: Option<String>) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || managed::check_latest_dsh(root.as_deref()))
        .await
        .map_err(|error| format!("检查 DSH 更新异常结束：{error}"))?
}

#[tauri::command]
async fn check_dsh_version(current_version: String) -> Result<managed::DshVersionInfo, String> {
    tauri::async_runtime::spawn_blocking(move || managed::check_dsh_version(&current_version))
        .await
        .map_err(|error| format!("检查 DSH 版本异常结束：{error}"))?
}

pub fn shutdown_service(handle: &AppHandle) {
    tray::stop();
    if let Some(state) = handle.try_state::<AppState>()
        && let Ok(mut service) = state.service.lock()
    {
        let _ = service.stop(Some(handle));
    }
}

pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            service: Mutex::new(ServiceManager::new()),
            maintenance: AtomicBool::new(false),
            content_hidden: AtomicBool::new(false),
            content_theme: Mutex::new(None),
        })
        .setup(|app| {
            app.handle()
                .plugin(tauri_plugin_updater::Builder::new().build())
                .map_err(|error| format!("注册 updater 插件失败：{error}"))?;

            // 主窗口：本地前端渲染自定义标题栏与空态页。macOS 保留原生红绿灯
            // （Overlay 覆盖在标题栏上），Windows/Linux 完全无边框、由前端自绘控制按钮。
            let mut main_builder =
                tauri::WebviewWindowBuilder::new(app.handle(), "main", tauri::WebviewUrl::default())
                    .title("DSH Desktop")
                    .inner_size(1180.0, 780.0)
                    .min_inner_size(760.0, 520.0)
                    .resizable(true);
            #[cfg(target_os = "macos")]
            {
                main_builder = main_builder
                    .title_bar_style(tauri::TitleBarStyle::Overlay)
                    .hidden_title(true)
                    .traffic_light_position(tauri::LogicalPosition::new(16.0, 13.0));
            }
            #[cfg(not(target_os = "macos"))]
            {
                main_builder = main_builder.decorations(false);
            }
            let main_window = main_builder
                .build()
                .map_err(|error| format!("创建主窗口失败：{error}"))?;
            // 恢复上次窗口位置/尺寸（仅当落在可见显示器范围内时才信任）
            let saved = config::WebviewWindowState::load()
                .filter(|state| service::window_state_is_visible(app.handle(), state));
            if let Some(state) = saved {
                let _ = main_window.set_size(PhysicalSize::new(
                    state.width.max(760),
                    state.height.max(520),
                ));
                let _ = main_window.set_position(PhysicalPosition::new(state.x, state.y));
            } else {
                let _ = main_window.center();
            }
            // 窗口尺寸变化时保持内容子 WebView 铺满标题栏以下区域
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                let app_handle = app.handle().clone();
                main_window.on_window_event(move |event| {
                    if matches!(event, tauri::WindowEvent::Resized(_)) {
                        service::sync_content_bounds(&app_handle);
                    }
                });
            }

            // 系统托盘初始化
            if let Err(error) = tray::init(app.handle()) {
                service::emit_log(
                    app.handle(),
                    "launcher",
                    "warning",
                    &format!("初始化系统托盘失败：{error}"),
                );
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            save_config,
            detect_dsh,
            validate_dsh,
            start_service,
            stop_service,
            force_stop_external_service,
            restart_service,
            service_status,
            embedded_webview_open,
            set_content_hidden,
            set_content_theme,
            open_embedded_view,
            open_project_page,
            open_dsh_github_page,
            open_service_url,
            open_profile_dir,
            list_profile_plugins,
            uninstall_profile_plugin,
            run_profile_clean,
            check_launcher_update,
            open_release_page,
            install_managed_runtime,
            upgrade_managed_dsh,
            managed_runtime_status,
            check_latest_dsh,
            check_dsh_version,
            app_update::app_update_check,
            app_update::app_update_install,
            app_update::app_update_restart,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build DSH Desktop");

    app.run(|handle, event| {
        // macOS 平台关闭窗口仅隐藏，不退出应用，应用与 DSH 继续常驻在状态栏托盘/Dock 中。
        if let RunEvent::WindowEvent {
            label,
            event: WindowEvent::CloseRequested { api, .. },
            ..
        } = &event
        {
            if label == "main" {
                api.prevent_close();
                // 多 WebView 窗口下，hide 若在 CloseRequested 回调帧内同步执行会与
                // 关闭流程重入互锁（红绿灯点击后无响应）。此处只登记待办，
                // 由 MainEventsCleared 帧（事件循环泵完当前批次后）再执行 hide。
                PENDING_MAIN_HIDE.store(true, Ordering::Release);
            }
            return;
        }

        if let RunEvent::MainEventsCleared = &event
            && PENDING_MAIN_HIDE.swap(false, Ordering::AcqRel)
        {
            if let Some(window) = handle.get_window("main") {
                service::save_window_state(&window);
                let _ = window.hide();
            }
        }

        // 点击 Dock 图标或外部唤起时恢复主窗口
        #[cfg(target_os = "macos")]
        if let RunEvent::Reopen { has_visible_windows, .. } = &event {
            if !has_visible_windows {
                tray::show_main_window(handle);
            }
        }

        if matches!(event, RunEvent::ExitRequested { .. } | RunEvent::Exit) {
            // 退出前保存主窗口状态，下次启动按原位恢复
            if let Some(window) = handle.get_window("main") {
                service::save_window_state(&window);
            }
            shutdown_service(handle);
        }
    });
}
