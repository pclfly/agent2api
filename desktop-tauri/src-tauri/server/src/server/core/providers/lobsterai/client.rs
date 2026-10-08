//! LobsterAI 出站请求头与 JSON 请求小工具。
//!
//! ── 出站指纹（本家在这里，逐字节对齐参考实现）────────────────
//! 只设四个基础头（Authorization / Accept / Content-Type / User-Agent）——
//! LobsterAI **不认** CodeBuddy 那套 `X-Domain` / `X-Product` / `X-IDE-*`
//! 归属头，带上不仅无用，还可能让服务端按错误的客户端形态归因。
//! 对话与模型列表另加两个 `X-LobsterAI-Client-*` 头：
//!   - `Capabilities` 声明客户端支持的 agentic 协议版本（**模型列表的准入
//!     条件**，也是 `reasoning_effort:"off"` 的前提）；
//!   - `Version` 是客户端版本号（**用动态真值**，见 `version.rs`）。
//!
//! ── 为什么单独一个文件 ──────────────────────────────────────
//! 四条链路（exchange / refresh / 目录 / 签到与余额）共用同一套头构造与
//! 发送辅助；每个调用点各写一遍必然会在超时或 Accept 上慢慢分叉。

use std::time::Duration;

use serde_json::Value;

use crate::server::core::egress;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::errors::GatewayError;

use super::credentials::Credential;
use super::product;

/// 上游响应体的读取上限（1 MiB）。
///
/// 留上限不是为了省内存，而是上游出错时可能回一整个 HTML 页面：
/// 没有截断的话，一次失败会把整页 HTML 拼进错误消息里，日志与客户端都会被淹掉。
const MAX_BODY_BYTES: usize = 1 << 20;

/// 一次请求的结果（HTTP 状态 + 文本体）。
pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl Reply {
    /// 尝试解析 JSON（失败给 None，由调用方按状态码决定怎么报）。
    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }
}

/// 带 Bearer 的通用请求头（Accept 由调用方指定）。
pub fn auth_headers(credential: &Credential, accept: &str) -> Vec<(String, String)> {
    vec![
        ("Authorization".to_string(), format!("Bearer {}", credential.access_token)),
        ("Accept".to_string(), accept.to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("User-Agent".to_string(), product::USER_AGENT.to_string()),
    ]
}

/// 对话请求头：基础四头 + 两个 `X-LobsterAI-Client-*`，Accept 为 SSE。
///
/// `client_version` 用动态真值（不是 `fallbackClientVersion`）：Go 侧一直发
/// 假值 `0.1.0` 未被拒绝，说明服务端不强校验，但**没有理由继续发假值**。
pub fn chat_headers(credential: &Credential, client_version: &str) -> Vec<(String, String)> {
    let mut headers = auth_headers(credential, "text/event-stream, application/json");
    headers.push((
        "X-LobsterAI-Client-Capabilities".to_string(),
        product::CLIENT_CAPABILITIES.to_string(),
    ));
    headers.push((
        "X-LobsterAI-Client-Version".to_string(),
        client_version.to_string(),
    ));
    headers
}

/// 模型列表请求头：与对话同样带两个客户端头，只是 Accept 为 JSON。
///
/// **这两个头在本端点是必需的，不是可有可无的元数据**（2026-09-17 实测）：
/// 服务端按 `X-LobsterAI-Client-Capabilities` 声明的能力**过滤模型集合** ——
/// 不带该头时 `kimi-k3` 不会出现在返回里（25 个模型），带上才 26 个。
pub fn models_headers(credential: &Credential, client_version: &str) -> Vec<(String, String)> {
    let mut headers = auth_headers(credential, "application/json");
    headers.push((
        "X-LobsterAI-Client-Capabilities".to_string(),
        product::CLIENT_CAPABILITIES.to_string(),
    ));
    headers.push((
        "X-LobsterAI-Client-Version".to_string(),
        client_version.to_string(),
    ));
    headers
}

/// 无认证请求头（exchange / refresh 用）。
///
/// 这两个端点**不需要** `Authorization` —— 换 token 时还没有 token，
/// 续期时服务端只认请求体里的 `refreshToken`。
pub fn anonymous_headers() -> Vec<(String, String)> {
    vec![
        ("Accept".to_string(), "application/json".to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("User-Agent".to_string(), product::USER_AGENT.to_string()),
    ]
}

/// `GET` 一个 JSON 端点（自动拼 `product::API_BASE`）。
pub async fn get_api(
    path: &str,
    headers: &[(String, String)],
    proxy: Option<&ResolvedProxy>,
    timeout: Duration,
) -> Result<Reply, GatewayError> {
    let url = format!("{}{path}", product::API_BASE);
    request("GET", &url, None, headers, proxy, timeout).await
}

/// `POST` 一个 JSON 体到 API 基址下的路径。
pub async fn post_api(
    path: &str,
    body: &Value,
    headers: &[(String, String)],
    proxy: Option<&ResolvedProxy>,
    timeout: Duration,
) -> Result<Reply, GatewayError> {
    let url = format!("{}{path}", product::API_BASE);
    request("POST", &url, Some(body), headers, proxy, timeout).await
}

/// 发一次请求（网络层错误归一成 502；HTTP 状态本身不在这里判成错误）。
///
/// 与 Trae 的 `http::post_json` 同一形态：多候选/重试要靠状态码决定下一步，
/// 因此状态码交给调用方判。
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
            format!("LobsterAI 请求发不出去：{}", egress::describe_error_detail(&error)),
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

    fn credential() -> Credential {
        Credential {
            access_token: "at-1".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn chat_headers_carry_client_fingerprint() {
        let headers = chat_headers(&credential(), "2026.9.4");
        let find = |key: &str| {
            headers
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(find("Authorization").as_deref(), Some("Bearer at-1"));
        assert_eq!(find("User-Agent").as_deref(), Some("LobsterAI/0.1.0"));
        assert_eq!(
            find("X-LobsterAI-Client-Capabilities").as_deref(),
            Some("kimi-k3-agentic-v1,thinking-level-control-v1")
        );
        assert_eq!(find("X-LobsterAI-Client-Version").as_deref(), Some("2026.9.4"));
        assert!(find("Accept").unwrap_or_default().contains("text/event-stream"));
    }

    #[test]
    fn anonymous_headers_have_no_authorization() {
        let headers = anonymous_headers();
        assert!(!headers.iter().any(|(name, _)| name == "Authorization"));
    }
}
