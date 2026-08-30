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
const NPM_LATEST_URL: &str = "https://registry.npmjs.org/@deepseek-ai%2Fdsh/latest";
const MANAGED_SCHEMA: u8 = 1;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const NPM_INSTALL_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const NPM_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
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

pub fn install_managed(app: AppHandle, root: PathBuf) -> Result<ManagedStatus, String> {
    let _guard = OperationGuard::acquire()?;
    validate_install_root(&root)?;
    emit_progress(&app, "metadata", "正在获取最新 Node LTS...", Some(5));
    let release = fetch_node_release()?;
    let archive_name = node_archive_name(&release.version);
    let base_url = format!("https://nodejs.org/dist/{}/", release.version);
    let staging = root.join(".dsh-launcher-staging");
    let archive = staging.join(&archive_name);
    let result = (|| {
        fs::create_dir_all(&staging).map_err(|e| format!("创建安装临时目录失败：{e}"))?;
        emit_progress(&app, "download", "正在下载 Node LTS...", Some(15));
        download_to(&(base_url.clone() + &archive_name), &archive)?;
        let checksums = fetch_text(&(base_url + "SHASUMS256.txt"))?;
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
        let staged_dsh = staging.join("dsh");
        emit_progress(&app, "install", "正在安装 DSH...", Some(70));
        npm_install(&app, &extracted_node, &staged_dsh)?;
        let final_node = root.join("node");
        let final_dsh = root.join("dsh");
        fs::rename(&extracted_node, &final_node).map_err(|e| format!("启用 Node 失败：{e}"))?;
        if let Err(error) = fs::rename(&staged_dsh, &final_dsh) {
            let _ = fs::rename(&final_node, &extracted_node);
            return Err(format!("启用 DSH 失败：{error}"));
        }
        let wrapper = create_wrapper(&root)?;
        let dsh_version = crate::service::dsh_version(&wrapper)?;
        let marker = Marker {
            schema: MANAGED_SCHEMA,
            node_version: release.version.trim_start_matches('v').to_string(),
            dsh_version: dsh_version.clone(),
        };
        write_marker(&root, &marker)?;
        emit_progress(&app, "complete", "Node 与 DSH 安装完成", Some(100));
        Ok(status_from(&root, marker))
    })();
    let _ = fs::remove_dir_all(&staging);
    if result.is_err() && !root.join(MARKER_NAME).is_file() {
        let _ = fs::remove_dir_all(root.join("node"));
        let _ = fs::remove_dir_all(root.join("dsh"));
        let _ = fs::remove_file(managed_dsh_path(&root));
    }
    result
}

pub fn upgrade_managed(app: AppHandle, root: PathBuf) -> Result<ManagedStatus, String> {
    let _guard = OperationGuard::acquire()?;
    let mut marker = read_marker(&root)?;
    let staging = root.join(".dsh-upgrade-staging");
    let backup = root.join(".dsh-upgrade-backup");
    let temporary_wrapper = root.join(if cfg!(windows) {
        ".dsh-upgrade-check.cmd"
    } else {
        ".dsh-upgrade-check"
    });
    let _ = fs::remove_dir_all(&staging);
    let _ = fs::remove_dir_all(&backup);
    emit_progress(&app, "upgrade", "服务已停止，正在升级 DSH...", Some(20));
    let result = (|| {
        npm_install(&app, &root.join("node"), &staging)?;
        create_wrapper_for(&root, &staging, &temporary_wrapper)?;
        let next_version = crate::service::dsh_version(&temporary_wrapper)?;
        let current = root.join("dsh");
        fs::rename(&current, &backup).map_err(|error| {
            format!("切换 DSH 版本失败：{error}。如遇文件占用，请停止服务后重试")
        })?;
        if let Err(error) = fs::rename(&staging, &current) {
            let _ = fs::rename(&backup, &current);
            return Err(format!("启用新版 DSH 失败：{error}"));
        }
        marker.dsh_version = next_version;
        if let Err(error) = write_marker(&root, &marker) {
            let _ = fs::remove_dir_all(&current);
            let _ = fs::rename(&backup, &current);
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
    if !node_executable(&root.join("node")).is_file() || !wrapper.is_file() {
        return Err("托管运行环境文件不完整".into());
    }
    Ok(status_from(root, marker))
}

pub fn check_latest_dsh() -> Result<String, String> {
    let payload = fetch_text(NPM_LATEST_URL)?;
    let latest: NpmLatest =
        serde_json::from_str(&payload).map_err(|e| format!("解析 DSH 最新版本失败：{e}"))?;
    semver::Version::parse(latest.version.trim())
        .map_err(|e| format!("DSH 最新版本格式无效：{e}"))?;
    Ok(latest.version)
}

fn fetch_node_release() -> Result<NodeRelease, String> {
    let payload = fetch_text(NODE_INDEX_URL)?;
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

fn download_to(url: &str, path: &Path) -> Result<(), String> {
    let response = ureq::AgentBuilder::new()
        .timeout(HTTP_TIMEOUT)
        .build()
        .get(url)
        .set("User-Agent", "dsh-launcher")
        .call()
        .map_err(|e| format!("下载 Node 失败：{e}"))?;
    let mut reader = response.into_reader();
    let mut file = File::create(path).map_err(|e| format!("创建下载文件失败：{e}"))?;
    io::copy(&mut reader, &mut file).map_err(|e| format!("保存 Node 下载文件失败：{e}"))?;
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

fn npm_install(app: &AppHandle, node_root: &Path, prefix: &Path) -> Result<(), String> {
    fs::create_dir_all(prefix).map_err(|e| format!("创建 DSH 安装目录失败：{e}"))?;
    let mut command = Command::new(node_executable(node_root));
    command
        .arg(npm_cli(node_root))
        .args([
            "install",
            "--no-audit",
            "--no-fund",
            "--no-progress",
            "--foreground-scripts",
            "--fetch-timeout=60000",
            "--fetch-retries=2",
            "--prefix",
        ])
        .arg(prefix)
        .arg("@deepseek-ai/dsh@latest")
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
            return Err(
                "npm 安装 DSH 超过 20 分钟，已终止。请检查 npm 网络或代理设置后重试".into(),
            );
        }
        if last_heartbeat.elapsed() >= NPM_HEARTBEAT_INTERVAL {
            crate::service::emit_log(app, "installer", "info", "npm 仍在下载并安装 DSH...");
            last_heartbeat = Instant::now();
        }
        thread::sleep(Duration::from_millis(250));
    };
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "npm 安装 DSH 失败，退出状态：{status}；详细信息见日志区"
        ))
    }
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
    create_wrapper_for(root, &root.join("dsh"), &wrapper)?;
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

fn managed_dsh_path(root: &Path) -> PathBuf {
    root.join(if cfg!(windows) {
        "dsh-managed.cmd"
    } else {
        "dsh-managed"
    })
}
fn node_executable(node_root: &Path) -> PathBuf {
    node_root.join(if cfg!(windows) {
        "node.exe"
    } else {
        "bin/node"
    })
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
    fn marker_rejects_other_schema() {
        let marker = Marker {
            schema: 2,
            node_version: "22.0.0".into(),
            dsh_version: "0.1.0".into(),
        };
        assert_ne!(marker.schema, MANAGED_SCHEMA);
    }
}
