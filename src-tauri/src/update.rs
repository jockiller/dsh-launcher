//! Launcher 自身版本更新检测。
//!
//! 启动时自动向 GitHub 发起一次网络请求；用户点击「检查更新」时可强制重新请求。
//! 请求在阻塞线程池中执行，不占用主线程。任何网络或解析失败都返回 `None`，由前端
//! 按场景静默或提示；没有定时轮询，也没有跨启动缓存。

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::async_runtime::spawn_blocking;

/// GitHub latest release API 地址。
const RELEASE_API_URL: &str = "https://api.github.com/repos/jockiller/dsh-desktop/releases/latest";

/// Release 列表兜底跳转地址；仅当 API 返回的 html_url 未通过校验时使用。
pub const RELEASES_PAGE_URL: &str = "https://github.com/jockiller/dsh-desktop/releases";

/// 允许打开的 URL 前缀：只放行本项目的 GitHub Release 相关页面。
const ALLOWED_URL_PREFIX: &str = "https://github.com/jockiller/dsh-desktop/releases";

/// 网络请求整体超时。
const FETCH_TIMEOUT: Duration = Duration::from_secs(12);
/// 等待首次检测结果的兜底超时（首次请求自带超时，再加一份余量）。
const WAIT_TIMEOUT: Duration = Duration::from_secs(20);

/// 启动时的版本更新检测结果；网络失败时命令整体返回 `None`（前端静默处理）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseUpdate {
    /// 最新 Release 版本（已去掉 v 前缀）；无法识别时为 `None`。
    pub latest_version: Option<String>,
    /// 对应的 Release 页面地址（已通过白名单校验）。
    pub release_url: Option<String>,
    /// 是否存在比当前版本更新的 Release。
    pub update_available: bool,
    /// Release 说明（GitHub body 原文，Markdown），供"更新日志"对话框展示。
    pub notes: Option<String>,
}

/// GitHub releases/latest 响应中与本功能相关的字段（其余字段忽略）。
#[derive(Debug, Deserialize)]
struct ReleasePayload {
    tag_name: Option<String>,
    html_url: Option<String>,
    body: Option<String>,
}

/// 进程内检测阶段：启动后缓存结果；用户强制检查时可重新请求。
enum CheckPhase {
    Pending,
    InFlight,
    Done(Option<ReleaseUpdate>),
}

static CHECK_PHASE: Mutex<CheckPhase> = Mutex::new(CheckPhase::Pending);
static CHECK_SIGNAL: Condvar = Condvar::new();

/// Tauri command 入口：执行（或返回已缓存的）启动更新检测。
/// `force` 为 true 时忽略缓存，重新向 GitHub 请求。
pub async fn release_update(force: bool) -> Option<ReleaseUpdate> {
    spawn_blocking(move || release_update_blocking(force))
        .await
        .ok()
        .flatten()
}

/// 默认复用进程内缓存；`force` 时重新请求。并发检测会等待进行中的那一次。
fn release_update_blocking(force: bool) -> Option<ReleaseUpdate> {
    let mut phase = CHECK_PHASE.lock().expect("版本检测状态锁中毒");
    if matches!(*phase, CheckPhase::InFlight) {
        // 已有检测在进行中：等待其结果（带兜底超时），不并发发起第二次请求。
        let deadline = Instant::now() + WAIT_TIMEOUT;
        while matches!(*phase, CheckPhase::InFlight) {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            let (waited, _) = CHECK_SIGNAL
                .wait_timeout(phase, remaining)
                .expect("版本检测状态锁中毒");
            phase = waited;
        }
        return match &*phase {
            CheckPhase::Done(cached) => cached.clone(),
            _ => None,
        };
    }
    if !force {
        if let CheckPhase::Done(cached) = &*phase {
            return cached.clone();
        }
    }
    *phase = CheckPhase::InFlight;
    drop(phase);
    let outcome = fetch_latest_release();
    let mut phase = CHECK_PHASE.lock().expect("版本检测状态锁中毒");
    *phase = CheckPhase::Done(outcome.clone());
    CHECK_SIGNAL.notify_all();
    outcome
}

fn fetch_latest_release() -> Option<ReleaseUpdate> {
    let agent = ureq::AgentBuilder::new().timeout(FETCH_TIMEOUT).build();
    let payload = agent
        .get(RELEASE_API_URL)
        .set("User-Agent", "dsh-desktop")
        .set("Accept", "application/vnd.github+json")
        .call()
        .ok()?
        .into_string()
        .ok()?;
    parse_release_json(&payload).map(|(tag_name, html_url, body)| {
        evaluate_release(
            &tag_name,
            &html_url,
            body.as_deref(),
            env!("CARGO_PKG_VERSION"),
        )
    })
}

/// 纯逻辑：解析 GitHub API 返回的 JSON，取 `tag_name`、`html_url` 与 `body`
/// （允许缺省 html_url 与 body）。
fn parse_release_json(payload: &str) -> Option<(String, String, Option<String>)> {
    let release: ReleasePayload = serde_json::from_str(payload).ok()?;
    let tag_name = release.tag_name?.trim().to_string();
    let body = release
        .body
        .map(|body| body.trim().to_string())
        .filter(|body| !body.is_empty());
    Some((tag_name, release.html_url.unwrap_or_default(), body))
}

/// 纯逻辑：依据 tag 与当前版本综合判断可用更新；任一版本无法解析时结果为「无更新」。
fn evaluate_release(
    tag_name: &str,
    html_url: &str,
    body: Option<&str>,
    current: &str,
) -> ReleaseUpdate {
    let latest = normalize_tag(tag_name).and_then(|tag| semver::Version::parse(&tag).ok());
    let current = semver::Version::parse(current.trim()).ok();
    let (Some(latest), Some(current)) = (latest, current) else {
        return ReleaseUpdate {
            latest_version: None,
            release_url: None,
            update_available: false,
            notes: None,
        };
    };
    // html_url 必须通过白名单校验才对外提供，否则回退到固定的 Release 列表页。
    let release_url = if validate_release_url(html_url) {
        html_url.trim().to_string()
    } else {
        RELEASES_PAGE_URL.to_string()
    };
    ReleaseUpdate {
        latest_version: Some(latest.to_string()),
        release_url: Some(release_url),
        update_available: latest > current,
        // body 是 GitHub 上的公开文本，截断到 4000 字符防止超长释放说明撑爆界面。
        notes: body.map(|body| body.chars().take(4000).collect()),
    }
}

/// 纯逻辑：去掉 tag 的 v/V 前缀，返回供 semver 解析的版本文本。
fn normalize_tag(tag: &str) -> Option<String> {
    let trimmed = tag.trim();
    let stripped = trimmed.strip_prefix(['v', 'V']).unwrap_or(trimmed);
    (!stripped.is_empty()).then(|| stripped.to_string())
}

/// 纯逻辑：仅放行本项目的 Release 页面（`https://github.com/jockiller/dsh-desktop/releases`
/// 及其子路径），供版本按钮点击时打开；拒绝其他协议、主机、转义字符与路径穿越，
/// 避免界面把任意 URL 交给系统打开。
pub fn validate_release_url(url: &str) -> bool {
    let url = url.trim();
    if url.len() > 500 {
        return false;
    }
    let Some(rest) = url.strip_prefix(ALLOWED_URL_PREFIX) else {
        return false;
    };
    if !(rest.is_empty() || rest.starts_with('/')) {
        return false;
    }
    if rest.bytes().any(is_forbidden_url_byte) {
        return false;
    }
    !rest.split('/').any(|segment| segment == "..")
}

/// 纯逻辑：Release 白名单路径中不允许出现的字节——禁用转义、引号、空白/控制字符与非 ASCII。
fn is_forbidden_url_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'%' | b'\\' | b'"' | b'\'' | b'<' | b'>' | b'{' | b'}' | b'|' | b'^' | b'`'
    ) || !(0x21..=0x7E).contains(&byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_url_accepts_project_release_pages_only() {
        assert!(validate_release_url(
            "https://github.com/jockiller/dsh-desktop/releases"
        ));
        assert!(validate_release_url(
            "https://github.com/jockiller/dsh-desktop/releases/"
        ));
        assert!(validate_release_url(
            "https://github.com/jockiller/dsh-desktop/releases/tag/v0.2.0"
        ));
        assert!(validate_release_url(
            "https://github.com/jockiller/dsh-desktop/releases/latest"
        ));
        assert!(validate_release_url(
            "  https://github.com/jockiller/dsh-desktop/releases/tag/v1.0.0  "
        ));
    }

    #[test]
    fn release_url_rejects_other_schemes_hosts_and_paths() {
        // 非 https 协议
        assert!(!validate_release_url(
            "http://github.com/jockiller/dsh-desktop/releases"
        ));
        // 非 Release 路径
        assert!(!validate_release_url(
            "https://github.com/jockiller/dsh-desktop"
        ));
        assert!(!validate_release_url(
            "https://github.com/jockiller/dsh-desktop/actions"
        ));
        // 前缀必须完整（ releasesX 之类不放行）
        assert!(!validate_release_url(
            "https://github.com/jockiller/dsh-desktop/releasesX"
        ));
        // 其他仓库与其他主机
        assert!(!validate_release_url(
            "https://github.com/evil/dsh-desktop/releases"
        ));
        assert!(!validate_release_url(
            "https://evil.com/jockiller/dsh-desktop/releases"
        ));
        // 转义与路径穿越
        assert!(!validate_release_url(
            "https://github.com/jockiller/dsh-desktop/releases/tag/v1%2e0"
        ));
        assert!(!validate_release_url(
            "https://github.com/jockiller/dsh-desktop/releases/../admin"
        ));
        // 其他协议头与注入形态
        assert!(!validate_release_url("javascript:alert(1)"));
        assert!(!validate_release_url(""));
        assert!(!validate_release_url("   "));
    }

    #[test]
    fn release_json_parses_github_payload() {
        const PAYLOAD: &str = r###"{
            "url": "https://api.github.com/repos/jockiller/dsh-desktop/releases/1",
            "tag_name": "v0.2.0",
            "name": "DSH Desktop 0.2.0",
            "html_url": "https://github.com/jockiller/dsh-desktop/releases/tag/v0.2.0",
            "body": "## 修复\n- 一键安装",
            "prerelease": false
        }"###;
        let (tag_name, html_url, body) = parse_release_json(PAYLOAD).unwrap();
        assert_eq!(tag_name, "v0.2.0");
        assert_eq!(
            html_url,
            "https://github.com/jockiller/dsh-desktop/releases/tag/v0.2.0"
        );
        assert_eq!(body.as_deref(), Some("## 修复\n- 一键安装"));
        // 缺 tag_name 视为失败；非法 JSON 同样失败
        assert!(parse_release_json(r#"{"html_url": "https://x"}"#).is_none());
        assert!(parse_release_json("not json").is_none());
        // body 为空白时归一化为 None
        let (_, _, body) = parse_release_json(r#"{"tag_name": "v1"}"#).unwrap();
        assert_eq!(body, None);
    }

    #[test]
    fn evaluate_release_compares_semver_with_prerelease() {
        // 新版本：红点
        let update = evaluate_release(
            "v0.2.0",
            "https://github.com/jockiller/dsh-desktop/releases/tag/v0.2.0",
            Some("## 更新内容\n- 修复一键安装"),
            "0.1.0",
        );
        assert_eq!(
            update,
            ReleaseUpdate {
                latest_version: Some("0.2.0".into()),
                release_url: Some(
                    "https://github.com/jockiller/dsh-desktop/releases/tag/v0.2.0".into()
                ),
                update_available: true,
                notes: Some("## 更新内容\n- 修复一键安装".into()),
            }
        );
        // prerelease 高于当前已发布版本时也算更新
        let update = evaluate_release("v0.1.1-rc.1", "", None, "0.1.0");
        assert_eq!(update.latest_version.as_deref(), Some("0.1.1-rc.1"));
        assert!(update.update_available);
        // 当前版本较新（例如本地构建版本超前）时不提示
        let update = evaluate_release("v0.1.0", "", None, "0.2.0");
        assert!(!update.update_available);
        assert_eq!(update.latest_version.as_deref(), Some("0.1.0"));
        // 相同版本不提示
        assert!(!evaluate_release("0.1.0", "", None, "0.1.0").update_available);
    }

    #[test]
    fn evaluate_release_falls_back_when_payload_is_invalid() {
        // tag 无法解析或当前版本无法解析：整体按无更新处理
        let update = evaluate_release("nightly-2024", "", None, "0.1.0");
        assert_eq!(
            update,
            ReleaseUpdate {
                latest_version: None,
                release_url: None,
                update_available: false,
                notes: None,
            }
        );
        // release 页面偏离白名单时回退到固定列表页
        let update = evaluate_release(
            "v0.2.0",
            "https://evil.com/jockiller/dsh-desktop/releases/tag/v0.2.0",
            None,
            "0.1.0",
        );
        assert_eq!(update.release_url.as_deref(), Some(RELEASES_PAGE_URL));
    }

    #[test]
    fn release_update_serializes_camel_case() {
        let json = serde_json::to_value(ReleaseUpdate {
            latest_version: Some("0.2.0".into()),
            release_url: Some(RELEASES_PAGE_URL.into()),
            update_available: true,
            notes: Some("### 修复".into()),
        })
        .unwrap();
        assert_eq!(json["latestVersion"], "0.2.0");
        assert_eq!(json["releaseUrl"], RELEASES_PAGE_URL);
        assert_eq!(json["updateAvailable"], true);
        assert_eq!(json["notes"], "### 修复");
    }
}
