//! International QwenWork device confirmation, statically verified in 1.0.4-26090501
//! on 2026-10-15 (startDeviceFlow / pollDeviceToken / international profile).
//! No code-init POST or local callback: the browser confirms a nonce + PKCE challenge.
//! The profile overrides the CN fallback client/redirect. Never infer region from
//! the bundle's legacy variable names. Live authorization has not been tested.

use super::{
    auth,
    credentials::{self, Credentials},
};
use crate::server::errors::GatewayError;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{future::Future, time::Duration};
use tokio::time::Instant;

const CLIENT_ID: &str = "cc65e5fc-05bd-4f5d-a4e8-0df19aa3d75a";
const REDIRECT_URI: &str = "qwenwork://";
const POLL_PATH: &str = "/api/v1/deviceToken/poll";
const LIMIT: usize = 256 * 1024;

// Deliberately neither Debug nor Serialize: verifier belongs only to the worker.
pub struct DeviceAuthorization {
    pub state: String,
    pub auth_url: String,
    nonce: String,
    verifier: String,
    challenge: String,
}

fn error(message: &str) -> GatewayError {
    GatewayError::with_status(502, message)
}
fn uuid() -> Result<String, GatewayError> {
    super::super::qoder::cosy::random_uuid()
}

impl DeviceAuthorization {
    pub fn new() -> Result<Self, GatewayError> {
        let mut random = [0u8; 32];
        getrandom::getrandom(&mut random).map_err(|_| error("无法生成 QwenWork 登录安全参数"))?;
        let verifier = URL_SAFE_NO_PAD.encode(random);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let nonce = uuid()?;
        // This is our login session's device ID, not an official-client file or
        // physical-machine identifier. The server binding is nonce + challenge.
        let machine = uuid()?;
        let mut url = reqwest::Url::parse(&format!("{}/device/selectAccounts", auth::BASE))
            .map_err(|_| error("QwenWork 授权地址无效"))?;
        url.query_pairs_mut().extend_pairs([
            ("challenge", challenge.as_str()),
            ("challenge_method", "S256"),
            ("nonce", nonce.as_str()),
            ("machine_id", machine.as_str()),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
        ]);
        Ok(Self {
            state: format!("qwenwork-{}", uuid()?),
            auth_url: url.into(),
            nonce,
            verifier,
            challenge,
        })
    }
}

fn poll_request(
    client: &reqwest::Client,
    start: &DeviceAuthorization,
) -> Result<reqwest::RequestBuilder, GatewayError> {
    Ok(auth::with_client_headers(
        client
            .get(format!("{}{POLL_PATH}", auth::BASE))
            .query(&[
                ("nonce", start.nonce.as_str()),
                ("verifier", start.verifier.as_str()),
                ("challenge_method", "S256"),
            ])
            .header("Accept", "application/json")
            .header("X-Request-Id", uuid()?)
            .timeout(Duration::from_secs(25)),
    ))
}

fn profile_request(
    client: &reqwest::Client,
    token: &str,
) -> Result<reqwest::RequestBuilder, GatewayError> {
    Ok(auth::with_client_headers(
        client
            .get(format!("{}{}", auth::BASE, auth::ACCOUNT))
            .bearer_auth(token)
            .header("Accept", "application/json")
            .header("User-Agent", auth::UA)
            .header("X-Request-Id", uuid()?)
            .timeout(Duration::from_secs(25)),
    ))
}

fn parse_poll(
    start: &DeviceAuthorization,
    status: u16,
    bytes: &[u8],
) -> Result<Option<Value>, GatewayError> {
    if status == 404 {
        return Ok(None);
    }
    if !(200..300).contains(&status) {
        return Err(error(&format!(
            "QwenWork 设备授权失败（HTTP {status}），请重新发起登录"
        )));
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| error("QwenWork 设备授权返回无效 JSON"))?;
    if !value.is_object()
        || !auth::business_ok(&value)
        || value.get("error").is_some_and(|v| !v.is_null())
    {
        return Err(error("QwenWork 设备授权返回业务错误"));
    }
    if value.get("token").is_none() {
        return Ok(None);
    }
    let token = credentials::text(&value, &["token"]);
    let refresh = credentials::text(&value, &["refresh_token"]);
    if token.is_empty() || refresh.is_empty() || token.len() > 32768 || refresh.len() > 32768 {
        return Err(error("QwenWork 设备授权凭据不完整或无效"));
    }
    if value.get("nonce").and_then(Value::as_str) != Some(&start.nonce)
        || value.get("code_challenge").and_then(Value::as_str) != Some(&start.challenge)
        || value.get("code_challenge_method").and_then(Value::as_str) != Some("S256")
    {
        return Err(error("QwenWork 设备授权绑定不匹配，未导入账号"));
    }
    Ok(Some(value))
}

fn credentials_from(grant: &Value, profile: &Value, now: i64) -> Result<Credentials, GatewayError> {
    let data = profile
        .get("data")
        .filter(|v| v.is_object())
        .unwrap_or(profile);
    if !profile.is_object() || !auth::business_ok(profile) || !auth::business_ok(data) {
        return Err(error("QwenWork 登录用户资料无效"));
    }
    let user = data.get("user").filter(|v| v.is_object()).unwrap_or(data);
    let id = credentials::text(user, &["id", "user_id", "uid"]);
    if id.is_empty() || id.len() > 512 {
        return Err(error("QwenWork 登录用户资料缺少有效 ID"));
    }
    let grant_id = credentials::text(grant, &["user_id"]);
    if !grant_id.is_empty() && grant_id != id {
        return Err(error("QwenWork 登录账号身份不匹配"));
    }
    let mut expires = credentials::timestamp(grant.get("expires_at"));
    if expires <= 0 {
        expires = grant
            .get("expires_in")
            .and_then(Value::as_i64)
            .filter(|n| *n > 0)
            .map(|n| now.saturating_add(n.saturating_mul(1000)))
            .unwrap_or(0);
    }
    Credentials::from_value(&json!({
        "accessToken": grant.get("token"), "refreshToken": grant.get("refresh_token"),
        "userId":id, "nickname":credentials::text(user, &["name","username","user_name"]),
        "email":credentials::text(user, &["email"]),
        // Numeric timestamps below 10^10 are seconds in the generic importer.
        // Assign the already-normalized milliseconds after parsing instead.
        "oauthClientId":CLIENT_ID, "oauthRedirectUri":REDIRECT_URI,
    }))
    .map(|mut c| {
        if expires > 0 {
            c.expires_at = expires;
        }
        c
    })
}

enum PollFailure {
    Network,
    Fatal(GatewayError),
}

async fn response_bytes(mut response: reqwest::Response) -> Result<(u16, Vec<u8>), PollFailure> {
    let status = response.status().as_u16();
    // Do not parse or retain arbitrary error bodies (they can echo credentials).
    if !(200..300).contains(&status) {
        return Ok((status, Vec::new()));
    }
    if response.content_length().is_some_and(|n| n > LIMIT as u64) {
        return Err(PollFailure::Fatal(error("QwenWork 登录响应过大")));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| PollFailure::Network)? {
        if bytes.len() + chunk.len() > LIMIT {
            return Err(PollFailure::Fatal(error("QwenWork 登录响应过大")));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((status, bytes))
}

// Cancellation also interrupts an in-flight HTTP request or sleep by dropping its
// future. A single monotonic deadline covers polling AND the identity request.
async fn bounded<T>(
    future: impl Future<Output = T>,
    canceled: &impl Fn() -> bool,
    deadline: Instant,
) -> Result<T, GatewayError> {
    tokio::pin!(future);
    loop {
        if canceled() {
            return Err(error("QwenWork 登录已取消"));
        }
        if Instant::now() >= deadline {
            return Err(error("QwenWork 登录等待超时（5 分钟），请重新发起"));
        }
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => return Err(error("QwenWork 登录等待超时（5 分钟），请重新发起")),
            result = &mut future => {
                if canceled() { return Err(error("QwenWork 登录已取消")); }
                return Ok(result);
            },
            _ = tokio::time::sleep(Duration::from_millis(100)) => {},
        }
    }
}

async fn complete_with<P, PF, U, UF, C>(
    mut poll: P,
    mut profile: U,
    canceled: C,
    lifetime: Duration,
    interval: Duration,
) -> Result<Credentials, GatewayError>
where
    P: FnMut() -> PF,
    PF: Future<Output = Result<Option<Value>, PollFailure>>,
    U: FnMut(String) -> UF,
    UF: Future<Output = Result<Value, GatewayError>>,
    C: Fn() -> bool,
{
    let deadline = Instant::now() + lifetime;
    let mut failures = 0;
    loop {
        match bounded(poll(), &canceled, deadline).await? {
            Ok(Some(grant)) => {
                let token = credentials::text(&grant, &["token"]);
                let user = bounded(profile(token), &canceled, deadline).await??;
                return credentials_from(&grant, &user, crate::server::logging::now_ms());
            }
            Ok(None) => failures = 0,
            Err(PollFailure::Fatal(e)) => return Err(e),
            Err(PollFailure::Network) => {
                failures += 1;
                if failures >= 5 {
                    return Err(error(
                        "QwenWork 登录连续网络失败，请检查网络或系统代理后重试",
                    ));
                }
            }
        }
        bounded(tokio::time::sleep(interval), &canceled, deadline).await?;
    }
}

pub async fn complete(
    start: &DeviceAuthorization,
    canceled: impl Fn() -> bool,
) -> Result<Credentials, GatewayError> {
    // Login follows the system proxy (no account exists yet). Never disable TLS
    // verification or follow redirects with the verifier-bearing poll URL.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .user_agent(auth::UA)
        .build()
        .map_err(|_| error("无法创建 QwenWork 登录客户端"))?;
    complete_with(
        || async {
            let request = poll_request(&client, start).map_err(PollFailure::Fatal)?;
            let response = request.send().await.map_err(|_| PollFailure::Network)?;
            let (status, bytes) = response_bytes(response).await?;
            parse_poll(start, status, &bytes).map_err(PollFailure::Fatal)
        },
        |token| {
            let client = &client;
            async move {
                let response = profile_request(client, &token)?
                    .send()
                    .await
                    .map_err(|_| error("QwenWork 登录用户资料查询失败，请重新发起登录"))?;
                let (status, bytes) = response_bytes(response)
                    .await
                    .map_err(|_| error("QwenWork 登录用户资料响应无效"))?;
                if !(200..300).contains(&status) {
                    return Err(error(&format!(
                        "QwenWork 登录用户资料查询失败（HTTP {status}）"
                    )));
                }
                serde_json::from_slice(&bytes)
                    .map_err(|_| error("QwenWork 登录用户资料返回无效 JSON"))
            }
        },
        canceled,
        Duration::from_secs(300),
        Duration::from_secs(1),
    )
    .await
}

#[cfg(test)]
#[path = "login_tests.rs"]
mod tests;
