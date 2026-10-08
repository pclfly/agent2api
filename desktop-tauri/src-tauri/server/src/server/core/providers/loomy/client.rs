//! Loomy 出站请求头、账号请求体与 JSON 请求小工具。
//!
//! ── 两套认证头（本家最大的坑）────────────────────────────────
//!   - **业务端点**（`/models`、`/points/*`）只认小写 `token` 头；
//!     带 `Authorization: Bearer` 会被判「缺少 token」；
//!   - **chat 端点**只认 `Authorization: Bearer`，但官方客户端在 session
//!     模式下**两个都发** —— 保持一致可避免上游将来改判据。
//!
//! ── 账号端点 ────────────────────────────────────────────────
//! 讯飞 CAccount 的 HMAC-SHA1 签名（`sign::auth_headers`），
//! 请求体信封 `{base, param}`（`account_body`）。

use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::egress;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::errors::GatewayError;

use super::product;
use super::sign;

/// 上游响应体的读取上限（1 MiB）。
const MAX_BODY_BYTES: usize = 1 << 20;

/// 一次请求的结果（HTTP 状态 + 文本体）。
pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl Reply {
    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }
}

/// 业务端点请求头（`/models`、`/points/*`）。
///
/// ⚠️ **只认小写 `token` 头**，刻意不加 Authorization。
pub fn business_headers(token: &str) -> Vec<(String, String)> {
    vec![
        ("Accept".to_string(), "application/json".to_string()),
        ("token".to_string(), token.to_string()),
    ]
}

/// chat 端点请求头。
///
/// ⚠️ **两个头都发**（官方客户端同款）；`Bearer ` 前缀必需
/// （实测无前缀同样回 `100002 缺少 token`）。
pub fn chat_headers(token: &str) -> Vec<(String, String)> {
    vec![
        ("Accept".to_string(), "text/event-stream".to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Authorization".to_string(), format!("Bearer {token}")),
        ("token".to_string(), token.to_string()),
    ]
}

/// 构造账号端点请求体信封 `{base, param}`。
///
/// 依据 `account-service.js:441-450`。`traceid` 每次调用重新生成
/// （去掉连字符的 uuid，32 位 hex）。
pub fn account_body(param: Value) -> Value {
    let traceid = sign::uuid_v4().replace('-', "");
    json!({
        "base": {
            "appid": product::APP_ID,
            "modelid": product::ACCOUNT_MODEL_ID,
            "version": product::ACCOUNT_VERSION,
            "devid": product::ACCOUNT_DEV_ID,
            "ua": product::ACCOUNT_UA,
            "traceid": traceid,
        },
        "param": param,
    })
}

/// `GET` 一个业务端点（自动拼 `product::API_BASE`）。
pub async fn get_business(
    path: &str,
    headers: &[(String, String)],
    proxy: Option<&ResolvedProxy>,
) -> Result<Reply, GatewayError> {
    let url = format!("{}{path}", product::API_BASE);
    request("GET", &url, None, headers, proxy, Duration::from_millis(product::REQUEST_TIMEOUT_MS)).await
}

/// `POST` 一个 JSON 体到业务端点。
pub async fn post_business(
    path: &str,
    body: &Value,
    headers: &[(String, String)],
    proxy: Option<&ResolvedProxy>,
) -> Result<Reply, GatewayError> {
    let url = format!("{}{path}", product::API_BASE);
    request("POST", &url, Some(body), headers, proxy, Duration::from_millis(product::REQUEST_TIMEOUT_MS)).await
}

/// `POST` 一个账号端点（自动拼 `product::ACCOUNT_BASE` + HMAC-SHA1 签名头）。
///
/// ⚠️ body 序列化一次、签名与发送共用 —— 这是签名能通过的前提。
pub async fn post_account(
    path: &str,
    param: Value,
    proxy: Option<&ResolvedProxy>,
) -> Result<Reply, GatewayError> {
    let body = account_body(param);
    let serialized = body.to_string();
    let headers = sign::auth_headers(&sign::SignOptions {
        access_key_id: product::ACCESS_KEY_ID,
        access_key_secret: product::ACCESS_KEY_SECRET,
        method: "POST",
        path,
        query_params: &[],
        body: &serialized,
        content_type: "application/json",
    });
    let url = format!("{}{path}", product::ACCOUNT_BASE);
    // 手动发送（要发 `serialized` 而不是 `body.to_string()` 的二次序列化）
    let mut builder = egress::client_for(proxy)
        .post(&url)
        .timeout(Duration::from_millis(product::REQUEST_TIMEOUT_MS));
    for (name, value) in &headers {
        builder = builder.header(name, value.clone());
    }
    let response = builder.body(serialized).send().await.map_err(|error| {
        GatewayError::with_status(
            502,
            format!("Loomy 账号请求发不出去：{}", egress::describe_error_detail(&error)),
        )
    })?;
    let status = response.status().as_u16();
    let text = read_limited(response).await;
    Ok(Reply { status, body: text })
}

/// 发一次请求（网络层错误归一成 502；HTTP 状态本身不在这里判成错误）。
pub async fn request(
    method: &str,
    url: &str,
    body: Option<&Value>,
    headers: &[(String, String)],
    proxy: Option<&ResolvedProxy>,
    timeout: Duration,
) -> Result<Reply, GatewayError> {
    let mut builder = match method {
        "POST" => egress::client_for(proxy).post(url),
        _ => egress::client_for(proxy).get(url),
    }
    .timeout(timeout);
    for (name, value) in headers {
        builder = builder.header(name, value.clone());
    }
    if let Some(payload) = body {
        builder = builder.json(payload);
    }
    let response = builder.send().await.map_err(|error| {
        GatewayError::with_status(
            502,
            format!("Loomy 请求发不出去：{}", egress::describe_error_detail(&error)),
        )
    })?;
    let status = response.status().as_u16();
    let text = read_limited(response).await;
    Ok(Reply { status, body: text })
}

/// 读响应体，最多 [`MAX_BODY_BYTES`]。
async fn read_limited(response: reqwest::Response) -> String {
    match response.bytes().await {
        Ok(bytes) if bytes.len() <= MAX_BODY_BYTES => String::from_utf8_lossy(&bytes).into_owned(),
        Ok(bytes) => String::from_utf8_lossy(&bytes[..MAX_BODY_BYTES]).into_owned(),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn business_headers_use_lowercase_token_only() {
        let headers = business_headers("sess-1");
        assert!(headers.iter().any(|(name, value)| name == "token" && value == "sess-1"));
        assert!(!headers.iter().any(|(name, _)| name == "Authorization"));
    }

    #[test]
    fn chat_headers_send_both() {
        let headers = chat_headers("sess-1");
        assert!(headers
            .iter()
            .any(|(name, value)| name == "Authorization" && value == "Bearer sess-1"));
        assert!(headers.iter().any(|(name, value)| name == "token" && value == "sess-1"));
    }

    #[test]
    fn account_body_wraps_base_and_param() {
        let body = account_body(json!({"ccode": "86", "phone": "13011111100"}));
        assert_eq!(body["base"]["appid"], "GM3LOOMY");
        assert_eq!(body["base"]["ua"], "Loomy|Desktop|Electron|macOS");
        assert_eq!(body["param"]["phone"], "13011111100");
        // traceid：32 位 hex（uuid 去掉连字符）
        let traceid = body["base"]["traceid"].as_str().unwrap_or("");
        assert_eq!(traceid.len(), 32);
        assert!(traceid.chars().all(|ch| ch.is_ascii_hexdigit()));
    }
}
