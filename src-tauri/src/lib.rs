mod config;
mod service;

use std::path::PathBuf;
use std::sync::Mutex;

use config::LauncherConfig;
use serde::Serialize;
use service::{ServiceManager, ServiceStatus};
use tauri::{AppHandle, Manager, RunEvent, State};

struct AppState {
    service: Mutex<ServiceManager>,
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
fn bootstrap(state: State<'_, AppState>) -> Result<Bootstrap, String> {
    let config = LauncherConfig::load();
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
fn restart_service(
    app: AppHandle,
    state: State<'_, AppState>,
    config: LauncherConfig,
) -> Result<ServiceStatus, String> {
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
fn open_service_url(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let status = state.service.lock().map_err(|e| e.to_string())?.status();
    let url = status.url.ok_or_else(|| "DSH 服务尚未运行".to_string())?;
    service::open_configured(&app, &LauncherConfig::load(), &url)
}

pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            service: Mutex::new(ServiceManager::new()),
        })
        .setup(|app| {
            let config = LauncherConfig::load();
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
            restart_service,
            service_status,
            embedded_webview_open,
            open_project_page,
            open_service_url,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build DSH Launcher");

    app.run(|handle, event| {
        if matches!(event, RunEvent::ExitRequested { .. } | RunEvent::Exit)
            && let Some(state) = handle.try_state::<AppState>()
            && let Ok(mut service) = state.service.lock()
        {
            let _ = service.stop(Some(handle));
        }
    });
}
