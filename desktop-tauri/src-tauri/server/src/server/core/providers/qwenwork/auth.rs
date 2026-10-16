//! QwenWork transport and COSY 1.0.0 signing. Requests are JSON, not Qoder's encoded body.
use super::credentials::Credentials;
use crate::server::core::{egress, proxies::ResolvedProxy};
use crate::server::errors::GatewayError;
use base64::Engine as _;
use md5::{Digest, Md5};
use serde_json::{json, Value};

pub const BASE: &str = "https://gateway.qwenwork.ai";
pub const CHAT: &str = "/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common";
pub const MODELS: &str = "/algo/api/v2/model/list?Encode=1";
pub const ACCOUNT: &str =
    "/api/v1/adapter/user/account-context?include=user,plan,quota,page,data_sharing";
pub const REFRESH: &str = "/api/v1/deviceToken/refresh";
pub const UA: &str = "qoderwork/1.0.4";

/// Public client metadata verified in international 1.0.4-26090501. Not host identity.
pub const CLIENT_HEADERS: [(&str, &str); 6] = [
    ("X-QwenWork-Version", "1.0.4"),
    ("X-QwenWork-Release-Version", "1.0.4-26090501"),
    ("X-QwenWork-Build", "26090501"),
    ("X-QwenWork-Platform", "win32"),
    ("X-QwenWork-Arch", "x64"),
    ("X-QwenWork-Channel", "stable"),
];

pub fn with_client_headers(mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    for (name, value) in CLIENT_HEADERS {
        request = request.header(name, value);
    }
    request
}

pub fn signature(meta: &str, key: &str, time: &str, body: &str, path: &str) -> String {
    let path = path.split('?').next().unwrap_or(path);
    let path = path.strip_prefix("/algo").unwrap_or(path);
    format!(
        "{:x}",
        Md5::digest(format!("{meta}\n{key}\n{time}\n{body}\n{path}"))
    )
}

pub fn cosy_headers(
    c: &Credentials,
    path: &str,
    body: &str,
) -> Result<Vec<(String, String)>, GatewayError> {
    let key = super::super::qoder::cosy::random_uuid()?.replace('-', "");
    let key = key
        .get(..16)
        .ok_or_else(|| GatewayError::new("无法生成 QwenWork 签名密钥"))?;
    let info = json!({"uid":c.user_id,"aid":"","name":c.name,"email":c.email,"security_oauth_token":c.token});
    let info = super::super::qoder::cosy::aes128_cbc_base64(key, info.to_string().as_bytes())?;
    let encrypted = super::super::qoder::cosy::rsa_encrypt_base64(key)?;
    let request_id = super::super::qoder::cosy::random_uuid()?.replace('-', "");
    let meta = base64::engine::general_purpose::STANDARD.encode(json!({
        "version":"v1","requestId":request_id,"info":info,"cosyVersion":"1.0.0","ideVersion":"1.0.4"
    }).to_string());
    let time = (crate::server::logging::now_ms() / 1000).to_string();
    let sig = signature(&meta, &encrypted, &time, body, path);
    Ok(vec![
        ("Authorization".into(), format!("Bearer COSY.{meta}.{sig}")),
        ("Cosy-User".into(), c.user_id.clone()),
        ("Cosy-Key".into(), encrypted),
        ("Cosy-Date".into(), time),
        ("Cosy-Version".into(), "1.0.0".into()),
        ("Cosy-ClientType".into(), "5".into()),
        ("Cosy-MachineOS".into(), "win32_x64".into()),
        // Stable non-identifying hostname, rather than leaking the local machine's real name.
        ("Cosy-MachineHostname".into(), "DESKTOP-QWENWORK".into()),
        (
            "X-Request-Id".into(),
            super::super::qoder::cosy::random_uuid()?.replace('-', ""),
        ),
        ("X-IDE-Platform".into(), "win32".into()),
        ("X-Version".into(), "1.0.4".into()),
        ("X-Machine-OS".into(), "win32".into()),
        ("User-Agent".into(), UA.into()),
    ])
}

pub fn proxy(record: &Value) -> Result<Option<ResolvedProxy>, GatewayError> {
    let resolution = crate::server::core::proxies::resolve_account_proxy(record.get("proxy"));
    match resolution {
        None => Ok(None),
        Some(crate::server::core::proxies::ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(crate::server::core::proxies::ProxyResolution::Failed(error)) => {
            Err(GatewayError::bad_request(error))
        }
    }
}

/// Temporary backend failures must not become account quota penalties.
pub fn error_status(status: u16, message: &str) -> u16 {
    let lower = message.to_ascii_lowercase();
    if matches!(status, 502 | 503 | 504)
        || lower.contains("all backends failed")
        || lower.contains("service unavailable")
    {
        return 503;
    }
    if status == 401
        || (status == 403 && lower.contains("auth"))
        || lower.contains("invalid token")
        || lower.contains("token expired")
    {
        return 401;
    }
    if status == 402
        || [
            "credit",
            "quota",
            "exhaust",
            "insufficient",
            "balance",
            "积分",
        ]
        .iter()
        .any(|s| lower.contains(s))
    {
        return 429;
    }
    if status >= 400 {
        status
    } else {
        502
    }
}

pub fn business_ok(value: &Value) -> bool {
    if value.get("success") == Some(&Value::Bool(false)) {
        return false;
    }
    match value.get("code") {
        None | Some(Value::Null) => true,
        Some(Value::Number(n)) => matches!(n.as_i64(), Some(0 | 200)),
        Some(Value::String(s)) => matches!(
            s.to_ascii_lowercase().as_str(),
            "0" | "200" | "ok" | "success"
        ),
        _ => false,
    }
}

pub async fn json_response(response: reqwest::Response) -> Result<Value, GatewayError> {
    let status = response.status().as_u16();
    let value: Value = response.json().await.map_err(|_| {
        GatewayError::with_status(
            if status >= 400 {
                i32::from(status)
            } else {
                502
            },
            "QwenWork 返回无效 JSON",
        )
    })?;
    if !(200..300).contains(&status) || !business_ok(&value) {
        let message = super::credentials::text(&value, &["message", "msg", "error_description"]);
        // Never include arbitrary response bodies: refresh failures may echo credentials.
        return Err(GatewayError::with_status(
            i32::from(error_status(status, &message)),
            format!("QwenWork 请求失败（HTTP {status}，请检查账号状态或稍后重试）"),
        ));
    }
    Ok(value)
}

pub async fn get(
    c: &Credentials,
    path: &str,
    signed: bool,
    proxy: Option<&ResolvedProxy>,
) -> Result<Value, GatewayError> {
    let mut request = egress::client_for(proxy)
        .get(format!("{BASE}{path}"))
        .header("Accept", "application/json")
        .header("User-Agent", UA)
        .timeout(std::time::Duration::from_secs(25));
    if signed {
        for (k, v) in cosy_headers(c, path, "")? {
            request = request.header(k, v);
        }
    } else {
        request = request.bearer_auth(&c.token);
    }
    let response = request
        .send()
        .await
        .map_err(|_| GatewayError::with_status(502, "QwenWork 请求连接失败或超时"))?;
    json_response(response).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transient_is_not_quota() {
        assert_eq!(error_status(429, "All backends failed"), 503);
        assert_eq!(error_status(200, "insufficient credits"), 429);
    }
    #[test]
    fn signature_uses_exact_body_and_clean_path() {
        assert_eq!(
            signature("m", "k", "1", "{}", "/algo/api/x?a=1"),
            signature("m", "k", "1", "{}", "/api/x")
        );
        assert_ne!(
            signature("m", "k", "1", "{}", "/api/x"),
            signature("m", "k", "1", "{ }", "/api/x")
        );
    }
}
