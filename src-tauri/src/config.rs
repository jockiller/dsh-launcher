use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LauncherConfig {
    pub dsh_path: String,
    pub profile: String,
    pub host: String,
    pub port: u16,
    pub launch_action: LaunchAction,
    pub custom_args: String,
    pub auto_start: bool,
    pub auto_scroll_logs: bool,
    pub managed_runtime_dir: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LaunchAction {
    None,
    DefaultBrowser,
    #[default]
    EmbeddedWebview,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebviewWindowState {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl WebviewWindowState {
    pub fn load() -> Option<Self> {
        let text = fs::read_to_string(webview_window_path()?).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self) -> Result<()> {
        let path = webview_window_path().context("无法定位用户配置目录")?;
        let parent = path.parent().context("窗口状态路径无父目录")?;
        fs::create_dir_all(parent).context("创建配置目录失败")?;
        fs::write(path, serde_json::to_vec_pretty(self)?).context("保存 Web 窗口状态失败")
    }
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            dsh_path: String::new(),
            profile: "web".into(),
            host: "127.0.0.1".into(),
            port: 3080,
            launch_action: LaunchAction::EmbeddedWebview,
            custom_args: "--no-open".into(),
            auto_start: false,
            auto_scroll_logs: true,
            managed_runtime_dir: String::new(),
        }
    }
}

impl LauncherConfig {
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path().context("无法定位用户配置目录")?;
        self.write_atomic(&path)
    }

    /// 同目录临时文件 + 重命名替换，保证任意时刻磁盘上都有一份完整配置。
    fn write_atomic(&self, path: &Path) -> Result<()> {
        let parent = path.parent().context("配置路径无父目录")?;
        fs::create_dir_all(parent).context("创建配置目录失败")?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(self)?).context("写入临时配置失败")?;
        if let Err(error) = replace_temporary(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(error.context("替换配置文件失败"));
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.profile.trim().is_empty() {
            return Err("Profile 不能为空".into());
        }
        if self.host.trim().is_empty() {
            return Err("主机不能为空".into());
        }
        if self.port == 0 {
            return Err("端口必须在 1 到 65535 之间".into());
        }
        Ok(())
    }
}

/// 用临时文件替换目标文件。
///
/// Unix 上 `fs::rename` 本身支持原子覆盖已存在目标；Windows 不允许 rename 覆盖
/// 已存在文件（os error 17），第二次保存会因此失败，必须先移除旧文件再重试。
fn replace_temporary(temporary: &Path, target: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        if fs::rename(temporary, target).is_ok() {
            return Ok(());
        }
        if target.exists() {
            fs::remove_file(target).context("移除旧配置文件失败")?;
        }
        fs::rename(temporary, target)?;
    }
    #[cfg(not(windows))]
    fs::rename(temporary, target)?;
    Ok(())
}

/// 平台抽象，便于在任意主机上对三套目录规则做单元测试。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlatformTarget {
    Windows,
    MacOS,
    Unix,
}

pub(crate) type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

pub(crate) fn current_platform() -> PlatformTarget {
    if cfg!(target_os = "windows") {
        PlatformTarget::Windows
    } else if cfg!(target_os = "macos") {
        PlatformTarget::MacOS
    } else {
        PlatformTarget::Unix
    }
}

fn home_var_for(platform: PlatformTarget) -> &'static str {
    if matches!(platform, PlatformTarget::Windows) {
        "USERPROFILE"
    } else {
        "HOME"
    }
}

pub(crate) fn home_dir_for(platform: PlatformTarget, lookup: EnvLookup<'_>) -> Option<PathBuf> {
    lookup(home_var_for(platform))
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
}

/// 读取当前运行平台的用户主目录（Windows 用 USERPROFILE，其余平台用 HOME）。
pub(crate) fn user_home() -> Option<PathBuf> {
    home_dir_for(current_platform(), &|key| std::env::var(key).ok())
}

/// 跨平台配置目录：Windows %APPDATA%\DSH Launcher（缺失时回退
/// USERPROFILE\AppData\Roaming），macOS ~/Library/Application Support/DSH Launcher，
/// Linux/其他遵循 XDG_CONFIG_HOME（仅接受绝对路径）否则回退 ~/.config。
fn platform_config_dir(platform: PlatformTarget, lookup: EnvLookup<'_>) -> Option<PathBuf> {
    let home = home_dir_for(platform, lookup)?;
    Some(match platform {
        PlatformTarget::Windows => {
            let base = lookup("APPDATA")
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData").join("Roaming"));
            base.join(APP_DIR_NAME)
        }
        PlatformTarget::MacOS => home
            .join("Library")
            .join("Application Support")
            .join(APP_DIR_NAME),
        PlatformTarget::Unix => {
            let base = lookup("XDG_CONFIG_HOME")
                .filter(|value| value.starts_with('/'))
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"));
            base.join(APP_DIR_NAME)
        }
    })
}

const APP_DIR_NAME: &str = "DSH Launcher";

fn env_lookup(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn config_dir() -> Option<PathBuf> {
    platform_config_dir(current_platform(), &env_lookup)
}

fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("config.json"))
}

fn webview_window_path() -> Option<PathBuf> {
    Some(config_dir()?.join("webview-window.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn lookup_for<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn defaults_match_launcher_contract() {
        let config = LauncherConfig::default();
        assert_eq!(config.profile, "web");
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 3080);
        assert!(matches!(
            config.launch_action,
            LaunchAction::EmbeddedWebview
        ));
        assert_eq!(
            serde_json::to_value(config.launch_action).unwrap(),
            "embedded_webview"
        );
        assert_eq!(config.custom_args, "--no-open");
        assert!(!config.auto_start);
        assert!(config.managed_runtime_dir.is_empty());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn home_resolution_follows_platform_contract() {
        let windows_lookup =
            |key: &str| (key == "USERPROFILE").then(|| "C:\\Users\\demo".to_string());
        assert_eq!(
            home_dir_for(PlatformTarget::Windows, &windows_lookup),
            Some(PathBuf::from("C:\\Users\\demo"))
        );
        let unix_lookup = |key: &str| (key == "HOME").then(|| "/Users/demo".to_string());
        assert_eq!(
            home_dir_for(PlatformTarget::MacOS, &unix_lookup),
            Some(PathBuf::from("/Users/demo"))
        );
        assert_eq!(
            home_dir_for(PlatformTarget::Unix, &unix_lookup),
            Some(PathBuf::from("/Users/demo"))
        );
        assert_eq!(home_dir_for(PlatformTarget::Unix, &|_: &str| None), None);
        assert_eq!(
            home_dir_for(PlatformTarget::Unix, &|key: &str| (key == "HOME")
                .then(String::new)),
            None
        );
    }

    #[test]
    fn current_platform_matches_build_target() {
        #[cfg(target_os = "macos")]
        assert!(matches!(current_platform(), PlatformTarget::MacOS));
        #[cfg(target_os = "windows")]
        assert!(matches!(current_platform(), PlatformTarget::Windows));
        #[cfg(all(unix, not(target_os = "macos")))]
        assert!(matches!(current_platform(), PlatformTarget::Unix));
    }

    #[test]
    fn config_dir_uses_appdata_on_windows() {
        // 测试可能运行在任意主机上，路径分隔符随平台而变，按“父目录 + 目录名”分段比较。
        let lookup = lookup_for(&[
            ("USERPROFILE", "C:\\Users\\demo"),
            ("APPDATA", "C:\\Users\\demo\\AppData\\Roaming"),
        ]);
        let dir = platform_config_dir(PlatformTarget::Windows, &lookup).unwrap();
        assert_eq!(
            dir.parent(),
            Some(Path::new("C:\\Users\\demo\\AppData\\Roaming"))
        );
        assert_eq!(dir.file_name(), Some(std::ffi::OsStr::new(APP_DIR_NAME)));
        // APPDATA 未设置时回退 USERPROFILE\AppData\Roaming（回退目录由 join 派生，比较需同构构造）
        let fallback = lookup_for(&[("USERPROFILE", "C:\\Users\\demo")]);
        let dir = platform_config_dir(PlatformTarget::Windows, &fallback).unwrap();
        let expected_base = PathBuf::from("C:\\Users\\demo")
            .join("AppData")
            .join("Roaming");
        assert_eq!(dir.parent(), Some(expected_base.as_path()));
        assert_eq!(dir.file_name(), Some(std::ffi::OsStr::new(APP_DIR_NAME)));
    }

    #[test]
    fn config_dir_keeps_macos_application_support() {
        let lookup = lookup_for(&[("HOME", "/Users/demo")]);
        assert_eq!(
            platform_config_dir(PlatformTarget::MacOS, &lookup),
            Some(PathBuf::from(
                "/Users/demo/Library/Application Support/DSH Launcher"
            ))
        );
    }

    #[test]
    fn config_dir_honours_absolute_xdg_only() {
        let absolute = lookup_for(&[
            ("HOME", "/home/demo"),
            ("XDG_CONFIG_HOME", "/etc/dsh-config"),
        ]);
        assert_eq!(
            platform_config_dir(PlatformTarget::Unix, &absolute),
            Some(PathBuf::from("/etc/dsh-config/DSH Launcher"))
        );
        // XDG 规范：相对路径视作无效，回退 ~/.config
        let relative = lookup_for(&[("HOME", "/home/demo"), ("XDG_CONFIG_HOME", "relative/dir")]);
        assert_eq!(
            platform_config_dir(PlatformTarget::Unix, &relative),
            Some(PathBuf::from("/home/demo/.config/DSH Launcher"))
        );
    }

    #[test]
    fn atomic_save_replaces_existing_file() {
        let nanos = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("dsh-launcher-test-{}-{nanos}", std::process::id()));
        let path = root.join("nested").join("config.json");
        let first = LauncherConfig::default();
        first.write_atomic(&path).unwrap();
        let second = LauncherConfig {
            port: 3199,
            ..Default::default()
        };
        // 第二次保存触发对已存在文件的覆盖（Windows 上的 rename 失败路径）。
        second.write_atomic(&path).unwrap();
        let reloaded: LauncherConfig =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reloaded.port, 3199);
        assert!(!path.with_extension("json.tmp").exists());
        let _ = fs::remove_dir_all(&root);
    }
}
