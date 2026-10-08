//! LobsterAI 登录：本地回调服务器 + `authCode` 换 token。
//!
//! ── 与参考实现的实现差异（有意为之）──────────────────────────
//! Go 侧是**两个进程 + 一个 `/tmp` 状态文件**（`login.exe url` 起回调服务器
//! 后把 `{port,state,uuid,firstKeyfrom}` 落盘，阻塞等 `.result` 文件出现；
//! `login.exe poll` 再读文件取结果）。本模块把这套编排**收进单个进程内的
//! 一次等待**：回调服务器收到 `code` 后**立即在本进程完成 exchange**，
//! 直接给出凭据 —— 没有跨进程状态文件、没有残留文件误判。
//!
//! ── 回调监听器（照 Trae 的模式）──────────────────────────────
//! 绑 `127.0.0.1` 的随机端口；回调 URL 是
//! `http://127.0.0.1:{port}/auth/callback?code=…&state=…`。
//! 登录页会**校验** redirect_uri 的形态（`main.go:223-225` 的注释记录了
//! 这一约束），因此端口必须是回调监听器真实占到的那个 —— 先绑、再拼 URL。
//!
//! ── state 逐字比对 ──────────────────────────────────────────
//! 回调里带 `state`；与本次登录生成的逐字比对，不匹配的请求回 400 且
//! **不投递**（监听器继续等 —— 浏览器市场噪音与旧轮次回调都可能落到这里）。
//!
//! ── 端口什么时候还 ──────────────────────────────────────────
//! 与 Trae 同一处置：`close()` 幂等显式释放；`Drop` 兜底。
//! 会话被 `Arc` 共享，只靠 `Drop` 会变成「等超时才还端口」。

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{OriginalUri, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use crate::server::errors::GatewayError;
use crate::server::logging;

use super::client;
use super::credentials::{self, Credential};
use super::product;
use super::version;

/// 登录会话（`uuid` + `firstKeyfrom` 由客户端生成并贯穿整个账号生命周期）。
///
/// 它们不在服务端响应里，而是要在 exchange 时提交、并在之后**每次续期**时
/// 原样回传（见 `credentials::refresh_body`）。因此必须随凭据持久化。
pub struct LoginSession {
    pub uuid: String,
    pub first_keyfrom: String,
}

/// 生成一次性登录会话。
pub fn create_login_session(now_ms: i64) -> LoginSession {
    LoginSession {
        uuid: super::checkin::uuid_v4(),
        first_keyfrom: now_ms.to_string(),
    }
}

/// 构造 portal 登录 URL。
///
/// 形态照抄 `main.go:225-228`：
/// `{portal}/portal#/login?source=electron&redirect_uri=…&state=…`
///
/// 三个 query 参数的语义：
///   - `source=electron` —— 声明登录来源是桌面客户端；
///   - `redirect_uri` —— **必须**是 `http://127.0.0.1:{port}/auth/callback`
///     形态，登录页会校验；
///   - `state` —— 防 CSRF 的一次性随机串，回调时原样带回并比对。
///
/// ⚠️ `redirect_uri` 含 `://` 与 `:`，必须百分号编码；hash 段（`#/login`）
/// 不能用 query 构造器 —— 它属于 fragment，故显式拼装。
pub fn build_login_url(port: u16, state: &str) -> String {
    let redirect_uri = format!("http://127.0.0.1:{port}{}", product::CALLBACK_PATH);
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("source", "electron")
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("state", state)
        .finish();
    format!("{}/portal#/login?{query}", product::PORTAL_BASE)
}

/// 用授权码换取凭据。
///
/// 请求体**必须**含 5 个字段（对齐 `main.go:264-270`）：
/// `authCode` / `firstKeyfrom` / `latestKeyfrom` / `uuid` / `version`。
/// `latestKeyfrom` 取**当前时刻**（登录那一刻）；`version` 用动态真值。
pub async fn exchange_auth_code(
    code: &str,
    session: &LoginSession,
    client_version: &str,
    now_ms: i64,
) -> Result<Credential, GatewayError> {
    let latest_keyfrom = now_ms.to_string();
    let body = credentials::exchange_body(
        code,
        &session.uuid,
        &session.first_keyfrom,
        &latest_keyfrom,
        client_version,
    );
    let reply = client::post_api(
        product::EXCHANGE_PATH,
        &body,
        &client::anonymous_headers(),
        None,
        Duration::from_millis(product::REQUEST_TIMEOUT_MS),
    )
    .await?;
    if !(200..300).contains(&reply.status) {
        return Err(GatewayError::with_status(
            502,
            format!("LobsterAI 换证失败：上游返回 HTTP {}", reply.status),
        ));
    }
    let Some(body) = reply.json() else {
        return Err(GatewayError::with_status(502, "LobsterAI 换证响应不是 JSON"));
    };
    let envelope = credentials::parse_envelope(&body)
        .map_err(|failure| GatewayError::with_status(502, format!("LobsterAI 换证失败：{}", failure.message)))?;
    let payload = credentials::parse_token_payload(&envelope.data);
    if payload.access_token.is_empty() {
        // 与 Go 的 `refresh_failed: no accessToken` 同理：没有令牌就没有
        // 可用的凭据，不能把半成品存进凭据库。
        return Err(GatewayError::with_status(502, "LobsterAI 换证响应缺少 accessToken"));
    }
    Ok(credentials::build_credential(
        &payload,
        (&session.uuid, &session.first_keyfrom, &latest_keyfrom),
        now_ms,
    ))
}

// ── 回调监听器 ──────────────────────────────────────────────

/// 一次登录占用的 loopback 监听器。
pub struct CallbackListener {
    port: u16,
    shutdown: watch::Sender<bool>,
    inbox: tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>,
}

impl CallbackListener {
    /// 绑一个空闲 loopback 端口（IPv4 必须成功；IPv6 尽力而为）。
    pub async fn bind(state: Arc<String>) -> Result<Self, String> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|error| format!("绑定本机回调端口失败：{error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| format!("读不到回调端口：{error}"))?
            .port();
        let (shutdown, receiver) = watch::channel(false);
        let (sender, inbox) = mpsc::unbounded_channel();
        // state 只给路由用（逐字比对回调里的 state），不在本结构里留存。
        let app = router(sender, state);
        spawn_server(listener, app.clone(), receiver.clone());
        // 浏览器把 localhost 解析成 ::1 时也要有人接
        if let Ok(v6) = TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await {
            spawn_server(v6, app, receiver);
        }
        logging::log(
            "[Login]",
            &format!("LobsterAI 登录回调已监听 127.0.0.1:{port}{}", product::CALLBACK_PATH),
        );
        Ok(Self { port, shutdown, inbox: tokio::sync::Mutex::new(inbox) })
    }

    /// 这一轮占到的端口（拼 redirect_uri 用）。
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 主动收摊（幂等）：让监听任务退出、端口释放。
    pub fn close(&self) {
        let _ = self.shutdown.send(true);
    }

    /// 等下一次**有效回调**（state 不匹配的请求不会进到这里）。
    pub async fn next_callback(&self) -> Option<String> {
        let mut guard = self.inbox.lock().await;
        guard.recv().await
    }
}

impl Drop for CallbackListener {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

/// 一次 LobsterAI 登录（监听器 + 会话 + 授权地址）。
pub struct Session {
    pub login_url: String,
    pub session: LoginSession,
    listener: CallbackListener,
}

impl Session {
    /// 起一轮登录：先绑端口，再生成会话与授权地址。
    ///
    /// 顺序有讲究：**端口必须先落地**（redirect_uri 里的端口就是它）。
    pub async fn begin() -> Result<Self, String> {
        let state = Arc::new(super::checkin::uuid_v4());
        let listener = CallbackListener::bind(state.clone()).await?;
        let session = create_login_session(logging::now_ms());
        let login_url = build_login_url(listener.port(), &state);
        Ok(Self { login_url, session, listener })
    }

    /// 主动结束这一轮（释放回调端口，等待方随之退出）。
    pub fn close(&self) {
        self.listener.close();
    }

    /// 等回调并换成凭据（**不**含落盘，落盘由调用方做）。
    pub async fn complete(&self) -> Result<Credential, GatewayError> {
        let query = self.listener.next_callback().await.ok_or_else(|| {
            GatewayError::with_status(408, "LobsterAI 登录回调已丢失（本轮监听已结束），请重新发起登录")
        })?;
        let code = query_param(&query, "code");
        if code.is_empty() {
            return Err(GatewayError::with_status(400, "LobsterAI 回调里没有授权码"));
        }
        let (client_version, _source) = version::resolve().await;
        exchange_auth_code(&code, &self.session, &client_version, logging::now_ms()).await
    }
}

/// 从原始 query 串里取一个参数（**不解码两次**：回调参数是简单 token）。
fn query_param(query: &str, key: &str) -> String {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default()
}

/// 只有 `/auth/callback` 一条路径，其余 404。
fn router(sender: mpsc::UnboundedSender<String>, state: Arc<String>) -> Router {
    Router::new()
        .route(product::CALLBACK_PATH, get(callback))
        .fallback(not_found)
        .with_state((sender, state))
}

/// 收下一次回调：state 匹配才投递，否则回 400 让监听器继续等。
async fn callback(
    State((sender, state)): State<(mpsc::UnboundedSender<String>, Arc<String>)>,
    uri: OriginalUri,
) -> Response {
    let query = uri.0.query().unwrap_or_default().to_string();
    let code = query_param(&query, "code");
    let got_state = query_param(&query, "state");
    if code.is_empty() || got_state != **state {
        // state 不匹配说明回调不是本次登录发起的（或为伪造）。
        // 与参考实现同款：回 400 但不结束等待（可能是噪音或旧轮次）。
        return (
            StatusCode::BAD_REQUEST,
            "登录回调参数无效",
        )
            .into_response();
    }
    if sender.send(query).is_err() {
        // 监听器已丢（超时/取消先走）：不能当成一次成功登录
        return callback_page(410, "Login expired", "这一轮登录已经结束，请回到面板重新发起。");
    }
    callback_page(200, "Login successful", "You can close this window now.")
}

/// 非回调路径：不暴露任何东西。
async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

/// 给浏览器的那一页。
fn callback_page(status: u16, title: &str, message: &str) -> Response {
    let escaped = message.replace('<', "&lt;").replace('>', "&gt;");
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{title}</title>\
         <div style=\"font:16px/1.6 system-ui;padding:40px;text-align:center\">\
         <h2>{title}</h2><p>{escaped}</p></div>"
    );
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        [("content-type", "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

/// 起一个只服务这一轮登录的 HTTP 任务，收到 shutdown 信号就退出。
fn spawn_server(listener: TcpListener, app: Router, mut shutdown: watch::Receiver<bool>) {
    crate::spawn_task(async move {
        let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
            if *shutdown.borrow() {
                return;
            }
            let _ = shutdown.changed().await;
        });
        if let Err(error) = serve.await {
            logging::log("[Login]", &format!("⚠️ LobsterAI 回调监听器异常退出: {error}"));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_url_shape() {
        let url = build_login_url(12345, "state-1");
        assert!(url.starts_with("https://lobsterai.youdao.com/portal#/login?"));
        assert!(url.contains("source=electron"));
        assert!(url.contains("state=state-1"));
        // redirect_uri 必须被编码（含 :// 与 :）
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A12345%2Fauth%2Fcallback"));
    }

    #[test]
    fn query_param_reads_raw_value() {
        let query = "code=abc123&state=xyz";
        assert_eq!(query_param(query, "code"), "abc123");
        assert_eq!(query_param(query, "state"), "xyz");
        assert_eq!(query_param(query, "missing"), "");
    }

    #[tokio::test]
    async fn listener_accepts_only_matching_state() {
        let state = Arc::new("expected-state".to_string());
        let listener = CallbackListener::bind(state.clone()).await.expect("端口应能绑上");
        let port = listener.port();
        // 噪音：非回调路径 → 404
        let response = reqwest::get(format!("http://127.0.0.1:{port}/favicon.ico"))
            .await
            .expect("本机应可达");
        assert_eq!(response.status().as_u16(), 404);
        // state 不匹配 → 400，且不投递
        let response = reqwest::get(format!(
            "http://127.0.0.1:{port}/auth/callback?code=abc&state=wrong"
        ))
        .await
        .expect("本机应可达");
        assert_eq!(response.status().as_u16(), 400);
        // 正确回调 → 200 且投递
        let response = reqwest::get(format!(
            "http://127.0.0.1:{port}/auth/callback?code=good&state=expected-state"
        ))
        .await
        .expect("本机应可达");
        assert_eq!(response.status().as_u16(), 200);
        let query = tokio::time::timeout(Duration::from_secs(2), listener.next_callback())
            .await
            .expect("应收到回调")
            .expect("通道应打开");
        assert!(query.contains("code=good"));
        listener.close();
    }
}
