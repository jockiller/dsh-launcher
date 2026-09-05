//! DSH 会话运行状态监视器。
//!
//! 流程（对应 service.rs 的 owned 启动路径）：
//! 1. 从 dsh 启动日志捕获的带 launch-token URL 中解析出 origin 与 token（token 只在内存保存）；
//! 2. 每次启动都用 token 换取新的浏览器会话 cookie：
//!    `GET {origin}/?token=...` → 303 + `Set-Cookie`（dsh 的 cookie 为
//!    `HttpOnly; SameSite=Strict`，浏览器与原生客户端可各自独立换发，互不影响）；
//! 3. 以固定轮询间隔调用 `POST /api/session/list`（typert RPC 的 HTTP unary 通道），
//!    统计 `running == true` 的会话数，并在变化时通过 launcher 日志通道输出。
//!
//! 说明：`session/list` 只统计本 DSH 进程内“已附着且正在执行回合”的会话；
//! 主会话按 `origin != "subagent"` 区分，子代理会话单独计数展示。

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use tauri::Url;

use crate::service::emit_log;

/// 轮询 `session/list` 的间隔。
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// token 换取 cookie 与轮询请求的单次网络超时。
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);
/// cookie 换取在连续失败多少次后放弃（每次间隔 POLL_INTERVAL）。
const EXCHANGE_MAX_ATTEMPTS: u32 = 30;

/// 一次轮询得到的运行中会话概况。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunningSnapshot {
    /// 正在执行回合的主会话数。
    pub main: usize,
    /// 正在执行的子代理会话数。
    pub subagents: usize,
}

/// DSH 会话鉴权凭据（在内存中保存，支持插件热重启后无缝复用）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SessionCredentials {
    /// 服务 base origin（如 `http://127.0.0.1:3080`）。
    pub origin: String,
    /// 从启动日志捕获的 launch token（若有）。
    pub token: Option<String>,
    /// 已换取的会话 Cookie（形如 `dsh-auth-...=...`）。
    pub cookie: Option<String>,
}

impl SessionCredentials {
    /// 从带 token URL 构建初始凭据。
    pub(crate) fn from_authenticated_url(url: &str) -> Option<Self> {
        let (origin, token) = parse_authenticated(url)?;
        Some(Self {
            origin,
            token: Some(token),
            cookie: None,
        })
    }
}

/// 启动会话监视线程。
///
/// `credentials` 持有当前服务的鉴权凭据；
/// `cancel` 与服务运行代际绑定，stop/restart/handoff 服务时随之终止当前监视线程。
///
/// 本模块是纯外挂的观察者：只在独立线程内做 HTTP 只读轮询，不持有/不修改
/// 任何服务状态；任何失败（网络、协议、甚至 panic）都最多产出一条日志后退出，
/// 绝不影响 DSH 服务的启动、停止与状态上报。
pub(crate) fn spawn(
    app: tauri::AppHandle,
    credentials: Arc<Mutex<Option<SessionCredentials>>>,
    cancel: Arc<AtomicBool>,
) {
    let _ = thread::Builder::new().name("session-monitor".into()).spawn(move || {
        // 兜底：任何意外 panic 只记一条日志，线程静默退出，不影响宿主。
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(app.clone(), credentials, cancel.clone());
        }));
        // 退出时的托盘指示复位（带 cancel 校验——stop/restart 时 service 层
        // 已将图标设为常态 Idle，此处不再覆盖）。
        crate::tray::set_running_checked(false, &cancel);
        if result.is_err() {
            emit_log(&app, "monitor", "error", "会话监视：意外异常退出（不影响其他功能）");
        }
    });
}

fn run(
    app: tauri::AppHandle,
    credentials: Arc<Mutex<Option<SessionCredentials>>>,
    cancel: Arc<AtomicBool>,
) {
    let (origin, token, mut cookie) = {
        let guard = credentials.lock().ok().and_then(|guard| guard.clone());
        let Some(creds) = guard else {
            emit_log(&app, "monitor", "warning", "会话监视：未提供有效鉴权凭据，未启动");
            return;
        };
        (creds.origin, creds.token, creds.cookie)
    };

    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(HTTP_TIMEOUT)
        .build();

    // 1) 若尚未换取 cookie，则使用 token 进行换取；若已持有 cookie 则直接复用。
    if cookie.is_none() {
        if let Some(token_val) = &token {
            let mut failed_attempts: u32 = 0;
            while cookie.is_none() {
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                match exchange_cookie(&agent, &origin, token_val) {
                    Ok(next) => {
                        cookie = Some(next.clone());
                        if let Ok(mut guard) = credentials.lock() {
                            if let Some(c) = guard.as_mut() {
                                c.cookie = Some(next);
                            }
                        }
                        emit_log(&app, "monitor", "info", "会话监视：launch token 换取 cookie 成功");
                    }
                    Err(error) => {
                        failed_attempts += 1;
                        // 只打首条与每 10 次，避免刷屏。
                        if failed_attempts == 1 || failed_attempts % 10 == 0 {
                            emit_log(
                                &app,
                                "monitor",
                                "error",
                                &format!("会话监视：token 换取 cookie 失败（第 {failed_attempts} 次）：{error}"),
                            );
                        }
                        if failed_attempts >= EXCHANGE_MAX_ATTEMPTS {
                            emit_log(&app, "monitor", "error", "会话监视：多次换取 cookie 失败，已停止监视");
                            return;
                        }
                        thread::sleep(POLL_INTERVAL);
                    }
                }
            }
        } else {
            emit_log(&app, "monitor", "warning", "会话监视：缺少 token 与 cookie，未启动");
            return;
        }
    } else {
        emit_log(
            &app,
            "monitor",
            "info",
            &format!("会话监视：已启动并复用已有会话凭据（{origin}）"),
        );
    }

    let mut current_cookie = match cookie {
        Some(cookie) => cookie,
        None => return,
    };

    // 2) 轮询运行中会话，驱动 Dock 图标指示（安静运行，不打印会话数日志）。
    let sequence = AtomicU64::new(0);
    let mut failures = 0u32;
    loop {
        if cancel.load(Ordering::Acquire) {
            break;
        }
        match poll_running(&agent, &origin, &current_cookie, &sequence) {
            Ok(snapshot) => {
                failures = 0;
                // 同步状态栏托盘图标指示：有运行中会话（含子代理）时闪烁指示。
                // 带代际校验：cancel（服务停止/重启）后旧线程的写入被忽略，
                // 不会覆盖 stop 时由 service 层设置的 Idle 状态。
                let active = snapshot.main > 0 || snapshot.subagents > 0;
                crate::tray::set_running_checked(active, &cancel);
            }
            Err(PollFailure::Unauthorized) => {
                failures = 0;
                if let Some(token_val) = &token {
                    emit_log(&app, "monitor", "warning", "会话监视：cookie 已失效，尝试重新用 token 换取");
                    match exchange_cookie(&agent, &origin, token_val) {
                        Ok(next) => {
                            current_cookie = next.clone();
                            if let Ok(mut guard) = credentials.lock() {
                                if let Some(c) = guard.as_mut() {
                                    c.cookie = Some(next);
                                }
                            }
                        }
                        Err(error) => {
                            emit_log(&app, "monitor", "error", &format!("会话监视：重新换取 cookie 失败：{error}"));
                        }
                    }
                } else {
                    emit_log(&app, "monitor", "error", "会话监视：cookie 已失效且无可用 token，已停止监视");
                    return;
                }
            }
            Err(PollFailure::Transport(error)) => {
                failures += 1;
                if failures >= 3 {
                    emit_log(&app, "monitor", "info", &format!("会话监视：DSH 无法访问（{error}），已退出"));
                    return;
                }
            }
            Err(PollFailure::Server(error)) => {
                failures += 1;
                emit_log(&app, "monitor", "error", &format!("会话监视：会话列表响应异常：{error}"));
                if failures >= 3 {
                    return;
                }
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
    emit_log(&app, "monitor", "info", "会话监视：已停止");
}

/// 从带 token URL 中解析 (origin, token)。
/// 仅接受 http(s) URL 且必须带 token 参数；origin 用于后续所有 API 调用，
/// 保证 Cookie 的 authority 绑定与请求 Host 头一致。
pub(crate) fn parse_authenticated(url: &str) -> Option<(String, String)> {
    let url = Url::parse(url.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let origin = url.origin().ascii_serialization();
    let token = url
        .query_pairs()
        .find(|(key, _)| key == "token")
        .map(|(_, value)| value.into_owned())?;
    (!token.is_empty()).then_some((origin, token))
}

/// 从 Set-Cookie 头提取 “name=value”（丢弃 Max-Age/HttpOnly 等属性）。
/// dsh 的会话 cookie 名形如 `dsh-auth-<sha256(base64url)>`。
pub(crate) fn cookie_from_set_cookie(header: &str) -> Option<String> {
    let pair = header.split(';').next()?.trim();
    let (name, value) = pair.split_once('=')?;
    let name = name.trim();
    let value = value.trim();
    (!name.is_empty() && !value.is_empty()).then(|| format!("{name}={value}"))
}

/// 用 launch token 换取新 cookie。
/// dsh 对 `GET /?token=...` 返回 3xx 重定向并附 Set-Cookie；ureq 设 redirects(0) 后
/// 3xx 会作为 Ok 响应返回（仅 >=400 才包装为 Err），从中读取 cookie。
/// token 通过 Url 编码注入，不依赖其字符集假设（对将来格式变化保持兼容）。
fn exchange_cookie(agent: &ureq::Agent, origin: &str, token: &str) -> Result<String, String> {
    let mut exchange_url = Url::parse(&format!("{origin}/")).map_err(|error| format!("origin 无效：{error}"))?;
    exchange_url.query_pairs_mut().append_pair("token", token);
    let response = agent
        .get(exchange_url.as_str())
        .call()
        .map_err(|error| format!("请求失败：{error}"))?;
    if !(300..400).contains(&response.status()) {
        return Err(format!("预期 3xx 重定向，实际状态码 {}", response.status()));
    }
    response
        .header("set-cookie")
        .and_then(cookie_from_set_cookie)
        .ok_or_else(|| "响应缺少 Set-Cookie 头".to_string())
}

/// 轮询失败的分类。
enum PollFailure {
    /// cookie 过期/无效，需要重新换取。
    Unauthorized,
    /// 服务端返回了不可用的业务结果。
    Server(String),
    /// 网络层失败（服务大概率已停止）。
    Transport(String),
}

/// 轮询一次运行中会话列表。
///
/// 协议（typert gateway 的 HTTP unary 通道，见 dsh-api-gateway）：
/// `POST {origin}/api/session/list`，body 为
/// `{type:"client-request", rpcId, method:"session/list", payload:{args:{_request:{}}}}`，
/// 响应为 `{type:"server-response", result:{ok, value:{items:[{sessionId, running, origin?}]}}}`。
fn poll_running(
    agent: &ureq::Agent,
    origin: &str,
    cookie: &str,
    sequence: &AtomicU64,
) -> Result<RunningSnapshot, PollFailure> {
    let rpc_id = sequence.fetch_add(1, Ordering::Relaxed);
    let body = serde_json::json!({
        "type": "client-request",
        "rpcId": format!("monitor-{rpc_id}"),
        "method": "session/list",
        "payload": { "args": { "_request": {} } },
    });
    let response = agent
        .post(&format!("{origin}/api/session/list"))
        .set("Content-Type", "application/json")
        .set("Cookie", cookie)
        .send_string(&body.to_string())
        .map_err(|error| match error {
            ureq::Error::Status(401, _) => PollFailure::Unauthorized,
            ureq::Error::Status(status, _) => PollFailure::Server(format!("HTTP {status}")),
            ureq::Error::Transport(inner) => PollFailure::Transport(inner.to_string()),
        })?;
    let text = response
        .into_string()
        .map_err(|error| PollFailure::Server(format!("读取响应失败：{error}")))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| PollFailure::Server(format!("响应不是合法 JSON：{error}")))?;
    parse_running_sessions(&value).ok_or_else(|| PollFailure::Server("响应缺少 result.value.items".into()))
}

/// 从 `server-response` 信封中解析运行中会话概况。
/// 输入不是成功的会话列表时返回 None；单个 item 字段缺失/异常仅跳过该项
/// （对 DSH 新版本调整 items 结构保持弹性），其余正常计数。
pub(crate) fn parse_running_sessions(value: &serde_json::Value) -> Option<RunningSnapshot> {
    let result = value.get("result")?;
    if result.get("ok")?.as_bool()? != true {
        return None;
    }
    let items = result.get("value")?.get("items")?.as_array()?;
    let mut snapshot = RunningSnapshot { main: 0, subagents: 0 };
    for item in items {
        let Some(running) = item.get("running").and_then(|running| running.as_bool()) else {
            continue; // running 字段缺失/异常的 item 不计入，也不让整轮失败。
        };
        if !running {
            continue;
        }
        // 主会话无 origin 字段；子代理会话标记 origin == "subagent"。
        if item.get("origin").and_then(|origin| origin.as_str()) == Some("subagent") {
            snapshot.subagents += 1;
        } else {
            snapshot.main += 1;
        }
    }
    Some(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_credentials_from_authenticated_url() {
        let creds = SessionCredentials::from_authenticated_url("http://127.0.0.1:3080/?token=AbC_123")
            .expect("应成功解析凭据");
        assert_eq!(creds.origin, "http://127.0.0.1:3080");
        assert_eq!(creds.token.as_deref(), Some("AbC_123"));
        assert_eq!(creds.cookie, None);

        assert_eq!(
            SessionCredentials::from_authenticated_url("http://127.0.0.1:3080/"),
            None
        );
    }

    #[test]
    fn parse_authenticated_extracts_origin_and_token() {
        let (origin, token) =
            parse_authenticated("http://127.0.0.1:3080/?token=AbC_123-xyz").expect("应解析成功");
        assert_eq!(origin, "http://127.0.0.1:3080");
        assert_eq!(token, "AbC_123-xyz");
        // 多余路径/查询不影响解析（取第一个 token 参数）。
        let (origin, token) =
            parse_authenticated("http://127.0.0.1:3080/?token=tk&other=1").expect("应解析成功");
        assert_eq!(origin, "http://127.0.0.1:3080");
        assert_eq!(token, "tk");
    }

    #[test]
    fn parse_authenticated_rejects_plain_url() {
        assert!(parse_authenticated("http://127.0.0.1:3080/").is_none());
        assert!(parse_authenticated("https://example.com/?token=").is_none());
        assert!(parse_authenticated("not a url").is_none());
    }

    #[test]
    fn cookie_from_set_cookie_strips_attributes() {
        let header = "dsh-auth-dG9rZW4=v1.payload.sig; Max-Age=2592000; Path=/; HttpOnly; SameSite=Strict";
        assert_eq!(
            cookie_from_set_cookie(header).as_deref(),
            Some("dsh-auth-dG9rZW4=v1.payload.sig")
        );
        assert_eq!(cookie_from_set_cookie("invalid"), None);
        assert_eq!(cookie_from_set_cookie(""), None);
    }

    #[test]
    fn parse_running_sessions_counts_main_and_subagent() {
        let value = json!({
            "type": "server-response",
            "rpcId": "monitor-1",
            "result": {
                "ok": true,
                "value": {
                    "items": [
                        { "sessionId": "aaaaaaaa-1111", "updatedAt": 1, "running": true },
                        { "sessionId": "bbbbbbbb-2222", "updatedAt": 2, "running": false },
                        { "sessionId": "cccccccc-3333", "updatedAt": 3, "running": true, "origin": "subagent" }
                    ]
                }
            }
        });
        let snapshot = parse_running_sessions(&value).expect("应解析成功");
        assert_eq!(snapshot, RunningSnapshot { main: 1, subagents: 1 });
    }

    #[test]
    fn parse_running_sessions_rejects_error_result() {
        let value = json!({
            "type": "server-response",
            "rpcId": "monitor-2",
            "result": { "ok": false, "error": { "code": "gateway/bad-request", "message": "x" } }
        });
        assert!(parse_running_sessions(&value).is_none());
    }
}