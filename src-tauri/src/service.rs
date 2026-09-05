use std::ffi::{OsStr, OsString};
use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
#[cfg(unix)]
use std::sync::OnceLock;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread;
#[cfg(unix)]
use std::time::SystemTime;
use std::time::{Duration, Instant};

use chrono::Local;
use serde::Serialize;
use tauri::webview::{NewWindowResponse, PageLoadEvent};
use tauri::{AppHandle, Emitter, Manager, Url, WebviewUrl};
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
use tauri::{PhysicalPosition, PhysicalSize, WebviewWindowBuilder, WindowEvent};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use tauri::{LogicalPosition, LogicalSize, Rect, WebviewBuilder};

use crate::config::{
    LauncherConfig, PlatformTarget, WebviewWindowState, home_dir_for, user_home,
};

const SYSTEM_THEME_SCRIPT: &str = r#"
(() => {
  const KEY = '__DSH_LAUNCHER_THEME__';
  const readStored = () => {
    try {
      const value = localStorage.getItem(KEY);
      return value === 'dark' || value === 'light' ? value : null;
    } catch (error) {
      return null;
    }
  };
  const apply = () => {
    // 启动器接管过主题时以存储值为准，否则跟随系统
    const theme = readStored()
      ?? (window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light');
    window.__DSH_SYSTEM_THEME__ = theme;
    if (document.documentElement) {
      document.documentElement.dataset.systemTheme = theme;
      document.documentElement.style.colorScheme = theme;
      document.documentElement.style.backgroundColor = theme === 'dark' ? '#17181b' : '#f7f7f8';
    }
    if (document.body) {
      if (theme === 'dark') document.body.setAttribute('data-ds-dark-theme', '');
      else document.body.removeAttribute('data-ds-dark-theme');
    }
    window.dispatchEvent(new CustomEvent('dsh-system-theme-change', { detail: { theme } }));
  };
  apply();
  document.addEventListener('DOMContentLoaded', apply, { once: true });
  if (!readStored()) {
    window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', apply);
  }
})();
"#;

/// 生成把指定主题写入 DSH 页面的脚本：持久化到 localStorage（下次加载免闪）、
/// 立即应用并派发主题变更事件（DSH 侧订阅该事件）。
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn theme_apply_script(theme: &str) -> String {
    format!(
        r#"(() => {{
  const theme = '{theme}';
  try {{ localStorage.setItem('__DSH_LAUNCHER_THEME__', theme); }} catch (error) {{}}
  window.__DSH_SYSTEM_THEME__ = theme;
  if (document.documentElement) {{
    document.documentElement.dataset.systemTheme = theme;
    document.documentElement.style.colorScheme = theme;
  }}
  if (document.body) {{
    if (theme === 'dark') document.body.setAttribute('data-ds-dark-theme', '');
    else document.body.removeAttribute('data-ds-dark-theme');
  }}
  window.dispatchEvent(new CustomEvent('dsh-system-theme-change', {{ detail: {{ theme }} }}));
}})();"#
    )
}

/// DSH → 启动器的主题与标题侦测脚本：
/// 1. 监听 DOM 变化并根据 DSH 的 `data-ds-dark-theme` / `color-scheme` 或背景判断深浅色，
///    通过 window.location.hash 桥接向后端汇报主题变更（hash 变化触发 on_navigation，同源放行）。
/// 2. 保持 document.title 为 DSH 真实标题，由 on_document_title_changed 转发给主窗口。
pub(crate) const CONTENT_THEME_WATCHER_SCRIPT: &str = r#"
(() => {
  const THEME_PREFIX = '#__dsh_theme__=';
  let last = null;

  const report = (theme) => {
    if (theme && theme !== last) {
      last = theme;
      try {
        window.location.hash = THEME_PREFIX + theme;
      } catch (e) {}
    }
  };

  // 1. 监听用户在 DSH 设置项中的点击交互（Light / Dark / Follow system）
  document.addEventListener('click', (e) => {
    const btn = e.target.closest('button');
    if (!btn) return;
    const text = (btn.textContent || '').trim();
    if (text.includes('跟随系统') || text.includes('Follow system') || text.includes('System')) {
      report('system');
    } else if (text.includes('深色') || text.includes('Dark')) {
      report('dark');
    } else if (text.includes('浅色') || text.includes('Light')) {
      report('light');
    }
  }, true);

  const parseColor = (color) => {
    const match = /rgba?\(([^)]+)\)/.exec(color);
    if (!match) return null;
    const parts = match[1].split(',').map((part) => parseFloat(part));
    if (parts.length < 3 || parts.some((value) => Number.isNaN(value))) return null;
    if (parts[3] === 0) return null;
    return parts;
  };

  const detect = () => {
    try {
      // 检查当前是否有选中的设置项（设置面板打开时）
      const selected = document.querySelector('button[aria-pressed="true"]');
      if (selected) {
        const text = (selected.textContent || '').trim();
        if (text.includes('跟随系统') || text.includes('Follow system') || text.includes('System')) {
          report('system');
          return;
        }
      }

      let theme = null;
      // 优先读取 DSH 官方属性 data-ds-dark-theme 与 color-scheme
      if (document.body && document.body.hasAttribute('data-ds-dark-theme')) {
        theme = 'dark';
      } else if (document.documentElement && document.documentElement.style.colorScheme) {
        theme = document.documentElement.style.colorScheme === 'dark' ? 'dark' : 'light';
      } else {
        // 回退到背景色亮度判断
        let el = document.body || document.documentElement;
        if (el) {
          let bg = getComputedStyle(el).backgroundColor;
          let rgb = parseColor(bg);
          if (rgb) {
            const luminance = (0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]) / 255;
            theme = luminance < 0.5 ? 'dark' : 'light';
          }
        }
      }
      if (theme) {
        report(theme);
      }
    } catch (error) {}
  };
  const schedule = () => setTimeout(detect, 60);
  const observer = new MutationObserver(schedule);
  const observe = () => {
    try {
      if (document.documentElement) {
        observer.observe(document.documentElement, { attributes: true, attributeFilter: ['style', 'class'] });
      }
      if (document.body) {
        observer.observe(document.body, { attributes: true, attributeFilter: ['data-ds-dark-theme', 'style', 'class'] });
      }
    } catch (error) {}
  };
  observe();
  document.addEventListener('DOMContentLoaded', () => { observe(); detect(); });
  window.addEventListener('load', detect);
  window.addEventListener('dsh-system-theme-change', schedule);
  // 轮询兜底：处理非属性驱动变更的场景
  setInterval(detect, 1000);
})();
"#;

/// macOS/Windows 合并窗口中标题栏（主 WebView）占用的逻辑高度。
/// 取 38px 使 macOS 红绿灯（traffic_light_position y=13，按钮中心 y=19）
/// 与状态胶囊在垂直方向同心对齐；前端 `styles.css` 的 `.titlebar` 高度必须一致。
pub(crate) const TITLEBAR_HEIGHT: f64 = 38.0;

/// 由窗口逻辑尺寸计算内容子 WebView 的可用宽高：固定让出顶部标题栏，
/// 异常小尺寸（最小化等）下保底 80px，避免计算出负值。
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn content_size(window_width: f64, window_height: f64) -> (f64, f64) {
    const MIN_CONTENT_SIZE: f64 = 80.0;
    (
        window_width.max(MIN_CONTENT_SIZE),
        (window_height - TITLEBAR_HEIGHT).max(MIN_CONTENT_SIZE),
    )
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    pub phase: String,
    pub pid: Option<u32>,
    pub url: Option<String>,
    pub message: String,
}

impl Default for ServiceStatus {
    fn default() -> Self {
        Self {
            phase: "stopped".into(),
            pid: None,
            url: None,
            message: "服务未运行".into(),
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LogEvent {
    timestamp: String,
    source: String,
    level: String,
    message: String,
}

struct OwnedChild {
    child: Arc<Mutex<Child>>,
    cancelled: Arc<AtomicBool>,
    #[cfg(unix)]
    process_group: i32,
}

/// 插件 detached 重启后留下的 DSH 进程。它不是 Launcher 的子进程，
/// 因此只能通过端口和命令行身份继续观察，不能使用 waitpid 获取退出状态。
#[derive(Debug, Clone)]
struct AdoptedProcess {
    pid: u32,
    port: u16,
    command_line: String,
}

pub struct ServiceManager {
    owned: Option<OwnedChild>,
    adopted: Arc<Mutex<Option<AdoptedProcess>>>,
    active_cancel: Option<Arc<AtomicBool>>,
    session_cancel: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    session_credentials: Arc<Mutex<Option<crate::session_monitor::SessionCredentials>>>,
    active_config: Option<LauncherConfig>,
    quarantine_cancel: Option<Arc<AtomicBool>>,
    quarantine_active: Arc<AtomicBool>,
    lifecycle: Arc<Mutex<()>>,
    handoff_active: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    status: Arc<Mutex<ServiceStatus>>,
    /// dsh 启动日志打印的带 launch-token 的 URL（`dsh web: http://...?token=...`）。
    /// 每次 start 重置；external 模式下为 None。打开 Web GUI 时优先于固定地址。
    authenticated_url: Arc<Mutex<Option<String>>>,
}

impl ServiceManager {
    pub fn new() -> Self {
        Self {
            owned: None,
            adopted: Arc::new(Mutex::new(None)),
            active_cancel: None,
            session_cancel: Arc::new(Mutex::new(None)),
            session_credentials: Arc::new(Mutex::new(None)),
            active_config: None,
            quarantine_cancel: None,
            quarantine_active: Arc::new(AtomicBool::new(false)),
            lifecycle: Arc::new(Mutex::new(())),
            handoff_active: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
            status: Arc::new(Mutex::new(ServiceStatus::default())),
            authenticated_url: Arc::new(Mutex::new(None)),
        }
    }

    pub fn status(&self) -> ServiceStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn start(
        &mut self,
        app: AppHandle,
        config: LauncherConfig,
        restarting: bool,
    ) -> Result<ServiceStatus, String> {
        let lifecycle = Arc::clone(&self.lifecycle);
        let _lifecycle_guard = lifecycle
            .lock()
            .map_err(|_| "DSH 生命周期锁已损坏".to_string())?;
        self.prune_exited_child();
        if let Some(quarantine_cancel) = self.quarantine_cancel.take() {
            quarantine_cancel.store(true, Ordering::Release);
        }
        self.quarantine_active.store(false, Ordering::Release);
        if self.owned.is_some()
            || self.adopted.lock().map(|slot| slot.is_some()).unwrap_or(true)
            || self.handoff_active.load(Ordering::Acquire)
        {
            return Err("DSH 服务已由启动器运行".into());
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let session_cancel = Arc::new(AtomicBool::new(false));
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.active_cancel = Some(Arc::clone(&cancelled));
        if let Ok(mut slot) = self.session_cancel.lock() {
            if let Some(old) = slot.take() {
                old.store(true, Ordering::Release);
            }
            *slot = Some(Arc::clone(&session_cancel));
        }
        if let Ok(mut slot) = self.session_credentials.lock() {
            *slot = None;
        }
        self.active_config = Some(config.clone());
        self.handoff_active.store(false, Ordering::Release);

        let dsh = match resolve_dsh(&config.dsh_path) {
            Some(path) => path,
            None => {
                self.clear_pending_start(&cancelled);
                return Err("未找到可执行的 dsh，请手动指定路径".into());
            }
        };
        let addresses = match resolve_addresses(&config.host, config.port) {
            Ok(addresses) => addresses,
            Err(error) => {
                self.clear_pending_start(&cancelled);
                return Err(error);
            }
        };
        if connect_any(&addresses, Duration::from_millis(400)) {
            if http_ready(&config.host, config.port) {
                let status = ServiceStatus {
                    phase: "external".into(),
                    pid: None,
                    url: Some(service_url(&config.host, config.port)),
                    message: "检测到端口上已有 Web 服务，启动器不会接管".into(),
                };
                set_status(&self.status, &app, status.clone());
                // 外部 DSH 在运行：托盘回到正常图标（服务健康）；因拿不到 launch token，
                // 无会话监视，保持常态。
                crate::tray::set_running_healthy();
                emit_log(
                    &app,
                    "launcher",
                    "warning",
                    &format!("External web service detected on port {}", config.port),
                );
                self.clear_pending_start(&cancelled);
                return Ok(status);
            }
            self.clear_pending_start(&cancelled);
            return Err(format!("端口 {} 已被其他程序占用", config.port));
        }

        let custom_args = match parse_custom_args(&config.custom_args) {
            Ok(args) => args,
            Err(error) => {
                self.clear_pending_start(&cancelled);
                return Err(error);
            }
        };
        let url = service_url(&config.host, config.port);
        if let Ok(mut slot) = self.authenticated_url.lock() {
            *slot = None;
        }
        set_status(
            &self.status,
            &app,
            ServiceStatus {
                phase: "starting".into(),
                pid: None,
                url: Some(url.clone()),
                message: "正在启动 DSH...".into(),
            },
        );
        emit_log(
            &app,
            "launcher",
            "info",
            &format!("Starting {}", dsh.display()),
        );

        // 托管 DSH 需要把自己的入口与 Node 前置到 PATH，保证服务内部解析到同一运行时；
        // 外部 DSH 只追加其目录，避免改变默认登录 Shell 中 nvm/pnpm 等工具的优先级。
        let managed_dsh = crate::managed::is_managed_dsh(
            &dsh,
            (!config.managed_runtime_dir.trim().is_empty())
                .then(|| Path::new(config.managed_runtime_dir.trim())),
        );
        let path_entries = crate::managed::service_path_entries(&dsh, managed_dsh);
        let (mut envs, _shell_env_loaded) = launcher_environment();
        if managed_dsh {
            prepend_service_path(&mut envs, &path_entries);
        } else {
            append_service_path(&mut envs, &path_entries);
        }

        let mut command = Command::new(&dsh);
        // Windows GUI 宿主下启动控制台程序（node/dsh.cmd→cmd.exe）会闪现控制台窗口。
        // .cmd/.bat 由 std 自动经 cmd.exe 调用并转义各参数（见 dsh_version 注释）；
        // 已知残留面：cmd 解析期会展开 %VAR%（含引号内），参数均来自受控配置而非拼接文本。
        suppress_console_window(&mut command);
        command
            .envs(envs)
            .arg("--profile")
            .arg(config.profile.trim())
            .arg("--host")
            .arg(config.host.trim())
            .arg("--port")
            .arg(config.port.to_string())
            .args(custom_args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                command.pre_exec(|| {
                    if libc::setpgid(0, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.clear_pending_start(&cancelled);
                set_status(
                    &self.status,
                    &app,
                    ServiceStatus {
                        phase: "failed".into(),
                        pid: None,
                        url: None,
                        message: format!("启动失败：{error}"),
                    },
                );
                return Err(format!("启动 dsh 失败：{error}"));
            }
        };
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let child = Arc::new(Mutex::new(child));

        if let Some(stdout) = stdout {
            pipe_logs(
                app.clone(),
                "stdout",
                "info",
                stdout,
                Some(Arc::clone(&self.authenticated_url)),
            );
        }
        if let Some(stderr) = stderr {
            pipe_logs(
                app.clone(),
                "stderr",
                "error",
                stderr,
                Some(Arc::clone(&self.authenticated_url)),
            );
        }

        let adopted_poisoned = self.adopted.lock().is_err();
        if adopted_poisoned {
            if let Ok(mut child) = child.lock() {
                let _ = child.kill();
            }
            self.clear_pending_start(&cancelled);
            return Err("DSH 服务状态锁已损坏".into());
        }
        if let Ok(mut slot) = self.adopted.lock() {
            slot.take();
        }
        self.owned = Some(OwnedChild {
            child: child.clone(),
            cancelled: cancelled.clone(),
            #[cfg(unix)]
            process_group: pid as i32,
        });

        let status = ServiceStatus {
            phase: "starting".into(),
            pid: Some(pid),
            url: Some(url.clone()),
            message: "正在等待健康检查...".into(),
        };
        set_status(&self.status, &app, status.clone());
        monitor_startup(
            child,
            cancelled,
            self.status.clone(),
            app,
            config,
            url,
            pid,
            restarting,
            Arc::clone(&self.authenticated_url),
            session_cancel,
            Arc::clone(&self.session_cancel),
            Arc::clone(&self.session_credentials),
            Arc::clone(&self.adopted),
            Arc::clone(&self.handoff_active),
            Arc::clone(&self.generation),
            generation,
            Arc::clone(&self.lifecycle),
        );
        Ok(status)
    }

    fn clear_pending_start(&mut self, cancelled: &AtomicBool) {
        cancelled.store(true, Ordering::Release);
        self.active_cancel.take();
        if let Ok(mut slot) = self.session_cancel.lock() {
            if let Some(session_cancel) = slot.take() {
                session_cancel.store(true, Ordering::Release);
            }
        }
        if let Ok(mut slot) = self.session_credentials.lock() {
            *slot = None;
        }
        self.active_config.take();
        self.handoff_active.store(false, Ordering::Release);
    }

    fn prune_exited_child(&mut self) {
        let exited = self
            .owned
            .as_ref()
            .and_then(|owned| owned.child.lock().ok())
            .and_then(|mut child| child.try_wait().ok())
            .flatten()
            .is_some();
        if exited {
            self.owned = None;
        }
    }

    pub fn force_stop_external(
        &mut self,
        app: &AppHandle,
        config: &LauncherConfig,
    ) -> Result<ServiceStatus, String> {
        self.prune_exited_child();
        if self.owned.is_some() {
            return Err("当前 DSH 由启动器管理，请使用普通停止功能".into());
        }
        if self.status().phase != "external" {
            return Err("当前未检测到可强制关闭的外部 DSH 服务".into());
        }

        let pid = external_listener_pid(config.port)?;
        let command_line = external_process_command(pid)?;
        if !looks_like_dsh_process(&command_line) {
            return Err(format!(
                "端口 {} 的监听进程未通过 DSH 身份校验，已拒绝终止",
                config.port
            ));
        }
        if external_listener_pid(config.port)? != pid {
            return Err("监听进程在校验期间发生变化，已拒绝终止".into());
        }

        close_embedded_webview(app);
        emit_log(
            app,
            "launcher",
            "warning",
            &format!("Force stopping external DSH process {pid}"),
        );
        terminate_external_process(pid, config.port, &command_line)?;

        let addresses = resolve_addresses(&config.host, config.port)?;
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            if !connect_any(&addresses, Duration::from_millis(150)) {
                let status = ServiceStatus::default();
                set_status(&self.status, app, status.clone());
                return Ok(status);
            }
            thread::sleep(Duration::from_millis(100));
        }
        Err(format!(
            "外部 DSH 进程已终止，但端口 {} 仍在监听",
            config.port
        ))
    }

    pub fn stop(&mut self, app: Option<&AppHandle>) -> Result<ServiceStatus, String> {
        let lifecycle = Arc::clone(&self.lifecycle);
        let _lifecycle_guard = lifecycle
            .lock()
            .map_err(|_| "DSH 生命周期锁已损坏".to_string())?;
        self.generation.fetch_add(1, Ordering::AcqRel);
        if let Some(cancelled) = self.active_cancel.take() {
            cancelled.store(true, Ordering::Release);
        }
        if let Ok(mut slot) = self.session_cancel.lock() {
            if let Some(session_cancel) = slot.take() {
                session_cancel.store(true, Ordering::Release);
            }
        }
        if let Ok(mut slot) = self.session_credentials.lock() {
            *slot = None;
        }
        if let Some(quarantine_cancel) = self.quarantine_cancel.take() {
            quarantine_cancel.store(true, Ordering::Release);
        }
        self.handoff_active.store(false, Ordering::Release);
        self.quarantine_active.store(false, Ordering::Release);
        if let Some(app) = app {
            close_embedded_webview(app);
        }
        self.prune_exited_child();
        let owned = self.owned.take();
        let adopted = self.adopted.lock().ok().and_then(|mut slot| slot.take());
        let had_service = owned.is_some() || adopted.is_some() || self.active_config.is_some();
        if !had_service {
            let status = ServiceStatus::default();
            if let Ok(mut current) = self.status.lock() {
                *current = status.clone();
            }
            return Ok(status);
        }

        if let Some(app) = app {
            set_status(
                &self.status,
                app,
                ServiceStatus {
                    phase: "stopping".into(),
                    pid: None,
                    url: None,
                    message: "正在停止 DSH...".into(),
                },
            );
        } else if let Ok(mut current) = self.status.lock() {
            current.phase = "stopping".into();
            current.message = "正在停止 DSH...".into();
        }

        if let Some(owned) = owned {
            owned.cancelled.store(true, Ordering::Release);
            #[cfg(unix)]
            unsafe {
                libc::kill(-owned.process_group, libc::SIGTERM);
            }
            #[cfg(windows)]
            {
                let pid = owned
                    .child
                    .lock()
                    .map(|child| child.id())
                    .unwrap_or_default();
                let mut taskkill = Command::new("taskkill");
                suppress_console_window(&mut taskkill);
                let _ = taskkill
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .status();
            }
            #[cfg(all(not(unix), not(windows)))]
            if let Ok(mut child) = owned.child.lock() {
                let _ = child.kill();
            }

            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                if let Ok(mut child) = owned.child.lock() {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) if Instant::now() < deadline => {}
                        Ok(None) => {
                            #[cfg(unix)]
                            unsafe {
                                libc::kill(-owned.process_group, libc::SIGKILL);
                            }
                            #[cfg(windows)]
                            {
                                let mut taskkill = Command::new("taskkill");
                                suppress_console_window(&mut taskkill);
                                let _ = taskkill
                                    .args(["/PID", &child.id().to_string(), "/T", "/F"])
                                    .status();
                            }
                            #[cfg(all(not(unix), not(windows)))]
                            let _ = child.kill();
                            let _ = child.wait();
                            break;
                        }
                        Err(error) => return Err(format!("检查 DSH 退出状态失败：{error}")),
                    }
                }
                thread::sleep(Duration::from_millis(80));
            }
        }

        if let Some(adopted) = adopted {
            if external_process_still_matches(adopted.pid, adopted.port, &adopted.command_line) {
                terminate_external_process(adopted.pid, adopted.port, &adopted.command_line)?;
            }
        }

        if let Some(config) = self.active_config.clone() {
            let quarantine_cancel = Arc::new(AtomicBool::new(false));
            self.quarantine_cancel = Some(Arc::clone(&quarantine_cancel));
            self.quarantine_active.store(true, Ordering::Release);
            spawn_stop_quarantine(
                app.cloned(),
                config,
                quarantine_cancel,
                Arc::clone(&self.quarantine_active),
                Arc::clone(&self.lifecycle),
                Arc::clone(&self.generation),
                self.generation.load(Ordering::Acquire),
            );
        }

        let status = ServiceStatus::default();
        if let Ok(mut current) = self.status.lock() {
            *current = status.clone();
        }
        Ok(status)
    }

    /// 返回当前捕获的带 token URL（若有）。external 模式或尚未打印启动行时为 None。
    pub fn authenticated_url(&self) -> Option<String> {
        self.authenticated_url
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
    }
}

const HANDOFF_TIMEOUT: Duration = Duration::from_secs(30);
const HANDOFF_POLL_INTERVAL: Duration = Duration::from_millis(250);
const QUARANTINE_TIMEOUT: Duration = Duration::from_secs(30);
const ADOPTED_MISS_LIMIT: u8 = 4;

fn lifecycle_active(
    cancelled: &AtomicBool,
    generation: &AtomicU64,
    run_generation: u64,
) -> bool {
    !cancelled.load(Ordering::Acquire) && generation.load(Ordering::Acquire) == run_generation
}

#[allow(clippy::too_many_arguments)]
fn monitor_startup(
    child: Arc<Mutex<Child>>,
    cancelled: Arc<AtomicBool>,
    status: Arc<Mutex<ServiceStatus>>,
    app: AppHandle,
    config: LauncherConfig,
    url: String,
    pid: u32,
    // 保留参数位：重启流程与首次启动未来可能有差异化的内容页处理
    _restarting: bool,
    authenticated_url: Arc<Mutex<Option<String>>>,
    session_cancel: Arc<AtomicBool>,
    session_cancel_slot: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    session_credentials: Arc<Mutex<Option<crate::session_monitor::SessionCredentials>>>,
    adopted: Arc<Mutex<Option<AdoptedProcess>>>,
    handoff_active: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    run_generation: u64,
    lifecycle: Arc<Mutex<()>>,
) {
    thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut logged_url = url.clone();
        while Instant::now() < deadline && !cancelled.load(Ordering::Acquire) {
            let exit = child
                .lock()
                .ok()
                .and_then(|mut child| child.try_wait().ok())
                .flatten();
            if let Some(exit) = exit {
                session_cancel.store(true, Ordering::Release);
                if let Ok(mut slot) = session_cancel_slot.lock() {
                    *slot = None;
                }
                if let Ok(mut slot) = session_credentials.lock() {
                    *slot = None;
                }
                if let Ok(_guard) = lifecycle.lock()
                    && lifecycle_active(&cancelled, &generation, run_generation)
                {
                    set_status(
                        &status,
                        &app,
                        ServiceStatus {
                            phase: "failed".into(),
                            pid: None,
                            url: None,
                            message: format!("DSH 在启动期间退出：{exit}"),
                        },
                    );
                }
                return;
            }
            if http_ready(&config.host, config.port) {
                // dsh 打印带 token URL 通常先于端口监听，但日志线程异步解析可能滞后于
                // 健康检查；此处限时等待捕获结果，避免打开旧固定地址导致认证失败。
                let token_deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    let captured = authenticated_url
                        .lock()
                        .ok()
                        .and_then(|slot| slot.clone());
                    if let Some(authenticated) = captured {
                        logged_url = authenticated;
                        break;
                    }
                    if Instant::now() >= token_deadline {
                        emit_log(
                            &app,
                            "launcher",
                            "warning",
                            "Launch-token URL not found in dsh output; falling back to plain URL",
                        );
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                if !lifecycle_active(&cancelled, &generation, run_generation) {
                    return;
                }
                set_status(
                    &status,
                    &app,
                    ServiceStatus {
                        phase: "running".into(),
                        pid: Some(pid),
                        url: Some(logged_url.clone()),
                        message: "DSH 服务运行中".into(),
                    },
                );
                emit_log(&app, "launcher", "info", "Health check passed");
                if let Some(creds) =
                    crate::session_monitor::SessionCredentials::from_authenticated_url(&logged_url)
                {
                    if let Ok(mut slot) = session_credentials.lock() {
                        *slot = Some(creds);
                    }
                }
                crate::session_monitor::spawn(
                    app.clone(),
                    Arc::clone(&session_credentials),
                    Arc::clone(&session_cancel),
                );
                // 启动成功后统一打开内置 WebView（含重启场景：复用并重新导航）
                if let Err(error) = open_content_view(&app, &logged_url, true) {
                    emit_log(
                        &app,
                        "launcher",
                        "error",
                        &english_launch_action_error(&error),
                    );
                }
                monitor_child(
                    child,
                    cancelled,
                    session_cancel,
                    session_cancel_slot,
                    session_credentials,
                    status,
                    app,
                    config,
                    url,
                    pid,
                    adopted,
                    handoff_active,
                    generation,
                    run_generation,
                    Arc::clone(&lifecycle),
                    Arc::clone(&authenticated_url),
                );
                return;
            }
            thread::sleep(Duration::from_millis(250));
        }
        if lifecycle_active(&cancelled, &generation, run_generation)
            && let Ok(_guard) = lifecycle.lock()
            && lifecycle_active(&cancelled, &generation, run_generation)
        {
            set_status(
                &status,
                &app,
                ServiceStatus {
                    phase: "failed".into(),
                    pid: Some(pid),
                    url: Some(url),
                    message: "DSH 启动超时，请停止服务后检查日志".into(),
                },
            );
            emit_log(
                &app,
                "launcher",
                "error",
                "Health check did not pass within 30 seconds",
            );
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn monitor_child(
    child: Arc<Mutex<Child>>,
    cancelled: Arc<AtomicBool>,
    session_cancel: Arc<AtomicBool>,
    session_cancel_slot: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    session_credentials: Arc<Mutex<Option<crate::session_monitor::SessionCredentials>>>,
    status: Arc<Mutex<ServiceStatus>>,
    app: AppHandle,
    config: LauncherConfig,
    url: String,
    pid: u32,
    adopted: Arc<Mutex<Option<AdoptedProcess>>>,
    handoff_active: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    run_generation: u64,
    lifecycle: Arc<Mutex<()>>,
    authenticated_url: Arc<Mutex<Option<String>>>,
) {
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(500));
        let exit = child
            .lock()
            .ok()
            .and_then(|mut child| child.try_wait().ok())
            .flatten();
        let Some(exit) = exit else { continue };
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        if exit.success() {
            session_cancel.store(true, Ordering::Release);
            begin_handoff(
                cancelled,
                status,
                app,
                config,
                url,
                pid,
                adopted,
                handoff_active,
                generation,
                run_generation,
                authenticated_url,
                session_credentials,
                session_cancel_slot,
                lifecycle,
            );
        } else {
            session_cancel.store(true, Ordering::Release);
            if let Ok(mut slot) = session_cancel_slot.lock() {
                *slot = None;
            }
            if let Ok(mut slot) = session_credentials.lock() {
                *slot = None;
            }
            if let Ok(_guard) = lifecycle.lock()
                && lifecycle_active(&cancelled, &generation, run_generation)
            {
                close_embedded_webview(&app);
                set_status(
                    &status,
                    &app,
                    ServiceStatus {
                        phase: "stopped".into(),
                        pid: None,
                        url: None,
                        message: format!("DSH 已退出：{exit}"),
                    },
                );
                emit_log(
                    &app,
                    "launcher",
                    "warning",
                    &format!("DSH process exited: {exit}"),
                );
            }
        }
        return;
    });
}

#[allow(clippy::too_many_arguments)]
fn begin_handoff(
    cancelled: Arc<AtomicBool>,
    status: Arc<Mutex<ServiceStatus>>,
    app: AppHandle,
    config: LauncherConfig,
    url: String,
    old_pid: u32,
    adopted: Arc<Mutex<Option<AdoptedProcess>>>,
    handoff_active: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    run_generation: u64,
    authenticated_url: Arc<Mutex<Option<String>>>,
    session_credentials: Arc<Mutex<Option<crate::session_monitor::SessionCredentials>>>,
    session_cancel_slot: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    lifecycle: Arc<Mutex<()>>,
) {
    if !lifecycle_active(&cancelled, &generation, run_generation)
        || handoff_active.swap(true, Ordering::AcqRel)
    {
        return;
    }
    {
        let Ok(_guard) = lifecycle.lock() else {
            handoff_active.store(false, Ordering::Release);
            return;
        };
        if !lifecycle_active(&cancelled, &generation, run_generation) {
            handoff_active.store(false, Ordering::Release);
            return;
        }
        set_status(
            &status,
            &app,
            ServiceStatus {
                phase: "restarting".into(),
                pid: None,
                url: Some(url.clone()),
                message: "DSH 正在由插件重启，等待新的服务进程...".into(),
            },
        );
    }
    emit_log(
        &app,
        "launcher",
        "info",
        &format!("DSH exited cleanly; waiting for plugin successor after PID {old_pid}"),
    );
    thread::spawn(move || {
        let deadline = Instant::now() + HANDOFF_TIMEOUT;
        while Instant::now() < deadline {
            if !lifecycle_active(&cancelled, &generation, run_generation) {
                handoff_active.store(false, Ordering::Release);
                return;
            }
            if let Some((candidate_pid, command_line)) = find_successor(&config, old_pid)
                && http_ready(&config.host, config.port)
            {
                let Ok(guard) = lifecycle.lock() else {
                    handoff_active.store(false, Ordering::Release);
                    return;
                };
                if !lifecycle_active(&cancelled, &generation, run_generation)
                    || !external_process_still_matches(
                        candidate_pid,
                        config.port,
                        &command_line,
                    )
                    || !http_ready(&config.host, config.port)
                {
                    drop(guard);
                    thread::sleep(HANDOFF_POLL_INTERVAL);
                    continue;
                }
                if let Ok(mut slot) = adopted.lock() {
                    *slot = Some(AdoptedProcess {
                        pid: candidate_pid,
                        port: config.port,
                        command_line: command_line.clone(),
                    });
                }
                handoff_active.store(false, Ordering::Release);
                let has_creds = session_credentials
                    .lock()
                    .ok()
                    .and_then(|slot| slot.clone())
                    .is_some();
                let new_session_cancel = Arc::new(AtomicBool::new(false));
                if let Ok(mut slot) = session_cancel_slot.lock() {
                    *slot = Some(Arc::clone(&new_session_cancel));
                }
                if has_creds {
                    crate::session_monitor::spawn(
                        app.clone(),
                        Arc::clone(&session_credentials),
                        Arc::clone(&new_session_cancel),
                    );
                    emit_log(
                        &app,
                        "launcher",
                        "info",
                        &format!("Reattached session monitor for adopted DSH process {candidate_pid}"),
                    );
                } else {
                    crate::tray::set_running_healthy();
                }
                set_status(
                    &status,
                    &app,
                    ServiceStatus {
                        phase: "running".into(),
                        pid: Some(candidate_pid),
                        url: Some(url.clone()),
                        message: "DSH 服务运行中（插件已完成重启）".into(),
                    },
                );
                drop(guard);
                emit_log(
                    &app,
                    "launcher",
                    "info",
                    &format!("Adopted plugin-restarted DSH process {candidate_pid}"),
                );
                monitor_adopted(
                    candidate_pid,
                    command_line,
                    status,
                    app,
                    config,
                    url,
                    adopted,
                    handoff_active,
                    generation,
                    run_generation,
                    cancelled,
                    authenticated_url,
                    session_credentials,
                    session_cancel_slot,
                    new_session_cancel,
                    lifecycle,
                );
                return;
            }
            thread::sleep(HANDOFF_POLL_INTERVAL);
        }
        handoff_active.store(false, Ordering::Release);
        let Ok(_guard) = lifecycle.lock() else {
            return;
        };
        if !lifecycle_active(&cancelled, &generation, run_generation) {
            return;
        }
        if let Ok(mut slot) = session_cancel_slot.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = session_credentials.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = authenticated_url.lock() {
            *slot = None;
        }
        close_embedded_webview(&app);
        set_status(
            &status,
            &app,
            ServiceStatus {
                phase: "stopped".into(),
                pid: None,
                url: None,
                message: "DSH 已退出，未找到插件重启的服务进程".into(),
            },
        );
        emit_log(
            &app,
            "launcher",
            "warning",
            "Plugin successor was not found within 30 seconds",
        );
    });
}

fn find_successor(config: &LauncherConfig, old_pid: u32) -> Option<(u32, String)> {
    let candidate_pid = external_listener_pid(config.port).ok()?;
    if candidate_pid == old_pid {
        return None;
    }
    let command_line = external_process_command(candidate_pid).ok()?;
    if !looks_like_dsh_process(&command_line)
        || !dsh_command_matches(&command_line, config)
    {
        return None;
    }
    Some((candidate_pid, command_line))
}

#[allow(clippy::too_many_arguments)]
fn monitor_adopted(
    pid: u32,
    command_line: String,
    status: Arc<Mutex<ServiceStatus>>,
    app: AppHandle,
    config: LauncherConfig,
    url: String,
    adopted: Arc<Mutex<Option<AdoptedProcess>>>,
    handoff_active: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    run_generation: u64,
    cancelled: Arc<AtomicBool>,
    authenticated_url: Arc<Mutex<Option<String>>>,
    session_credentials: Arc<Mutex<Option<crate::session_monitor::SessionCredentials>>>,
    session_cancel_slot: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    current_session_cancel: Arc<AtomicBool>,
    lifecycle: Arc<Mutex<()>>,
) {
    thread::spawn(move || {
        let mut misses = 0_u8;
        loop {
            thread::sleep(Duration::from_millis(500));
            if !lifecycle_active(&cancelled, &generation, run_generation) {
                return;
            }
            if external_process_still_matches(pid, config.port, &command_line) {
                misses = 0;
                continue;
            }
            misses = misses.saturating_add(1);
            if process_alive(pid) && misses < ADOPTED_MISS_LIMIT {
                continue;
            }
            if let Ok(mut slot) = adopted.lock() {
                if slot.as_ref().map(|process| process.pid) == Some(pid) {
                    *slot = None;
                }
            }
            current_session_cancel.store(true, Ordering::Release);
            begin_handoff(
                cancelled.clone(),
                status,
                app,
                config,
                url,
                pid,
                adopted,
                handoff_active,
                generation,
                run_generation,
                authenticated_url,
                session_credentials,
                session_cancel_slot,
                lifecycle,
            );
            return;
        }
    });
}

fn spawn_stop_quarantine(
    app: Option<AppHandle>,
    config: LauncherConfig,
    cancelled: Arc<AtomicBool>,
    quarantine_active: Arc<AtomicBool>,
    lifecycle: Arc<Mutex<()>>,
    generation: Arc<AtomicU64>,
    run_generation: u64,
) {
    thread::spawn(move || {
        let deadline = Instant::now() + QUARANTINE_TIMEOUT;
        while Instant::now() < deadline {
            if cancelled.load(Ordering::Acquire)
                || generation.load(Ordering::Acquire) != run_generation
            {
                quarantine_active.store(false, Ordering::Release);
                return;
            }
            if let Some((pid, command_line)) = find_successor(&config, 0)
                && let Ok(_guard) = lifecycle.lock()
                && !cancelled.load(Ordering::Acquire)
                && generation.load(Ordering::Acquire) == run_generation
                && external_process_still_matches(pid, config.port, &command_line)
            {
                let _ = terminate_external_process(pid, config.port, &command_line);
                if let Some(app) = &app {
                    emit_log(
                        app,
                        "launcher",
                        "warning",
                        &format!("Stopped late plugin successor {pid} after user stop"),
                    );
                }
            }
            thread::sleep(HANDOFF_POLL_INTERVAL);
        }
        quarantine_active.store(false, Ordering::Release);
    });
}

fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(native_pid) = i32::try_from(pid) else {
            return false;
        };
        if unsafe { libc::kill(native_pid, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        external_process_command(pid).is_ok()
    }
}

fn dsh_command_matches(command_line: &str, config: &LauncherConfig) -> bool {
    let tokens = command_tokens(command_line);
    option_matches(&tokens, "--profile", config.profile.trim())
        && option_matches(&tokens, "--host", config.host.trim())
        && option_matches(&tokens, "--port", &config.port.to_string())
        && custom_args_match(&tokens, &config.custom_args)
}

fn command_tokens(command_line: &str) -> Vec<String> {
    shell_words::split(command_line).unwrap_or_else(|_| {
        command_line
            .split_whitespace()
            .map(str::to_string)
            .collect()
    })
}

fn custom_args_match(tokens: &[String], custom_args: &str) -> bool {
    let Ok(expected) = parse_custom_args(custom_args) else {
        return false;
    };
    if expected.is_empty() {
        return true;
    }
    tokens
        .windows(expected.len())
        .any(|window| window == expected.as_slice())
}

fn option_matches(tokens: &[String], option: &str, expected: &str) -> bool {
    tokens.iter().enumerate().any(|(index, token)| {
        token == option && tokens.get(index + 1).map(String::as_str) == Some(expected)
            || token.strip_prefix(&format!("{option}=") ) == Some(expected)
    })
}

fn pipe_logs<R>(
    app: AppHandle,
    source: &'static str,
    level: &'static str,
    reader: R,
    authenticated_url: Option<Arc<Mutex<Option<String>>>>,
) where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            if let Some(store) = &authenticated_url
                && let Some(url) = extract_authenticated_url(&line)
                && let Ok(mut slot) = store.lock()
            {
                *slot = Some(url);
            }
            emit_log(&app, source, level, &line);
        }
    });
}

/// 从 dsh 启动日志行提取带 launch-token 的 URL：
/// `dsh web: http://...?token=... (LAN: http://...)`（LAN 部分可能是旧 cookie 的地址，取第一个本地 URL）。
/// 仅接受与 launcher 所配置 host:port 同源的 URL；无 token 时返回 None。
fn extract_authenticated_url(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("dsh web: ")?.trim();
    // 行格式：`http://host:port/?token=... (LAN: http://...?token=...)`；取第一段
    let first = rest.split(" (LAN:").next()?.trim();
    let url = Url::parse(first).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.query_pairs()
        .any(|(key, _)| key == "token")
        .then(|| first.to_string())
}

fn set_status(status: &Arc<Mutex<ServiceStatus>>, app: &AppHandle, next: ServiceStatus) {
    if let Ok(mut current) = status.lock() {
        *current = next.clone();
    }
    // 托盘指示联动：服务未处于 "running" 即回到常态/Idle
    // （已停止/启动中/失败/外部检测皆视为服务未就绪；会话监视随后接手 Healthy/Busy）。
    if next.phase != "running" {
        crate::tray::set_idle();
    }
    let _ = app.emit("service-status", next);
}

pub(crate) fn emit_log(app: &AppHandle, source: &str, level: &str, message: &str) {
    let _ = app.emit(
        "service-log",
        LogEvent {
            timestamp: Local::now().format("%H:%M:%S%.3f").to_string(),
            source: source.into(),
            level: level.into(),
            message: message.into(),
        },
    );
}

fn normalized_host(host: &str) -> &str {
    host.trim()
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or_else(|| host.trim())
}

fn resolve_addresses(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    let addresses = (normalized_host(host), port)
        .to_socket_addrs()
        .map_err(|_| "主机或端口格式无效".to_string())?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err("主机或端口格式无效".into());
    }
    Ok(addresses)
}

fn connect_any(addresses: &[SocketAddr], timeout: Duration) -> bool {
    addresses
        .iter()
        .any(|address| TcpStream::connect_timeout(address, timeout).is_ok())
}

fn host_authority(host: &str, port: u16) -> String {
    let host = normalized_host(host);
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn service_url(host: &str, port: u16) -> String {
    format!("http://{}", host_authority(host, port))
}

fn http_ready(host: &str, port: u16) -> bool {
    let Ok(addresses) = resolve_addresses(host, port) else {
        return false;
    };
    addresses.into_iter().any(|address| {
        let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(300))
        else {
            return false;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
        if write!(
            stream,
            "GET / HTTP/1.0\r\nHost: {}\r\n\r\n",
            host_authority(host, port)
        )
        .is_err()
        {
            return false;
        }
        let mut response = [0_u8; 12];
        stream.read(&mut response).is_ok() && response.starts_with(b"HTTP/")
    })
}

#[cfg(unix)]
fn external_listener_pid(port: u16) -> Result<u32, String> {
    let output = Command::new("lsof")
        .args(["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .output()
        .map_err(|error| format!("无法执行 lsof 定位监听进程：{error}"))?;
    if !output.status.success() {
        return Err(format!("无法定位端口 {port} 的监听进程"));
    }
    parse_single_pid(&String::from_utf8_lossy(&output.stdout), port)
}

#[cfg(windows)]
fn external_listener_pid(port: u16) -> Result<u32, String> {
    let script = format!(
        "(Get-NetTCPConnection -State Listen -LocalPort {port} -ErrorAction Stop | Select-Object -ExpandProperty OwningProcess -Unique) -join \"`n\""
    );
    let mut command = Command::new("powershell.exe");
    suppress_console_window(&mut command);
    let output = command
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .map_err(|error| format!("无法定位监听进程：{error}"))?;
    if !output.status.success() {
        return Err(format!("无法定位端口 {port} 的监听进程"));
    }
    parse_single_pid(&String::from_utf8_lossy(&output.stdout), port)
}

#[cfg(not(any(unix, windows)))]
fn external_listener_pid(_port: u16) -> Result<u32, String> {
    Err("当前平台不支持关闭外部 DSH 服务".into())
}

fn parse_single_pid(output: &str, port: u16) -> Result<u32, String> {
    let mut pids = output
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect::<Vec<_>>();
    pids.sort_unstable();
    pids.dedup();
    match pids.as_slice() {
        [pid] => Ok(*pid),
        [] => Err(format!("无法定位端口 {port} 的监听进程")),
        _ => Err(format!("端口 {port} 对应多个监听进程，已拒绝终止")),
    }
}

#[cfg(unix)]
fn external_process_command(pid: u32) -> Result<String, String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .map_err(|error| format!("无法读取监听进程信息：{error}"))?;
    if !output.status.success() {
        return Err("无法读取监听进程信息，可能没有足够权限".into());
    }
    let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if command.is_empty() {
        Err("监听进程命令行为空，已拒绝终止".into())
    } else {
        Ok(command)
    }
}

#[cfg(windows)]
fn external_process_command(pid: u32) -> Result<String, String> {
    let script = format!(
        "(Get-CimInstance Win32_Process -Filter \"ProcessId = {pid}\" -ErrorAction Stop).CommandLine"
    );
    let mut command = Command::new("powershell.exe");
    suppress_console_window(&mut command);
    let output = command
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .map_err(|error| format!("无法读取监听进程信息：{error}"))?;
    if !output.status.success() {
        return Err("无法读取监听进程信息，可能没有足够权限".into());
    }
    let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if command.is_empty() {
        Err("监听进程命令行为空，已拒绝终止".into())
    } else {
        Ok(command)
    }
}

#[cfg(not(any(unix, windows)))]
fn external_process_command(_pid: u32) -> Result<String, String> {
    Err("当前平台不支持读取外部进程信息".into())
}

fn looks_like_dsh_process(command_line: &str) -> bool {
    let lower = command_line.to_ascii_lowercase().replace('\\', "/");
    lower.split_whitespace().any(|token| {
        let token = token.trim_matches(['\'', '"']);
        token == "dsh"
            || token.ends_with("/dsh")
            || token.ends_with("/dsh.cmd")
            || token.ends_with("/dsh.ps1")
            || token.contains("/@deepseek-ai/dsh/")
            || token.contains("/deepseek-harness/")
    })
}

fn external_process_still_matches(pid: u32, port: u16, expected_command: &str) -> bool {
    external_listener_pid(port).ok() == Some(pid)
        && external_process_command(pid).ok().as_deref() == Some(expected_command)
}

#[cfg(unix)]
fn terminate_external_process(pid: u32, port: u16, expected_command: &str) -> Result<(), String> {
    if !external_process_still_matches(pid, port, expected_command) {
        return Err("监听进程在终止前发生变化，已拒绝操作".into());
    }
    let native_pid = i32::try_from(pid).map_err(|_| "监听进程 PID 无效".to_string())?;
    if unsafe { libc::kill(native_pid, libc::SIGTERM) } == -1 {
        return Err(format!(
            "终止外部 DSH 失败：{}",
            std::io::Error::last_os_error()
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        // 仅依据信号探测结果判断进程是否仍在，避免每次迭代都 spawn lsof
        if unsafe { libc::kill(native_pid, 0) } == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            // EPERM 等其他错误继续轮询，交由外层端口复查给出结论
            thread::sleep(Duration::from_millis(150));
            continue;
        }
        thread::sleep(Duration::from_millis(150));
    }
    // 升级 SIGKILL 前重新校验身份；进程若已退出或释放端口则无需强制终止
    if external_listener_pid(port).ok() != Some(pid) {
        return Ok(());
    }
    if external_process_command(pid)?.as_str() != expected_command {
        return Err("监听进程在强制终止前发生变化，已拒绝操作".into());
    }
    if unsafe { libc::kill(native_pid, libc::SIGKILL) } == -1 {
        return Err(format!(
            "强制终止外部 DSH 失败：{}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn terminate_external_process(pid: u32, port: u16, expected_command: &str) -> Result<(), String> {
    if !external_process_still_matches(pid, port, expected_command) {
        return Err("监听进程在终止前发生变化，已拒绝操作".into());
    }
    let mut command = Command::new("taskkill");
    suppress_console_window(&mut command);
    let status = command
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status()
        .map_err(|error| format!("终止外部 DSH 失败：{error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("终止外部 DSH 失败，taskkill 退出状态：{status}"))
    }
}

#[cfg(not(any(unix, windows)))]
fn terminate_external_process(
    _pid: u32,
    _port: u16,
    _expected_command: &str,
) -> Result<(), String> {
    Err("当前平台不支持关闭外部 DSH 服务".into())
}

pub fn resolve_dsh(manual: &str) -> Option<PathBuf> {
    if !manual.trim().is_empty() {
        return resolve_manual(manual);
    }
    if let Some(path) = find_on_path() {
        return Some(path);
    }
    if let Some(path) = find_via_login_shell() {
        return Some(path);
    }
    find_common_install()
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Windows 下以 GUI 子系统（windows_subsystem=windows）运行时，直接 spawn 控制台程序
/// （node、cmd.exe、taskkill 等）会闪现控制台窗口；CREATE_NO_WINDOW 只隐藏窗口，不影响管道日志。
#[cfg(windows)]
fn suppress_console_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn suppress_console_window(_command: &mut Command) {}

/// 手动指定路径优先按原样接收；Windows 下兼容省略扩展名的写法，
/// 自动按 PATHEXT 候选（.exe/.cmd/.bat）补全后再验证存在性。
fn resolve_manual(manual: &str) -> Option<PathBuf> {
    let path = PathBuf::from(manual.trim());
    if is_executable_dsh(&path) {
        return Some(path);
    }
    #[cfg(windows)]
    if path.extension().is_none() {
        let pathext = std::env::var("PATHEXT").ok();
        for ext in windows_extensions(pathext.as_deref()) {
            let candidate = path.with_extension(ext.trim_start_matches('.'));
            if is_executable_dsh(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// 纯逻辑：汇总 PATHEXT 与默认扩展，生成 dsh 的候选扩展名。
/// 原生 exe 最优先（批处理需要额外经 cmd.exe 执行）；其余遵循 PATHEXT 顺序，
/// 仅接受 .cmd/.bat（std 原生支持经 cmd.exe 安全调用的批处理扩展），缺省补齐。
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_extensions(pathext: Option<&str>) -> Vec<String> {
    const KNOWN_SCRIPT_EXTS: [&str; 2] = [".cmd", ".bat"];
    let mut extensions = vec![".exe".to_string()];
    if let Some(value) = pathext {
        for token in value.split(';') {
            let token = token.trim().to_ascii_lowercase();
            if token.is_empty() {
                continue;
            }
            let token = if token.starts_with('.') {
                token
            } else {
                format!(".{token}")
            };
            if KNOWN_SCRIPT_EXTS.contains(&token.as_str()) && !extensions.contains(&token) {
                extensions.push(token);
            }
        }
    }
    for default_ext in KNOWN_SCRIPT_EXTS {
        if !extensions.iter().any(|ext| ext == default_ext) {
            extensions.push(default_ext.to_string());
        }
    }
    extensions
}

/// 纯逻辑：在目录内枚举 dsh 的候选完整路径；Windows 按 PATHEXT 展开，其余平台仅 `dsh` 本名。
fn dsh_candidates(dir: &Path, windows: bool, pathext: Option<&str>) -> Vec<PathBuf> {
    if windows {
        windows_extensions(pathext)
            .into_iter()
            .map(|ext| dir.join(format!("dsh{ext}")))
            .collect()
    } else {
        vec![dir.join("dsh")]
    }
}

/// 纯逻辑：枚举 Windows 上 dsh 的常见安装位置候选（按优先级）：
/// npm 全局目录（%APPDATA%\npm）、nvm-windows 符号链接/主目录、Node.js 安装目录、
/// Scoop shims、用户自定义 npm 全局前缀（~\.npm-global）。
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_common_candidates(
    lookup: &dyn Fn(&str) -> Option<String>,
    pathext: Option<&str>,
) -> Vec<PathBuf> {
    let home = home_dir_for(PlatformTarget::Windows, lookup);
    let appdata = lookup("APPDATA").map(PathBuf::from).or_else(|| {
        home.as_ref()
            .map(|home| home.join("AppData").join("Roaming"))
    });
    let mut candidates = Vec::new();
    if let Some(appdata) = &appdata {
        candidates.extend(dsh_candidates(&appdata.join("npm"), true, pathext));
    }
    // nvm-windows：NVM_SYMLINK 指向当前 Node 符号链接目录，NVM_HOME 为版本库根目录
    for env_name in ["NVM_SYMLINK", "NVM_HOME"] {
        if let Some(dir) = lookup(env_name) {
            candidates.extend(dsh_candidates(Path::new(&dir), true, pathext));
        }
    }
    if let Some(program_files) = lookup("ProgramFiles") {
        candidates.extend(dsh_candidates(
            &PathBuf::from(program_files).join("nodejs"),
            true,
            pathext,
        ));
    }
    if let Some(home) = &home {
        candidates.extend(dsh_candidates(
            &home.join("scoop").join("shims"),
            true,
            pathext,
        ));
        candidates.push(home.join(".npm-global").join("dsh.cmd"));
    }
    candidates
}

/// nvm-windows 把各 Node 版本放在 %APPDATA%\nvm\vX.Y.Z（全局 npm 包同目录）；
/// 目录名是 `v` 前缀的版本号，按名称降序返回保证新版本优先。
#[cfg_attr(not(windows), allow(dead_code))]
fn nvm_windows_version_dirs(nvm_root: &Path) -> Vec<PathBuf> {
    let mut entries = fs::read_dir(nvm_root)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with('v'))
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    entries.reverse();
    entries.into_iter().map(|entry| entry.path()).collect()
}

/// 组装启动 DSH 子进程的环境：先继承 Launcher 环境，再用默认登录 Shell 中的工具链
/// 变量覆盖 PATH、PNPM_HOME、nvm/volta 等。GUI 启动时通常没有读取用户 shell rc
/// 文件；采集失败时仍保留当前环境，避免丢失 DSH_HOME、凭据等运行时变量。
fn launcher_environment() -> (Vec<(OsString, OsString)>, bool) {
    let base = std::env::vars_os().collect::<Vec<_>>();

    #[cfg(unix)]
    {
        static SHELL_ENVIRONMENT: OnceLock<Option<Vec<(OsString, OsString)>>> = OnceLock::new();
        let shell_env = SHELL_ENVIRONMENT.get_or_init(|| {
            run_login_shell_capture(
                LOGIN_SHELL_ENV_SCRIPT,
                LOGIN_SHELL_FISH_ENV_SCRIPT,
                LOGIN_SHELL_TIMEOUT,
            )
            .map(|payload| {
                parse_env_output(&payload)
                    .into_iter()
                    .filter(|(key, _)| should_overlay_shell_environment_key(key))
                    .collect()
            })
        });
        let mut envs = base;
        if let Some(shell_env) = shell_env {
            merge_environment(&mut envs, shell_env.iter().cloned());
            return (envs, true);
        }
        (envs, false)
    }

    #[cfg(not(unix))]
    {
        (base, false)
    }
}

#[cfg(unix)]
fn should_overlay_shell_environment_key(key: &OsStr) -> bool {
    matches!(
        key.to_string_lossy().as_ref(),
        "PATH"
            | "MANPATH"
            | "PNPM_HOME"
            | "NVM_DIR"
            | "NVM_BIN"
            | "NVM_PATH"
            | "VOLTA_HOME"
            | "VOLTA_BIN"
            | "ASDF_DIR"
            | "ASDF_DATA_DIR"
            | "MISE_DATA_DIR"
            | "FNM_DIR"
            | "FNM_MULTISHELL_PATH"
    )
}

fn environment_key_eq(left: &OsStr, right: &OsStr) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

#[cfg(unix)]
fn merge_environment(
    base: &mut Vec<(OsString, OsString)>,
    overlay: impl IntoIterator<Item = (OsString, OsString)>,
) {
    for (key, value) in overlay {
        if let Some(existing) = base
            .iter_mut()
            .find(|(existing, _)| environment_key_eq(existing, &key))
        {
            existing.1 = value;
        } else {
            base.push((key, value));
        }
    }
}

/// 纯逻辑：把待前置的目录列表拼到 envs 中已有的 PATH 值前面（原值来自登录 Shell，
/// launcher_environment 已保证失败时回退当前进程 PATH）。环境键比较遵循各平台语义，
/// Windows 下兼容 PATH 的大小写变体。
fn prepend_service_path(envs: &mut Vec<(OsString, OsString)>, entries: &[PathBuf]) {
    if entries.is_empty() {
        return;
    }
    let existing = envs
        .iter()
        .find(|(key, _)| environment_key_eq(key, OsStr::new("PATH")))
        .map(|(_, value)| value.clone())
        .or_else(|| std::env::var_os("PATH"));
    let mut parts: Vec<OsString> = entries
        .iter()
        .map(|entry| entry.as_os_str().to_os_string())
        .collect();
    if let Some(existing) = existing {
        // 原始 PATH 是平台分隔符拼接的整串，必须先 split 再合入；
        // 直接把整串当条目交给 join_paths 会因含分隔符而失败，注入就永远不会生效。
        parts.extend(
            std::env::split_paths(&existing)
                .filter(|part| !part.as_os_str().is_empty())
                .map(|part| part.into_os_string()),
        );
    }
    let Ok(joined) = std::env::join_paths(&parts) else {
        // join_paths 仅在条目本身含路径分隔符时失败（托管安装已拒绝此类目录）；
        // 此时放弃注入，保持旧行为，绝不写坏 PATH。
        return;
    };
    match envs
        .iter_mut()
        .find(|(key, _)| environment_key_eq(key, OsStr::new("PATH")))
    {
        Some(slot) => slot.1 = joined,
        None => envs.push((OsString::from("PATH"), joined)),
    }
}

fn append_service_path(envs: &mut Vec<(OsString, OsString)>, entries: &[PathBuf]) {
    if entries.is_empty() {
        return;
    }
    let existing = envs
        .iter()
        .find(|(key, _)| environment_key_eq(key, OsStr::new("PATH")))
        .map(|(_, value)| value.clone())
        .or_else(|| std::env::var_os("PATH"));
    let mut parts = existing
        .as_deref()
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .filter(|part| !part.as_os_str().is_empty())
        .map(|part| part.into_os_string())
        .collect::<Vec<_>>();
    parts.extend(entries.iter().map(|entry| entry.as_os_str().to_os_string()));
    let Ok(joined) = std::env::join_paths(&parts) else {
        return;
    };
    match envs
        .iter_mut()
        .find(|(key, _)| environment_key_eq(key, OsStr::new("PATH")))
    {
        Some(slot) => slot.1 = joined,
        None => envs.push((OsString::from("PATH"), joined)),
    }
}

/// 登录 Shell 采集总耗时预算（含尝试多个 Shell 与参数组合），防止用户 Shell 初始化卡死时无限等待。
#[cfg(unix)]
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(8);

/// 登录 Shell 输出分界标记：脚本先打印它再输出有效内容，启动文件的 stdout 噪音（提示、装饰等）在标记之前，可被安全剔除。
/// 下方两个脚本常量内联了同一字面量，修改时务必同步。
#[cfg(unix)]
const LOGIN_SHELL_MARKER: &str = "__DSH_LAUNCHER_ENV__";

/// 通过登录 Shell 采集完整环境变量：`env -0` 保留值中的换行；若系统 `env` 不支持 `-0`，
/// 回退到普通换行分隔输出。POSIX 与 fish 使用各自的短脚本，避免把一种 Shell 的控制运算符
/// 误传给另一种 Shell。
#[cfg(unix)]
const LOGIN_SHELL_ENV_SCRIPT: &str = concat!(
    "umask 077; ",
    "echo ",
    "__DSH_LAUNCHER_ENV__",
    r#" > "$DSH_LAUNCHER_ENV_FILE"; "#,
    r#"env -0 >> "$DSH_LAUNCHER_ENV_FILE" 2>/dev/null || env >> "$DSH_LAUNCHER_ENV_FILE" 2>/dev/null"#
);

#[cfg(unix)]
const LOGIN_SHELL_FISH_ENV_SCRIPT: &str = concat!(
    "umask 077; ",
    "echo ",
    "__DSH_LAUNCHER_ENV__",
    r#" > "$DSH_LAUNCHER_ENV_FILE"; "#,
    r#"env -0 >> "$DSH_LAUNCHER_ENV_FILE" 2>/dev/null; or env >> "$DSH_LAUNCHER_ENV_FILE" 2>/dev/null"#
);

/// 通过登录 Shell 定位 dsh：`command -v` 是内建命令，POSIX Shell 与 fish 均支持。
#[cfg(unix)]
const LOGIN_SHELL_DSH_SCRIPT: &str = concat!(
    "umask 077; ",
    r#"echo "__DSH_LAUNCHER_ENV__" > "$DSH_LAUNCHER_ENV_FILE"; "#,
    r#"command -v dsh >> "$DSH_LAUNCHER_ENV_FILE" 2>/dev/null"#
);

/// 登录 Shell 启动参数：优先 `-li -c`（与原实现一致的登录+交互环境，zsh 需 -i 才能读到 .zshrc 中的 PATH 设置）；
/// 再回退裸 `-c`，覆盖不支持 `-li` 的 Shell。每组的最后一项必须是 `-c`，保证脚本被执行。
#[cfg(unix)]
const LOGIN_SHELL_ARGSETS: &[&[&str]] = &[&["-li", "-c"], &["-c"]];

/// SHELL 缺失或不可用时依次尝试的登录 Shell 路径，全部覆盖常见发行版：bash（多数 Linux 默认）、zsh（macOS 默认）、sh（POSIX 兜底）。
#[cfg(unix)]
const FALLBACK_LOGIN_SHELLS: &[&str] = &[
    "/bin/bash",
    "/usr/bin/bash",
    "/bin/zsh",
    "/usr/bin/zsh",
    "/usr/local/bin/zsh",
    "/opt/homebrew/bin/zsh",
    "/usr/local/bin/bash",
    "/opt/homebrew/bin/bash",
    "/bin/fish",
    "/usr/bin/fish",
    "/usr/local/bin/fish",
    "/opt/homebrew/bin/fish",
    "/bin/sh",
    "/usr/bin/sh",
];

/// 纯逻辑：把 $SHELL 的值与默认候选归并成去重后的存在候选列表，$SHELL 优先、空白值跳过、不存在者过滤。
#[cfg(unix)]
fn login_shell_candidates(env_shell: Option<&str>, exists: &dyn Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for shell in env_shell
        .into_iter()
        .chain(FALLBACK_LOGIN_SHELLS.iter().copied())
    {
        let trimmed = shell.trim();
        if trimmed.is_empty() {
            continue;
        }
        let path = PathBuf::from(trimmed);
        if !candidates.contains(&path) && exists(&path) {
            candidates.push(path);
        }
    }
    candidates
}

#[cfg(unix)]
fn available_login_shells() -> Vec<PathBuf> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(default_login_shell);
    login_shell_candidates(shell.as_deref(), &|path| path.is_file())
}

#[cfg(unix)]
fn default_login_shell() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let user = std::env::var("USER")
            .ok()
            .filter(|value| !value.trim().is_empty())?;
        let record = Command::new("dscl")
            .args([".", "-read", &format!("/Users/{user}"), "UserShell"])
            .output()
            .ok()?;
        if !record.status.success() {
            return None;
        }
        return String::from_utf8_lossy(&record.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("UserShell:").map(str::trim))
            .filter(|shell| !shell.is_empty())
            .map(str::to_string);
    }

    #[cfg(not(target_os = "macos"))]
    {
        let user = std::env::var("USER")
            .ok()
            .filter(|value| !value.trim().is_empty())?;
        let record = Command::new("getent")
            .args(["passwd", &user])
            .output()
            .ok()?;
        if !record.status.success() {
            return None;
        }
        return String::from_utf8_lossy(&record.stdout)
            .lines()
            .next()
            .and_then(|line| line.split(':').nth(6))
            .map(str::trim)
            .filter(|shell| !shell.is_empty())
            .map(str::to_string);
    }
}

/// 纯逻辑：按「Shell 优先、参数组其次」展开全部尝试组合。
#[cfg(unix)]
fn login_shell_attempt_plan(shells: &[PathBuf]) -> Vec<(PathBuf, &'static [&'static str])> {
    shells
        .iter()
        .flat_map(|shell| {
            LOGIN_SHELL_ARGSETS
                .iter()
                .map(|argset| (shell.clone(), *argset))
        })
        .collect()
}

/// 纯逻辑：解析 `env` 输出。NUL 分隔（`env -0`）与换行分隔（回退）自动识别；
/// 丢弃没有 `=` 或键为空的片段；值中的 `=` 与（NUL 模式下的）换行原样保留。
#[cfg(unix)]
fn parse_env_output(raw: &[u8]) -> Vec<(OsString, OsString)> {
    let null_separated = raw.contains(&0);
    let separator = if null_separated { 0 } else { b'\n' };
    raw.split(|byte| *byte == separator)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| {
            let mut entry = entry.to_vec();
            if !null_separated {
                // 换行模式兼容 CRLF：仅去掉行尾的 `\\r`。
                while entry.last() == Some(&b'\r') {
                    entry.pop();
                }
            }
            let position = entry.iter().position(|byte| *byte == b'=')?;
            if position == 0 {
                return None;
            }
            let key = OsString::from_vec(entry[..position].to_vec());
            let value = OsString::from_vec(entry[position + 1..].to_vec());
            Some((key, value))
        })
        .collect()
}

/// 纯逻辑：取标记之后的有效负载，并跳过紧跟标记的一个换行（含 `\r\n`）。标记不存在时返回 None。
#[cfg(unix)]
fn extract_after_marker<'a>(raw: &'a [u8], marker: &str) -> Option<&'a [u8]> {
    let marker = marker.as_bytes();
    let position = raw
        .windows(marker.len())
        .position(|window| window == marker)?;
    let rest = &raw[position + marker.len()..];
    Some(
        rest.strip_prefix(b"\r\n")
            .or_else(|| rest.strip_prefix(b"\n"))
            .unwrap_or(rest),
    )
}

/// 纯逻辑：从命令输出中取第一条非空行（`command -v` 只输出一行，这里再防御一次）。
#[cfg(unix)]
fn first_non_empty_line(payload: &[u8]) -> Option<String> {
    String::from_utf8_lossy(payload)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

#[cfg(unix)]
enum SpawnOutcome {
    /// 子进程正常退出，携带退出状态与（可能的）文件内容。
    Completed { success: bool },
    /// 程序无法启动（不存在、无权限等）。
    NotRunnable,
    /// 超时，子进程已被强杀并回收。
    TimedOut,
}

/// 标准库实现的限时执行：轮询 `try_wait`，超时先 `kill` 再 `wait` 回收。
/// stdin/stdout/stderr 全部置空，杜绝管道写满导致的隐藏死锁；需要输出时由脚本写入临时文件。
#[cfg(unix)]
fn spawn_with_timeout(
    program: &Path,
    args: &[&OsStr],
    timeout: Duration,
    environment: Option<&[(OsString, OsString)]>,
) -> SpawnOutcome {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(environment) = environment {
        command.env_clear().envs(environment.iter().cloned());
    }
    let Ok(mut child) = command.spawn() else {
        return SpawnOutcome::NotRunnable;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return SpawnOutcome::Completed {
                    success: status.success(),
                };
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return SpawnOutcome::TimedOut;
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.kill();
                return SpawnOutcome::NotRunnable;
            }
        }
    }
}

#[cfg(unix)]
fn login_shell_base_environment(shell: &Path, output_file: &Path) -> Vec<(OsString, OsString)> {
    let mut environment = std::env::vars_os()
        .filter(|(key, _)| !environment_key_eq(key, OsStr::new("PATH")))
        .collect::<Vec<_>>();
    set_environment_value(&mut environment, OsStr::new("SHELL"), shell.as_os_str());
    set_environment_value(
        &mut environment,
        OsStr::new("DSH_LAUNCHER_ENV_FILE"),
        output_file.as_os_str(),
    );
    set_environment_value(
        &mut environment,
        OsStr::new("PATH"),
        default_login_shell_path().as_os_str(),
    );
    environment
}

#[cfg(unix)]
fn default_login_shell_path() -> OsString {
    let mut entries = vec![
        PathBuf::from("/usr/local/sbin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/sbin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/sbin"),
        PathBuf::from("/bin"),
    ];
    #[cfg(target_os = "macos")]
    {
        entries.insert(0, PathBuf::from("/opt/homebrew/bin"));
    }
    std::env::join_paths(entries)
        .unwrap_or_else(|_| OsString::from("/usr/bin:/bin:/usr/sbin:/sbin"))
}

#[cfg(test)]
fn environment_value<'a>(envs: &'a [(OsString, OsString)], key: &str) -> Option<&'a OsString> {
    let key = OsStr::new(key);
    envs.iter()
        .find(|(name, _)| environment_key_eq(name, key))
        .map(|(_, value)| value)
}

#[cfg(unix)]
fn set_environment_value(environment: &mut Vec<(OsString, OsString)>, key: &OsStr, value: &OsStr) {
    if let Some(existing) = environment.iter_mut().find(|(name, _)| name == key) {
        existing.1 = value.to_os_string();
    } else {
        environment.push((key.to_os_string(), value.to_os_string()));
    }
}

/// 依次用候选登录 Shell 执行脚本，把结果读回内存。使用临时文件承接输出（脚本内 `umask 077` 收紧权限），
/// 读取后立即删除；所有尝试共享时间预算，超时就放弃采集（调用方回退到继承当前环境）。
/// Shell 使用平台最小系统 PATH 启动，避免 GUI 继承的旧 PATH 污染 `.zprofile/.zshrc`，
/// 同时保证 rc 文件在设置用户 PATH 前仍能调用 `uname`、`dirname` 等基础命令。
#[cfg(unix)]
fn create_login_shell_capture_file() -> Option<(PathBuf, PathBuf)> {
    let stamp = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let temp_root = std::env::temp_dir();
    for attempt in 0..16 {
        let directory = temp_root.join(format!(
            "dsh-launcher-{}-{stamp}-{attempt}",
            std::process::id()
        ));
        if fs::create_dir(&directory).is_err() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&directory, fs::Permissions::from_mode(0o700));
        }
        let file = directory.join("environment");
        match OpenOptions::new().write(true).create_new(true).open(&file) {
            Ok(_) => return Some((directory, file)),
            Err(_) => {
                let _ = fs::remove_dir_all(&directory);
            }
        }
    }
    None
}

#[cfg(unix)]
fn run_login_shell_capture(
    posix_script: &str,
    fish_script: &str,
    timeout: Duration,
) -> Option<Vec<u8>> {
    let (temp_directory, temp_file) = create_login_shell_capture_file()?;
    let deadline = Instant::now() + timeout;
    let mut payload: Option<Vec<u8>> = None;
    for (shell, argset) in login_shell_attempt_plan(&available_login_shells()) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let shell_environment = login_shell_base_environment(&shell, &temp_file);
        let script = if shell.file_name().is_some_and(|name| name == "fish") {
            fish_script
        } else {
            posix_script
        };
        let mut argv: Vec<&OsStr> = argset.iter().map(OsStr::new).collect();
        argv.push(OsStr::new(script));
        argv.push(OsStr::new("dsh-launcher"));
        if let SpawnOutcome::Completed { success: true } =
            spawn_with_timeout(&shell, &argv, remaining, Some(&shell_environment))
            && let Ok(content) = fs::read(&temp_file)
            && let Some(after_marker) = extract_after_marker(&content, LOGIN_SHELL_MARKER)
        {
            // 标记存在才说明脚本真正执行成功；否则（只有 Shell 自身输出）继续尝试下一组合。
            payload = Some(after_marker.to_vec());
            break;
        }
    }
    let _ = fs::remove_dir_all(&temp_directory);
    payload
}

fn find_via_login_shell() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        let payload = run_login_shell_capture(
            LOGIN_SHELL_DSH_SCRIPT,
            LOGIN_SHELL_DSH_SCRIPT,
            LOGIN_SHELL_TIMEOUT,
        )?;
        let line = first_non_empty_line(&payload)?;
        let path = PathBuf::from(line);
        is_executable_dsh(&path).then_some(path)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn find_on_path() -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    let pathext = std::env::var("PATHEXT").ok();
    std::env::split_paths(&paths)
        .filter(|dir| !dir.as_os_str().is_empty())
        .find_map(|dir| {
            dsh_candidates(&dir, cfg!(windows), pathext.as_deref())
                .into_iter()
                .find(|path| is_executable_dsh(path))
        })
}

#[cfg(windows)]
fn find_common_install() -> Option<PathBuf> {
    let lookup = |key: &str| {
        std::env::var(key)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    let pathext = lookup("PATHEXT");
    let mut candidates = windows_common_candidates(&lookup, pathext.as_deref());
    // nvm-windows 的各版本目录（%APPDATA%\nvm\vX.Y.Z，新版优先）
    if let Some(appdata) = lookup("APPDATA") {
        let nvm_root = PathBuf::from(appdata).join("nvm");
        for version_dir in nvm_windows_version_dirs(&nvm_root) {
            candidates.extend(dsh_candidates(&version_dir, true, pathext.as_deref()));
        }
    }
    candidates.into_iter().find(|path| is_executable_dsh(path))
}

#[cfg(not(windows))]
fn find_common_install() -> Option<PathBuf> {
    for candidate in ["/opt/homebrew/bin/dsh", "/usr/local/bin/dsh"] {
        let path = PathBuf::from(candidate);
        if is_executable_dsh(&path) {
            return Some(path);
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let versions = home.join(".nvm/versions/node");
    let mut entries = fs::read_dir(versions).ok()?.flatten().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    entries.reverse();
    entries
        .into_iter()
        .map(|entry| entry.path().join("bin/dsh"))
        .find(|path| is_executable_dsh(path))
}

fn is_executable_dsh(path: &Path) -> bool {
    path.is_file()
}

/// 读取 dsh 版本。Windows 的 .cmd/.bat 批处理由标准库自动经由 cmd.exe 调用：程序路径与
/// 各参数被批处理感知地转义（BatBadBut，Rust ≥ 1.77.2 修复，含 `&`、`"` 等元字符），无法
/// 安全转义的参数直接报 InvalidInput 而非注入；因此这里保持逐参数传递、不拼接命令行即可
/// 避免 cmd 注入。CREATE_NO_WINDOW 防止 GUI 宿主下闪现控制台窗口。
///
/// 版本探测与正式启动共用登录 Shell 工具链环境，保证 nvm 等以 `#!/usr/bin/env node`
/// 安装的 dsh 能解析到 node。Shell 环境只采集一次并缓存，后续 detect/validate
/// 不会重复执行 rc 文件。成功时同时从 stdout 与 stderr 提取语义化版本；提取不到时
/// 给出明确错误（附带输出片段便于排查）。
pub fn dsh_version(path: &Path) -> Result<String, String> {
    let mut command = Command::new(path);
    let (envs, _) = launcher_environment();
    command.envs(envs);
    suppress_console_window(&mut command);
    let mut child = command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法执行 dsh：{error}"))?;
    let stdout = child.stdout.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut output = String::new();
            let _ = pipe.read_to_string(&mut output);
            output
        })
    });
    let stderr = child.stderr.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut output = String::new();
            let _ = pipe.read_to_string(&mut output);
            output
        })
    });
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(40)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout.map(|reader| reader.join());
                let _ = stderr.map(|reader| reader.join());
                return Err("执行 dsh --version 超时".into());
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("检查 dsh --version 状态失败：{error}"));
            }
        }
    };
    let stdout = stdout
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    let stderr = stderr
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    if !status.success() {
        let detail = stderr.trim();
        let detail = if detail.is_empty() {
            stdout.trim()
        } else {
            detail
        };
        return Err(if detail.is_empty() {
            format!("dsh --version 以退出码 {status} 失败")
        } else {
            detail.to_string()
        });
    }
    parse_dsh_version(&stdout, &stderr).ok_or_else(|| version_parse_error(&stdout, &stderr))
}

/// 纯逻辑：从 dsh `--version` 的 stdout/stderr 中提取语义化版本，先 stdout 后 stderr。
fn parse_dsh_version(stdout: &str, stderr: &str) -> Option<String> {
    extract_semver_token(stdout).or_else(|| extract_semver_token(stderr))
}

/// 纯逻辑：成功输出中找不到版本时，给出附带输出片段的明确错误。
fn version_parse_error(stdout: &str, stderr: &str) -> String {
    let mut seen = stdout.trim().to_string();
    let stderr = stderr.trim();
    if stderr.is_empty() {
        if seen.is_empty() {
            return "dsh --version 未返回任何输出，无法识别版本".into();
        }
    } else {
        if !seen.is_empty() {
            seen.push(' ');
        }
        seen.push_str(stderr);
    }
    const SNIPPET_LIMIT: usize = 160;
    let snippet: String = seen.chars().take(SNIPPET_LIMIT).collect();
    format!("未能从 dsh 输出中识别语义化版本：{snippet}")
}

/// 纯逻辑：在一段文本中扫描语义化版本号，兼容可省略的 `v`/`V` 前缀以及 prerelease 与
/// build 元数据。只接受边界完整的 token：起点不能紧贴字母、数字、点、下划线、斜杠或
/// 减号（避免从长数字、路径或文件名中间截取），终点同理；这样 `127.0.0.1`、
/// `.nvm/versions/node/v22.19.0/bin` 之类不会被误判为版本。
fn extract_semver_token(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    for start in 0..bytes.len() {
        let (token_start, has_prefix) = match bytes[start] {
            b'v' | b'V' => (start + 1, true),
            b'0'..=b'9' => (start, false),
            _ => continue,
        };
        // v 前缀只有紧跟数字才是版本 token（v1.2.3），否则可能是普通单词。
        if has_prefix && !matches!(bytes.get(token_start), Some(byte) if byte.is_ascii_digit()) {
            continue;
        }
        if start > 0 && is_semver_token_edge(bytes[start - 1]) {
            continue;
        }
        // 前缀不进入结果，返回规范化后的版本号。
        let Some(end) = parse_semver_at(bytes, token_start) else {
            continue;
        };
        if !semver_terminator_ok(bytes, end) {
            continue;
        }
        let token = &text[token_start..end];
        if semver::Version::parse(token).is_ok() {
            return Some(token.to_string());
        }
    }
    None
}

/// 纯逻辑：版本 token 起点的前一字符若属于这些字符，说明落在更长 token 内部，不能作为版本。
fn is_semver_token_edge(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/')
}

/// 纯逻辑：从 `start` 起解析 `主.次.修订[-先行][+构建]`，成功返回结束下标（不含）。
/// 三段数字必须存在；先行/构建段为点分隔的非空标识（字符集 `[0-9A-Za-z-]`）。
fn parse_semver_at(bytes: &[u8], start: usize) -> Option<usize> {
    let mut cursor = start;
    for component in 0..3 {
        let digits_start = cursor;
        while matches!(bytes.get(cursor), Some(byte) if byte.is_ascii_digit()) {
            cursor += 1;
        }
        if cursor == digits_start {
            return None;
        }
        if component < 2 {
            if bytes.get(cursor) != Some(&b'.') {
                return None;
            }
            cursor += 1;
        }
    }
    for lead_in in *b"-+" {
        if bytes.get(cursor) != Some(&lead_in) {
            continue;
        }
        cursor += 1;
        loop {
            let ident_start = cursor;
            while matches!(bytes.get(cursor), Some(byte) if byte.is_ascii_alphanumeric() || *byte == b'-')
            {
                cursor += 1;
            }
            if cursor == ident_start {
                return None;
            }
            if bytes.get(cursor) == Some(&b'.') {
                cursor += 1;
            } else {
                break;
            }
        }
    }
    Some(cursor)
}

/// 纯逻辑：检查版本 token 末尾边界是否完整，排除 `1.2.3.4`、`1.2.3beta` 之类的粘连误读；
/// 句末的 `1.2.3.` 仍被接受（点后不再跟数字即可）。
fn semver_terminator_ok(bytes: &[u8], end: usize) -> bool {
    match bytes.get(end) {
        None => true,
        Some(b'.') => !matches!(bytes.get(end + 1), Some(next) if next.is_ascii_digit()),
        Some(byte) => !(byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-'),
    }
}

pub fn dsh_home_directory() -> Option<PathBuf> {
    std::env::var_os("DSH_HOME")
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
        // 跨平台主目录：Windows 读 USERPROFILE，其余平台读 HOME（与 dsh 自身行为一致）。
        .or_else(|| user_home().map(|home| home.join(".dsh")))
}

pub fn discover_profiles() -> Vec<String> {
    let Some(home) = dsh_home_directory() else {
        return vec!["web".into()];
    };
    let mut profiles = fs::read_dir(home.join("profiles"))
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.') && name != "node_modules")
        .collect::<Vec<_>>();
    if !profiles.iter().any(|name| name == "web") {
        profiles.push("web".into());
    }
    profiles.sort();
    profiles
}

pub fn profile_directory(profile: &str) -> Option<PathBuf> {
    let home = dsh_home_directory()?;
    let trimmed = profile.trim();
    let name = if trimmed.is_empty() { "web" } else { trimmed };
    if name == "." || name == ".." || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')) {
        return None;
    }
    Some(home.join("profiles").join(name))
}

/// 读取 DSH 全局主题偏好（"system" | "dark" | "light"，未配置或解析失败默认 "system"）。
pub fn read_dsh_theme_preference() -> String {
    let Some(home) = dsh_home_directory() else {
        return "system".into();
    };
    let path = home.join("settings.yaml");
    let Ok(content) = fs::read_to_string(path) else {
        return "system".into();
    };
    parse_dsh_theme_preference(&content)
}

/// 解析 YAML 文本中的 `ui-theme.preference` 字段
pub fn parse_dsh_theme_preference(yaml_text: &str) -> String {
    let mut in_ui_theme = false;
    for line in yaml_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("ui-theme:") {
            in_ui_theme = true;
            continue;
        }
        if in_ui_theme {
            if !line.starts_with(' ') && !line.starts_with('\t') && trimmed.contains(':') {
                in_ui_theme = false;
                continue;
            }
            if let Some(val) = trimmed.strip_prefix("preference:") {
                let val = val.trim().trim_matches(|c| c == '\'' || c == '"');
                if val == "dark" || val == "light" || val == "system" {
                    return val.to_string();
                }
            }
        }
    }
    "system".into()
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePlugin {
    pub name: String,
    pub version: String,
}

/// 校验插件名：允许 npm 包名（含一个 `@scope/` 前缀），拒绝路径穿越与命令元字符。
fn validate_plugin_name(name: &str) -> Result<(), String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > 214 {
        return Err("插件名无效".into());
    }
    let valid = |segment: &str| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
    };
    let ok = match trimmed.split_once('/') {
        Some((scope, package)) => valid(scope) && valid(package),
        None => valid(trimmed),
    };
    if ok {
        Ok(())
    } else {
        Err("插件名无效".into())
    }
}

/// 读取 Profile 已安装插件（package.json dependencies，与 DSH 插件安装机制一致）。
pub fn read_profile_plugins(profile: &str) -> Result<Vec<ProfilePlugin>, String> {
    let dir = profile_directory(profile).ok_or_else(|| "无法定位 Profile 目录".to_string())?;
    let text = fs::read_to_string(dir.join("package.json"))
        .map_err(|_| "Profile 尚未初始化（未找到 package.json），请先启动一次 DSH".to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| format!("package.json 解析失败：{error}"))?;
    let mut plugins = Vec::new();
    if let Some(deps) = value.get("dependencies").and_then(|deps| deps.as_object()) {
        for (name, spec) in deps {
            plugins.push(ProfilePlugin {
                name: name.clone(),
                version: spec.as_str().unwrap_or_default().to_string(),
            });
        }
    }
    plugins.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(plugins)
}

fn pnpm_command(args: &[&str]) -> Command {
    #[cfg(windows)]
    {
        // Windows 上 pnpm 是 pnpm.cmd，必须经 cmd 解析
        let mut command = Command::new("cmd");
        suppress_console_window(&mut command);
        command.arg("/C").arg(format!("pnpm {}", args.join(" ")));
        command
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new("pnpm");
        command.args(args);
        command
    }
}

const PNPM_CLEAN_TIMEOUT: Duration = Duration::from_secs(120);
const PNPM_INSTALL_TIMEOUT: Duration = Duration::from_secs(600);

/// 为辅助命令配置独立的进程组（Unix），便于超时整组终止子孙进程。
fn configure_command_process_group(_command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            _command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

/// 杀死子进程及其可能派生的孙进程（Unix 组杀，Windows taskkill /T /F）。
fn kill_child_tree(child: &mut Child) {
    let pid = child.id();
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let mut taskkill = Command::new("taskkill");
        suppress_console_window(&mut taskkill);
        let _ = taskkill
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
    }
    let _ = child.kill();
}

/// 等待子进程退出（带超时），返回合并后的输出；非零退出视为失败。
fn wait_for_output(mut child: Child, display: String, timeout: Duration) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child
                    .wait_with_output()
                    .ok()
                    .map(|out| {
                        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        if !stderr.trim().is_empty() {
                            if !text.trim().is_empty() {
                                text.push('\n');
                            }
                            text.push_str(&stderr);
                        }
                        text
                    })
                    .unwrap_or_default();
                return if status.success() {
                    Ok(output)
                } else {
                    Err(format!("{display} 失败：{output}"))
                };
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_child_tree(&mut child);
                    return Err(format!(
                        "{display} 执行超时（{} 秒），子进程已被终止",
                        timeout.as_secs()
                    ));
                }
                thread::sleep(Duration::from_millis(200));
            }
            Err(error) => return Err(format!("等待 {display} 退出失败：{error}")),
        }
    }
}

/// 在指定目录执行 pnpm 命令（带超时），返回合并后的输出。
fn run_pnpm(dir: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut command = pnpm_command(args);
    command.current_dir(dir).stdout(Stdio::piped()).stderr(Stdio::piped());
    configure_command_process_group(&mut command);
    let child = command
        .spawn()
        .map_err(|error| format!("启动 pnpm 失败：{error}"))?;
    wait_for_output(child, format!("pnpm {}", args.join(" ")), timeout)
}

/// 执行 dsh 命令行（带超时），返回合并后的输出。
fn run_dsh_cli(dsh_path: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut command = Command::new(dsh_path);
    let (envs, _) = launcher_environment();
    command.envs(envs);
    suppress_console_window(&mut command);
    command.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    configure_command_process_group(&mut command);
    let child = command
        .spawn()
        .map_err(|error| format!("启动 dsh 失败：{error}"))?;
    wait_for_output(child, format!("dsh {}", args.join(" ")), timeout)
}

/// 通过 DSH 官方命令卸载插件：`dsh plugin --profile <p> uninstall <name>`。
/// 依赖、lockfile 与 dsh.profile.bundles 清单均由 dsh 自行维护，避免手动
/// 清理遗漏（如 bundles 残留导致启动时无法解析 bundle）。
pub fn uninstall_profile_plugin(app: &AppHandle, profile: &str, name: &str) -> Result<(), String> {
    validate_plugin_name(name)?;
    let config = LauncherConfig::load();
    let dsh_path = resolve_dsh(&config.dsh_path)
        .ok_or_else(|| "未找到可执行的 dsh，请先在设置中指定 DSH 命令".to_string())?;
    emit_log(app, "plugins", "info", &format!("正在通过 dsh 卸载插件 {name}..."));
    let args = ["plugin", "--profile", profile, "uninstall", name];
    match run_dsh_cli(&dsh_path, &args, PNPM_INSTALL_TIMEOUT) {
        Ok(output) => {
            if !output.trim().is_empty() {
                emit_log(app, "plugins", "info", output.trim());
            }
            emit_log(app, "plugins", "info", &format!("插件 {name} 已卸载"));
            Ok(())
        }
        Err(error) => {
            emit_log(app, "plugins", "error", &error);
            Err("通过 dsh 卸载插件失败，详见服务日志".into())
        }
    }
}

/// 在 Profile 目录执行 `pnpm clean --lockfile` 快捷清理，输出写入服务日志。
pub fn run_profile_clean(app: &AppHandle, profile: &str) -> Result<(), String> {
    let dir = profile_directory(profile).ok_or_else(|| "无法定位 Profile 目录".to_string())?;
    if !dir.join("package.json").exists() {
        return Err("Profile 尚未初始化（未找到 package.json）".into());
    }
    emit_log(app, "plugins", "info", "正在执行 pnpm clean --lockfile...");
    if let Err(error) = run_pnpm(&dir, &["clean", "--lockfile"], PNPM_CLEAN_TIMEOUT) {
        emit_log(app, "plugins", "error", &error);
        return Err(error);
    }
    emit_log(app, "plugins", "info", "正在执行 pnpm install...");
    match run_pnpm(&dir, &["install"], PNPM_INSTALL_TIMEOUT) {
        Ok(output) => {
            if !output.trim().is_empty() {
                emit_log(app, "plugins", "info", output.trim());
            }
            emit_log(app, "plugins", "info", "依赖已清理并重新安装完成");
            Ok(())
        }
        Err(error) => {
            emit_log(
                app,
                "plugins",
                "error",
                "pnpm install 失败，Profile 可能不可用，请检查上方日志",
            );
            Err(error)
        }
    }
}

fn english_launch_action_error(error: &str) -> String {
    for (prefix, translated) in [
        ("无效的 DSH URL：", "Invalid DSH URL: "),
        (
            "打开内置 WebView 失败：",
            "Failed to open embedded WebView: ",
        ),
        ("打开浏览器失败：", "Failed to open browser: "),
    ] {
        if let Some(detail) = error.strip_prefix(prefix) {
            return format!("{translated}{detail}");
        }
    }
    if error == "打开浏览器失败" {
        return "Failed to open browser".into();
    }
    "Post-launch action failed".into()
}

fn parse_custom_args(value: &str) -> Result<Vec<String>, String> {
    shell_words::split(value.trim()).map_err(|error| format!("DSH 参数格式无效：{error}"))
}

/// 打开内嵌 DSH 视图。macOS/Windows 上是主窗口内的子 WebView（标题栏下方）；
/// Linux 不支持单窗口多 WebView，回退为独立 dsh-webview 窗口。
/// `allow_navigate`：仅服务启动/重启后的自动打开为 true（需要刷新 token 重新导航）；
/// 用户点击"打开"按钮时为 false——内容已存在时只做揭示，绝不重载页面（避免闪烁）。
pub fn open_content_view(app: &AppHandle, url: &str, allow_navigate: bool) -> Result<(), String> {
    let parsed: Url = url
        .parse()
        .map_err(|error| format!("无效的 DSH URL：{error}"))?;
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    return open_content_webview_child(app, parsed, url, allow_navigate);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    return open_embedded_webview_window(app, parsed, url, allow_navigate);
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn open_content_webview_child(
    app: &AppHandle,
    parsed: Url,
    url: &str,
    allow_navigate: bool,
) -> Result<(), String> {
    let Some(main_window) = app.get_window("main") else {
        return Err("主窗口不可用".into());
    };
    // 与旧 dsh-webview 窗口行为一致：打开/复用时唤起主窗口
    let raise_window = |window: &tauri::Window| -> Result<(), String> {
        window
            .show()
            .and_then(|_| window.unminimize())
            .and_then(|_| window.set_focus())
            .map_err(|error| format!("显示内置 WebView 失败：{error}"))
    };
    // 标题栏弹层/模态/过渡激活期间内容保持隐藏（点击启停时先把状态呈现给用户）
    let content_should_hide = || -> bool {
        app.try_state::<crate::AppState>()
            .map(|state| state.content_hidden.load(Ordering::Acquire))
            .unwrap_or(false)
    };
    if let Some(webview) = app.get_webview("content") {
        // 已存在时按需导航：服务重启后 launch token 已更换，旧页面会停留在
        // "authentication required"，只有自动打开（allow_navigate）才重新定位；
        // 用户点击"打开"时只做揭示，重载会造成页面闪烁。
        let current = webview.url().ok().map(|current| current.to_string());
        if allow_navigate && current.as_deref() != Some(url) {
            webview
                .navigate(parsed)
                .map_err(|error| format!("导航内置 WebView 失败：{error}"))?;
        } else {
            // 不导航的复用不会触发新的加载事件：页面已在展示，直接标记为就绪
            let _ = app.emit("content-page-load", true);
        }
        if !allow_navigate {
            // 用户显式请求揭示：确保取消隐藏标记并展示
            if let Some(state) = app.try_state::<crate::AppState>() {
                state.content_hidden.store(false, Ordering::Release);
            }
            let _ = webview.show();
            raise_window(&main_window)?;
        } else if content_should_hide() {
            let _ = webview.hide();
        } else {
            let _ = webview.show();
            raise_window(&main_window)?;
        }
        return Ok(());
    }
    let open_error =
        |error: tauri::Error| format!("打开内置 WebView 失败：{error}");
    let scale = main_window.scale_factor().map_err(open_error)?;
    let size = main_window.inner_size().map_err(open_error)?;
    let logical = size.to_logical::<f64>(scale);
    let (width, height) = content_size(logical.width, logical.height);
    // 先发未就绪标记再创建：页面加载极快时 Finished 事件可能在创建过程中
    // 到达，若标记在其后发出会把前端就绪状态压回去，导致内容无法揭示
    let _ = app.emit("content-page-load", false);
    let webview = main_window
        .add_child(
            content_webview_builder(app, parsed),
            LogicalPosition::new(0.0, TITLEBAR_HEIGHT),
            LogicalSize::new(width, height),
        )
        .map_err(open_error)?;
    // 创建后一律先隐藏：等页面加载完成（content-page-load Finished）后由前端
    // 揭示，避免页面未渲染完成时露出 WebView 默认亮色底的闪现
    let _ = webview.hide();
    raise_window(&main_window)?;
    let _ = app.emit("content-webview-changed", true);
    Ok(())
}

/// 构造 DSH 内容子 WebView：同源导航放行，外部 http(s) 链接转交系统浏览器，
/// 新窗口一律拒绝，并注入系统主题脚本供 DSH 页面跟随深浅色。
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn content_webview_builder(app: &AppHandle, parsed: Url) -> tauri::WebviewBuilder<tauri::Wry> {
    let allowed_scheme = parsed.scheme().to_string();
    let allowed_host = parsed.host_str().map(str::to_string);
    let allowed_port = parsed.port_or_known_default();
    let app_nav = app.clone();
    let is_dark = crate::tray::is_system_dark(app);
    let bg_color = if is_dark {
        tauri::webview::Color(23, 24, 27, 255)
    } else {
        tauri::webview::Color(247, 247, 248, 255)
    };
    WebviewBuilder::new("content", WebviewUrl::External(parsed))
        .background_color(bg_color)
        .on_navigation(move |next_url| {
            if let Some(fragment) = next_url.fragment() {
                if let Some(theme) = fragment.strip_prefix("__dsh_theme__=") {
                    let theme = match theme {
                        "system" => "system",
                        "dark" => "dark",
                        "light" => "light",
                        _ => "system",
                    };
                    let _ = app_nav.emit("content-theme-changed", theme);
                }
            }
            let same_origin = next_url.scheme() == allowed_scheme
                && next_url.host_str().map(str::to_string) == allowed_host
                && next_url.port_or_known_default() == allowed_port;
            if same_origin {
                return true;
            }
            if matches!(next_url.scheme(), "http" | "https") {
                let _ = open_default(next_url.as_str());
            }
            false
        })
        .on_new_window(move |next_url, _| {
            if matches!(next_url.scheme(), "http" | "https") {
                let _ = open_default(next_url.as_str());
            }
            NewWindowResponse::Deny
        })
        .initialization_script(SYSTEM_THEME_SCRIPT)
        .initialization_script(CONTENT_THEME_WATCHER_SCRIPT)
        // 把 DSH 页面的真实 document.title 上报给前端主窗口标题栏展示
        .on_document_title_changed(|webview, title| {
            let app = webview.app_handle();
            let _ = app.emit("content-title-changed", title);
        })
        // 页面加载进度上报：标题栏层在 Finished 前保持内容隐藏，避免启动/切换
        // 时 webview 未渲染完成的闪烁；Finished 同时重推启动器主题
        .on_page_load(|webview, payload| {
            let finished = matches!(payload.event(), PageLoadEvent::Finished);
            let app = webview.app_handle();
            if finished {
                // 先应用记忆的主题，再通知前端揭示，保证揭示时主题已就位
                if let Some(theme) = webview
                    .app_handle()
                    .try_state::<crate::AppState>()
                    .and_then(|state| state.content_theme.lock().ok().and_then(|guard| guard.clone()))
                {
                    let _ = webview.eval(theme_apply_script(&theme));
                }
            }
            let _ = app.emit("content-page-load", finished);
        })
}

/// 按当前窗口尺寸与面板让位，重新摆放内容子 WebView；由窗口 Resized 事件
/// 与 set_content_insets 命令共同驱动。
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn sync_content_bounds(app: &AppHandle) {
    let (Some(main_window), Some(content)) =
        (app.get_window("main"), app.get_webview("content"))
    else {
        return;
    };
    let Ok(scale) = main_window.scale_factor() else {
        return;
    };
    let Ok(size) = main_window.inner_size() else {
        return;
    };
    let logical = size.to_logical::<f64>(scale);
    if logical.width < 120.0 || logical.height < TITLEBAR_HEIGHT + 120.0 {
        // 最小化或窗口动画过程中的异常尺寸，跳过本次同步
        return;
    }
    let (width, height) = content_size(logical.width, logical.height);
    let _ = content.set_bounds(Rect {
        position: LogicalPosition::new(0.0, TITLEBAR_HEIGHT).into(),
        size: LogicalSize::new(width, height).into(),
    });
}

pub fn embedded_view_open(app: &AppHandle) -> bool {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    return app.get_webview("content").is_some();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    return app
        .get_webview_window("dsh-webview")
        .and_then(|window| window.is_visible().ok())
        .unwrap_or(false);
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn open_embedded_webview_window(
    app: &AppHandle,
    parsed: Url,
    url: &str,
    allow_navigate: bool,
) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("dsh-webview") {
        // 窗口已存在时按需导航（同上：仅自动打开才重新定位）
        let current = window.url().ok().map(|current| current.to_string());
        if allow_navigate && current.as_deref() != Some(url) {
            window
                .navigate(parsed)
                .map_err(|error| format!("导航内置 WebView 失败：{error}"))?;
        } else {
            let _ = app.emit("content-page-load", true);
        }
        return window
            .show()
            .and_then(|_| window.unminimize())
            .and_then(|_| window.set_focus())
            .map_err(|error| format!("显示内置 WebView 失败：{error}"));
    }
    let allowed_scheme = parsed.scheme().to_string();
    let allowed_host = parsed.host_str().map(str::to_string);
    let allowed_port = parsed.port_or_known_default();
    let saved = WebviewWindowState::load().filter(|state| window_state_is_visible(app, state));
    let window = WebviewWindowBuilder::new(app, "dsh-webview", WebviewUrl::External(parsed))
        .on_navigation(move |next_url| {
            let same_origin = next_url.scheme() == allowed_scheme
                && next_url.host_str().map(str::to_string) == allowed_host
                && next_url.port_or_known_default() == allowed_port;
            if same_origin {
                return true;
            }
            if matches!(next_url.scheme(), "http" | "https") {
                let _ = open_default(next_url.as_str());
            }
            false
        })
        .on_new_window(move |next_url, _| {
            if matches!(next_url.scheme(), "http" | "https") {
                let _ = open_default(next_url.as_str());
            }
            NewWindowResponse::Deny
        })
        .title("DeepSeek Harness")
        .inner_size(1180.0, 780.0)
        .min_inner_size(760.0, 520.0)
        .visible(false)
        .initialization_script(SYSTEM_THEME_SCRIPT)
        .build()
        .map_err(|error| format!("打开内置 WebView 失败：{error}"))?;

    if let Some(state) = saved {
        window
            .set_size(PhysicalSize::new(
                state.width.max(760),
                state.height.max(520),
            ))
            .map_err(|error| format!("恢复内置 WebView 大小失败：{error}"))?;
        window
            .set_position(PhysicalPosition::new(state.x, state.y))
            .map_err(|error| format!("恢复内置 WebView 位置失败：{error}"))?;
    } else {
        window
            .center()
            .map_err(|error| format!("居中内置 WebView 失败：{error}"))?;
    }

    register_window_state_persistence(&window);
    window
        .show()
        .and_then(|_| window.set_focus())
        .map_err(|error| format!("显示内置 WebView 失败：{error}"))?;
    let _ = app.emit("content-page-load", true);
    let _ = app.emit("content-webview-changed", true);
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn register_window_state_persistence(window: &WebviewWindow) {
    let app = window.app_handle().clone();
    window.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event
            && let Some(window) = app.get_webview_window("dsh-webview")
        {
            save_window_state(&window);
            api.prevent_close();
            let _ = window.hide();
        }
    });
}

pub(crate) fn save_window_state(window: &tauri::Window) {
    let (Ok(position), Ok(size)) = (window.outer_position(), window.inner_size()) else {
        return;
    };
    if size.width < 100 || size.height < 100 {
        return;
    }
    let _ = WebviewWindowState {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
    }
    .save();
}

pub(crate) fn window_state_is_visible(app: &AppHandle, state: &WebviewWindowState) -> bool {
    let Ok(monitors) = app.available_monitors() else {
        return false;
    };
    monitors.iter().any(|monitor| {
        let position = monitor.position();
        let size = monitor.size();
        rectangles_overlap(
            state.x,
            state.y,
            state.width,
            state.height,
            position.x,
            position.y,
            size.width,
            size.height,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn rectangles_overlap(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    monitor_x: i32,
    monitor_y: i32,
    monitor_width: u32,
    monitor_height: u32,
) -> bool {
    let right = i64::from(x) + i64::from(width);
    let bottom = i64::from(y) + i64::from(height);
    let monitor_right = i64::from(monitor_x) + i64::from(monitor_width);
    let monitor_bottom = i64::from(monitor_y) + i64::from(monitor_height);
    right >= i64::from(monitor_x) + 80
        && bottom >= i64::from(monitor_y) + 50
        && i64::from(x) <= monitor_right - 80
        && i64::from(y) <= monitor_bottom - 50
}

fn close_embedded_webview(app: &AppHandle) {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if let Some(webview) = app.get_webview("content") {
        let _ = webview.close();
        let _ = app.emit("content-webview-changed", false);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    if let Some(window) = app.get_webview_window("dsh-webview") {
        save_window_state(&window);
        let _ = window.destroy();
        let _ = app.emit("content-webview-changed", false);
    }
}

#[cfg(target_os = "macos")]
pub fn open_default(url: &str) -> Result<(), String> {
    Command::new("open")
        .arg(url)
        .status()
        .map_err(|error| format!("打开浏览器失败：{error}"))?
        .success()
        .then_some(())
        .ok_or_else(|| "打开浏览器失败".into())
}

#[cfg(target_os = "windows")]
pub fn open_default(url: &str) -> Result<(), String> {
    open_default_windows(url)
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn open_default(url: &str) -> Result<(), String> {
    open_default_unix(url)
}

#[cfg(windows)]
fn open_default_windows(url: &str) -> Result<(), String> {
    let mut failures = Vec::new();

    let mut explorer = Command::new("explorer.exe");
    explorer.arg(url);
    suppress_console_window(&mut explorer);
    match explorer.status() {
        Ok(status) if status.success() => return Ok(()),
        Ok(status) => failures.push(format!("explorer.exe 退出码 {status}")),
        Err(error) => failures.push(format!("explorer.exe：{error}")),
    }

    // `start` is a cmd built-in. Keep this fallback for stripped-down Windows
    // environments where Explorer cannot resolve the URL shell association.
    let mut command = Command::new("cmd.exe");
    command.args(["/D", "/C", "start", "", url]);
    suppress_console_window(&mut command);
    match command.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!(
            "打开浏览器失败：{}；cmd.exe 退出码 {status}",
            failures.join("；")
        )),
        Err(error) => Err(format!(
            "打开浏览器失败：{}；cmd.exe：{error}",
            failures.join("；")
        )),
    }
}

/// 纯逻辑：按桌面环境给出打开 URL 的候选程序及参数，按尝试顺序排列。
/// Linux 在 `xdg-open` 之外提供 `gio open` 作为回退；其余桌面仅保留 xdg-open。
#[cfg(unix)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn open_program_candidates(is_linux: bool) -> Vec<(&'static str, &'static [&'static str])> {
    let mut candidates: Vec<(&'static str, &'static [&'static str])> =
        vec![("xdg-open", &[] as &[&str])];
    if is_linux {
        candidates.push(("gio", &["open"]));
    }
    candidates
}

/// 纯逻辑：单次打开尝试的结果分类，用于决定是否继续回退。
#[cfg(unix)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenerOutcome {
    Opened,
    Failed,
    TimedOut,
}

#[cfg(unix)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
impl OpenerOutcome {
    /// 只有确定的失败才回退到下一个候选；成功无需回退；
    /// 超时状态不明（浏览器可能已被拉起），继续回退可能造成重复打开标签页，因此不回退。
    fn should_try_next(self) -> bool {
        !matches!(self, OpenerOutcome::Opened | OpenerOutcome::TimedOut)
    }
}

/// 非 macOS Unix（主要是 Linux）的打开 URL 实现：xdg-open → gio open 逐个尝试，均带限时保护。
#[cfg(unix)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
const OPEN_URL_TIMEOUT: Duration = Duration::from_secs(6);

#[cfg(unix)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn open_default_unix(url: &str) -> Result<(), String> {
    let mut failures: Vec<String> = Vec::new();
    for (program, args) in open_program_candidates(cfg!(target_os = "linux")) {
        let mut argv: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        argv.push(OsStr::new(url));
        let (outcome, detail) =
            match spawn_with_timeout(Path::new(program), &argv, OPEN_URL_TIMEOUT, None) {
                SpawnOutcome::Completed { success: true } => (OpenerOutcome::Opened, String::new()),
                SpawnOutcome::Completed { success: false } => {
                    (OpenerOutcome::Failed, "退出码非零".to_string())
                }
                SpawnOutcome::NotRunnable => (OpenerOutcome::Failed, "无法启动".to_string()),
                SpawnOutcome::TimedOut => (
                    OpenerOutcome::TimedOut,
                    format!("超过 {OPEN_URL_TIMEOUT:?}"),
                ),
            };
        match outcome {
            OpenerOutcome::Opened => return Ok(()),
            OpenerOutcome::TimedOut => {
                return Err(format!(
                    "打开浏览器失败：{program} 执行超时（{detail}），子进程已被终止；状态不明确，不再回退其他程序"
                ));
            }
            OpenerOutcome::Failed => failures.push(format!("{program} {detail}")),
        }
        if !outcome.should_try_next() {
            break;
        }
    }
    Err(format!("打开浏览器失败：{}", failures.join("；")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_status_is_stopped() {
        assert_eq!(ServiceStatus::default().phase, "stopped");
    }

    #[test]
    fn extract_authenticated_url_parses_dsh_web_line() {
        let line = "dsh web: http://127.0.0.1:3080/?token=abc_-123 (LAN: http://192.168.50.197:3080/?token=abc_-123)";
        assert_eq!(
            extract_authenticated_url(line).as_deref(),
            Some("http://127.0.0.1:3080/?token=abc_-123")
        );
    }

    #[test]
    fn extract_authenticated_url_rejects_non_token_lines() {
        assert_eq!(extract_authenticated_url("dsh web: http://127.0.0.1:3080"), None);
        assert_eq!(extract_authenticated_url("Health check passed"), None);
        assert_eq!(extract_authenticated_url(""), None);
    }

    #[test]
    fn dsh_command_matches_plugin_restart_argv() {
        let config = LauncherConfig {
            profile: "web".into(),
            host: "127.0.0.1".into(),
            port: 3080,
            custom_args: "--no-open".into(),
            ..Default::default()
        };
        assert!(dsh_command_matches(
            "/Users/jockiller/.nvm/versions/node/v22.19.0/bin/node /Users/jockiller/.nvm/versions/node/v22.19.0/bin/dsh --profile web --host 127.0.0.1 --port 3080 --no-open",
            &config
        ));
        assert!(!dsh_command_matches(
            "node /usr/local/bin/dsh --profile other --host 127.0.0.1 --port 3080 --no-open",
            &config
        ));
        assert!(!dsh_command_matches(
            "node /usr/local/bin/dsh --profile web --host 127.0.0.1 --port 3081 --no-open",
            &config
        ));
        assert!(!dsh_command_matches(
            "node /usr/local/bin/dsh --profile web --host 127.0.0.1 --port 3080",
            &config
        ));
    }

    #[test]
    fn profile_discovery_always_includes_web() {
        assert!(discover_profiles().iter().any(|profile| profile == "web"));
    }

    #[test]
    fn plugin_name_validation_blocks_traversal_and_metacharacters() {
        assert!(validate_plugin_name("dsh-sound").is_ok());
        assert!(validate_plugin_name("@dickpy/dsh-cloud-sync").is_ok());
        assert!(validate_plugin_name("dsh-plugin-gptpro.v2_beta").is_ok());
        assert!(validate_plugin_name("").is_err());
        assert!(validate_plugin_name("../escape").is_err());
        assert!(validate_plugin_name("pkg/..").is_err());
        assert!(validate_plugin_name("pkg; rm -rf").is_err());
        assert!(validate_plugin_name("pkg name").is_err());
        assert!(validate_plugin_name("@scope/").is_err());
        assert!(validate_plugin_name("/pkg").is_err());
    }

    #[test]
    fn dsh_theme_preference_parsing_matches_settings() {
        assert_eq!(
            parse_dsh_theme_preference("ui-theme:\n  preference: system"),
            "system"
        );
        assert_eq!(
            parse_dsh_theme_preference("ui-theme:\n  preference: dark"),
            "dark"
        );
        assert_eq!(
            parse_dsh_theme_preference("ui-theme:\n  preference: 'light'"),
            "light"
        );
        assert_eq!(
            parse_dsh_theme_preference("other:\n  preference: dark"),
            "system"
        );
        assert_eq!(parse_dsh_theme_preference(""), "system");
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn content_size_reserves_titlebar() {
        assert_eq!(content_size(1180.0, 780.0), (1180.0, 742.0));
        // 最小化等异常尺寸下保底，不出现负宽高
        assert_eq!(content_size(10.0, 10.0), (80.0, 80.0));
    }

    #[test]
    fn custom_dsh_args_preserve_quoted_values() {
        assert_eq!(
            parse_custom_args("--no-open --trusted-host 'host name'").unwrap(),
            vec!["--no-open", "--trusted-host", "host name"]
        );
    }

    #[test]
    fn custom_dsh_args_reject_unclosed_quotes() {
        assert!(parse_custom_args("--trusted-host 'broken").is_err());
    }

    #[test]
    fn launcher_log_errors_are_english() {
        assert_eq!(
            english_launch_action_error("打开内置 WebView 失败：window error"),
            "Failed to open embedded WebView: window error"
        );
        assert_eq!(
            english_launch_action_error("未知错误"),
            "Post-launch action failed"
        );
    }

    #[test]
    fn host_authority_supports_names_and_ipv6() {
        assert_eq!(host_authority("localhost", 3080), "localhost:3080");
        assert_eq!(host_authority("::1", 3080), "[::1]:3080");
        assert_eq!(host_authority("[::1]", 3080), "[::1]:3080");
        assert_eq!(service_url("::1", 3080), "http://[::1]:3080");
        assert!(!resolve_addresses("localhost", 3080).unwrap().is_empty());
    }

    #[test]
    fn saved_window_must_remain_visibly_on_screen() {
        assert!(rectangles_overlap(100, 100, 1180, 780, 0, 0, 1920, 1080));
        assert!(rectangles_overlap(-1100, 100, 1180, 780, 0, 0, 1920, 1080));
        assert!(!rectangles_overlap(-2000, 100, 1180, 780, 0, 0, 1920, 1080));
        assert!(!rectangles_overlap(2000, 100, 1180, 780, 0, 0, 1920, 1080));
    }

    // ---------- 登录 Shell 候选与尝试计划 ----------

    #[cfg(unix)]
    #[test]
    fn login_shell_candidates_prefer_env_shell_then_defaults() {
        let present = ["/usr/bin/zsh", "/bin/bash"];
        let exists = |path: &Path| present.contains(&path.to_str().unwrap_or_default());
        let candidates = login_shell_candidates(Some("/usr/bin/zsh"), &exists);
        assert_eq!(
            candidates,
            vec![PathBuf::from("/usr/bin/zsh"), PathBuf::from("/bin/bash"),]
        );
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_candidates_skip_missing_and_blank_shell() {
        let exists = |path: &Path| path == Path::new("/bin/sh");
        assert_eq!(
            login_shell_candidates(Some("/does/not/exist"), &exists),
            vec![PathBuf::from("/bin/sh")]
        );
        // 空白 SHELL 等同于未设置
        assert_eq!(
            login_shell_candidates(Some("   "), &exists),
            vec![PathBuf::from("/bin/sh")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_candidates_dedup_when_shell_matches_fallback() {
        let exists = |path: &Path| path == Path::new("/bin/bash");
        let candidates = login_shell_candidates(Some("/bin/bash"), &exists);
        assert_eq!(candidates, vec![PathBuf::from("/bin/bash")]);
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_attempt_plan_is_shell_major_with_fallback_args() {
        let shells = vec![PathBuf::from("/bin/zsh"), PathBuf::from("/bin/bash")];
        let plan = login_shell_attempt_plan(&shells);
        assert_eq!(plan.len(), shells.len() * LOGIN_SHELL_ARGSETS.len());
        assert_eq!(plan[0].0, PathBuf::from("/bin/zsh"));
        assert_eq!(plan[0].1, &["-li", "-c"]);
        assert_eq!(plan[1].0, PathBuf::from("/bin/zsh"));
        assert_eq!(plan[1].1, &["-c"], "同一 Shell 的第二参数组为裸 -c 兜底");
        assert_eq!(plan[2].0, PathBuf::from("/bin/bash"));
        assert_eq!(plan[3].0, PathBuf::from("/bin/bash"));
        assert!(
            LOGIN_SHELL_ARGSETS
                .iter()
                .all(|argset| argset.last() == Some(&"-c"))
        );
    }

    // ---------- env 输出解析 ----------

    #[cfg(unix)]
    #[test]
    fn env_output_parses_nul_separated_entries() {
        let raw = b"A=1\0PATH=/usr/bin:/bin\0EMPTY=\0BROKEN\0=NO_KEY\0";
        let entries = parse_env_output(raw);
        assert_eq!(entries.len(), 3, "无键或缺失 = 的片段应被丢弃");
        assert_eq!(entries[0], (OsString::from("A"), OsString::from("1")));
        assert_eq!(
            entries[1],
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin"))
        );
        assert_eq!(entries[2], (OsString::from("EMPTY"), OsString::new()));
    }

    #[cfg(unix)]
    #[test]
    fn env_output_keeps_equals_and_newlines_in_nul_mode() {
        let entries = parse_env_output(b"PATH=/a=b\0MULTI=line1\nline2\0");
        assert_eq!(entries[0], (OsString::from("PATH"), OsString::from("/a=b")));
        assert_eq!(
            entries[1],
            (OsString::from("MULTI"), OsString::from("line1\nline2"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn env_output_falls_back_to_newline_separation_with_crlf() {
        let entries = parse_env_output(b"A=1\nPATH=/bin\r\nC=3\n");
        assert_eq!(
            entries,
            vec![
                (OsString::from("A"), OsString::from("1")),
                (OsString::from("PATH"), OsString::from("/bin")),
                (OsString::from("C"), OsString::from("3")),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn env_output_empty_input_yields_no_entries() {
        assert!(parse_env_output(b"").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn clean_shell_environment_keeps_user_variables_and_uses_default_path() {
        let original = std::env::vars_os().collect::<Vec<_>>();
        let environment = login_shell_base_environment(
            Path::new("/bin/sh"),
            Path::new("/tmp/dsh-launcher-test-env"),
        );
        for (key, value) in original {
            if !environment_key_eq(&key, OsStr::new("PATH"))
                && !environment_key_eq(&key, OsStr::new("SHELL"))
                && !environment_key_eq(&key, OsStr::new("DSH_LAUNCHER_ENV_FILE"))
            {
                assert_eq!(
                    environment_value(&environment, &key.to_string_lossy()),
                    Some(&value)
                );
            }
        }
        assert_eq!(
            environment_value(&environment, "SHELL"),
            Some(&OsString::from("/bin/sh"))
        );
        assert_eq!(
            environment_value(&environment, "DSH_LAUNCHER_ENV_FILE"),
            Some(&OsString::from("/tmp/dsh-launcher-test-env"))
        );
        assert_eq!(
            environment_value(&environment, "PATH"),
            Some(&default_login_shell_path())
        );
    }

    #[cfg(unix)]
    #[test]
    fn overlay_keys_cover_toolchain_not_launcher_secrets() {
        assert!(should_overlay_shell_environment_key(OsStr::new("PATH")));
        assert!(should_overlay_shell_environment_key(OsStr::new(
            "PNPM_HOME"
        )));
        assert!(should_overlay_shell_environment_key(OsStr::new("NVM_DIR")));
        assert!(!should_overlay_shell_environment_key(OsStr::new("HOME")));
        assert!(!should_overlay_shell_environment_key(OsStr::new(
            "DSH_HOME"
        )));
        assert!(!should_overlay_shell_environment_key(OsStr::new(
            "DSH_LAUNCHER_ENV_FILE"
        )));
    }

    #[cfg(unix)]
    #[test]
    fn shell_environment_overlays_base_without_dropping_launcher_variables() {
        let mut base = vec![
            (OsString::from("PATH"), OsString::from("/gui/bin")),
            (OsString::from("DSH_HOME"), OsString::from("/dsh")),
            (OsString::from("LAUNCHER_ONLY"), OsString::from("kept")),
        ];
        merge_environment(
            &mut base,
            vec![
                (OsString::from("PATH"), OsString::from("/shell/bin")),
                (OsString::from("SHELL_ONLY"), OsString::from("added")),
            ],
        );
        assert_eq!(
            base,
            vec![
                (OsString::from("PATH"), OsString::from("/shell/bin")),
                (OsString::from("DSH_HOME"), OsString::from("/dsh")),
                (OsString::from("LAUNCHER_ONLY"), OsString::from("kept")),
                (OsString::from("SHELL_ONLY"), OsString::from("added")),
            ]
        );
    }

    #[test]
    fn environment_key_matching_follows_platform_case_rules() {
        assert!(environment_key_eq(OsStr::new("PATH"), OsStr::new("PATH")));
        #[cfg(windows)]
        assert!(environment_key_eq(OsStr::new("Path"), OsStr::new("PATH")));
        #[cfg(not(windows))]
        assert!(!environment_key_eq(OsStr::new("Path"), OsStr::new("PATH")));
    }

    // ---------- 标记与输出提取 ----------

    #[cfg(unix)]
    #[test]
    fn marker_extraction_skips_startup_noise() {
        let raw = b"neofetch banner\n__DSH_LAUNCHER_ENV__\nPATH=/bin\n";
        assert_eq!(
            extract_after_marker(raw, LOGIN_SHELL_MARKER),
            Some(&b"PATH=/bin\n"[..])
        );
    }

    #[cfg(unix)]
    #[test]
    fn marker_extraction_handles_crlf_and_missing_marker() {
        assert_eq!(
            extract_after_marker(b"noise\n__DSH_LAUNCHER_ENV__\r\nA=1", LOGIN_SHELL_MARKER),
            Some(&b"A=1"[..])
        );
        // 标记位于最前面且后面没有换行
        assert_eq!(
            extract_after_marker(b"__DSH_LAUNCHER_ENV__A=x", LOGIN_SHELL_MARKER),
            Some(&b"A=x"[..])
        );
        assert_eq!(extract_after_marker(b"no marker", LOGIN_SHELL_MARKER), None);
    }

    #[cfg(unix)]
    #[test]
    fn first_non_empty_line_skips_blank_lines() {
        assert_eq!(
            first_non_empty_line(b"\n\n /usr/bin/dsh \n"),
            Some("/usr/bin/dsh".to_string())
        );
        assert_eq!(first_non_empty_line(b"  \n\t"), None);
        assert_eq!(first_non_empty_line(b""), None);
    }

    // ---------- Linux 打开 URL 回退 ----------

    #[cfg(unix)]
    #[test]
    fn open_candidates_add_gio_fallback_only_on_linux() {
        let linux = open_program_candidates(true);
        assert_eq!(linux.len(), 2);
        assert_eq!(linux[0], ("xdg-open", &[][..] as &[&str]));
        assert_eq!(linux[1].0, "gio");
        assert_eq!(linux[1].1, &["open"]);

        let not_linux = open_program_candidates(false);
        assert_eq!(not_linux, vec![("xdg-open", &[][..] as &[&str])]);
    }

    #[cfg(unix)]
    #[test]
    fn opener_fallback_tries_next_only_on_definite_failure() {
        assert!(!OpenerOutcome::Opened.should_try_next());
        assert!(OpenerOutcome::Failed.should_try_next());
        // 状态不明（超时）时必须停下，避免同一 URL 被打开两次
        assert!(!OpenerOutcome::TimedOut.should_try_next());
    }

    #[cfg(unix)]
    #[test]
    fn opener_helpers_remain_well_typed_on_every_unix_target() {
        // 仅做类型检查引用：保证仅在 Linux/BSD 分支使用的实现也能在 macOS 编译验证。
        let _ = open_default_unix as fn(&str) -> Result<(), String>;
        assert_eq!(open_program_candidates(false).len(), 1);
    }

    // ---------- Windows 可执行文件检测 ----------

    #[test]
    fn windows_extensions_prefer_exe_then_pathext_scripts() {
        assert_eq!(
            windows_extensions(None),
            vec![".exe", ".cmd", ".bat"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
        // .exe 提前；其余按 PATHEXT 顺序
        assert_eq!(
            windows_extensions(Some(".COM;.EXE;.BAT;.CMD")),
            vec![".exe", ".bat", ".cmd"]
        );
        // 无关扩展被忽略，缺省的 .bat 补齐
        assert_eq!(
            windows_extensions(Some(".py;.CMD")),
            vec![".exe", ".cmd", ".bat"]
        );
        // 无点前缀的 PATHEXT 条目同样可识别
        assert_eq!(
            windows_extensions(Some("CMD")),
            vec![".exe", ".cmd", ".bat"]
        );
    }

    #[test]
    fn path_candidates_switch_between_platforms() {
        let dir = Path::new("/tools");
        assert_eq!(
            dsh_candidates(dir, false, None),
            vec![PathBuf::from("/tools/dsh")]
        );
        assert_eq!(
            dsh_candidates(dir, true, Some(".EXE;.CMD;.BAT")),
            vec![
                PathBuf::from("/tools/dsh.exe"),
                PathBuf::from("/tools/dsh.cmd"),
                PathBuf::from("/tools/dsh.bat"),
            ]
        );
    }

    #[test]
    fn manual_resolution_rejects_missing_paths() {
        assert!(resolve_manual("   ").is_none());
        assert!(resolve_manual("/definitely/not/a/dsh/binary").is_none());
    }

    #[test]
    fn windows_common_candidates_cover_npm_nvm_nodejs_and_scoop() {
        // 与实现一致：候选路径基于传入基础路径逐段 join，分段构造预期（分隔符随平台而变）。
        fn joined(base: &str, parts: &[&str]) -> PathBuf {
            let mut path = PathBuf::from(base);
            for part in parts {
                path = path.join(part);
            }
            path
        }
        let lookup = |key: &str| match key {
            "APPDATA" => Some("C:\\Users\\demo\\AppData\\Roaming".into()),
            "NVM_SYMLINK" => Some("C:\\Program Files\\nodejs".into()),
            "ProgramFiles" => Some("C:\\Program Files".into()),
            "USERPROFILE" => Some("C:\\Users\\demo".into()),
            _ => None,
        };
        let candidates = windows_common_candidates(&lookup, Some(".COM;.EXE;.CMD;.BAT"));
        for (base, parts) in [
            // npm 全局目录（PATHEXT 全展开）
            ("C:\\Users\\demo\\AppData\\Roaming", vec!["npm", "dsh.exe"]),
            ("C:\\Users\\demo\\AppData\\Roaming", vec!["npm", "dsh.cmd"]),
            ("C:\\Users\\demo\\AppData\\Roaming", vec!["npm", "dsh.bat"]),
            // nvm-windows 符号链接目录（NVM_SYMLINK）与 Node.js 安装目录
            ("C:\\Program Files\\nodejs", vec!["dsh.exe"]),
            ("C:\\Program Files\\nodejs", vec!["dsh.cmd"]),
            // Scoop shims 与自定义 npm 全局前缀
            ("C:\\Users\\demo", vec!["scoop", "shims", "dsh.exe"]),
            ("C:\\Users\\demo", vec![".npm-global", "dsh.cmd"]),
        ] {
            let expected = joined(base, &parts);
            assert!(
                candidates.contains(&expected),
                "缺少候选 {expected:?}，实际：{candidates:?}"
            );
        }
        // APPDATA 缺失时回退 USERPROFILE\AppData\Roaming\npm（回退目录由 join 派生，比较需同构构造）
        let fallback = |key: &str| (key == "USERPROFILE").then(|| "C:\\Users\\demo".to_string());
        let candidates = windows_common_candidates(&fallback, None);
        assert!(candidates.contains(&joined(
            "C:\\Users\\demo",
            &["AppData", "Roaming", "npm", "dsh.cmd"][..]
        )));
    }

    #[test]
    fn nvm_windows_version_dirs_sort_newest_first() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("dsh-launcher-nvm-{}-{stamp}", std::process::id()));
        for name in ["v20.11.0", "v22.19.0", "v9.11.2"] {
            fs::create_dir_all(root.join(name)).unwrap();
        }
        // 非 v 前缀目录与普通文件都应被排除
        fs::create_dir_all(root.join("cache")).unwrap();
        fs::write(root.join("settings.txt"), "x").unwrap();
        let dirs = nvm_windows_version_dirs(&root);
        // 与 Unix nvm 目录逻辑一致：按目录名字符串降序
        assert_eq!(
            dirs.iter()
                .map(|dir| dir.file_name().unwrap().to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["v9.11.2", "v22.19.0", "v20.11.0"]
        );
        let _ = fs::remove_dir_all(&root);
    }

    // ---------- dsh --version 输出中的语义化版本提取 ----------

    #[test]
    fn semver_token_extracts_plain_prefixed_and_prerelease() {
        assert_eq!(extract_semver_token("1.2.5"), Some("1.2.5".into()));
        // v/V 前缀不进入结果
        assert_eq!(extract_semver_token("v1.2.5"), Some("1.2.5".into()));
        assert_eq!(extract_semver_token("V1.2.5"), Some("1.2.5".into()));
        // prerelease 与 build 元数据完整保留
        assert_eq!(
            extract_semver_token("v1.2.5-beta.2"),
            Some("1.2.5-beta.2".into())
        );
        assert_eq!(
            extract_semver_token("1.2.5+build.9"),
            Some("1.2.5+build.9".into())
        );
        assert_eq!(
            extract_semver_token("1.2.5-rc.1+build.9"),
            Some("1.2.5-rc.1+build.9".into())
        );
        // 带说明文字与多 token 时取第一个完整版本
        assert_eq!(
            extract_semver_token("dsh 2.10.0 (requires node v18+)"),
            Some("2.10.0".into())
        );
    }

    #[test]
    fn semver_token_rejects_addresses_paths_and_partial_numbers() {
        // IP 地址不按 127.0.0 误读
        assert_eq!(extract_semver_token("http://127.0.0.1:3080"), None);
        // 路径里的版本目录不作数
        assert_eq!(
            extract_semver_token("/root/.nvm/versions/node/v22.19.0/bin"),
            None
        );
        // 粘连 token 不拆：1.2.3.4 与 1.2.3beta
        assert_eq!(extract_semver_token("1.2.3.4"), None);
        assert_eq!(extract_semver_token("1.2.3beta"), None);
        // 句末句点不阻碍识别
        assert_eq!(
            extract_semver_token("version is 1.2.3."),
            Some("1.2.3".into())
        );
        // 非法语义化版本（前导零、纯文本、缺失段）
        assert_eq!(extract_semver_token("v01.2.3"), None);
        assert_eq!(extract_semver_token("1.2"), None);
        assert_eq!(extract_semver_token("no version here"), None);
        assert_eq!(extract_semver_token(""), None);
    }

    #[test]
    fn dsh_version_output_prefers_stdout_then_stderr() {
        assert_eq!(
            parse_dsh_version("1.4.2\n", "warning: legacy mode\n"),
            Some("1.4.2".into())
        );
        // stdout 无版本时回退 stderr（部分 CLI 把版本写到 stderr）
        assert_eq!(
            parse_dsh_version("", "v0.2.0-rc.1\n"),
            Some("0.2.0-rc.1".into())
        );
        assert_eq!(parse_dsh_version("   ", "\n"), None);
    }

    #[test]
    fn version_parse_error_is_explicit_about_missing_version() {
        // 全空输出：明确提示未返回输出
        let error = version_parse_error("  ", "\n");
        assert!(error.contains("未返回任何输出"), "实际：{error}");
        // 有输出但无版本：附上原始片段
        let error = version_parse_error("hello\n", "world\n");
        assert!(error.contains("语义化版本"), "实际：{error}");
        assert!(error.contains("hello"), "实际：{error}");
        // 片段超长时截断
        let error = version_parse_error(&"x".repeat(400), "");
        assert!(error.chars().count() < 400, "实际：{error}");
    }

    #[test]
    fn service_manager_initializes_session_slots() {
        let manager = ServiceManager::new();
        assert!(manager.session_cancel.lock().unwrap().is_none());
        assert!(manager.session_credentials.lock().unwrap().is_none());
        assert!(manager.authenticated_url.lock().unwrap().is_none());
    }
}
