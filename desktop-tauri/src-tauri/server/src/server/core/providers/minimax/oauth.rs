//! MiniMax OAuth：设备码授权 + PKCE S256 + 轮询 + 续期。
//!
//! ```text
//! POST {accountHost}/oauth2/device/code   {client_id, scope, audience, code_challenge, code_challenge_method:S256}
//! POST {accountHost}/oauth2/token         {grant_type: urn:ietf:params:oauth:grant-type:device_code, device_code, client_id, code_verifier}
//! POST {accountHost}/oauth2/token         {grant_type: refresh_token, refresh_token, client_id, scope, audience}
//! ```
//!
//! ── 两种「还在等」的形态都要认（真实差异，别只认一种）──────
//!   - **HTTP 200 + `status`**：`pending` / `slow_down` / `denied` /
//!     `expired`（asar 客户端形态）；
//!   - **非 200 + `error`**：`authorization_pending` / `slow_down`（OAuth 标准）。
//!
//! ── 令牌响应的硬校验 ────────────────────────────────────────
//! 见 `credentials::parse_token_grant`（access/refresh 非空、Bearer、
//! expires_in 正数、scope 含声明值）。

use base64::Engine;
use serde_json::Value;

use crate::server::errors::GatewayError;
use crate::server::logging;

use super::client;
use super::credentials::{self, Credential};
use super::product;

/// 设备码授权信息。
#[derive(Clone, Debug)]
pub struct DeviceAuthorization {
    pub device_code: String,
    /// PKCE verifier（轮询时要带回）。
    pub code_verifier: String,
    pub user_code: String,
    pub verification_uri: String,
    /// 含 `user_code` 的完整授权链接（**优先用它**，用户点开即完成）。
    pub verification_uri_complete: String,
    pub expires_in_sec: i64,
    /// ⚠️ 单位是**秒**。
    pub interval_sec: i64,
}

/// 登录被用户拒绝（终态）。
pub struct LoginDenied;

/// 登录过期（设备码作废，重新发起即可）。
pub struct LoginExpired;

/// 生成 PKCE code_verifier / code_challenge（S256）。
pub fn create_pkce() -> (String, String) {
    let mut bytes = [0u8; 32];
    if getrandom::getrandom(&mut bytes).is_err() {
        bytes = [0u8; 32];
    }
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let challenge = {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(verifier.as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
    };
    (verifier, challenge)
}

/// 申请设备码（内含 PKCE 生成）。
pub async fn start_device_authorization() -> Result<DeviceAuthorization, GatewayError> {
    let (code_verifier, code_challenge) = create_pkce();
    let form = vec![
        ("client_id".to_string(), product::CLIENT_ID.to_string()),
        ("scope".to_string(), product::SCOPE.to_string()),
        ("audience".to_string(), product::AUDIENCE.to_string()),
        ("code_challenge".to_string(), code_challenge),
        ("code_challenge_method".to_string(), "S256".to_string()),
    ];
    let reply = client::post_form(
        product::DEVICE_CODE_PATH,
        &form,
        std::time::Duration::from_millis(product::OAUTH_TIMEOUT_MS),
    )
    .await?;
    if !(200..300).contains(&reply.status) {
        return Err(GatewayError::with_status(
            502,
            format!("MiniMax 设备码申请失败（HTTP {}）", reply.status),
        ));
    }
    let Some(body) = reply.json() else {
        return Err(GatewayError::with_status(502, "MiniMax 设备码响应不是 JSON"));
    };
    let text = |key: &str| body.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let device_code = text("device_code");
    let user_code = text("user_code");
    let verification_uri = {
        let uri = text("verification_uri");
        if uri.is_empty() { text("verification_url") } else { uri }
    };
    let expires_in = body.get("expires_in").and_then(Value::as_i64).unwrap_or(0);
    if device_code.is_empty() || user_code.is_empty() || verification_uri.is_empty() || expires_in <= 0 {
        return Err(GatewayError::with_status(502, "MiniMax 设备码响应无法解析"));
    }
    let verification_uri_complete = {
        let complete = text("verification_uri_complete");
        if complete.is_empty() { verification_uri.clone() } else { complete }
    };
    // interval 缺省 5 秒（照 asar 的默认值）
    let interval = body
        .get("interval")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .unwrap_or(5);
    Ok(DeviceAuthorization {
        device_code,
        code_verifier,
        user_code,
        verification_uri,
        verification_uri_complete,
        expires_in_sec: expires_in,
        interval_sec: interval,
    })
}

/// 轮询直到拿到令牌（或拒绝 / 过期 / 取消）。
///
/// `cancel` 返回 true 时中止（外部取消信号）；`sleep_ms` 由调用方注入
/// （测试可用 0）。
pub async fn poll_device_token(
    auth: &DeviceAuthorization,
    mut cancelled: impl FnMut() -> bool,
) -> Result<Credential, PollOutcome> {
    let deadline = logging::now_ms() + auth.expires_in_sec * 1000;
    let mut interval_ms = auth.interval_sec * 1000;
    while logging::now_ms() < deadline {
        if cancelled() {
            return Err(PollOutcome::Cancelled);
        }
        let form = vec![
            (
                "grant_type".to_string(),
                "urn:ietf:params:oauth:grant-type:device_code".to_string(),
            ),
            ("device_code".to_string(), auth.device_code.clone()),
            ("client_id".to_string(), product::CLIENT_ID.to_string()),
            ("code_verifier".to_string(), auth.code_verifier.clone()),
        ];
        let reply = client::post_form(
            product::TOKEN_PATH,
            &form,
            std::time::Duration::from_millis(product::OAUTH_TIMEOUT_MS),
        )
        .await
        .map_err(|error| PollOutcome::Failed(error.message))?;
        let body = reply.json().unwrap_or(Value::Null);
        let status = body.get("status").and_then(Value::as_str).unwrap_or("");
        let error = body.get("error").and_then(Value::as_str).unwrap_or("");
        let ok = (200..300).contains(&reply.status);

        // 形态一：HTTP 200 + status
        if ok && status == "pending" {
            tokio::time::sleep(std::time::Duration::from_millis(interval_ms.max(0) as u64)).await;
            continue;
        }
        if ok && status == "slow_down" {
            interval_ms += 5000;
            tokio::time::sleep(std::time::Duration::from_millis(interval_ms.max(0) as u64)).await;
            continue;
        }
        if ok && (status == "denied" || status == "access_denied") {
            return Err(PollOutcome::Denied);
        }
        if ok && (status == "expired" || status == "expired_token") {
            return Err(PollOutcome::Expired);
        }
        // 形态二：非 200 + error（OAuth 标准）
        if error == "authorization_pending" {
            tokio::time::sleep(std::time::Duration::from_millis(interval_ms.max(0) as u64)).await;
            continue;
        }
        if error == "slow_down" {
            interval_ms += 5000;
            tokio::time::sleep(std::time::Duration::from_millis(interval_ms.max(0) as u64)).await;
            continue;
        }
        if ok {
            return match credentials::parse_token_grant(&body, None, product::SCOPE) {
                Ok(grant) => Ok(credentials::build_credential(
                    &grant,
                    logging::now_ms(),
                    None,
                )),
                Err(message) => Err(PollOutcome::Failed(message)),
            };
        }
        return Err(PollOutcome::Failed(format!(
            "MiniMax 授权失败：{}",
            if error.is_empty() { format!("HTTP {}", reply.status) } else { error.to_string() }
        )));
    }
    Err(PollOutcome::Expired)
}

/// 轮询结果（三种终态分开，让上层给出不同文案）。
pub enum PollOutcome {
    /// 用户拒绝
    Denied,
    /// 设备码过期
    Expired,
    /// 被取消
    Cancelled,
    /// 其它失败（含网络与解析）
    Failed(String),
}

/// 续期（refresh_token grant）。
pub async fn refresh_credential(
    refresh_token: &str,
    previous: &Credential,
) -> Result<Credential, GatewayError> {
    if refresh_token.trim().is_empty() {
        return Err(GatewayError::with_status(401, "该账号没有 refreshToken，无法续期"));
    }
    let form = vec![
        ("grant_type".to_string(), "refresh_token".to_string()),
        ("refresh_token".to_string(), refresh_token.to_string()),
        ("client_id".to_string(), product::CLIENT_ID.to_string()),
        ("scope".to_string(), product::SCOPE.to_string()),
        ("audience".to_string(), product::AUDIENCE.to_string()),
    ];
    let reply = client::post_form(
        product::TOKEN_PATH,
        &form,
        std::time::Duration::from_millis(product::OAUTH_TIMEOUT_MS),
    )
    .await?;
    let Some(body) = reply.json() else {
        return Err(GatewayError::with_status(502, "MiniMax 续期响应不是 JSON"));
    };
    if !(200..300).contains(&reply.status) {
        let error = body.get("error").and_then(Value::as_str).unwrap_or("");
        return Err(GatewayError::with_status(
            401,
            format!(
                "MiniMax 续期被拒（{}）：请重新登录该账号",
                if error.is_empty() { format!("HTTP {}", reply.status) } else { error.to_string() }
            ),
        ));
    }
    let grant = credentials::parse_token_grant(&body, Some(refresh_token), product::SCOPE)
        .map_err(|message| GatewayError::with_status(502, format!("MiniMax 续期响应异常：{message}")))?;
    Ok(credentials::build_credential(&grant, logging::now_ms(), Some(previous)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_shape() {
        let (verifier, challenge) = create_pkce();
        // 32 字节 base64url 无填充 = 43 字符
        assert_eq!(verifier.len(), 43);
        assert_eq!(challenge.len(), 43);
        assert!(verifier.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'));
        // 两次调用不同
        let (verifier2, _) = create_pkce();
        assert_ne!(verifier, verifier2);
    }
}
