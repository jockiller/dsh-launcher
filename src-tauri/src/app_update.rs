//! 应用自身热更新：检测、下载、安装与重启。
//!
//! 稳定性约定：
//! - 更新包一律经 minisign 签名校验（公钥内嵌 tauri.conf.json，无法关闭）；
//! - 安装前必须停止 Launcher 托管的 DSH 服务，避免进程占用与误伤外部服务；
//! - 任一环节失败只返回错误文本，不触碰已有安装，由前端回退到"打开 Release 页面"；
//! - 下载进度以 `app-update-progress` 事件发给前端渲染，不写日志区避免刷屏。

use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::UpdaterExt;

use crate::AppState;

/// 静态互斥标记：同时只允许一个应用内更新任务（与 DSH 托管安装互不冲突但也要防重入）。
static APP_UPDATE_ACTIVE: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateProgress {
    /// 下载阶段标记：progress / finished
    pub phase: &'static str,
    pub received: u64,
    pub total: Option<u64>,
}

fn emit_progress(app: &AppHandle, phase: &'static str, received: u64, total: Option<u64>) {
    let _ = app.emit(
        "app-update-progress",
        AppUpdateProgress {
            phase,
            received,
            total,
        },
    );
}

/// 停止托管 DSH 服务：热更新会退出/替换 Launcher 自身，子进程必须先行回收。
/// 外部启动的 DSH 无法由 Launcher 停止，直接拒绝并提示用户自行处理——宁可不动也不误伤。
fn stop_managed_service(app: &AppHandle) -> Result<(), String> {
    app.try_state::<crate::AppState>()
        .ok_or("应用状态尚未初始化")?
        .service
        .lock()
        .map_err(|error| format!("服务管理器不可用：{error}"))
        .and_then(|mut service| {
            if service.status().phase == "external" {
                return Err("检测到 Launcher 无法停止的外部 DSH 服务，请在更新前手动停止".into());
            }
            service.stop(Some(app)).map(|_| ())
        })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateInfo {
    pub version: String,
    pub notes: Option<String>,
}

/// 检查应用更新：复用 updater 插件（endpoint 指向 GitHub Release 的 latest.json），
/// 已经是最新或无法解析版本时返回 `None`。
#[tauri::command]
pub async fn app_update_check(app: AppHandle) -> Result<Option<AppUpdateInfo>, String> {
    let update = app
        .updater()
        .map_err(|error| format!("初始化应用更新失败：{error}"))?
        .check()
        .await
        .map_err(|error| format!("检查应用更新失败：{error}"))?;
    Ok(update.map(|update| AppUpdateInfo {
        version: update.version.clone(),
        notes: update.body.clone(),
    }))
}

/// 下载并安装应用更新：先置维护标记（防止下载期间用户重新拉起 DSH），停托管 DSH，
/// 再走插件下载（minisign 签名校验）与安装。云端没有可安装的更新时返回可读错误；
/// 成功后调用 `app_update_restart` 完成重启，失败则尝试恢复 DSH 并回滚维护标记。
#[tauri::command]
pub async fn app_update_install(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    if APP_UPDATE_ACTIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err("应用更新正在进行中".into());
    }
    // 维护标记必须与 APP_UPDATE_ACTIVE 一样早于一切操作：DSH 停止后到进程退出/重启前，
    // 不得让用户重新启动服务，否则 Windows 热更强退进程时会留下孤儿 DSH。
    if state
        .maintenance
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        APP_UPDATE_ACTIVE.store(false, Ordering::Release);
        return Err("DSH 正在维护升级，暂时不能更新 Launcher".into());
    }
    let (result, _service_started) = app_update_install_inner(&app).await;
    let install_failed = result.is_err();

    // 失败路径里 DSH 处于停止状态；恢复启动，让失败不至于打断用户正在用的服务。
    // 成功路径不恢复：Windows 将由 NSIS 安装器重启应用，macOS/Linux 由用户点"立即重启"。
    if install_failed {
        let start_app = app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            // 先克隆一份再移交所有权：state 的借用与 start 的所有权都指向同一个 AppHandle。
            let start_handle = start_app.clone();
            if let Some(app_state) = start_app.try_state::<AppState>()
                && let Ok(mut service) = app_state.service.lock()
                && service.status().phase == "stopped"
            {
                let config = crate::config::LauncherConfig::load();
                let _ = service.start(start_handle, config, false);
            }
        })
        .await
        .ok();
    }

    // 维护保护结束；无论成败都复位，用户可自由启动/重启服务。
    APP_UPDATE_ACTIVE.store(false, Ordering::Release);
    state
        .maintenance
        .store(false, std::sync::atomic::Ordering::Release);
    result
}

/// 返回 (结果, 是否已把 DSH 重新启动)——安装成功时 DSH 保持停止（等新版拉起后由用户启动）。
async fn app_update_install_inner(app: &AppHandle) -> (Result<(), String>, bool) {
    // 1. 安装前停止托管 DSH：更新会退出并替换 Launcher，不允许把服务拖进不确定状态。
    if let Err(error) = tauri::async_runtime::spawn_blocking({
        let app = app.clone();
        move || stop_managed_service(&app)
    })
    .await
    .map_err(|error| format!("停止 DSH 服务任务异常：{error}"))
    .and_then(|inner| inner.map(|_| ()))
    {
        return (Err(error), false);
    }

    // 2. 检查更新：经 updater_builder 注册 on_before_exit 钩子——Windows install 阶段
    //    插件会 std::process::exit(0) 拉起 NSIS 安装器（/UPDATE 自动重启新版本），
    //    不经过 RunEvent::Exit；该钩子是这条强退路径上回收 DSH 子进程的最后机会。
    let update = match check_update(app).await {
        Ok(update) => update,
        Err(error) => return (Err(error), false),
    };

    // 3. 下载并安装；任何失败都保留 DSH 停止状态，由外层尝试恢复启动。
    download_and_install(app, update)
        .await
        .map(|()| (Ok(()), false))
        .unwrap_or_else(|error| (Err(error), false))
}

async fn check_update(app: &AppHandle) -> Result<tauri_plugin_updater::Update, String> {
    let exit_hook = app.clone();
    app.updater_builder()
        .on_before_exit(move || crate::shutdown_service(&exit_hook))
        .build()
        .map_err(|error| format!("初始化应用更新失败：{error}"))?
        .check()
        .await
        .map_err(|error| format!("检查应用更新失败：{error}"))?
        .ok_or_else(|| "当前已是最新版本，无需应用内更新".to_string())
}

async fn download_and_install(
    app: &AppHandle,
    update: tauri_plugin_updater::Update,
) -> Result<(), String> {
    // 3. 下载并安装：minisign 签名校验在插件内强制执行；进度只走事件通道。
    // 插件回调的 chunk 是本次增量而非累计值，用原子计数器累计后再发给前端。
    // 注意 Windows：install 阶段插件会直接 exit(0) 并由 NSIS 安装器（/UPDATE）重启应用，
    // 该进程内后续代码与 `app_update_restart` 的"立即重启"按钮不会执行。
    let received = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    emit_progress(app, "progress", 0, None);
    update
        .download_and_install(
            {
                let received = std::sync::Arc::clone(&received);
                move |chunk, total| {
                    let received = received
                        .fetch_add(chunk as u64, Ordering::Relaxed)
                        .saturating_add(chunk as u64);
                    emit_progress(app, "progress", received, total);
                }
            },
            || {
                emit_progress(app, "finished", 0, None);
            },
        )
        .await
        .map_err(|error| format!("下载或安装应用更新失败：{error}"))?;

    emit_progress(app, "finished", 0, None);
    Ok(())
}

/// 重启应用完成更新：退出前回收 DSH 子进程，再由系统以原参数拉起新版本。
/// 仅 macOS / Linux 会走到这里；Windows 上 install 阶段已由 NSIS 安装器（/UPDATE）重启应用。
#[tauri::command]
pub fn app_update_restart(app: AppHandle) -> Result<(), String> {
    crate::shutdown_service(&app);
    app.restart();
    // restart 正常情况下不返回（进程退出）；防御式返回，避免编译器对 `!` 类型推断差异报错。
    #[allow(unreachable_code)]
    Ok(())
}
