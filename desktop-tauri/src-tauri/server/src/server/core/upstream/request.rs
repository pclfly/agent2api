//! 对话转发的**传输层**：请求发送、上游错误解析、追踪 id 生成。
//!
//! ── 本文件为什么只做传输（Agent2API 改造 W2b-T3）────────────
//! 改造前这里还有头集合、URL 拼接、system 注入与 11128/6004 的判定 ——
//! 那些全是 **workbuddy 专属知识**（X-IDE-* 头、`/v2/chat/completions`、
//! 首条消息必须是 system、11128 敏感词拦截码），已整体搬进
//! `core::providers::workbuddy`。改造后 provider 差异全部收在适配器里，
//! 本文件对「上游是哪一家」一无所知：它只认 `TransportRequest` 这个
//! **协议无关**的形态（URL + 头 + 已序列化的 body + 出口）。
//!
//! 只保留的三件事：
//!   1. `send_chat_request`：按出口取 reqwest Client 发一次请求，
//!      不读 body（SSE 需要 `bytes_stream()`）；
//!   2. `read_upstream_error`：把上游错误响应归一化成 `{code, message}`
//!      —— 「怎么读一个 HTTP 错误体」是协议层的事，与哪一家无关；
//!   3. `new_request_id`：一轮对话的追踪 id（适配器拼追踪头时用）。
//!
//! ── 出网 ────────────────────────────────────────────────────
//! 只走 `core::egress::client_for`：同一出口共用一个连接池，
//! 客户端上的 connect/read 超时也一并复用（见 egress 头部的旋钮映射）。

use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::egress;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::errors::GatewayError;

/// 单次对话请求的总超时（除退避重试外的等待时间）。
///
/// 与 Node 版的差别：Node 的 proxyFetch 不设总超时（bodyTimeout: 0），
/// 完全靠客户端断开与上游主动结束。Rust 侧的 reqwest client 用
/// `read_timeout(600s)` 限制「两次数据之间」的间隔（见 core::egress 的说明），
/// 这里不再叠加总超时 —— 那会掐断长回答的 SSE 流。
pub const NO_TOTAL_TIMEOUT: Option<u64> = None;

/// 单次尝试**等待上游响应头**的上限（毫秒）。
///
/// 与 OmniProxy 的 `headers_timeout`（默认 300 秒）同义：连接建立之后、
/// 响应头到达之前的静默等待必须有个上限 —— `read_timeout` 管的是「两次数据
/// 之间」，虽然首包之前的等待也计入它，但分钟级的等待 × 重试链（同账号重发 ×
/// 换号）会把一条请求拖到几十分钟，而客户端早就等不及了。
///
/// 现在是**配置项**（设置页「请求超时 → 等待响应超时」，默认 300 秒，
/// 1–3600）：每次发送时从内存快照取一次，改完设置下一个请求就生效。
///
/// 为什么默认 300 秒是安全的：网关请求上游**恒带 `stream: true`**（见
/// `UpstreamService::forward` 的说明），响应头在 SSE 建立时就到达，
/// 与「模型思考多久」无关；非流式的长回答也走这条路径（本地聚合）。
/// 超时按传输层失败处理（502 + 既有退避重试），与连接失败同一档。
fn headers_timeout() -> Duration {
    Duration::from_millis(crate::server::config::timeout_settings().headers_ms())
}

/// 「连接中超时」的设定值（秒）：连接超时文案标注实际生效的秒数用，
/// 与设置页「请求超时 → 连接中超时」同一份配置。
fn connect_timeout_seconds() -> u64 {
    crate::server::config::timeout_settings().connect_ms() / 1000
}

/// 一次上游请求的全部素材（协议无关形态；provider 差异在构造阶段已消解）
pub struct TransportRequest {
    /// 上游完整 URL（由适配器给出）
    pub url: String,
    /// 请求头（由适配器给出；不含出网代理相关）
    pub headers: Vec<(String, String)>,
    /// 已序列化的请求体（由编排层从适配器的 `plan.body` 序列化而来）
    pub payload: String,
    /// 出网代理（账号级；与 provider 无关，由编排层解析后带上）
    pub proxy: Option<ResolvedProxy>,
    /// 账号未配代理时本家是否**跟随系统代理**出网（编排层按适配器的能力位填，
    /// 默认 false = 直连）。为 true 的家见 `ProviderAdapter::system_proxy_when_unset`。
    pub system_proxy_when_unset: bool,
    /// 是否放宽 TLS 校验（不验证书链）。**只由 `vertex` 协议在提供商记录
    /// 显式声明 `insecureTls: true` 时置真** —— 用于上游经 MITM 型出网代理
    /// （它现签证书、签发者不在信任链里）。默认 false，其余协议一律严格校验。
    pub accept_any_cert: bool,
    /// 链式出网的前置代理（可选）：先经它出去，再由 `proxy` 做第二跳
    /// （典型形态：Clash 选地区 → Vault 注入凭据）。为 None 时 `proxy` 单独生效。
    ///
    /// 为什么不在 `proxy` 里表达两跳：`ResolvedProxy` 描述的是**一个**代理，
    /// 而 HTTP 的 CONNECT 一次只能指定一跳。两跳需要真实建两级隧道，
    /// 由 `core::proxy_chain` 在本地开中转端口来兑现（见该模块的模块头）。
    pub front_proxy: Option<ResolvedProxy>,
}

/// 归一化后的上游错误：`{code, message}`
///
/// `message` 的兜底是响应文本的前 500 个字符（Node 的 `text.slice(0, 500)`）。
/// 适配器的 `classify_error` 收的就是本结构的 JSON 形态（`to_value`）——
/// 于是「上游错误怎么读」与「这条错误属于哪一档」彻底分开：
/// 前者是本文件（协议层），后者是适配器（provider 层）。
pub struct UpstreamErrorDetail {
    pub code: Option<i64>,
    pub message: String,
}

impl UpstreamErrorDetail {
    /// 适配器入参形态：`{code, message}`（键名与上游 JSON 一致，
    /// 于是适配器可以直接用 `error_body.get("code")` 读，不需要中间结构体）
    pub fn to_value(&self) -> Value {
        json!({
            "code": self.code.map(Value::from).unwrap_or(Value::Null),
            "message": self.message,
        })
    }
}

/// 上游错误响应 → `{code, message}`（对照 Node 的 readUpstreamError）。
///
/// `capture`：调试模式的采集器（None = 未开启）。**必须在这里采** —— 本函数
/// 用 `response.text()` 把响应体整个吃掉，调用方拿不到第二份；不在这里顺手
/// 旁路，错误响应体就永远进不了调试报文（而失败现场恰恰是最需要看的那一半）。
pub async fn read_upstream_error(
    response: reqwest::Response,
    capture: Option<&crate::server::core::debug_traffic::TrafficCapture>,
) -> UpstreamErrorDetail {
    if let Some(capture) = capture {
        capture.attach_response(response.status().as_u16(), response.headers());
    }
    let text = response.text().await.unwrap_or_default();
    if let Some(capture) = capture {
        capture.push(text.as_bytes());
    }
    let mut code = None;
    let mut message: String = text.chars().take(500).collect();
    if let Ok(payload) = serde_json::from_str::<Value>(&text) {
        code = payload.get("code").and_then(Value::as_i64);
        // Node: `payload?.message || payload?.msg || payload?.error?.message || message`
        let candidate = payload
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .or_else(|| {
                payload
                    .get("msg")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
            })
            .or_else(|| {
                payload
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
            });
        if let Some(candidate) = candidate {
            message = candidate.to_string();
        }
    }
    UpstreamErrorDetail { code, message }
}

/// 发一次上游请求（不读 body，保留原始响应给流式转发与错误解析）。
///
/// 与 `core::auth_http::send_raw` 的分工：那个把响应读成文本，用于管理接口；
/// 这个把 `reqwest::Response` 原样交给调用方，SSE 需要 `bytes_stream()`。
pub async fn send_chat_request(
    plan: &TransportRequest,
) -> Result<reqwest::Response, UpstreamRequestError> {
    // 客户端三选一：账号级代理 → 跟随系统代理（本家能力位，见
    // `TransportRequest::system_proxy_when_unset`）→ 直连
    //
    // 账号代理若同时带前置代理（`front_proxy`），先在本机起一个两跳中转，
    // 把出口指向它 —— reqwest 仍只看到「一个普通代理」，两跳由
    // `proxy_chain` 内部兑现（Clash 选地区 → Vault 注入凭据）。
    let chained = match (plan.front_proxy.as_ref(), plan.proxy.as_ref()) {
        (Some(front), Some(back)) => crate::server::core::proxy_chain::chain_endpoint(front, back),
        _ => None,
    };
    let effective_proxy = match (chained.as_ref(), plan.proxy.as_ref()) {
        (Some(chain), Some(back)) => {
            let mut local = back.clone();
            let (host, port) = chain
                .local_addr
                .rsplit_once(':')
                .unwrap_or((chain.local_addr.as_str(), "0"));
            local.host = host.to_string();
            local.port = port.parse::<u16>().ok();
            // 中转不需要认证：凭据已封装在隧道建立时
            local.username = String::new();
            local.password = String::new();
            local.label = chain.label.clone();
            Some(local)
        }
        (_, back) => back.cloned(),
    };
    let client = match effective_proxy.as_ref() {
        Some(proxy) => egress::client_for_with_tls(Some(proxy), plan.accept_any_cert),
        None if plan.system_proxy_when_unset => egress::client_for_system_proxy(),
        None => egress::client_for_with_tls(None, plan.accept_any_cert),
    };
    let mut builder = client.post(&plan.url).body(plan.payload.clone());
    for (key, value) in &plan.headers {
        builder = builder.header(key, value);
    }
    if let Some(timeout) = NO_TOTAL_TIMEOUT {
        builder = builder.timeout(Duration::from_millis(timeout));
    }
    // 出口说明（失败文案用）：文案照抄 Node 的 fetchViaProxy
    let via = match &plan.proxy {
        // 链式出口：把两跳都写出来，排障时能一眼看出「地区节点 + 注入代理」
        Some(proxy) if plan.front_proxy.is_some() => {
            let front = plan.front_proxy.as_ref();
            format!(
                "经链式代理（前置 {} → 第二跳 {}）",
                front.map(|p| format!("{}:{}", p.host, p.port.unwrap_or(0))).unwrap_or_default(),
                proxy.host
            )
        }
        Some(proxy) if !proxy.label.is_empty() => format!("经代理 {}", proxy.label),
        Some(proxy) => format!("经代理 {}", proxy.host),
        // 跟随系统代理的家（Antigravity）在账号未配代理时走的是系统代理，
        // 文案不能再说「直连」—— 诊断时要能一眼看出流量从哪出去（见
        // `ProviderAdapter::system_proxy_when_unset`）
        None if plan.system_proxy_when_unset => "跟随系统代理".to_string(),
        None => "直连".to_string(),
    };
    // 等待响应头有上限（见 headers_timeout 的说明）：超时后 future 被丢弃，
    // 上游连接随之关闭（与客户端断开时的取消是同一机制）
    let headers_budget = headers_timeout();
    let sent = send_builder_with_tls(builder, plan.accept_any_cert, plan.front_proxy.as_ref()).await;
    let result: Result<_, tokio::time::error::Elapsed> = Ok(sent);
    match result {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(error)) => {
            // `send()` 阶段的超时只可能来自连接（等待响应头由外层计时器管，
            // 它的预算 ≤ 客户端 read_timeout，见 egress 的说明）：此时错误链
            // 对用户没有信息量（就是「没连上」），文案直接给设置页的旋钮名
            // 与实际生效的秒数；其余（ECONNREFUSED、代理鉴权失败、DNS…）
            // 保留根因与出口说明，便于判断是不是代理配错了
            let reason = if error.is_timeout() {
                format!("连接中超时({}秒)", connect_timeout_seconds())
            } else {
                "上游连接失败".to_string()
            };
            let message = if error.is_timeout() {
                format!("连接中超时({}秒，出口 {via})", connect_timeout_seconds())
            } else {
                format!(
                    "上游请求失败（{via}）: {}",
                    egress::describe_error_detail(&error)
                )
            };
            Err(UpstreamRequestError { message, reason, local: error.local })
        }
        Err(_elapsed) => {
            let seconds = headers_budget.as_secs();
            Err(UpstreamRequestError {
                local: false,
                reason: format!("等待响应超时({seconds}秒)"),
                message: format!("等待响应超时({seconds}秒，出口 {via})"),
            })
        }
    }
}

/// Send a prebuilt request without regenerating body, credentials, or session headers.
/// Stateful providers call this at each HTTP exchange, never replaying a conversation.
#[derive(Debug)]
pub(crate) struct SendError { message: String, timeout: bool, pub local: bool }
impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.message) }
}
impl std::error::Error for SendError {}
impl From<reqwest::Error> for SendError {
    fn from(error: reqwest::Error) -> Self { Self { message:egress::describe_error_detail(&error), timeout:error.is_timeout(), local:error.is_builder() } }
}
impl SendError { pub fn is_timeout(&self) -> bool { self.timeout } }
pub(crate) async fn send_builder(builder: reqwest::RequestBuilder) -> Result<reqwest::Response, SendError> {
    send_builder_with_tls(builder, false, None).await
}
async fn send_builder_with_tls(builder: reqwest::RequestBuilder, tls: bool, front: Option<&ResolvedProxy>) -> Result<reqwest::Response, SendError> {
    let (original_client, request) = builder.build_split();
    let request = match request {
        Ok(request) => request,
        Err(error) => {
            if let Some(context) = crate::server::core::proxy_rotate::context() { context.lock().await.local_failure = true; }
            return Err(SendError::from(error));
        }
    };
    let origin = request.url().clone();
    let replay = request.try_clone();
    let mut first = Some(request);
    let context = crate::server::core::proxy_rotate::context();
    let active = match &context { Some(c) => { let mut a = c.lock().await; a.begin_exchange(); a.active() }, None => false };
    // Each HTTP exchange owns its tried set. Background heartbeat/cleanup exchanges
    // may overlap; they share sticky publication, never mutate each other's attempts.
    let mut exchange = match &context { Some(c) => c.lock().await.clone(), None => Default::default() };
    exchange.set_origin(&origin);
    let deadline = if active { exchange.deadline.unwrap_or_else(|| tokio::time::Instant::now() + headers_timeout()) }
        else { tokio::time::Instant::now() + headers_timeout() };
    let mut failed = false;
    let mut last = None;
    let mut sends = 0;
    let mut retries_500 = 0;
    let mut retried_throttle = false;
    loop {
        if tokio::time::Instant::now() >= deadline || sends >= crate::server::core::proxy_rotate::MAX_ATTEMPTS {
            return last.unwrap_or_else(|| Err(SendError { message:"轮询总选路预算已耗尽".into(), timeout:true, local:false }));
        }
        let client = if active {
            let context = context.as_ref().expect("active context");
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let selected = match tokio::time::timeout(remaining, exchange.select(failed)).await {
                Ok(selected) => selected,
                Err(_) => return last.unwrap_or_else(|| Err(SendError { message:"轮询出口等待超时".into(), timeout:true, local:false })),
            };
            context.lock().await.adopt(&exchange);
            match selected {
                Ok(Some(mut proxy)) => {
                    egress::validate_account_route(Some(&proxy), front)
                        .map_err(|message| SendError { message, timeout:false, local:true })?;
                    if let Some(front) = front {
                        let endpoint = crate::server::core::proxy_chain::chain_endpoint(front, &proxy)
                            .ok_or_else(|| SendError { message:"建立代理链失败".into(), timeout:false, local:true })?;
                        let address: std::net::SocketAddr = endpoint.local_addr.parse()
                            .map_err(|e| SendError { message:format!("代理链端点无效: {e}"), timeout:false, local:true })?;
                        proxy.host = address.ip().to_string(); proxy.port = Some(address.port());
                        proxy.username.clear(); proxy.password.clear();
                    }
                    egress::client_for_with_tls(Some(&proxy), tls).as_ref().clone()
                },
                _ if last.is_some() => return last.take().unwrap(),
                Err(message) => return Err(SendError { message, timeout:false, local:true }),
                _ => return Err(SendError { message:"轮询没有可用出口".into(), timeout:false, local:true }),
            }
        } else { original_client.clone() };
        let Some(copy) = first.take().or_else(|| replay.as_ref().and_then(reqwest::Request::try_clone)) else {
            return last.unwrap_or_else(|| Err(SendError { message:"请求体不可重放".into(), timeout:false, local:true }));
        };
        if let Some(context) = &context { context.lock().await.network_started = true; }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        sends += 1;
        let mut result = match tokio::time::timeout(remaining, client.execute(copy)).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(e)) => Err(SendError { message:egress::describe_error_detail(&e), timeout:e.is_timeout(), local:e.is_builder() }),
            Err(_) => Err(SendError { message:"等待响应超时".into(), timeout:true, local:false }),
        };
        if let Some(lease) = exchange.lease() {
            result = result.map(|response| hold_response_lease(response, lease));
        }
        if !active { return result; }
        let status = result.as_ref().ok().map(|r| r.status().as_u16());
        let retry_after = result.as_ref().ok().map(|r| retry_after_delay(r.headers())).unwrap_or_default();
        let mut sample = String::new();
        if status.is_some_and(|s| s == 403 || s == 429) {
            if let Ok(response) = result {
                let (response, body) = buffer_error_sample(response, deadline.saturating_duration_since(tokio::time::Instant::now()).min(Duration::from_secs(2))).await;
                sample = String::from_utf8_lossy(&body).into_owned(); result = Ok(response);
            }
        }
        let same_exit_throttle = status == Some(429) && !exchange.all_errors() && !retried_throttle
            && short_throttle(&sample, retry_after) && retry_status(429,&sample,false,0).is_none();
        if same_exit_throttle && replay.is_some() && sends < crate::server::core::proxy_rotate::MAX_ATTEMPTS
            && retry_after < deadline.saturating_duration_since(tokio::time::Instant::now()) {
            retried_throttle = true;
            last = Some(match result { Ok(response) => Ok(buffer_error_response(response, deadline.saturating_duration_since(tokio::time::Instant::now()).min(Duration::from_secs(2))).await), error => error });
            crate::server::logging::log("[ProxyRotate]", &format!("attempt={sends} retry=short_throttle_same_exit status=429"));
            tokio::time::sleep(retry_after).await;
            failed = false; continue;
        }
        let reason = match &result {
            Ok(response) => retry_status(response.status().as_u16(), &sample, exchange.all_errors(), retries_500),
            Err(error) if !error.local => Some("transport"),
            _ => None,
        };
        if reason.is_some() { exchange.finish_health(true, retry_after); }
        else if status.is_some_and(|s| s < 400) {
            exchange.finish_health(false, Duration::ZERO);
            if let Some(context) = &context { context.lock().await.adopt(&exchange); }
            result = result.map(|response| hold_response_health(response, exchange.clone()));
        }
        // Unknown quota/auth errors are returned unchanged to provider refresh/classification.
        // No wait/replay after successful headers; the actual response Body keeps its lease.
        if reason.is_none() || replay.is_none() { return result; }
        if status == Some(500) { retries_500 += 1; }
        last = Some(match result {
            Ok(response) => Ok(buffer_error_response(response, deadline.saturating_duration_since(tokio::time::Instant::now()).min(Duration::from_secs(2))).await),
            error => error,
        });
        if let Some(context) = &context { context.lock().await.release_exchange_lease(); }
        if sends >= crate::server::core::proxy_rotate::MAX_ATTEMPTS || retry_after >= deadline.saturating_duration_since(tokio::time::Instant::now()) { return last.unwrap(); }
        // Fixed reason codes only: no URL, account, proxy label, response or credentials.
        crate::server::logging::log("[ProxyRotate]", &format!("attempt={sends} retry={} status={}", reason.unwrap(), status.unwrap_or(0)));
        if !retry_after.is_zero() { tokio::time::sleep(retry_after).await; }
        failed = true;
    }
}

fn short_throttle(body: &str, delay: Duration) -> bool {
    let body = body.to_ascii_lowercase();
    !delay.is_zero() && delay <= Duration::from_secs(5)
        && !["quota", "credit", "billing", "insufficient", "daily limit", "monthly limit", "余额", "额度", "配额"].iter().any(|s| body.contains(s))
        && ["too many requests", "rate limit", "rate_limit", "throttl"].iter().any(|s| body.contains(s))
}
fn retry_status(status: u16, body: &str, all_errors: bool, retries_500: usize) -> Option<&'static str> {
    if all_errors && (400..600).contains(&status) { return Some("all_errors"); }
    match status {
        502..=504 => Some("gateway"),
        500 if retries_500 == 0 => Some("server_500_once"),
        403 | 429 => {
            let body = body.to_ascii_lowercase();
            // Explicit exit-scoped signals only, never generic "quota/rate limit".
            ["ip_rate_limit", "ip rate limit", "rate limit per ip", "rate limit exceeded for ip",
             "ip address blocked", "ip_address_blocked", "unsupported_country", "country not supported",
             "region not supported", "geo_blocked", "地区不支持", "ip限流", "ip 限流"]
                .iter().any(|signal| body.contains(signal)).then_some("exit_restricted")
        }
        _ => None,
    }
}
fn retry_after_delay(headers: &reqwest::header::HeaderMap) -> Duration {
    let Some(value) = headers.get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()) else { return Duration::ZERO };
    if let Ok(seconds) = value.trim().parse::<u64>() { return Duration::from_secs(seconds.min(86400)); }
    chrono::DateTime::parse_from_rfc2822(value).ok()
        .map(|date| Duration::from_secs((date.timestamp() - chrono::Utc::now().timestamp()).clamp(0,86400) as u64)).unwrap_or_default()
}
// Inspect only a bounded prefix, retaining the untouched remainder for provider errors.
async fn buffer_error_sample(response: reqwest::Response, budget: Duration) -> (reqwest::Response, Vec<u8>) {
    use futures::StreamExt;
    let raw: axum::http::Response<reqwest::Body> = response.into();
    let (parts, body) = raw.into_parts();
    let mut stream = reqwest::Response::from(axum::http::Response::new(body)).bytes_stream();
    let mut chunks = Vec::new(); let mut sample = Vec::new();
    let _ = tokio::time::timeout(budget, async {
        while let Some(chunk) = stream.next().await {
            let done = match &chunk { Ok(bytes) => { let left = (64 * 1024usize).saturating_sub(sample.len()); sample.extend_from_slice(&bytes[..bytes.len().min(left)]); sample.len() >= 64*1024 }, Err(_) => true };
            chunks.push(chunk); if done { break; }
        }
    }).await;
    let body = reqwest::Body::wrap_stream(futures::stream::iter(chunks).chain(stream));
    (reqwest::Response::from(axum::http::Response::from_parts(parts, body)), sample)
}

fn hold_response_health(response: reqwest::Response, mut exchange: crate::server::core::proxy_rotate::Attempts) -> reqwest::Response {
    use futures::StreamExt;
    let raw: axum::http::Response<reqwest::Body> = response.into();
    let (parts, body) = raw.into_parts();
    let source = reqwest::Response::from(axum::http::Response::new(body)).bytes_stream();
    let held = source.map(move |chunk| {
        if chunk.is_err() { exchange.retire_failed_stream(); }
        chunk
    });
    reqwest::Response::from(axum::http::Response::from_parts(parts, reqwest::Body::wrap_stream(held)))
}

fn hold_response_lease<T: Send + Sync + 'static>(response: reqwest::Response, lease: std::sync::Arc<T>) -> reqwest::Response {
    use futures::StreamExt;
    let raw: axum::http::Response<reqwest::Body> = response.into();
    let (parts, body) = raw.into_parts();
    let source = reqwest::Response::from(axum::http::Response::new(body)).bytes_stream();
    let held = source.map(move |chunk| { let _lease = &lease; chunk });
    reqwest::Response::from(axum::http::Response::from_parts(parts, reqwest::Body::wrap_stream(held)))
}
async fn buffer_error_response(response: reqwest::Response, budget: Duration) -> reqwest::Response {
    use futures::StreamExt;
    let raw: axum::http::Response<reqwest::Body> = response.into();
    let (mut parts,body) = raw.into_parts();
    let mut stream = reqwest::Response::from(axum::http::Response::new(body)).bytes_stream();
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(budget, async {
        while let Some(Ok(chunk)) = stream.next().await {
            let left = (64 * 1024usize).saturating_sub(bytes.len());
            bytes.extend_from_slice(&chunk[..chunk.len().min(left)]);
            if bytes.len() == 64 * 1024 { break; }
        }
    }).await;
    drop(stream); // Drop the old sidecar body owner before resolving the next exit.
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    parts.headers.remove(axum::http::header::TRANSFER_ENCODING);
    reqwest::Response::from(axum::http::Response::from_parts(parts, reqwest::Body::from(bytes)))
}

/// 传输层失败（统一收敛成 502，与 Node 的 fetchViaProxy 一致）
#[derive(Clone, Debug)]
pub struct UpstreamRequestError {
    pub local: bool,
    pub message: String,
    /// 简短原因（重试链里「这次为什么重试」的展示文案）：
    /// 「连接中超时(N秒)」/「等待响应超时(N秒)」/「上游连接失败」。
    /// `message` 是最终失败的详细文案（带出口说明），这里是它的简短形态。
    pub reason: String,
}

impl UpstreamRequestError {
    pub fn to_gateway_error(&self) -> GatewayError {
        GatewayError::with_status(502, self.message.clone())
    }
}

#[cfg(test)]
mod failover_tests {
    use super::*;
    use crate::server::core::proxy_rotate::{Attempts, ATTEMPTS};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn proxy(status: u16, body: &'static str) -> (Value, tokio::sync::mpsc::UnboundedReceiver<String>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else { break };
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") { match s.read_u8().await { Ok(b) => head.push(b), Err(_) => return } }
                let text = String::from_utf8(head.clone()).unwrap();
                let len = text.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length: ").and_then(|s| s.parse::<usize>().ok())).unwrap_or(0);
                let mut payload = vec![0;len]; s.read_exact(&mut payload).await.unwrap(); head.extend(payload);
                let _ = tx.send(String::from_utf8(head).unwrap());
                s.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        });
        (json!({"id":format!("mock-{port}"),"source":"manual","protocol":"http","host":"127.0.0.1","port":port}), rx, task)
    }
    #[test]
    fn smart_matrix_and_retry_after_are_conservative() {
        for status in [400,401,402,404,408,409,413,422,500,501] {
            assert!(retry_status(status,"",false,1).is_none(),"{status}");
        }
        for status in [403,429] {
            for body in ["quota exhausted","rate limit exceeded","insufficient credits","forbidden", "token expired"] {
                assert!(retry_status(status,body,false,0).is_none());
            }
            assert_eq!(retry_status(status,"IP_RATE_LIMIT",false,0),Some("exit_restricted"));
            assert_eq!(retry_status(status,"unsupported_country",false,0),Some("exit_restricted"));
        }
        assert!(retry_status(500,"",false,0).is_some());
        for status in [502,503,504] { assert!(retry_status(status,"",false,1).is_some()); }
        for status in [400,401,403,413,429,500] { assert_eq!(retry_status(status,"",true,3),Some("all_errors")); }
        let mut headers=reqwest::header::HeaderMap::new(); headers.insert("retry-after","12".parse().unwrap());
        assert_eq!(retry_after_delay(&headers),Duration::from_secs(12));
        headers.insert("retry-after",(chrono::Utc::now()+chrono::Duration::seconds(60)).to_rfc2822().parse().unwrap());
        assert!((58..=60).contains(&retry_after_delay(&headers).as_secs()));
    }
    #[tokio::test]
    async fn deterministic_errors_do_not_visit_second_exit_and_keep_original_body() {
        for (status,body) in [(400,"bad request"),(401,"refresh token"),(403,"forbidden"),(413,"too large"),(422,"invalid"),(429,"quota exhausted")] {
            let (a,mut arx,at)=proxy(status,body).await; let (b,mut brx,bt)=proxy(200,"wrong").await;
            let key=new_request_id(); let route=Arc::new(tokio::sync::Mutex::new(Attempts::mock(&key,vec![a,b])));
            ATTEMPTS.scope(route,async {
                let response=send_builder(reqwest::Client::new().post("http://example.invalid/").body("fixed")).await.unwrap();
                assert_eq!(response.status(),status); assert_eq!(response.text().await.unwrap(),body);
            }).await;
            assert!(arx.recv().await.is_some()); assert!(brx.try_recv().is_err());
            at.abort();bt.abort();crate::server::core::proxy_rotate::retire(&key);
        }
    }
    #[tokio::test]
    async fn only_one_500_retry_but_gateway_failures_have_five_attempt_cap() {
        for (status,count) in [(500,2),(503,5)] {
            let mut items=Vec::new();let mut receivers=Vec::new();let mut tasks=Vec::new();
            for _ in 0..7 { let (item,rx,task)=proxy(status,"failure").await;items.push(item);receivers.push(rx);tasks.push(task); }
            let key=new_request_id();let route=Arc::new(tokio::sync::Mutex::new(Attempts::mock(&key,items)));
            ATTEMPTS.scope(route,async { assert_eq!(send_builder(reqwest::Client::new().post("http://example.invalid/").body("fixed")).await.unwrap().status(),status); }).await;
            for (i,rx) in receivers.iter_mut().enumerate() { assert_eq!(rx.try_recv().is_ok(),i<count); }
            for task in tasks { task.abort(); } crate::server::core::proxy_rotate::retire(&key);
        }
    }
    #[tokio::test]
    async fn aggressive_errors_and_explicit_ip_restrictions_can_rotate() {
        for aggressive in [false,true] {
            let (a,mut arx,at)=proxy(if aggressive {401}else{403},"ip_address_blocked").await;
            let (b,mut brx,bt)=proxy(200,"ok").await;let key=new_request_id();
            let attempts=if aggressive {Attempts::mock_aggressive(&key,vec![a,b])}else{Attempts::mock(&key,vec![a,b])};
            ATTEMPTS.scope(Arc::new(tokio::sync::Mutex::new(attempts)),async {assert_eq!(send_builder(reqwest::Client::new().post("http://example.invalid/").body("fixed")).await.unwrap().text().await.unwrap(),"ok");}).await;
            assert!(arx.recv().await.is_some());assert!(brx.recv().await.is_some());at.abort();bt.abort();crate::server::core::proxy_rotate::retire(&key);
        }
    }
    #[tokio::test]
    async fn connection_failure_rotates_without_rebuilding_payload() {
        let dead=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let port=dead.local_addr().unwrap().port();drop(dead);
        let (b,mut rx,task)=proxy(200,"ok").await;let key=new_request_id();
        let route=Arc::new(tokio::sync::Mutex::new(Attempts::mock(&key,vec![json!({"id":format!("dead-{port}"),"source":"manual","host":"127.0.0.1","port":port}),b])));
        ATTEMPTS.scope(route,async {assert_eq!(send_builder(reqwest::Client::new().post("http://example.invalid/").body("fixed-payload")).await.unwrap().status(),200);}).await;
        assert!(rx.recv().await.unwrap().ends_with("fixed-payload"));task.abort();crate::server::core::proxy_rotate::retire(&key);
    }
    #[tokio::test]
    async fn preparation_and_http_share_deadline() {
        let (a,mut rx,task)=proxy(200,"late").await;let key=new_request_id();
        let mut attempts=Attempts::mock(&key,vec![a]); attempts.deadline=Some(tokio::time::Instant::now()-Duration::from_millis(1));
        let route=Arc::new(tokio::sync::Mutex::new(attempts));
        let error=ATTEMPTS.scope(route,send_builder(reqwest::Client::new().post("http://example.invalid/").body("fixed"))).await.unwrap_err();
        assert!(error.timeout);assert!(rx.try_recv().is_err());task.abort();
    }
    #[tokio::test]
    async fn short_retry_after_retries_same_exit_once_but_quota_does_not() {
        for (body,expected) in [("Too many requests",2),("quota exhausted",1)] {
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let port=listener.local_addr().unwrap().port();
            let hits=Arc::new(std::sync::atomic::AtomicUsize::new(0));let counter=hits.clone();
            let server=tokio::spawn(async move {loop {let (mut s,_)=listener.accept().await.unwrap();let mut buf=[0;4096];let _=s.read(&mut buf).await;counter.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
                s.write_all(format!("HTTP/1.1 429 Test\r\nRetry-After: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();}});
            let (b,mut rx,task)=proxy(200,"wrong").await;let key=new_request_id();
            let route=Arc::new(tokio::sync::Mutex::new(Attempts::mock(&key,vec![json!({"id":format!("short-{port}"),"source":"manual","host":"127.0.0.1","port":port}),b])));
            let start=std::time::Instant::now();
            ATTEMPTS.scope(route,async {let r=send_builder(reqwest::Client::new().post("http://example.invalid/").body("fixed")).await.unwrap();assert_eq!(r.status(),429);assert_eq!(r.text().await.unwrap(),body);}).await;
            if expected==2 {assert!(start.elapsed()>=Duration::from_secs(1));}
            assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst),expected);assert!(rx.try_recv().is_err());
            server.abort();task.abort();crate::server::core::proxy_rotate::retire(&key);
        }
    }
    #[tokio::test]
    async fn retry_after_over_budget_returns_original_response_without_rotation() {
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let port=listener.local_addr().unwrap().port();
        let server=tokio::spawn(async move { let (mut s,_)=listener.accept().await.unwrap();let mut buf=[0;4096];let _=s.read(&mut buf).await;
            s.write_all(b"HTTP/1.1 429 Test\r\nRetry-After: 3600\r\nContent-Length: 13\r\nConnection: close\r\n\r\nip_rate_limit").await.unwrap(); });
        let (b,mut rx,task)=proxy(200,"wrong").await;let key=new_request_id();
        let route=Arc::new(tokio::sync::Mutex::new(Attempts::mock(&key,vec![json!({"id":format!("retry-{port}"),"source":"manual","host":"127.0.0.1","port":port}),b])));
        ATTEMPTS.scope(route,async { let r=send_builder(reqwest::Client::new().post("http://example.invalid/").body("fixed")).await.unwrap();assert_eq!(r.status(),429);assert_eq!(r.headers()["retry-after"],"3600");assert_eq!(r.text().await.unwrap(),"ip_rate_limit"); }).await;
        assert!(rx.try_recv().is_err());server.await.unwrap();task.abort();crate::server::core::proxy_rotate::retire(&key);
    }
    #[tokio::test]
    async fn successful_sse_outlives_selection_deadline() {
        use futures::StreamExt;
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let port=listener.local_addr().unwrap().port();
        // Share an absolute deadline: the body must arrive after it, while header
        // selection has enough headroom on a busy parallel Windows test runner.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let server=tokio::spawn(async move { let (mut s,_)=listener.accept().await.unwrap();let mut buf=[0;4096];let _=s.read(&mut buf).await;
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: first\n\n").await.unwrap();
            tokio::time::sleep_until(deadline + Duration::from_millis(50)).await;s.write_all(b"data: later\n\n").await.unwrap(); });
        let key=new_request_id();let mut a=Attempts::mock(&key,vec![json!({"id":format!("sse-{port}"),"source":"manual","host":"127.0.0.1","port":port})]);
        a.deadline=Some(deadline);
        ATTEMPTS.scope(Arc::new(tokio::sync::Mutex::new(a)),async { let r=send_builder(reqwest::Client::new().post("http://example.invalid/").body("fixed")).await.unwrap();let mut stream=r.bytes_stream();let mut bytes=Vec::new();while let Some(chunk)=stream.next().await {bytes.extend(chunk.unwrap());}assert!(String::from_utf8(bytes).unwrap().contains("later")); }).await;
        server.await.unwrap();crate::server::core::proxy_rotate::retire(&key);
    }
    #[tokio::test]
    async fn http_500_switches_once_and_preserves_payload_headers_then_sticks() {
        let (a, mut arx, at) = proxy(500,"failed").await;
        let (b, mut brx, bt) = proxy(200,"ok").await;
        let key = new_request_id();
        let route = Arc::new(tokio::sync::Mutex::new(Attempts::mock(&key, vec![a,b])));
        ATTEMPTS.scope(route.clone(), async {
            for _ in 0..2 {
                let request = reqwest::Client::new().post("http://example.invalid/chat")
                    .header("authorization","Bearer fixed-key").header("x-session-id","unchanged")
                    .body("{\"messages\":[\"same\"]}");
                assert_eq!(send_builder(request).await.unwrap().text().await.unwrap(),"ok");
            }
        }).await;
        let first = arx.recv().await.unwrap();
        let second = brx.recv().await.unwrap();
        assert_eq!(first,second);
        assert_eq!(second,brx.recv().await.unwrap());
        assert!(arx.try_recv().is_err());
        crate::server::core::proxy_rotate::retire(&key); at.abort(); bt.abort();
    }
    #[tokio::test]
    async fn actual_response_body_owns_lease_after_head_consumed() {
        use futures::StreamExt;
        struct Lease(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Lease { fn drop(&mut self) { self.0.fetch_add(1,std::sync::atomic::Ordering::SeqCst); } }
        let drops=Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let lease=Arc::new(Lease(drops.clone()));
        let (tx,rx)=tokio::sync::mpsc::channel::<Result<bytes::Bytes,std::io::Error>>(2);
        let response=reqwest::Response::from(axum::http::Response::new(reqwest::Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(rx))));
        let response=hold_response_lease(response,lease.clone());drop(lease);
        let mut stream=response.bytes_stream();
        assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst),0);
        tx.send(Ok(bytes::Bytes::from_static(b"partial"))).await.unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap(),"partial");
        assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst),0);
        drop(tx);assert!(stream.next().await.is_none());drop(stream);
        assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst),1);
    }
    #[tokio::test]
    async fn new_http_exchange_can_retry_after_previous_exchange_exhaustion() {
        let (a,mut rx,task)=proxy(401,"original error body").await;
        let key=new_request_id();
        let route=Arc::new(tokio::sync::Mutex::new(Attempts::mock(&key,vec![a])));
        ATTEMPTS.scope(route,async {
            for token in ["old","refreshed"] {
                let response=send_builder(reqwest::Client::new().post("http://example.invalid/").header("authorization",token).body("same")).await.unwrap();
                assert_eq!(response.status(),401); assert_eq!(response.text().await.unwrap(),"original error body");
            }
            // The project's spawn helper must retain this route beyond its parent scope.
            let inherited=crate::spawn_task(async { crate::server::core::proxy_rotate::context().is_some() }).await.unwrap();
            assert!(inherited);
        }).await;
        assert!(rx.recv().await.unwrap().contains("authorization: old"));
        assert!(rx.recv().await.unwrap().contains("authorization: refreshed"));
        task.abort();crate::server::core::proxy_rotate::retire(&key);
    }
    #[tokio::test]
    async fn local_builder_error_does_not_send_or_rotate() {
        let (a, mut rx, task) = proxy(200,"ok").await;
        let key = new_request_id();
        let route = Arc::new(tokio::sync::Mutex::new(Attempts::mock(&key,vec![a])));
        ATTEMPTS.scope(route, async {
            let result = send_builder(reqwest::Client::new().post("http://example.invalid/").header("bad\nheader","v")).await;
            assert!(result.unwrap_err().local);
        }).await;
        assert!(rx.try_recv().is_err()); task.abort();
    }
    #[tokio::test]
    async fn noncloneable_body_is_sent_once_without_rotation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut s,_) = listener.accept().await.unwrap();
            let mut bytes=[0;4096]; let _ = s.read(&mut bytes).await.unwrap();
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await.unwrap();
        });
        let stream = futures::stream::iter(vec![Ok::<_,std::io::Error>(bytes::Bytes::from_static(b"hello"))]);
        let response = send_builder(reqwest::Client::new().post(format!("http://{address}/")).body(reqwest::Body::wrap_stream(stream))).await.unwrap();
        assert_eq!(response.status(),200); task.await.unwrap();
    }
    #[tokio::test]
    async fn cancelled_send_does_not_try_another_exit() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port=listener.local_addr().unwrap().port();
        let (b,mut rx, task) = proxy(200,"ok").await;
        let key=new_request_id();
        let route=Arc::new(tokio::sync::Mutex::new(Attempts::mock(&key,vec![json!({"id":"stalled","source":"manual","host":"127.0.0.1","port":port}),b])));
        let result=tokio::time::timeout(Duration::from_millis(100),ATTEMPTS.scope(route,send_builder(reqwest::Client::new().post("http://example.invalid/").body("fixed")))).await;
        assert!(result.is_err()); assert!(rx.try_recv().is_err());
        crate::server::core::proxy_rotate::retire(&key); task.abort();
    }
    #[tokio::test]
    async fn old_body_error_does_not_retire_new_exchange_through_forward_stream_context() {
        use futures::StreamExt;
        use crate::server::core::upstream::{ForwardStream, connections::{ConnectionGuard, Connections}, sse::FramePolicy, usage::RequestTelemetry};
        let (a,_,at)=proxy(200,"a").await;let (b,_,bt)=proxy(200,"b").await;
        let key=new_request_id();let origin=url::Url::parse("http://snapshot.invalid/").unwrap();
        let mut old=Attempts::mock(&key,vec![a.clone()]);old.set_origin(&origin);old.select(false).await.unwrap();old.finish_health(false,Duration::ZERO);
        let context=Arc::new(tokio::sync::Mutex::new(old.clone()));
        let (tx,rx)=tokio::sync::mpsc::channel::<Result<bytes::Bytes,std::io::Error>>(1);
        let raw=reqwest::Response::from(axum::http::Response::new(reqwest::Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(rx))));
        let response=hold_response_health(raw,old);
        let inner=response.bytes_stream().map(|chunk|chunk.map_err(std::io::Error::other));
        let mut forwarded=ATTEMPTS.scope(context.clone(),async {ForwardStream::from_translated(Box::pin(inner),None,ConnectionGuard::new(Connections::new()),Arc::new(RequestTelemetry::new()),FramePolicy::default())}).await;
        crate::server::core::proxy_rotate::retire(&key);
        let mut new=Attempts::mock(&key,vec![b.clone()]);new.set_origin(&origin);let expected=new.select(false).await.unwrap().unwrap();new.finish_health(false,Duration::ZERO);
        context.lock().await.adopt(&new);
        tx.send(Err(std::io::Error::other("old body failure"))).await.unwrap();drop(tx);
        while forwarded.next().await.is_some() {}
        tokio::task::yield_now().await;tokio::time::sleep(Duration::from_millis(20)).await;
        // Re-acquire health: the old mutable-context callback would have poisoned B.
        let mut verify=Attempts::mock(&key,vec![b]);verify.set_origin(&origin);
        assert_eq!(verify.select(false).await.unwrap().unwrap().port,expected.port);
        let mut failed=Attempts::mock("old-body-origin",vec![a]);failed.set_origin(&origin);assert!(failed.select(false).await.is_err());
        at.abort();bt.abort();crate::server::core::proxy_rotate::retire(&key);
    }
    #[tokio::test]
    async fn actual_body_failure_records_origin_health_after_sticky_retirement() {
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let port=listener.local_addr().unwrap().port();
        let (release,wait)=tokio::sync::oneshot::channel();
        let server=tokio::spawn(async move {let (mut s,_)=listener.accept().await.unwrap();let mut buf=[0;4096];let _=s.read(&mut buf).await;
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\nConnection: close\r\n\r\npartial").await.unwrap();let _=wait.await;});
        let key=new_request_id();let items=vec![json!({"id":format!("body-health-{port}"),"source":"manual","host":"127.0.0.1","port":port})];
        let route=Arc::new(tokio::sync::Mutex::new(Attempts::mock(&key,items.clone())));
        let response=ATTEMPTS.scope(route,send_builder(reqwest::Client::new().post("http://body-health.invalid/").body("fixed"))).await.unwrap();
        crate::server::core::proxy_rotate::retire(&key);release.send(()).unwrap();assert!(response.bytes().await.is_err());
        let mut next=Attempts::mock(&key,items.clone());next.set_origin(&url::Url::parse("http://body-health.invalid/").unwrap());assert!(next.select(false).await.is_err());
        let mut other=Attempts::mock(&key,items);other.set_origin(&url::Url::parse("http://other-health.invalid/").unwrap());assert!(other.select(false).await.is_ok());
        server.await.unwrap();crate::server::core::proxy_rotate::retire(&key);
    }
    #[tokio::test]
    async fn successful_headers_and_partial_body_never_replay() {
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port=listener.local_addr().unwrap().port();
        let partial=tokio::spawn(async move {
            let (mut s,_)=listener.accept().await.unwrap(); let mut buf=[0;4096]; let _=s.read(&mut buf).await;
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\nConnection: close\r\n\r\ndata: partial\n\n").await.unwrap();
        });
        let (b,mut rx, task)=proxy(200,"duplicate").await;
        let key=new_request_id();
        let route=Arc::new(tokio::sync::Mutex::new(Attempts::mock(&key,vec![json!({"id":"partial","source":"manual","host":"127.0.0.1","port":port}),b])));
        ATTEMPTS.scope(route,async {
            let response=send_builder(reqwest::Client::new().post("http://example.invalid/").body("fixed")).await.unwrap();
            assert!(response.bytes().await.is_err());
        }).await;
        assert!(rx.try_recv().is_err()); partial.await.unwrap(); task.abort(); crate::server::core::proxy_rotate::retire(&key);
    }
}

/// 生成一轮对话的追踪 id（对应 Node 的 `randomUUID()`）。
///
/// 用 RFC 4122 v4 形态：上游会把 X-Request-ID 记进服务端日志，
/// 保持标准 UUID 形态便于与官方客户端的行为对齐（也便于下游按 UUID 解析）。
/// 随机源取自 `RandomState`（由 OS 随机种子初始化）+ 进程内计数器 + 纳秒时钟，
/// 三者拼出的 id 在单机排障场景足够唯一 —— 它不是安全凭证，不用引 rand 依赖。
pub fn new_request_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut bytes = [0u8; 16];
    let mix = |label: &[u8], salt: u64| -> u64 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write(label);
        hasher.write_u64(salt);
        hasher.finish()
    };
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let first = mix(b"wb-request-id-a", nanos ^ pid);
    let second = mix(b"wb-request-id-b", counter.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    bytes[..8].copy_from_slice(&first.to_be_bytes());
    bytes[8..].copy_from_slice(&second.to_be_bytes());
    // 版本位 4 + 变体位 10xx（RFC 4122）
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

// ─── UUIDv7 家族（WorkBuddy 对话链路的会话/消息/链路 id）──────
//
// 2026-10-16 对国际版桌面客户端 5.8.4 的抓包结论（参考实现
// workbuddy2api-hub #381，本仓与之对齐）：官方客户端的请求 / 消息 id 是
// 32 位无连字符 hex、会话 id 是带连字符的 UUID，都是 UUIDv7 家族
// （48 位毫秒时间戳打头，时间有序）。上游不校验版本位 —— 抓包里主对话的
// X-Conversation-ID 实际是 v4、辅助请求是 v7，两种都被正常接受 —— 所以
// 形态只求贴近，不承担语义。上游也不拿这些 id 存对话历史（同一 id 连发两轮、
// 第二轮只带 system+user，模型仍答不出上一轮），因此**每次请求现生成**，
// 不按会话维护映射；缺了整个头族才会触发渠道校验（400 / code 11-128，
// `conversation id missing`）。

/// 生成 16 字节的 UUIDv7（RFC 9562）：毫秒时间戳 + 版本位 7 + 变体位 10。
///
/// 随机源与 [`new_request_id`] 同款（`RandomState` 种子 + 进程内计数器），
/// 不为此引入新的随机依赖。
fn uuid7_bytes() -> [u8; 16] {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mix = |label: &[u8], salt: u64| -> u64 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write(label);
        hasher.write_u64(salt);
        hasher.finish()
    };
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
        & ((1 << 48) - 1);
    let rand_a = mix(b"wb-uuid7-a", counter) & 0x0fff;
    let rand_b = mix(b"wb-uuid7-b", counter.wrapping_mul(0x9E37_79B9_7F4A_7C15)) & ((1 << 62) - 1);
    let int = ((millis as u128) << 80)
        | (0x7u128 << 76)
        | ((rand_a as u128) << 64)
        | (0b10u128 << 62)
        | (rand_b as u128);
    int.to_be_bytes()
}

/// 请求 / 消息 id（`X-Request-ID` / `X-Conversation-Message-ID` /
/// `X-Conversation-Request-ID` / `X-Root-Request-ID`）：32 位无连字符 hex。
pub fn new_message_id() -> String {
    let bytes = uuid7_bytes();
    let mut id = String::with_capacity(32);
    for byte in bytes {
        id.push_str(&format!("{byte:02x}"));
    }
    id
}

/// 会话 id（`X-Conversation-ID`）：带连字符的 UUID 形态。
pub fn new_conversation_id() -> String {
    let bytes = uuid7_bytes();
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

/// 链路追踪的 span id（`traceparent` / b3 / `X-B3-SpanId`）：16 位 hex。
pub fn new_span_id() -> String {
    let bytes = uuid7_bytes();
    let mut id = String::with_capacity(16);
    for byte in &bytes[..8] {
        id.push_str(&format!("{byte:02x}"));
    }
    id
}
