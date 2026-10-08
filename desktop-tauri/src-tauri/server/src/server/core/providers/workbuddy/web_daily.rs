//! WorkBuddy **国际版**的「每日活跃打卡」。
//!
//! ── 上游事实（照抄 hub 的实测结论，issue #75 / #59 / #90）─────
//! 国际版没有国内站那种「每日签到」接口，官方的每日活跃奖励认的是
//! **跑完的网页端 agent 会话**：
//!   - 桌面身份发 `chat/completions` **不计数**（#75、#59 实测）；
//!   - 只建会话、不接沙箱也**不计数**（#90 实测）—— 会话会永远停在
//!     `CREATING`、没有任何输出。
//!
//! 所以完整的一次打卡 = 两件事都做：
//!   ① 桌面端身分的轻量对话（`POST {endpoint}/v2/chat/completions`，
//!      与转发同一条链路、同一套头集）；
//!   ② 网页通道的 agent 会话，并且**跑到 `completed`**：
//!      `POST /console/as/conversations/`（只排队）
//!      → `GET …/{id}/session` 拿沙箱 `link` + `token`
//!      → ACP over HTTP+SSE 的 `initialize` / `session/load` / `session/prompt`
//!      → 轮询 `GET …/{id}` 到 `completed`。
//!
//! ── 为什么网页通道只用两个凭据头 ────────────────────────────
//! 网页版 app 的出站头只有 `Authorization: Bearer <accessToken>` 与
//! `X-User-Id`，**没有**桌面端的 `X-IDE-*` 指纹（见 [`web_headers`]）——
//! 网关手里同一份账号凭据可以直接用，不需要另做一套登录。
//!
//! ── ACP 传输（[`AcpChannel`]）───────────────────────────────
//! 一条长连 SSE 收事件、若干短连 POST 发请求：
//!   1. `GET <link>`（`Accept: text/event-stream`）→ 响应头里的
//!      `Acp-Connection-Id` 是后续所有 POST 的关联键；
//!   2. `POST <link>`（JSON-RPC 2.0，带该头）→ 服务端 202 受理，
//!      结果通过第 1 条 SSE 推回来；
//!   3. 本轮结束的判据是**会话状态**（`GET …/{id}` 的 `status`），
//!      不是 SSE 断开 —— 断流只说明这一轮没有更多事件。
//!
//! 打卡只需要 agent 自己把话说完，因此 `clientCapabilities` 里
//! **文件系统与终端全部关掉**（照抄 hub）：开着的话沙箱会等我们回调，
//! 而本实现不提供这些回调，只会白等到超时。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::egress;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::core::providers::adapter::ProviderAdapter;
use crate::server::errors::GatewayError;
use crate::server::logging;

/// 网页版 app 的来源（`Origin` / `Referer` 用）。
const WEB_ORIGIN: &str = "https://www.workbuddy.ai";
/// 网页端会话集合（建会话用；GET 单条时后面拼 id）。
const WEB_CONVERSATIONS_PATH: &str = "/console/as/conversations/";
/// 网页端浏览器 UA（照抄 hub 实测值）。
const WEB_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 Edg/140.0.0.0";
/// 打卡用的模型（hub 实测：轻、快、够用）。
const DAILY_CHAT_MODEL: &str = "deepseek-v4.1-flash";
/// 打卡用的提示词（一句话即可，内容不影响计数）。
const DAILY_CHAT_PROMPT: &str = "Hi";
/// 单请求超时（建会话 / 取沙箱 / 轮询状态）。
const REQUEST_TIMEOUT_MS: u64 = 30_000;
/// 网页通道一轮最多等多久（实测一次「Hi」十几秒跑完；沙箱冷启动会慢一些，
/// 留足余量）。判据是**事件级结束信号 + 会话状态**双保险，所以这里只是兜底。
const TURN_TIMEOUT_MS: u64 = 180_000;
/// 轮询会话状态的间隔。
const POLL_INTERVAL_MS: u64 = 3_000;
/// ACP 协议版本（照抄 hub）。
const ACP_PROTOCOL_VERSION: i64 = 1;

/// 该账号是否走「每日活跃打卡」：WorkBuddy 系 + 国际版。
///
/// 国内版走的是腾讯的每日签到接口（`billing::checkin`），与国际版这条
/// 网页通道互不相通 —— 拿国内版账号去打网页通道（或反过来）只会白跑一轮。
pub fn supports_daily_chat(account: &Value) -> bool {
    let provider = account
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or(crate::server::core::providers::DEFAULT_PROVIDER_ID);
    provider == crate::server::core::providers::DEFAULT_PROVIDER_ID
        && account.get("edition").and_then(Value::as_str) == Some("intl")
}

/// 跑一次完整的每日活跃打卡，返回 `{success, msg, ...}`（签到的 claim 形状）。
///
/// ── 成败口径 ────────────────────────────────────────────────
/// `success` 只由**网页通道**决定 —— 官方只认「跑完的 agent 会话」，
/// 桌面端那一步是既有习惯（hub 也发），失败只记进 `desktopOk` 不阻断：
/// 为一个不影响计数的一半把整轮判失败，会让用户白等一天。
pub async fn run_daily_chat(
    store: &AccountStore,
    account: &Value,
) -> Result<Value, GatewayError> {
    let id = account.get("id").and_then(Value::as_str).unwrap_or("");
    let Some(entry) = store.get_session_by_id(id) else {
        return Err(GatewayError::with_status(401, "没有可用凭证，无法打卡"));
    };
    let session = entry.session;
    let token = session
        .pointer("/auth/accessToken")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let uid = session
        .pointer("/account/uid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if token.is_empty() || uid.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "国际版账号缺少 accessToken / uid，无法走网页通道打卡",
        ));
    }
    let client = egress::client_for(proxy_of(&session).as_ref());

    // ① 桌面端身分的轻量对话（best effort，见函数头的成败口径）
    let desktop = desktop_light_chat(&client, &session).await;
    if let Err(reason) = &desktop {
        logging::verbose(
            "[Accounts]",
            &format!("账号 {id} 的桌面端轻量对话未完成（{reason}），继续走网页通道"),
        );
    }

    // ② 网页通道：建会话 → 接沙箱 → 把这一轮跑完
    let web = web_turn(&client, &token, &uid).await?;
    let chunks = web.get("chunks").and_then(Value::as_u64).unwrap_or(0);
    let elapsed = web.get("elapsedMs").and_then(Value::as_u64).unwrap_or(0);
    let ok = web.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if !ok {
        let reason = web
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("网页通道未跑完");
        return Ok(json!({
            "success": false,
            "msg": format!("每日活跃打卡未完成（网页通道：{reason}）"),
            "desktopOk": desktop.is_ok(),
            "web": web,
        }));
    }
    Ok(json!({
        "success": true,
        "msg": format!("每日活跃对话完成（网页通道 {chunks} 段输出，{elapsed} ms）"),
        "desktopOk": desktop.is_ok(),
        "web": web,
    }))
}

/// 桌面端身分的轻量对话：`POST {endpoint}/v2/chat/completions`（流式，读完即弃）。
///
/// 请求体交给本家适配器的 `build_chat_request` 构造 —— 它与转发链路是**同一段
/// 逻辑**（出站归一 + 注入兜底 system 消息）。这一点不是可选的美化：上游要求
/// 首条消息必须是 system，裸发一条 user 消息会稳定 400（实测如此）。
async fn desktop_light_chat(
    client: &reqwest::Client,
    session: &Value,
) -> Result<String, String> {
    let body = json!({
        "model": DAILY_CHAT_MODEL,
        "messages": [{ "role": "user", "content": DAILY_CHAT_PROMPT }],
        "stream": true,
        "max_tokens": 10,
        "reasoning_effort": "none",
    });
    let plan = super::WORKBUDDY_ADAPTER
        .build_chat_request(session, &body, &axum::http::HeaderMap::new())
        .map_err(|error| error.message)?;
    let mut request = client
        .post(&plan.url)
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .json(&plan.body);
    for (name, value) in plan.headers {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|error| egress::describe_error_detail(&error))?;
    let status = response.status().as_u16();
    // 流式响应要读到结束才算这一轮真的发出去过；内容本身不关心
    let _ = response.bytes().await;
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    Ok("桌面端轻量对话已完成".to_string())
}

/// 网页通道的一轮：建会话 → 取沙箱 → ACP 驱动 → 轮询到 `completed`。
async fn web_turn(
    client: &reqwest::Client,
    token: &str,
    uid: &str,
) -> Result<Value, GatewayError> {
    let base = format!("{WEB_ORIGIN}{WEB_CONVERSATIONS_PATH}");
    let body = json!({
        "prompt": DAILY_CHAT_PROMPT,
        "model": DAILY_CHAT_MODEL,
        // 网页端建会话时固定带上这两项（hub 抓包所得），保持请求形态一致
        "conversationOrigin": "workbuddy-app",
        "plugins": [{ "name": "weixinpay", "marketplace": "codebuddy-builtin" }],
    });
    let response = apply_headers(
        client.post(&base).timeout(Duration::from_millis(REQUEST_TIMEOUT_MS)).json(&body),
        token,
        uid,
    )
    .send()
    .await
    .map_err(|error| {
        GatewayError::with_status(502, format!("网页通道建会话失败：{}", egress::describe_error_detail(&error)))
    })?;
    let status = response.status().as_u16();
    let payload: Value = response.json().await.unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        return Ok(json!({
            "ok": false,
            "error": format!("建会话 HTTP {status}: {}", truncate(&payload.to_string(), 120)),
        }));
    }
    let code = payload.get("code").and_then(Value::as_i64);
    let conversation = payload
        .pointer("/data/id")
        .map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    if code.unwrap_or(0) != 0 || conversation.is_empty() {
        return Ok(json!({
            "ok": false,
            "error": format!(
                "建会话被拒（code={} msg={}）",
                code.map(|value| value.to_string()).unwrap_or_else(|| "-".to_string()),
                payload.get("msg").and_then(Value::as_str).unwrap_or("")
            ),
        }));
    }

    // 沙箱信息：link（ACP 地址）+ token（沙箱自己的凭据，不是账号 token）
    let session_url = format!("{base}{conversation}/session");
    let info: Value = apply_headers(
        client.get(&session_url).timeout(Duration::from_millis(REQUEST_TIMEOUT_MS)),
        token,
        uid,
    )
    .send()
    .await
    .map_err(|error| {
        GatewayError::with_status(502, format!("网页通道取沙箱失败：{}", egress::describe_error_detail(&error)))
    })?
    .json()
    .await
    .unwrap_or(Value::Null);
    let sandbox = info.get("data").cloned().unwrap_or(Value::Null);
    let text_of = |key: &str| {
        sandbox
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let link = {
        let link = text_of("link");
        if link.is_empty() { text_of("endpoint") } else { link }
    };
    let sandbox_token = text_of("token");
    let session_id = {
        let value = text_of("sessionId");
        if value.is_empty() { text_of("session_id") } else { value }
    };
    let session_id = if session_id.is_empty() { conversation.clone() } else { session_id };
    let cwd = {
        let value = text_of("cwd");
        if value.is_empty() { "/workspace".to_string() } else { value }
    };
    if link.is_empty() || sandbox_token.is_empty() {
        return Ok(json!({
            "ok": false,
            "conversation": conversation,
            "error": "沙箱未就绪（没有 link/token）",
        }));
    }

    let started = Instant::now();
    let mut channel = AcpChannel::new(client.clone(), link, sandbox_token);
    let mut status_text = String::new();
    let mut error = String::new();
    let mut finished = false;
    if let Err(failure) = channel.open().await {
        error = failure.message;
    } else {
        let steps = [
            ("initialize", json!({
                "protocolVersion": ACP_PROTOCOL_VERSION,
                // 打卡不需要文件系统/终端：开着沙箱会等我们回调（见模块头）
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false,
                },
            })),
            ("session/load", json!({
                "sessionId": session_id,
                "cwd": cwd,
                "mcpServers": [],
            })),
            ("session/prompt", json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": DAILY_CHAT_PROMPT }],
            })),
        ];
        for (index, (method, params)) in steps.into_iter().enumerate() {
            if let Err(failure) = channel.request(method, params, index as i64 + 1).await {
                error = failure.message;
                break;
            }
        }
    }

    let deadline = Instant::now() + Duration::from_millis(TURN_TIMEOUT_MS);
    while error.is_empty() && Instant::now() < deadline {
        // 事件级的结束信号优先（`isFinalStop`）——它比会话状态更早到达，
        // 少等一个轮询周期；状态是兜底（协议变化时仍能收敛）
        if channel.finished() {
            finished = true;
            break;
        }
        status_text = conversation_status(client, &base, &conversation, token, uid).await;
        if status_text == "completed" {
            finished = true;
            break;
        }
        if status_text == "failed" || status_text == "error" {
            error = format!("会话状态={status_text}");
            break;
        }
        tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
    if error.is_empty() && !finished {
        error = format!(
            "会话在 {}s 内没有跑完（状态={}）",
            TURN_TIMEOUT_MS / 1000,
            if status_text.is_empty() { "未知" } else { status_text.as_str() }
        );
    }
    let elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(json!({
        "ok": finished && error.is_empty(),
        "conversation": conversation,
        "status": status_text,
        "events": channel.updates(),
        "chunks": channel.chunks(),
        "elapsedMs": elapsed_ms,
        "error": error,
    }))
}

/// 读一条会话的当前状态（`GET /console/as/conversations/{id}` 的 `data.status`）。
async fn conversation_status(
    client: &reqwest::Client,
    base: &str,
    conversation: &str,
    token: &str,
    uid: &str,
) -> String {
    let url = format!("{base}{conversation}");
    let Ok(response) = apply_headers(
        client.get(&url).timeout(Duration::from_millis(REQUEST_TIMEOUT_MS)),
        token,
        uid,
    )
    .send()
    .await
    else {
        return String::new();
    };
    let Ok(payload) = response.json::<Value>().await else {
        return String::new();
    };
    payload
        .pointer("/data/status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// 网页版 app 的出站头（**只有 bearer 与 `X-User-Id`，没有桌面端 `X-IDE-*` 指纹**）。
fn apply_headers(
    request: reqwest::RequestBuilder,
    token: &str,
    uid: &str,
) -> reqwest::RequestBuilder {
    request
        .header("Authorization", format!("Bearer {token}"))
        .header("X-User-Id", uid)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/plain, */*")
        .header("Origin", WEB_ORIGIN)
        .header("Referer", format!("{WEB_ORIGIN}/app"))
        .header("User-Agent", WEB_USER_AGENT)
}

/// 会话里的出口（`session.proxy` → `ResolvedProxy`），解析失败按直连处理。
fn proxy_of(session: &Value) -> Option<ResolvedProxy> {
    match ResolvedProxy::from_json(session.get("proxy").unwrap_or(&Value::Null)) {
        Ok(proxy) => proxy,
        Err(reason) => {
            logging::verbose(
                "[Accounts]",
                &format!("网页通道打卡：账号代理不可用（{reason}），本次回退直连"),
            );
            None
        }
    }
}

/// 截断长文案（错误信息进日志与界面，不该带一整页 HTML）。
fn truncate(text: &str, limit: usize) -> String {
    let cleaned = text.replace(['\n', '\r'], " ");
    if cleaned.chars().count() <= limit {
        return cleaned;
    }
    cleaned.chars().take(limit).collect::<String>() + "…"
}

// ─── ACP over streamable HTTP ───────────────────────────────

/// ACP 的 streamable-HTTP 传输：**一条长连 SSE 收事件，POST 发 JSON-RPC 请求**。
struct AcpChannel {
    client: reqwest::Client,
    link: String,
    token: String,
    connection_id: String,
    updates: Arc<AtomicUsize>,
    chunks: Arc<AtomicUsize>,
    finished: Arc<AtomicBool>,
}

impl AcpChannel {
    fn new(client: reqwest::Client, link: String, token: String) -> Self {
        Self {
            client,
            link,
            token,
            connection_id: String::new(),
            updates: Arc::new(AtomicUsize::new(0)),
            chunks: Arc::new(AtomicUsize::new(0)),
            finished: Arc::new(AtomicBool::new(false)),
        }
    }

    fn updates(&self) -> usize {
        self.updates.load(Ordering::Relaxed)
    }

    fn chunks(&self) -> usize {
        self.chunks.load(Ordering::Relaxed)
    }

    /// 这一轮是否已经收到结束信号（`isFinalStop`，见 [`marks_turn_end`]）。
    fn finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }

    /// 建立 SSE 通道并启动后台读流；返回服务端给的 `Acp-Connection-Id`。
    async fn open(&mut self) -> Result<String, GatewayError> {
        let response = self
            .client
            .get(&self.link)
            .header("Accept", "text/event-stream")
            .header("Authorization", format!("Bearer {}", self.token))
            .header("User-Agent", WEB_USER_AGENT)
            // SSE 是长连：不能套用单请求超时，否则刚到点就被掐断
            .send()
            .await
            .map_err(|error| {
                GatewayError::with_status(502, format!("ACP 通道建立失败：{}", egress::describe_error_detail(&error)))
            })?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(GatewayError::with_status(
                502,
                format!("ACP 通道返回 HTTP {status}"),
            ));
        }
        let connection_id = response
            .headers()
            .get("Acp-Connection-Id")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        if connection_id.is_empty() {
            return Err(GatewayError::with_status(
                502,
                "ACP 通道没有返回 Acp-Connection-Id",
            ));
        }
        self.connection_id = connection_id.clone();
        let updates = self.updates.clone();
        let chunks = self.chunks.clone();
        let finished = self.finished.clone();
        // 后台读流：事件用于统计与结束判据（读失败也不影响主流程 —— 状态轮询兜底）
        crate::spawn_task(async move {
            read_events(response, updates, chunks, finished).await;
        });
        Ok(connection_id)
    }

    /// 发一个 JSON-RPC 请求（成功 = 200/202，结果经 SSE 推回）。
    async fn request(
        &self,
        method: &str,
        params: Value,
        id: i64,
    ) -> Result<u16, GatewayError> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let response = self
            .client
            .post(&self.link)
            .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Acp-Connection-Id", &self.connection_id)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("User-Agent", WEB_USER_AGENT)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                GatewayError::with_status(
                    502,
                    format!("ACP {method} 发送失败：{}", egress::describe_error_detail(&error)),
                )
            })?;
        let status = response.status().as_u16();
        if status != 200 && status != 202 {
            return Err(GatewayError::with_status(
                502,
                format!("ACP {method} 返回 HTTP {status}"),
            ));
        }
        Ok(status)
    }
}

/// 读 SSE 流：数 `session/update`、认 `isFinalStop`（其余消息忽略）。
async fn read_events(
    response: reqwest::Response,
    updates: Arc<AtomicUsize>,
    chunks: Arc<AtomicUsize>,
    finished: Arc<AtomicBool>,
) {
    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break };
        buffer.extend_from_slice(&bytes);
        // 按行切分（SSE 以 `\n` 结束一行；`data:` 行可能被 TCP 分包）
        while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=position).collect();
            let text = String::from_utf8_lossy(&line);
            if let Some(message) = parse_sse_line(&text) {
                let (update_count, chunk_count) = count_update(&message);
                if update_count > 0 {
                    updates.fetch_add(update_count, Ordering::Relaxed);
                }
                if chunk_count > 0 {
                    chunks.fetch_add(chunk_count, Ordering::Relaxed);
                }
                if marks_turn_end(&message) {
                    finished.store(true, Ordering::Relaxed);
                }
            }
        }
    }
}

/// 这条消息是不是「本轮结束」的信号。
///
/// 实测（2026-10）沙箱在收尾时推一条
/// `session/update` → `session_info_update`，其 `_meta.codebuddy.ai.isFinalStop`
/// 为 `true`；紧接着会话状态才变 `completed`。用它比只轮询状态早一个周期，
/// 也不依赖状态字段的取值口径。
fn marks_turn_end(message: &Value) -> bool {
    message
        .pointer("/params/update/_meta/codebuddy.ai/isFinalStop")
        .and_then(Value::as_bool)
        == Some(true)
}

/// 一行 SSE 文本 → JSON-RPC 消息（不是 `data:` 行 / 空行 / `[DONE]` → `None`）。
fn parse_sse_line(line: &str) -> Option<Value> {
    let payload = line.trim().strip_prefix("data:")?.trim();
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }
    serde_json::from_str::<Value>(payload)
        .ok()
        .filter(|value| value.is_object())
}

/// 一条消息贡献的 `(session/update 数, agent_message_chunk 数)`。
fn count_update(message: &Value) -> (usize, usize) {
    if message.get("method").and_then(Value::as_str) != Some("session/update") {
        return (0, 0);
    }
    let update = message
        .pointer("/params/update")
        .cloned()
        .unwrap_or(Value::Null);
    let is_chunk = update.get("sessionUpdate").and_then(Value::as_str)
        == Some("agent_message_chunk");
    (1, usize::from(is_chunk))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_intl_workbuddy_accounts_are_eligible() {
        assert!(supports_daily_chat(&json!({"provider": "workbuddy", "edition": "intl"})));
        // provider 缺失 = workbuddy（账号存储的兜底口径）
        assert!(supports_daily_chat(&json!({"edition": "intl"})));
        // 国内版走签到接口，不走网页通道
        assert!(!supports_daily_chat(&json!({"provider": "workbuddy", "edition": "cn"})));
        assert!(!supports_daily_chat(&json!({"provider": "workbuddy"})));
        // 其它家即便带 intl 也不走这条链
        assert!(!supports_daily_chat(&json!({"provider": "raccoon", "edition": "intl"})));
        assert!(!supports_daily_chat(&json!({"provider": "autoclaw-intl", "edition": "intl"})));
    }

    #[test]
    fn sse_lines_are_parsed_like_the_hub_does() {
        let message = parse_sse_line(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"update\":{\"sessionUpdate\":\"agent_message_chunk\"}}}",
        )
        .expect("data 行应解析出对象");
        assert_eq!(count_update(&message), (1, 1));
        // 其它 session/update 只计事件、不计输出段
        let other = parse_sse_line(
            "data: {\"method\":\"session/update\",\"params\":{\"update\":{\"sessionUpdate\":\"tool_call\"}}}",
        )
        .expect("data 行应解析出对象");
        assert_eq!(count_update(&other), (1, 0));
        // 非 session/update 的消息不计
        assert_eq!(count_update(&json!({"id": 1, "result": {}})), (0, 0));
        // 空行 / 注释行 / [DONE] / 非 JSON 一律忽略
        assert!(parse_sse_line("").is_none());
        assert!(parse_sse_line(": keep-alive").is_none());
        assert!(parse_sse_line("event: message").is_none());
        assert!(parse_sse_line("data: [DONE]").is_none());
        assert!(parse_sse_line("data: not-json").is_none());
        assert!(parse_sse_line("data: [1,2,3]").is_none(), "非对象不当消息");
    }

    #[test]
    fn error_text_is_truncated_to_one_line() {
        assert_eq!(truncate("ok", 10), "ok");
        assert_eq!(truncate("a\nb", 10), "a b");
        assert_eq!(truncate("abcdef", 3), "abc…");
    }

    /// 结束信号取 `session_info_update` 里的 `isFinalStop`（实测形态）。
    #[test]
    fn final_stop_marks_the_end_of_the_turn() {
        let end = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "2106683637163745280",
                "update": {
                    "sessionUpdate": "session_info_update",
                    "_meta": { "codebuddy.ai": { "isFinalStop": true } },
                }
            }
        });
        assert!(marks_turn_end(&end));
        // 普通输出段不是结束信号
        let chunk = json!({
            "method": "session/update",
            "params": { "update": { "sessionUpdate": "agent_message_chunk" } }
        });
        assert!(!marks_turn_end(&chunk));
        // 显式 false 也不算
        let not_yet = json!({
            "method": "session/update",
            "params": { "update": { "_meta": { "codebuddy.ai": { "isFinalStop": false } } } }
        });
        assert!(!marks_turn_end(&not_yet));
    }
}
