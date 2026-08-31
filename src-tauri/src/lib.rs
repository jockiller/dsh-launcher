mod app_update;
mod config;
mod dock_blink;
mod managed;
mod service;
mod session_monitor;
mod update;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use config::LauncherConfig;
use serde::Serialize;
use service::{ServiceManager, ServiceStatus};
#[cfg(windows)]
use tauri::WindowEvent;
use tauri::{AppHandle, Manager, RunEvent, State};

struct AppState {
    service: Mutex<ServiceManager>,
    maintenance: AtomicBool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Bootstrap {
    app_version: &'static str,
    config: LauncherConfig,
    detected_dsh: Option<String>,
    dsh_version: Option<String>,
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

    Ok(Bootstrap {
        app_version: env!("CARGO_PKG_VERSION"),
        config,
        detected_dsh: detected.map(|path| path.to_string_lossy().into_owned()),
        dsh_version: version,
        profiles,
        status,
    })
}

#[tauri::command]
fn save_config(config: LauncherConfig) -> Result<(), String> {
    config.validate()?;
    config.save().map_err(|e| e.to_string())
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
    app.get_webview_window("dsh-webview")
        .and_then(|window| window.is_visible().ok())
        .unwrap_or(false)
}

#[tauri::command]
fn open_project_page() -> Result<(), String> {
    service::open_default("https://github.com/jockiller/dsh-launcher")
}

#[tauri::command]
fn open_dsh_github_page() -> Result<(), String> {
    service::open_default("https://github.com/deepseek-ai/deepseek-harness")
}

#[tauri::command]
fn open_service_url(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let service = state.service.lock().map_err(|e| e.to_string())?;
    // 优先使用 dsh 启动日志捕获的带 token URL；回退到状态里的固定地址
    let url = service
        .authenticated_url()
        .or_else(|| service.status().url)
        .ok_or_else(|| "DSH 服务尚未运行".to_string())?;
    service::open_configured(&app, &LauncherConfig::load(), &url)
}

/// 启动时的 Launcher 更新检测：每次进程生命周期至多发起一次 GitHub 请求（网络请求
/// 在阻塞线程池执行，不阻塞主线程）。网络失败返回 `None`，由前端静默处理。
#[tauri::command]
async fn check_launcher_update() -> Option<update::ReleaseUpdate> {
    update::release_update().await
}

/// 打开版本按钮对应的 Release 页面：只放行本项目 GitHub Release 相关 URL，
/// 其余一律拒绝，避免把任意 URL 交给系统默认浏览器。
#[tauri::command]
fn open_release_page(url: String) -> Result<(), String> {
    if update::validate_release_url(&url) {
        service::open_default(url.trim())
    } else {
        Err("仅允许打开 DSH Launcher 的 GitHub Release 页面".into())
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
        })
        .setup(|app| {
            app.handle()
                .plugin(tauri_plugin_updater::Builder::new().build())
                .map_err(|error| format!("注册 updater 插件失败：{error}"))?;

            let config = LauncherConfig::load();
            // Dock 图标动态指示（macOS）：纯外挂，失败只影响自身，不影响主流程。
            dock_blink::init(app.handle().clone());
            if config.auto_start {
                let state = app.state::<AppState>();
                if let Ok(mut service) = state.service.lock() {
                    let _ = service.start(app.handle().clone(), config, false);
                }
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
            open_project_page,
            open_dsh_github_page,
            open_service_url,
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
        .expect("failed to build DSH Launcher");

    app.run(|handle, event| {
        #[cfg(windows)]
        if let RunEvent::WindowEvent {
            label,
            event: WindowEvent::CloseRequested { api, .. },
            ..
        } = &event
            && label == "main"
        {
            api.prevent_close();
            shutdown_service(handle);
            handle.exit(0);
            return;
        }

        if matches!(event, RunEvent::ExitRequested { .. } | RunEvent::Exit) {
            shutdown_service(handle);
        }
    });
}
