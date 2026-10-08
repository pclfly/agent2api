//! MiniMax 出站请求头与 JSON 请求小工具。
//!
//! ── 出站指纹 ────────────────────────────────────────────────
//!   - 业务端点（目录 / 签到 / 积分）**只需 Bearer**（实测），不需要 machine 头
//!     或签名；
//!   - 推理端点（Anthropic Messages）**实测不需要 `anthropic-version` 头**
//!     （2026-09-29 真实请求：只带 `Authorization` + `Content-Type` + `Accept`
//!     即 HTTP 200）。故**不照抄 Anthropic 官方文档**加那个头 —— 加未经验证的
//!     头是猜测。
//!   - OAuth 端点用 `client_id` / `scope` / `audience` 常量（见 `product.rs`）。

use std::time::Duration;

use serde_json::Value;

use crate::server::core::egress;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::errors::GatewayError;

use super::credentials::Credential;
use super::product;

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

/// 业务端点请求头（**只需 Bearer**）。
pub fn business_headers(credential: &Credential) -> Vec<(String, String)> {
    vec![
        ("Authorization".to_string(), format!("Bearer {}", credential.access_token)),
        ("Accept".to_string(), "application/json".to_string()),
    ]
}

/// 推理请求头（Anthropic Messages 端点；**不加 `anthropic-version`**）。
pub fn infer_headers(credential: &Credential) -> Vec<(String, String)> {
    vec![
        ("Authorization".to_string(), format!("Bearer {}", credential.access_token)),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "text/event-stream".to_string()),
    ]
}

/// `GET` 一个业务端点（自动拼 `product::API_HOST`）。
pub async fn get_api(
    path: &str,
    headers: &[(String, String)],
    proxy: Option<&ResolvedProxy>,
    timeout: Duration,
) -> Result<Reply, GatewayError> {
    let url = format!("{}{path}", product::API_HOST);
    request("GET", &url, None, headers, proxy, timeout).await
}

/// `POST` 一个 JSON 体到业务端点。
pub async fn post_api(
    path: &str,
    body: &Value,
    headers: &[(String, String)],
    proxy: Option<&ResolvedProxy>,
    timeout: Duration,
) -> Result<Reply, GatewayError> {
    let url = format!("{}{path}", product::API_HOST);
    request("POST", &url, Some(body), headers, proxy, timeout).await
}

/// `POST` 一个表单体到 OAuth 端点（`application/x-www-form-urlencoded`）。
pub async fn post_form(
    path: &str,
    form: &[(String, String)],
    timeout: Duration,
) -> Result<Reply, GatewayError> {
    let url = format!("{}{path}", product::ACCOUNT_HOST);
    let body = form
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                url::form_urlencoded::byte_serialize(key.as_bytes()).collect::<String>(),
                url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("&");
    let response = egress::client_for(None)
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .timeout(timeout)
        .body(body)
        .send()
        .await
        .map_err(|error| {
            GatewayError::with_status(
                502,
                format!("MiniMax OAuth 请求发不出去：{}", egress::describe_error_detail(&error)),
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
            format!("MiniMax 请求发不出去：{}", egress::describe_error_detail(&error)),
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

/// 矩阵请求的签名密钥（与 unigate 的 `minimaxMatrixHeaders` 同源，实测有效）。
const MATRIX_SIGN_SECRET: &str = "I*7Cf%WZ#S&%1RlZJ&C2";

/// 矩阵形态请求（签到 / 积分）。
///
/// ── 为什么需要它（2026-10 实测）─────────────────────────────
/// `/minimax-cloud/*` 接口对两种 token 的认证形态不同：
///   - `mmoat_`（OAuth 设备码）：`Authorization: Bearer` + `?timezone_id=`；
///   - **JWT（local-runtime 导入）：Bearer 会 401 `invalid access token`**，
///     必须走矩阵形态 —— `token` 头 + `x-signature`（md5 签名）+ 完整 query
///     （含 `user_id`=realUserID）。
/// 分派由调用方按「token 是否 JWT」做（见 `checkin::request_envelope`）。
///
/// `path` 里可能带着调用方拼的 `?timezone_id=…`（Bearer 形态的约定）——
/// 矩阵形态自带 `timezone_offset`，这里会把已有 query **剥掉**再拼自己的。
/// 剥掉的那部分要保留时，改用 `extra_query` 传进来（本函数会把它们追加在
/// 矩阵参数之后）—— 余额的 `page_size` 就是这么传的（见 `product::CREDIT_PAGE_SIZE`）。
///
/// 参数与签名算法对齐 unigate 的 `minimaxQuery` / `minimaxMatrixHeaders`
/// （同一批实测口径）；时区偏移取本机（`timezone_offset`，秒）。
///
/// ⚠️ 签名只覆盖 `ts + secret + body`，**不含 query** —— 所以追加 `extra_query`
/// 不会破坏签名（实测：带 `page_size=100` 的请求正常返回 200）。
pub async fn matrix_request(
    method: &str,
    path: &str,
    body: Option<&Value>,
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
    timeout: Duration,
    extra_query: &[(&str, &str)],
) -> Result<Reply, GatewayError> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let body_text = body.map(|payload| payload.to_string()).unwrap_or_default();
    let signature = md5_hex(&format!("{ts}{MATRIX_SIGN_SECRET}{body_text}"));
    let timezone_offset = chrono::Local::now().offset().local_minus_utc();
    let mut query = matrix_query(ts, &credential.real_user_id, timezone_offset);
    if !extra_query.is_empty() {
        query.push('&');
        query.push_str(&encode_pairs(extra_query));
    }
    let base_path = path.split('?').next().unwrap_or(path);
    let url = format!("{}{base_path}?{query}", product::API_HOST);
    let headers = vec![
        ("token".to_string(), credential.access_token.clone()),
        ("User-Agent".to_string(), "MiniMaxAgent Mozilla/5.0".to_string()),
        ("Origin".to_string(), product::API_HOST.to_string()),
        ("Referer".to_string(), format!("{}/", product::API_HOST)),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("x-timestamp".to_string(), ts.to_string()),
        ("x-signature".to_string(), signature),
    ];
    request(method, &url, body, &headers, proxy, timeout).await
}

/// 矩阵请求的 query 串（与 unigate 的 `minimaxQuery` 逐字对齐）。
fn matrix_query(ts: i64, user_id: &str, timezone_offset_seconds: i32) -> String {
    let uuid = crate::server::core::upstream::request::new_request_id().replace('-', "");
    let pairs: [(&str, String); 21] = [
        ("device_platform", "web".to_string()),
        ("biz_id", "3".to_string()),
        ("app_id", "3001".to_string()),
        ("version_code", "22201".to_string()),
        ("unix", ts.to_string()),
        ("timezone_offset", timezone_offset_seconds.to_string()),
        ("is_desktop", "1".to_string()),
        ("sys_language", "zh".to_string()),
        ("lang", "zh".to_string()),
        ("uuid", uuid),
        ("device_id", "81124884".to_string()),
        ("os_name", "Windows".to_string()),
        ("browser_name", "Chrome".to_string()),
        ("device_memory", "32".to_string()),
        ("cpu_core_num", "12".to_string()),
        ("browser_language", "zh-CN".to_string()),
        ("browser_platform", "Win32".to_string()),
        ("user_id", user_id.to_string()),
        ("screen_width", "1920".to_string()),
        ("screen_height", "1080".to_string()),
        ("client", "desktop".to_string()),
    ];
    pairs
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                url::form_urlencoded::byte_serialize(key.as_bytes()).collect::<String>(),
                url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// md5 十六进制小写（矩阵签名用）
fn md5_hex(text: &str) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 把 `(key, value)` 列表编码成 query 片段（`k=v&k2=v2`，两段都 percent-encode）。
fn encode_pairs(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                url::form_urlencoded::byte_serialize(key.as_bytes()).collect::<String>(),
                url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// 把额外查询参数追加到路径上（按已有 `?` 自动选 `&`）。
///
/// 给 **Bearer 形态**用：矩阵形态走 [`matrix_request`] 的 `extra_query`
/// （它会先剥掉 path 里的 query 再拼自己的）。签到那条路径已经带了
/// `?timezone_id=`，所以这里必须判断分隔符而不是无条件用 `?`。
pub fn append_query(path: &str, extra_query: &[(&str, &str)]) -> String {
    if extra_query.is_empty() {
        return path.to_string();
    }
    let separator = if path.contains('?') { '&' } else { '?' };
    format!("{path}{separator}{}", encode_pairs(extra_query))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential() -> Credential {
        Credential {
            access_token: "mmoat_a".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn infer_headers_have_no_anthropic_version() {
        let headers = infer_headers(&credential());
        assert!(headers
            .iter()
            .any(|(name, value)| name == "Authorization" && value == "Bearer mmoat_a"));
        // 实测不需要 anthropic-version —— 不能照抄官方文档加它
        assert!(!headers.iter().any(|(name, _)| name == "anthropic-version"));
        assert!(headers
            .iter()
            .any(|(name, value)| name == "Accept" && value == "text/event-stream"));
    }

    #[test]
    fn append_query_picks_the_right_separator() {
        // 无 query 的路径用 `?`
        assert_eq!(
            append_query("/minimax-cloud/api/v1/credit/details", &[("page_size", "100")]),
            "/minimax-cloud/api/v1/credit/details?page_size=100"
        );
        // 已有 query（签到路径）用 `&`
        assert_eq!(
            append_query(
                "/minimax-cloud/api/v1/signin/status?timezone_id=Asia%2FShanghai",
                &[("page_size", "100")]
            ),
            "/minimax-cloud/api/v1/signin/status?timezone_id=Asia%2FShanghai&page_size=100"
        );
        // 没有额外参数时原样返回
        assert_eq!(append_query("/x?a=1", &[]), "/x?a=1");
    }

    #[test]
    fn matrix_query_carries_the_identity_params() {
        let query = matrix_query(1791104203, "548212364582207496", 28800);
        assert!(query.contains("user_id=548212364582207496"));
        assert!(query.contains("timezone_offset=28800"));
        assert!(query.contains("client=desktop"));
        // 21 个固定参数（与 unigate 的 minimaxQuery 对齐）
        assert_eq!(query.split('&').count(), 21);
    }
}
