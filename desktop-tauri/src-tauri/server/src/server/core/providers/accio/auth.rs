//! Accio 的请求封装与用户资料查询。
//!
//! ── 与另外几家的差别：token 不进请求头 ──────────────────────
//! 桌面端的业务接口**不走 Authorization**：GET 把 `accessToken` 拼进 query
//! （`GET /api/entitlement/quota?accessToken=…`），POST 放进 JSON body
//! （`POST /api/auth/refresh_token {accessToken, refreshToken}`）。本模块把
//! 这个约定收在两处构造里（[`get_with_token`] / [`post_json`]），别处不再各写。
//!
//! ── 并发限流与风控（2026-10 按 unigate / accio2api 的实测结论移植）──
//! 三个事实（都来自对 phoenix-gw 的实测，不是推测）：
//!   1. **突发并发会被 RST**：22 个并发 TLS 连接可失败 19 个（unigate 的
//!      `accioNetSem` 注释），因此所有业务请求过一道全局并发闸（4 个许可，
//!      对齐 accio2api `RefreshAllAccountPlans` 的 concurrency=4）；
//!   2. **网络错误需要重试**：RST / 超时是瞬态的，重试 2 次（退避 500ms /
//!      1s）能把「偶发失败」收敛掉（unigate 的 `accioNetDo` 同款）；
//!   3. **555 是风控限速码**（`blocked by sentinel rate limit`）：出现后必须
//!      短退避，继续打会把限速升级成封禁 —— 全局退避 30 秒起、连续触发翻倍、
//!      上限 5 分钟（对齐 unigate 的 `accioSet555Block`）。
//!
//! 限流只覆盖**业务接口**（余额 / 续期 / 模型目录 / 资料查询 / 登录换码）——
//! 转发（`chat.rs`）是另一条链路，不经过本模块的 `request`。

use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Semaphore;

use crate::server::core::auth_http::{send_raw, ApiResponse};
use crate::server::core::proxies::{resolve_account_proxy, ProxyResolution, ResolvedProxy};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::credentials::{self, Credentials};
use super::endpoints::{self, Region};

/// 业务接口(非转发)的请求超时：与另外几家的 15 秒同量级
pub const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// 全局并发许可数（对齐 accio2api 实测值：并发超过 4 后 RST 概率明显上升）
const NET_CONCURRENCY: usize = 4;
/// 网络错误的重试次数（不含首次；对齐 unigate accioNetDo 的「总计 3 次尝试」）
const REQUEST_RETRIES: u32 = 2;
/// 555 退避基数 / 上限
const SENTINEL_BASE_MS: i64 = 30_000;
const SENTINEL_MAX_MS: i64 = 5 * 60_000;

/// 上游业务请求的全局并发闸（见模块头第 1 条）
static NET_SEM: OnceLock<Semaphore> = OnceLock::new();
/// 555 风控退避的截止时刻（毫秒；0 = 未触发）
static SENTINEL_UNTIL_MS: AtomicI64 = AtomicI64::new(0);
/// 连续触发次数（决定退避时长翻几倍）
static SENTINEL_STRIKES: AtomicU32 = AtomicU32::new(0);

fn net_sem() -> &'static Semaphore {
    NET_SEM.get_or_init(|| Semaphore::new(NET_CONCURRENCY))
}

/// 一次出网请求（错误文案带地区名，便于排障时区分两站）。
///
/// 三重防护（顺序即语义）：555 退避检查 → 全局并发闸 → 网络错误重试。
/// 并发位在**整个重试期间不释放**（`_permit` 存活到函数结束）—— 否则重试
/// 风暴会绕过限流，把刚压下去的突发又打回去。
pub async fn request(
    method: &str,
    url: &str,
    body: Option<&Value>,
    headers: &[(String, String)],
    proxy: Option<&ResolvedProxy>,
) -> Result<ApiResponse, GatewayError> {
    // 555 退避期内直接拒绝：继续打只会把限速升级成封禁
    if let Some(remaining_ms) = sentinel_backoff_remaining() {
        return Err(GatewayError::with_status(
            429,
            format!(
                "Accio 触发上游风控限速，已暂停请求（约 {} 秒后自动恢复）",
                (remaining_ms / 1000).max(1)
            ),
        )
        .with_code("accio_sentinel"));
    }

    let _permit = net_sem()
        .acquire()
        .await
        .map_err(|_| GatewayError::with_status(503, "Accio 请求调度不可用，请稍后重试"))?;

    let mut last_error: Option<reqwest::Error> = None;
    for attempt in 0..=REQUEST_RETRIES {
        if attempt > 0 {
            // 退避：500ms、1s（对齐 unigate accioNetDo 的递增退避）
            tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
        }
        match send_raw(method, url, body, headers, proxy, Some(REQUEST_TIMEOUT_MS)).await {
            Ok(response) => {
                // 555 检测要在**返回给调用方之前**：调用方（余额 / 续期）拿到的是
                // 业务失败响应，但全局退避必须现在就置上，拦住后续排队中的请求。
                if response.payload.as_ref().is_some_and(is_sentinel_payload) {
                    note_sentinel();
                }
                return Ok(response);
            }
            Err(error) => {
                if !is_retryable(&error) {
                    return Err(transport_error(error));
                }
                last_error = Some(error);
            }
        }
    }
    Err(transport_error(last_error.expect("至少执行过一次请求")))
}

/// 传输错误 → 网关错误（超时与其它网络失败分开报）
fn transport_error(error: reqwest::Error) -> GatewayError {
    if error.is_timeout() {
        GatewayError::with_status(504, "Accio 请求超时，请检查网络后重试").with_code("accio_transport")
    } else {
        GatewayError::with_status(502, "无法连接 Accio，请检查网络或账号代理设置")
            .with_code("accio_transport")
    }
}

/// 是否值得重试：只重试**网络层错误**（连接被 RST / 超时 / 发送失败）。
/// 已收到 HTTP 响应的一律不重试 —— 那是上游的业务答案，重试与否由调用方
/// 按语义决定（如余额查询的 401 → 刷新后重试一次）。
fn is_retryable(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request()
}

/// 555 退避剩余毫秒（不在退避期返回 None）
fn sentinel_backoff_remaining() -> Option<i64> {
    let until = SENTINEL_UNTIL_MS.load(Ordering::Relaxed);
    let now = logging::now_ms();
    (until > now).then(|| until - now)
}

/// 置 555 全局退避：30 秒起，连续触发（含刚结束 5 分钟内再次触发）翻倍，上限 5 分钟。
fn note_sentinel() {
    let now = logging::now_ms();
    let until = SENTINEL_UNTIL_MS.load(Ordering::Relaxed);
    let recent = until > now || (until > 0 && now - until < 5 * 60_000);
    let strikes = if recent {
        SENTINEL_STRIKES.fetch_add(1, Ordering::Relaxed) + 1
    } else {
        SENTINEL_STRIKES.store(1, Ordering::Relaxed);
        1
    };
    let mut delay = SENTINEL_BASE_MS;
    for _ in 1..strikes {
        if delay >= SENTINEL_MAX_MS {
            break;
        }
        delay *= 2;
    }
    let delay = delay.min(SENTINEL_MAX_MS);
    SENTINEL_UNTIL_MS.store(now + delay, Ordering::Relaxed);
    logging::log(
        "[Accio]",
        &format!(
            "⚠️ 上游返回风控限速码（555），暂停 Accio 请求 {} 秒（连续第 {strikes} 次）",
            delay / 1000
        ),
    );
}

/// 响应 payload 是否是 555 风控码（对齐 unigate 的 `accioIs555`：
/// `code == "555"` 或 message 含 sentinel / rate limit）
fn is_sentinel_payload(payload: &Value) -> bool {
    let code = match payload.get("code") {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    };
    if code == "555" {
        return true;
    }
    let message = payload
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    message.contains("sentinel") || message.contains("rate limit")
}

/// GET + token 进 query（业务接口的约定之一）
pub async fn get_with_token(
    region: Region,
    path: &str,
    token: &str,
    query: &[(&str, &str)],
    proxy: Option<&ResolvedProxy>,
) -> Result<ApiResponse, GatewayError> {
    let mut url = url::Url::parse(&format!("{}{}", endpoints::gateway_base(), path))
        .map_err(|_| GatewayError::with_status(500, "Accio 请求地址无效"))?;
    {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in query {
            pairs.append_pair(key, value);
        }
        if !token.is_empty() {
            pairs.append_pair("accessToken", token);
        }
    }
    request("GET", url.as_str(), None, &endpoints::api_headers(region), proxy).await
}

/// POST + JSON body（业务接口的约定之二）
pub async fn post_json(
    region: Region,
    path: &str,
    body: &Value,
    proxy: Option<&ResolvedProxy>,
) -> Result<ApiResponse, GatewayError> {
    let url = format!("{}{}", endpoints::gateway_base(), path);
    request("POST", &url, Some(body), &endpoints::api_headers(region), proxy).await
}

/// 把响应翻成「有效 JSON 对象」，失败时给出人话。
pub fn payload(response: ApiResponse, action: &str) -> Result<Value, GatewayError> {
    if !response.ok {
        let hint = match response.status {
            401 | 403 => "，请确认凭证有效或重新登录",
            429 => "，请稍后重试",
            _ => "",
        };
        return Err(GatewayError::with_status(
            i32::from(response.status),
            format!("Accio {action}失败（HTTP {}）{hint}", response.status),
        ));
    }
    response
        .payload
        .filter(Value::is_object)
        .ok_or_else(|| GatewayError::with_status(502, format!("Accio {action}未返回有效 JSON 对象")))
}

/// 账号级出口代理（解析失败按 400 报，不静默直连）
pub fn account_proxy(record: &Value) -> Result<Option<ResolvedProxy>, GatewayError> {
    match resolve_account_proxy(record.get("proxy")) {
        Some(ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(ProxyResolution::Failed(reason)) => Err(GatewayError::with_status(400, reason)),
        None => Ok(None),
    }
}

/// 查用户资料（`GET /api/auth/userinfo?accessToken=…`）。
///
/// 上游把资料放在 `data` 里还是顶层，两种形态都见过（桌面端读的是
/// `data.user` / 顶层混合），这里都接受。
pub async fn fetch_profile(
    token: &str,
    region: Region,
    proxy: Option<&ResolvedProxy>,
) -> Result<Value, GatewayError> {
    let response = get_with_token(region, endpoints::USER_INFO_PATH, token, &[], proxy).await?;
    let payload = payload(response, "用户信息查询")?;
    Ok(unwrap_data(payload))
}

/// 上游响应常见两层信封（`{success, data:{…}}`）；取内层
pub fn unwrap_data(payload: Value) -> Value {
    match payload.get("data") {
        Some(Value::Object(_)) => payload.get("data").cloned().unwrap_or(payload),
        _ => payload,
    }
}

/// 把资料里的身份/名字搬进凭证。
pub fn apply_profile(credentials: &mut Credentials, profile: &Value) {
    let user_id = credentials::text(profile, &["id", "userId", "user_id", "uid"]);
    if !user_id.is_empty() {
        credentials.user_id = user_id;
    }
    let name = credentials::text(profile, &["nickname", "name", "displayName", "userName"]);
    if !name.is_empty() {
        credentials.name = name.chars().take(100).collect();
    }
    let email = credentials::text(profile, &["email", "emailAddress"]);
    if !email.is_empty() {
        credentials.email = email.chars().take(320).collect();
    }
    let device_id = credentials::text(profile, &["deviceId", "device_id"]);
    if !device_id.is_empty() && credentials.device_id.is_empty() {
        credentials.device_id = device_id;
    }
}

/// 「填写凭证」路径的入口：解析 payload → 补资料 → 补齐设备指纹。
///
/// ── 为什么要查一次资料（多一次往返）────────────────────────
/// token 是不透明串，账号列表上总得显示「这是谁」；查一次资料就有了 userId /
/// 昵称 / 邮箱三样。查询失败**不算失败**：拿不到资料照常建账号（id 走凭证
/// 指纹兜底、名字走备注名），只是少一行副标题 —— 与 AutoClaw 的
/// `email_or_empty` 同一取舍。
pub async fn prepare_account(payload: &Value) -> Result<Credentials, GatewayError> {
    let mut credentials = Credentials::from_payload(payload)?;
    credentials.ensure_identity();
    if credentials.user_id.is_empty() || credentials.name.is_empty() {
        match fetch_profile(&credentials.access_token, credentials.region, None).await {
            Ok(profile) => apply_profile(&mut credentials, &profile),
            Err(error) => logging::verbose(
                "[Accio]",
                &format!("用户资料查询失败（不影响添加）: {}", error.message),
            ),
        }
    }
    credentials.ensure_identity();
    Ok(credentials)
}

/// 额度查询用的「只读快照」：不触发续期（余额是展示动作，烧一次 refreshToken
/// 不划算 —— 过期就让上游回 401，由调用方的刷新重试那条既有链路处置）。
pub fn snapshot(record: &Value) -> Result<Credentials, GatewayError> {
    let mut credentials = Credentials::from_payload(record)?;
    credentials.ensure_identity();
    Ok(credentials)
}

/// 一个便于日志的账号摘要（绝不回显 token）
pub fn describe(credentials: &Credentials) -> String {
    let who = if !credentials.user_id.is_empty() {
        credentials.user_id.clone()
    } else if !credentials.email.is_empty() {
        credentials.email.clone()
    } else {
        "未知账号".to_string()
    };
    format!("{}（{}）", who, credentials.region.label())
}

/// 空 body 的占位（部分接口要求 POST 但不需要体）
pub fn empty_body() -> Value {
    json!({})
}
