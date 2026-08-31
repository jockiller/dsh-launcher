use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(not(windows))]
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter};

const MARKER_NAME: &str = ".dsh-launcher-managed.json";
const NODE_INDEX_URL: &str = "https://nodejs.org/dist/index.json";
const NODE_MIRROR_INDEX_URL: &str = "https://npmmirror.com/mirrors/node/index.json";
const NPM_LATEST_URL: &str = "https://registry.npmjs.org/@deepseek-ai%2Fdsh/latest";
const NPM_MIRROR_LATEST_URL: &str = "https://registry.npmmirror.com/@deepseek-ai%2Fdsh/latest";
const DSH_RELEASE_API_URL: &str = "https://api.github.com/repos/deepseek-ai/deepseek-harness/releases/tags/";
const MANAGED_SCHEMA: u8 = 1;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const NPM_INSTALL_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const NPM_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const NPM_OFFICIAL_REGISTRY: &str = "https://registry.npmjs.org";
const NPM_MIRROR_REGISTRY: &str = "https://registry.npmmirror.com";
const NODE_OFFICIAL_DIST: &str = "https://nodejs.org/dist";
const NODE_MIRROR_DIST: &str = "https://npmmirror.com/mirrors/node";
static OPERATION_ACTIVE: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedStatus {
    pub managed_root: String,
    pub dsh_path: String,
    pub node_version: String,
    pub dsh_version: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManagedProgress {
    phase: &'static str,
    message: String,
    percent: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Marker {
    schema: u8,
    node_version: String,
    dsh_version: String,
    #[serde(default)]
    use_mirror: bool,
}

#[derive(Debug, Deserialize)]
struct NodeRelease {
    version: String,
    lts: serde_json::Value,
    files: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct NpmLatest {
    version: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DshVersionInfo {
    pub current_version: String,
    pub current_notes: Option<String>,
    pub latest_version: String,
    pub latest_notes: Option<String>,
    pub update_available: bool,
}

#[derive(Debug, Deserialize)]
struct ReleaseNotes {
    body: Option<String>,
}

struct OperationGuard;

impl OperationGuard {
    fn acquire() -> Result<Self, String> {
        OPERATION_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self)
            .map_err(|_| "已有安装或升级任务正在进行".to_string())
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        OPERATION_ACTIVE.store(false, Ordering::Release);
    }
}

pub fn install_managed(
    app: AppHandle,
    root: PathBuf,
    use_mirror: bool,
) -> Result<ManagedStatus, String> {
    let _guard = OperationGuard::acquire()?;
    validate_install_root(&root)?;
    emit_progress(&app, "metadata", "正在获取最新 Node LTS...", Some(5));
    let release = fetch_node_release(use_mirror)?;
    let archive_name = node_archive_name(&release.version);
    let download_dist = if use_mirror {
        NODE_MIRROR_DIST
    } else {
        NODE_OFFICIAL_DIST
    };
    let base_url = format!("{download_dist}/{}/", release.version);
    let checksum_base_url = format!("{NODE_OFFICIAL_DIST}/{}/", release.version);
    let staging = root.join(".dsh-launcher-staging");
    let archive = staging.join(&archive_name);
    let result = (|| {
        fs::create_dir_all(&staging).map_err(|e| format!("创建安装临时目录失败：{e}"))?;
        emit_progress(&app, "download", "正在下载 Node LTS...", Some(15));
        crate::service::emit_log(
            &app,
            "installer",
            "info",
            if use_mirror {
                "Node 使用国内镜像下载，SHA-256 校验清单仍来自 Node 官方"
            } else {
                "Node 使用官方源下载"
            },
        );
        download_to(&app, &(base_url + &archive_name), &archive)?;
        let checksums = fetch_text(&(checksum_base_url + "SHASUMS256.txt"))?;
        emit_progress(&app, "verify", "正在校验 Node 下载文件...", Some(45));
        verify_sha256(&archive, expected_checksum(&checksums, &archive_name)?)?;
        emit_progress(&app, "extract", "正在解压 Node...", Some(55));
        let extracted = staging.join("extracted");
        fs::create_dir_all(&extracted).map_err(|e| format!("创建解压目录失败：{e}"))?;
        extract_archive(&archive, &extracted)?;
        let extracted_node = extracted.join(archive_top_dir(&archive_name));
        if !node_executable(&extracted_node).is_file() {
            return Err("Node 压缩包缺少可执行文件".into());
        }
        let staged_dsh = staging.join("dsh-runtime");
        emit_progress(&app, "install", "正在安装 DSH...", Some(70));
        let expected_dsh_version = fetch_latest_dsh(use_mirror)?;
        npm_install(
            &app,
            &extracted_node,
            &staged_dsh,
            use_mirror,
            &expected_dsh_version,
        )?;
        let final_node = root.join("node");
        let final_runtime = runtime_dir(&root);
        fs::rename(&extracted_node, &final_node).map_err(|e| format!("启用 Node 失败：{e}"))?;
        if let Err(error) = fs::rename(&staged_dsh, &final_runtime) {
            let _ = fs::rename(&final_node, &extracted_node);
            return Err(format!("启用 DSH 失败：{error}"));
        }
        let wrapper = create_wrapper(&root)?;
        let dsh_version = crate::service::dsh_version(&wrapper)?;
        if dsh_version != expected_dsh_version {
            return Err(format!(
                "DSH 版本校验失败：期望 {expected_dsh_version}，实际 {dsh_version}"
            ));
        }
        let marker = Marker {
            schema: MANAGED_SCHEMA,
            node_version: release.version.trim_start_matches('v').to_string(),
            dsh_version: dsh_version.clone(),
            use_mirror,
        };
        write_marker(&root, &marker)?;
        emit_progress(&app, "complete", "Node 与 DSH 安装完成", Some(100));
        Ok(status_from(&root, marker))
    })();
    let _ = fs::remove_dir_all(&staging);
    if result.is_err() && !root.join(MARKER_NAME).is_file() {
        let _ = fs::remove_dir_all(root.join("node"));
        let _ = fs::remove_dir_all(runtime_dir(&root));
        let _ = fs::remove_file(managed_dsh_path(&root));
    }
    result
}

pub fn upgrade_managed(app: AppHandle, root: PathBuf) -> Result<ManagedStatus, String> {
    let _guard = OperationGuard::acquire()?;
    let mut marker = read_marker(&root)?;
    let staging = root.join(".dsh-upgrade-staging");
    let backup = root.join(".dsh-upgrade-backup");
    let failed = root.join(".dsh-upgrade-failed");
    let temporary_wrapper = root.join(if cfg!(windows) {
        ".dsh-upgrade-check.cmd"
    } else {
        ".dsh-upgrade-check"
    });
    let _ = fs::remove_dir_all(&staging);
    let _ = fs::remove_dir_all(&backup);
    let _ = fs::remove_dir_all(&failed);
    emit_progress(&app, "upgrade", "服务已停止，正在升级 DSH...", Some(20));
    let result = (|| {
        let expected_dsh_version = fetch_latest_dsh(marker.use_mirror)?;
        npm_install(
            &app,
            &root.join("node"),
            &staging,
            marker.use_mirror,
            &expected_dsh_version,
        )?;
        create_wrapper_for(&root, &staging, &temporary_wrapper)?;
        let next_version = crate::service::dsh_version(&temporary_wrapper)?;
        if next_version != expected_dsh_version {
            return Err(format!(
                "DSH 版本校验失败：期望 {expected_dsh_version}，实际 {next_version}"
            ));
        }
        // 升级只替换 npm 前缀目录（dsh-runtime），名为 dsh 的入口包装脚本保持不变。
        let current = runtime_dir(&root);
        fs::rename(&current, &backup).map_err(|error| {
            format!("切换 DSH 版本失败：{error}。如遇文件占用，请停止服务后重试")
        })?;
        if let Err(error) = fs::rename(&staging, &current) {
            let _ = fs::rename(&backup, &current);
            return Err(format!("启用新版 DSH 失败：{error}"));
        }
        marker.dsh_version = next_version;
        if let Err(error) = write_marker(&root, &marker) {
            fs::rename(&current, &failed).map_err(|move_error| {
                format!(
                    "{error}；回滚准备失败：{move_error}。旧版仍保留在 {}",
                    backup.display()
                )
            })?;
            if let Err(restore_error) = fs::rename(&backup, &current) {
                let _ = fs::rename(&failed, &current);
                return Err(format!(
                    "{error}；恢复旧版失败：{restore_error}。旧版仍保留在 {}",
                    backup.display()
                ));
            }
            let _ = fs::remove_dir_all(&failed);
            return Err(error);
        }
        let _ = fs::remove_dir_all(&backup);
        emit_progress(
            &app,
            "complete",
            "DSH 升级完成，将在下次启动时生效",
            Some(100),
        );
        Ok(status_from(&root, marker))
    })();
    let _ = fs::remove_file(temporary_wrapper);
    let _ = fs::remove_dir_all(staging);
    result
}

pub fn managed_status(root: &Path) -> Result<ManagedStatus, String> {
    let marker = read_marker(root)?;
    let wrapper = managed_dsh_path(root);
    if !node_executable(&root.join("node")).is_file()
        || !runtime_dir(root).is_dir()
        || !wrapper.is_file()
    {
        return Err("托管运行环境文件不完整".into());
    }
    Ok(status_from(root, marker))
}

/// 检测 DSH 最新版本；root 给定时读取托管标记并沿用安装时选择的 npm 镜像，
/// 标记缺失/损坏时回退官方源。latestDsh 判空后由前端隐藏"已是最新"的升级按钮。
fn marker_mirror(root: Option<&str>) -> bool {
    root.and_then(|path| read_marker(Path::new(path)).ok())
        .map(|marker| marker.use_mirror)
        .unwrap_or(false)
}

pub fn check_latest_dsh(root: Option<&str>) -> Result<String, String> {
    fetch_latest_dsh(marker_mirror(root))
}

/// 获取当前 DSH 的发布说明并检测最新版本。网络请求和版本比较均在调用方的
/// 阻塞线程中执行；发布说明获取失败不影响版本检测结果。
pub fn check_dsh_version(current_version: &str) -> Result<DshVersionInfo, String> {
    let current = semver::Version::parse(current_version.trim())
        .map_err(|error| format!("当前 DSH 版本格式无效：{error}"))?;
    let latest = semver::Version::parse(fetch_latest_dsh(false)?.trim())
        .map_err(|error| format!("最新 DSH 版本格式无效：{error}"))?;
    let update_available = latest > current;
    let current_version = current.to_string();
    let latest_version = latest.to_string();
    let current_notes = fetch_release_notes(&current_version);
    let latest_notes = if update_available {
        fetch_release_notes(&latest_version)
    } else {
        current_notes.clone()
    };
    Ok(DshVersionInfo {
        current_version,
        current_notes,
        latest_version,
        latest_notes,
        update_available,
    })
}

fn fetch_latest_dsh(use_mirror: bool) -> Result<String, String> {
    let url = if use_mirror {
        NPM_MIRROR_LATEST_URL
    } else {
        NPM_LATEST_URL
    };
    let payload = fetch_text(url)?;
    let latest: NpmLatest =
        serde_json::from_str(&payload).map_err(|e| format!("解析 DSH 最新版本失败：{e}"))?;
    semver::Version::parse(latest.version.trim())
        .map_err(|e| format!("DSH 最新版本格式无效：{e}"))?;
    Ok(latest.version)
}

fn fetch_release_notes(version: &str) -> Option<String> {
    let url = format!("{DSH_RELEASE_API_URL}v{version}");
    let payload = ureq::AgentBuilder::new()
        .timeout(HTTP_TIMEOUT)
        .build()
        .get(&url)
        .set("User-Agent", "dsh-launcher")
        .set("Accept", "application/vnd.github+json")
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let release: ReleaseNotes = serde_json::from_str(&payload).ok()?;
    release
        .body
        .map(|body| body.trim().to_string())
        .filter(|body| !body.is_empty())
}

fn fetch_node_release(use_mirror: bool) -> Result<NodeRelease, String> {
    let url = if use_mirror {
        NODE_MIRROR_INDEX_URL
    } else {
        NODE_INDEX_URL
    };
    let payload = fetch_text(url)?;
    let releases: Vec<NodeRelease> =
        serde_json::from_str(&payload).map_err(|e| format!("解析 Node 版本列表失败：{e}"))?;
    let file_key = node_file_key();
    releases
        .into_iter()
        .find(|release| {
            release.lts != serde_json::Value::Bool(false)
                && release.files.iter().any(|file| file == file_key)
        })
        .ok_or_else(|| format!("Node 官方未提供当前平台架构的 LTS 包：{file_key}"))
}

fn fetch_text(url: &str) -> Result<String, String> {
    ureq::AgentBuilder::new()
        .timeout(HTTP_TIMEOUT)
        .build()
        .get(url)
        .set("User-Agent", "dsh-launcher")
        .call()
        .map_err(|e| format!("网络请求失败：{e}"))?
        .into_string()
        .map_err(|e| format!("读取网络响应失败：{e}"))
}

fn download_to(app: &AppHandle, url: &str, path: &Path) -> Result<(), String> {
    let response = ureq::AgentBuilder::new()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .get(url)
        .set("User-Agent", "dsh-launcher")
        .call()
        .map_err(|e| format!("下载 Node 失败：{e}"))?;
    let total = response
        .header("Content-Length")
        .and_then(|value| value.parse::<u64>().ok());
    let mut reader = response.into_reader();
    let mut file = File::create(path).map_err(|e| format!("创建下载文件失败：{e}"))?;
    let mut downloaded = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    let mut last_update = Instant::now();
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|e| format!("读取 Node 下载内容失败：{e}"))?;
        if count == 0 {
            break;
        }
        io::Write::write_all(&mut file, &buffer[..count])
            .map_err(|e| format!("保存 Node 下载文件失败：{e}"))?;
        downloaded += count as u64;
        if last_update.elapsed() >= Duration::from_secs(1) {
            let percent = total.map(|size| {
                let download_percent = downloaded.saturating_mul(27) / size.max(1);
                (15 + download_percent.min(27)) as u8
            });
            emit_progress(app, "download", "正在下载 Node LTS...", percent);
            last_update = Instant::now();
        }
    }
    Ok(())
}

fn expected_checksum<'a>(manifest: &'a str, filename: &str) -> Result<&'a str, String> {
    manifest
        .lines()
        .filter_map(|line| line.split_once(char::is_whitespace))
        .find_map(|(hash, name)| (name.trim_start_matches([' ', '*']) == filename).then_some(hash))
        .ok_or_else(|| "Node 校验清单中没有目标文件".into())
}

fn verify_sha256(path: &Path, expected: &str) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| format!("读取 Node 下载文件失败：{e}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("计算 Node 校验值失败：{e}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(())
    } else {
        Err("Node 下载文件 SHA-256 校验失败".into())
    }
}

#[cfg(not(windows))]
fn extract_archive(archive: &Path, destination: &Path) -> Result<(), String> {
    let decoder =
        GzDecoder::new(File::open(archive).map_err(|e| format!("打开 Node 压缩包失败：{e}"))?);
    let mut tar = tar::Archive::new(decoder);
    for entry in tar
        .entries()
        .map_err(|e| format!("读取 Node 压缩包失败：{e}"))?
    {
        let mut entry = entry.map_err(|e| format!("读取 Node 压缩项失败：{e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("读取 Node 压缩路径失败：{e}"))?;
        validate_relative_path(&path)?;
        entry
            .unpack_in(destination)
            .map_err(|e| format!("解压 Node 失败：{e}"))?;
    }
    Ok(())
}

#[cfg(windows)]
fn extract_archive(archive: &Path, destination: &Path) -> Result<(), String> {
    let file = File::open(archive).map_err(|e| format!("打开 Node 压缩包失败：{e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("读取 Node zip 失败：{e}"))?;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|e| format!("读取 Node zip 项失败：{e}"))?;
        let path = entry
            .enclosed_name()
            .ok_or_else(|| "Node zip 包含不安全路径".to_string())?;
        validate_relative_path(&path)?;
        let output = destination.join(path);
        if entry.is_dir() {
            fs::create_dir_all(&output).map_err(|e| format!("创建 Node 解压目录失败：{e}"))?;
        } else {
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("创建 Node 解压目录失败：{e}"))?;
            }
            let mut target =
                File::create(output).map_err(|e| format!("创建 Node 解压文件失败：{e}"))?;
            io::copy(&mut entry, &mut target).map_err(|e| format!("解压 Node 文件失败：{e}"))?;
        }
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<(), String> {
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("Node 压缩包包含不安全路径".into());
    }
    Ok(())
}

fn npm_install(
    app: &AppHandle,
    node_root: &Path,
    prefix: &Path,
    use_mirror: bool,
    dsh_version: &str,
) -> Result<(), String> {
    fs::create_dir_all(prefix).map_err(|e| format!("创建 DSH 安装目录失败：{e}"))?;
    #[cfg(not(windows))]
    fs::create_dir_all(prefix.join("lib"))
        .map_err(|e| format!("创建 DSH 全局安装目录失败：{e}"))?;
    let cache = prefix.join(".npm-cache");
    let registry = if use_mirror {
        NPM_MIRROR_REGISTRY
    } else {
        NPM_OFFICIAL_REGISTRY
    };
    crate::service::emit_log(
        app,
        "installer",
        "info",
        &format!("正在使用 npm 源安装 DSH：{registry}"),
    );
    let mut command = Command::new(node_executable(node_root));
    command
        .arg(npm_cli(node_root))
        .args([
            "install",
            "--global",
            "--no-audit",
            "--no-fund",
            "--no-progress",
            "--foreground-scripts",
            "--package-lock=false",
            "--strict-ssl",
            "--fetch-timeout=60000",
            "--fetch-retries=2",
            "--registry",
        ])
        .arg(registry)
        .arg("--cache")
        .arg(&cache)
        .arg("--prefix")
        .arg(prefix)
        .arg(format!("@deepseek-ai/dsh@{dsh_version}"))
        .current_dir(prefix)
        .env_remove("NODE_OPTIONS")
        .env(
            "PATH",
            script_path(node_root, std::env::var_os("PATH").as_deref()),
        )
        .env("CI", "true")
        .env("npm_config_yes", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_managed_child(&mut command);
    let mut child = command
        .spawn()
        .map_err(|e| format!("执行 npm 安装失败：{e}"))?;
    if let Some(pipe) = child.stdout.take() {
        pipe_managed_logs(app.clone(), "npm-out", "info", pipe);
    }
    if let Some(pipe) = child.stderr.take() {
        pipe_managed_logs(app.clone(), "npm-err", "error", pipe);
    }

    let started = Instant::now();
    let mut last_heartbeat = started;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("检查 npm 安装状态失败：{e}"))?
        {
            break status;
        }
        if started.elapsed() >= NPM_INSTALL_TIMEOUT {
            terminate_managed_child(child.id());
            let _ = child.wait();
            return Err("npm 安装 DSH 超过 20 分钟，已终止".into());
        }
        if last_heartbeat.elapsed() >= NPM_HEARTBEAT_INTERVAL {
            crate::service::emit_log(app, "installer", "info", "npm 仍在下载并安装 DSH...");
            last_heartbeat = Instant::now();
        }
        thread::sleep(Duration::from_millis(250));
    };
    if !status.success() {
        return Err(format!("npm 退出状态：{status}；详细信息见日志区"));
    }
    normalize_global_install(prefix)?;
    let _ = fs::remove_dir_all(&cache);
    dsh_entry(prefix)
        .is_file()
        .then_some(())
        .ok_or_else(|| "安装结束但 DSH 入口文件缺失".into())
}

#[cfg(not(windows))]
fn normalize_global_install(prefix: &Path) -> Result<(), String> {
    let global_modules = prefix.join("lib/node_modules");
    let managed_modules = prefix.join("node_modules");
    fs::rename(&global_modules, &managed_modules)
        .map_err(|e| format!("整理 DSH 安装目录失败：{e}"))?;
    let _ = fs::remove_dir_all(prefix.join("lib"));
    let _ = fs::remove_dir_all(prefix.join("bin"));
    Ok(())
}

#[cfg(windows)]
fn normalize_global_install(_prefix: &Path) -> Result<(), String> {
    Ok(())
}

fn pipe_managed_logs<R>(app: AppHandle, source: &'static str, level: &'static str, reader: R)
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            crate::service::emit_log(&app, source, level, &line);
        }
    });
}

#[cfg(unix)]
fn configure_managed_child(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
fn configure_managed_child(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
}

#[cfg(unix)]
fn terminate_managed_child(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(windows)]
fn terminate_managed_child(pid: u32) {
    use std::os::windows::process::CommandExt;
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(0x0800_0000)
        .status();
}

fn validate_install_root(root: &Path) -> Result<(), String> {
    if !root.is_absolute() {
        return Err("安装目录必须是绝对路径".into());
    }
    fs::create_dir_all(root).map_err(|e| format!("创建安装目录失败：{e}"))?;
    let mut entries = fs::read_dir(root).map_err(|e| format!("读取安装目录失败：{e}"))?;
    if entries.next().is_some() {
        return Err("首次安装请选择空目录".into());
    }
    #[cfg(windows)]
    if root.to_string_lossy().contains('%') {
        return Err("Windows 托管安装目录不能包含 % 字符".into());
    }
    #[cfg(windows)]
    if is_windows_network_path(&root.to_string_lossy()) {
        return Err(
            "Windows 托管安装目录不能使用网络共享路径（如 \\\\Mac\\Home 共享目录），请选择本地磁盘目录"
                .into(),
        );
    }
    // PATH 分隔符会让 npm 脚本子进程的 PATH 拼接在 cmd/sh 中再被拆开，
    // 导致托管 Node 解析失败（复现 exit 127），必须在入口拒绝。
    #[cfg(windows)]
    if root.to_string_lossy().contains(';') {
        return Err("Windows 托管安装目录不能包含 ; 字符".into());
    }
    #[cfg(not(windows))]
    if root.to_string_lossy().contains(':') {
        return Err("安装目录不能包含 : 字符".into());
    }
    Ok(())
}

fn read_marker(root: &Path) -> Result<Marker, String> {
    let payload = fs::read_to_string(root.join(MARKER_NAME))
        .map_err(|_| "该目录不是 Launcher 托管的运行环境".to_string())?;
    let marker: Marker =
        serde_json::from_str(&payload).map_err(|e| format!("托管环境标记损坏：{e}"))?;
    if marker.schema != MANAGED_SCHEMA {
        return Err("托管环境标记版本不受支持".into());
    }
    Ok(marker)
}

fn write_marker(root: &Path, marker: &Marker) -> Result<(), String> {
    let temporary = root.join(format!("{MARKER_NAME}.tmp"));
    let target = root.join(MARKER_NAME);
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(marker).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("写入托管环境标记失败：{e}"))?;
    #[cfg(windows)]
    if target.exists() {
        fs::remove_file(&target).map_err(|e| format!("替换旧托管环境标记失败：{e}"))?;
    }
    fs::rename(&temporary, target).map_err(|e| format!("保存托管环境标记失败：{e}"))
}

fn create_wrapper(root: &Path) -> Result<PathBuf, String> {
    let wrapper = managed_dsh_path(root);
    create_wrapper_for(root, &runtime_dir(root), &wrapper)?;
    Ok(wrapper)
}

#[cfg(not(windows))]
fn create_wrapper_for(root: &Path, dsh_prefix: &Path, wrapper: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let node = shell_quote(&node_executable(&root.join("node")).to_string_lossy());
    let script = shell_quote(&dsh_entry(dsh_prefix).to_string_lossy());
    fs::write(wrapper, format!("#!/bin/sh\nexec {node} {script} \"$@\"\n"))
        .map_err(|e| format!("创建 DSH 启动脚本失败：{e}"))?;
    fs::set_permissions(wrapper, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("设置 DSH 启动脚本权限失败：{e}"))
}

#[cfg(windows)]
fn create_wrapper_for(root: &Path, dsh_prefix: &Path, wrapper: &Path) -> Result<(), String> {
    let node = node_executable(&root.join("node"));
    let script = dsh_entry(dsh_prefix);
    fs::write(
        wrapper,
        format!(
            "@echo off\r\n\"{}\" \"{}\" %*\r\n",
            node.display(),
            script.display()
        ),
    )
    .map_err(|e| format!("创建 DSH 启动脚本失败：{e}"))
}

#[cfg(not(windows))]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn status_from(root: &Path, marker: Marker) -> ManagedStatus {
    ManagedStatus {
        managed_root: root.to_string_lossy().into_owned(),
        dsh_path: managed_dsh_path(root).to_string_lossy().into_owned(),
        node_version: marker.node_version,
        dsh_version: marker.dsh_version,
    }
}

fn emit_progress(app: &AppHandle, phase: &'static str, message: &str, percent: Option<u8>) {
    crate::service::emit_log(app, "installer", "info", message);
    let _ = app.emit(
        "managed-progress",
        ManagedProgress {
            phase,
            message: message.into(),
            percent,
        },
    );
}

#[cfg(windows)]
fn node_file_key() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "win-arm64-zip"
    } else {
        "win-x64-zip"
    }
}
#[cfg(target_os = "macos")]
fn node_file_key() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "osx-arm64-tar"
    } else {
        "osx-x64-tar"
    }
}
#[cfg(all(unix, not(target_os = "macos")))]
fn node_file_key() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "linux-arm64"
    } else {
        "linux-x64"
    }
}

fn node_archive_name(version: &str) -> String {
    let platform = if cfg!(windows) {
        "win"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
    format!("node-{version}-{platform}-{arch}.{extension}")
}

fn archive_top_dir(filename: &str) -> String {
    filename
        .trim_end_matches(".tar.gz")
        .trim_end_matches(".zip")
        .to_string()
}

/// 托管安装的目录布局：
/// `<root>/dsh`（Windows 为 `dsh.cmd`）是名为 dsh 的可执行入口包装脚本，
/// `<root>/dsh-runtime` 是 npm 前缀目录（node_modules），
/// `<root>/node` 是托管 Node。入口必须叫 `dsh`，注入 PATH 后 `which dsh` 才能命中。
fn runtime_dir(root: &Path) -> PathBuf {
    root.join("dsh-runtime")
}

/// 托管 DSH 可执行入口：安装根目录下名为 `dsh` 的包装脚本（Windows 需 .cmd 后缀供 cmd 解析）。
fn managed_dsh_path(root: &Path) -> PathBuf {
    root.join(if cfg!(windows) {
        "dsh.cmd"
    } else {
        "dsh"
    })
}

/// 旧版布局的包装脚本名（dsh-managed / dsh-managed.cmd），迁移时改写为兼容转发脚本。
pub fn legacy_managed_dsh_path(root: &Path) -> PathBuf {
    root.join(if cfg!(windows) {
        "dsh-managed.cmd"
    } else {
        "dsh-managed"
    })
}

/// 旧版布局一次性迁移：入口文件从 `dsh-managed` 改名为根目录下的 `dsh`，
/// 原 npm 前缀目录 `root/dsh` 改名为 `root/dsh-runtime`。幂等，可在每次启动时调用。
/// 返回迁移（或校验）后的托管入口完整路径。
pub fn migrate_layout(root: &Path) -> Result<PathBuf, String> {
    let wrapper = managed_dsh_path(root);
    if !root.join(MARKER_NAME).is_file() {
        return Ok(wrapper);
    }
    let runtime = runtime_dir(root);
    let legacy_prefix = root.join("dsh");
    let mut renamed = false;
    if legacy_prefix.is_dir() {
        if runtime.exists() {
            // rename 是原子的，两目录并存只可能是用户手工整理的结果；此时不动任何
            // 文件并显式报错，避免脚本化清理掩盖真实状态。
            return Err(format!(
                "托管目录状态异常：{} 与 {} 同时存在，请手动整理后重试",
                legacy_prefix.display(),
                runtime.display()
            ));
        }
        fs::rename(&legacy_prefix, &runtime)
            .map_err(|e| format!("迁移托管目录布局失败：{e}"))?;
        renamed = true;
    }
    // 入口创建必须先于任何收尾动作：创建失败时回滚 rename，保证迁移要么完成、
    // 要么完整回到旧布局，绝不出现两头都不可用的中间态。
    if dsh_entry(&runtime).is_file()
        && !wrapper.is_file()
        && let Err(error) = create_wrapper(root)
    {
        if renamed && !wrapper.exists() {
            let _ = fs::rename(&runtime, &legacy_prefix);
        }
        return Err(format!("重建 dsh 入口失败：{error}"));
    }
    // 旧包装脚本不删除，改写为指向同一入口的兼容转发（内容与 dsh 一致）：
    // 用户脚本/配置中对 dsh-managed 的既有引用保持可用。
    let legacy_wrapper = legacy_managed_dsh_path(root);
    if legacy_wrapper != wrapper && (renamed || legacy_wrapper.is_file()) {
        let _ = create_wrapper_for(root, &runtime, &legacy_wrapper);
    }
    Ok(wrapper)
}

/// 启动服务进程时需要前置进子进程 PATH 的目录：托管安装下是安装根目录（内有名为
/// dsh 的入口）与托管 Node 的 bin 目录，保证服务内部 `which dsh` / `node` 可解析；
/// 外部 dsh 仅前置其所在目录（多半已在 PATH 中，重复前置无害）。
pub fn service_path_entries(dsh: &Path) -> Vec<PathBuf> {
    let Some(parent) = dsh.parent() else {
        return Vec::new();
    };
    let mut entries = vec![parent.to_path_buf()];
    if parent.join(MARKER_NAME).is_file() {
        entries.push(node_bin_dir(&parent.join("node")));
    }
    entries
}
fn node_executable(node_root: &Path) -> PathBuf {
    node_root.join(if cfg!(windows) {
        "node.exe"
    } else {
        "bin/node"
    })
}
fn node_bin_dir(node_root: &Path) -> PathBuf {
    if cfg!(windows) {
        node_root.to_path_buf()
    } else {
        node_root.join("bin")
    }
}

/// npm 生命周期脚本通过 cmd/sh 执行，其 PATH 不会自动包含正在运行 npm 的 node 目录；
/// Launcher 又不修改系统 PATH，GUI 会话环境里通常没有 node。
/// 这里把托管 Node 的 bin 目录前置拼进子进程 PATH，脚本内的 `node` 才能解析。
fn script_path(node_root: &Path, existing: Option<&OsStr>) -> OsString {
    let node_dir = node_bin_dir(node_root);
    let Some(existing) = existing else {
        return node_dir.into_os_string();
    };
    // split_paths 产出临时 PathBuf，join_paths 需要 AsRef<OsStr> 项：
    // 统一收拢为 OsString 后再借用，避免闭包内借用局部值。
    let mut entries: Vec<OsString> = vec![node_dir.into_os_string()];
    entries.extend(std::env::split_paths(existing).map(|entry| entry.into_os_string()));
    match std::env::join_paths(entries.iter().map(|entry| entry.as_os_str())) {
        Ok(joined) => joined,
        Err(_) => {
            // join_paths 只在条目内含平台分隔符时失败；直接拼接兜底，托管 Node 仍保持在最前。
            let delimiter = if cfg!(windows) { ";" } else { ":" };
            let mut joined = node_bin_dir(node_root).into_os_string();
            joined.push(delimiter);
            joined.push(existing);
            joined
        }
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn is_windows_network_path(text: &str) -> bool {
    text.replace('/', "\\").starts_with("\\\\")
}
fn npm_cli(node_root: &Path) -> PathBuf {
    if cfg!(windows) {
        node_root.join("node_modules/npm/bin/npm-cli.js")
    } else {
        node_root.join("lib/node_modules/npm/bin/npm-cli.js")
    }
}
fn dsh_entry(prefix: &Path) -> PathBuf {
    prefix.join("node_modules/@deepseek-ai/dsh/lib/bin.js")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_manifest_requires_exact_filename() {
        let manifest = "abc  node-v22.tar.gz\ndef *node-v24.tar.gz\n";
        assert_eq!(
            expected_checksum(manifest, "node-v24.tar.gz").unwrap(),
            "def"
        );
        assert!(expected_checksum(manifest, "node-v2.tar.gz").is_err());
    }

    #[test]
    fn archive_paths_reject_traversal() {
        assert!(validate_relative_path(Path::new("node/bin/node")).is_ok());
        assert!(validate_relative_path(Path::new("../node")).is_err());
        assert!(validate_relative_path(Path::new("/node")).is_err());
    }

    #[test]
    fn archive_name_has_expected_shape() {
        let name = node_archive_name("v22.19.0");
        assert!(name.starts_with("node-v22.19.0-"));
        assert_eq!(
            archive_top_dir(&name),
            name.trim_end_matches(".tar.gz").trim_end_matches(".zip")
        );
    }

    #[test]
    fn old_marker_defaults_to_official_source() {
        let marker: Marker =
            serde_json::from_str(r#"{"schema":1,"nodeVersion":"22.0.0","dshVersion":"0.1.0"}"#)
                .unwrap();
        assert!(!marker.use_mirror);
    }

    #[test]
    fn marker_rejects_other_schema() {
        let marker = Marker {
            schema: 2,
            node_version: "22.0.0".into(),
            dsh_version: "0.1.0".into(),
            use_mirror: false,
        };
        assert_ne!(marker.schema, MANAGED_SCHEMA);
    }

    #[test]
    fn marker_mirror_follows_saved_marker() {
        let dir =
            std::env::temp_dir().join(format!("dsh-launcher-mirror-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        write_marker(
            &dir,
            &Marker {
                schema: MANAGED_SCHEMA,
                node_version: "22.0.0".into(),
                dsh_version: "0.1.0".into(),
                use_mirror: true,
            },
        )
        .unwrap();
        assert!(marker_mirror(dir.to_str()));
        // 标记缺失或无法读取时回退官方源
        assert!(!marker_mirror(dir.join("missing").to_str()));
        assert!(!marker_mirror(None));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn script_path_puts_managed_node_bin_first() {
        let node_root = Path::new("/node-root");
        let existing = if cfg!(windows) {
            r"C:\Windows;C:\Tools"
        } else {
            "/usr/bin:/usr/local/bin"
        };
        let joined = script_path(node_root, Some(OsStr::new(existing)));
        let entries: Vec<PathBuf> = std::env::split_paths(&joined).collect();
        assert_eq!(entries.first(), Some(&node_bin_dir(node_root)));
        let original: Vec<PathBuf> = std::env::split_paths(OsStr::new(existing)).collect();
        assert_eq!(&entries[1..], original.as_slice());
    }

    #[test]
    fn script_path_without_inherited_path_keeps_only_node() {
        let node_root = Path::new("/node-root");
        let joined = script_path(node_root, None);
        assert_eq!(
            std::env::split_paths(&joined).collect::<Vec<PathBuf>>(),
            vec![node_bin_dir(node_root)]
        );
    }

    #[test]
    fn windows_network_paths_are_detected() {
        assert!(is_windows_network_path(r"\\Mac\Home\Downloads\td"));
        assert!(is_windows_network_path("//Mac/Home/Downloads/td"));
        assert!(!is_windows_network_path("C:\\Users\\me\\dsh"));
        assert!(!is_windows_network_path("C:/Users/me/dsh"));
    }

    #[test]
    fn script_path_keeps_managed_node_first_even_with_separator_entries() {
        // Unix：join_paths 对含 ':' 的条目返回 Err，退化为直接拼接，托管节点仍在最前且无引号。
        // Windows：join_paths 不会报错，而是给含 ';' 的条目加引号（cmd 对带引号 PATH 解析
        // 不可靠，因此 validate_install_root 已直接拒绝该类目录）；这里只锁住“托管节点目录排最前”。
        let node_root = if cfg!(windows) {
            Path::new(r"C:\no;de")
        } else {
            Path::new("/node:root")
        };
        let existing = if cfg!(windows) { r"C:\bin" } else { "/usr/bin" };
        let joined = script_path(node_root, Some(OsStr::new(existing)));
        let text = joined.to_string_lossy();
        if cfg!(windows) {
            assert!(text.starts_with(&format!(
                r#""{}""#,
                node_bin_dir(node_root).to_string_lossy()
            )));
        } else {
            assert!(text.starts_with(&node_bin_dir(node_root).to_string_lossy().to_string()));
            // Unix 兜底拼接不会引入引号
            assert!(!text.contains('"'));
        }
        assert!(text.contains(existing));
    }
}
