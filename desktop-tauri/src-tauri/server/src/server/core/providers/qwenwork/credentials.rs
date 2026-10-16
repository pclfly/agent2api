//! QwenWork credentials. Import is explicit; never write the desktop client's files.
use crate::server::errors::GatewayError;
use base64::Engine as _;
use serde_json::{json, Value};

#[derive(Clone)]
pub struct Credentials {
    pub token: String,
    pub refresh_token: String,
    pub user_id: String,
    pub name: String,
    pub email: String,
    pub expires_at: i64,
    pub client_id: String,
    pub redirect_uri: String,
}

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            value
                .get(*key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("")
        .to_string()
}

pub fn timestamp(value: Option<&Value>) -> i64 {
    let Some(value) = value else { return 0 };
    if let Some(n) = value.as_i64() {
        return if n > 0 && n < 10_000_000_000 {
            n.saturating_mul(1000)
        } else {
            n
        };
    }
    value
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp_millis())
        .unwrap_or(0)
}

fn claims(token: &str) -> Value {
    token
        .split('.')
        .nth(1)
        .and_then(|part| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(part.trim_end_matches('='))
                .ok()
        })
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

impl Credentials {
    pub fn from_value(value: &Value) -> Result<Self, GatewayError> {
        let parsed;
        // The generic form can paste the full reference credential in its token field.
        let value = if let Some(raw) =
            value
                .get("credentials")
                .and_then(Value::as_str)
                .or_else(|| {
                    value
                        .get("accessToken")
                        .and_then(Value::as_str)
                        .filter(|s| s.trim_start().starts_with('{'))
                }) {
            parsed = serde_json::from_str::<Value>(raw)
                .map_err(|_| GatewayError::bad_request("QwenWork 凭据 JSON 无效"))?;
            &parsed
        } else {
            value
        };
        let token = text(
            value,
            &["accessToken", "token", "access_token", "device_token"],
        );
        let token = token
            .strip_prefix("Bearer ")
            .unwrap_or(&token)
            .trim()
            .to_string();
        let jwt = claims(&token);
        let user = value.get("user").unwrap_or(&Value::Null);
        let mut user_id = text(value, &["userId", "uid"]);
        if user_id.is_empty() {
            user_id = text(user, &["id"]);
        }
        if user_id.is_empty() {
            user_id = text(&jwt, &["sub"]);
        }
        if token.is_empty() || user_id.is_empty() {
            return Err(GatewayError::bad_request(
                "QwenWork 需要 accessToken 和用户 ID（可粘贴含 user.id 的完整凭据 JSON）",
            ));
        }
        let refresh_token = text(value, &["refreshToken", "refresh_token"]);
        if token.len() > 32768 || refresh_token.len() > 32768 || user_id.len() > 512 {
            return Err(GatewayError::bad_request("QwenWork 凭据字段过长"));
        }
        let expires = timestamp(value.get("expiresAt").or_else(|| value.get("expires_at")));
        Ok(Self {
            token,
            refresh_token,
            user_id,
            name: {
                let name = text(value, &["nickname"]);
                if name.is_empty() {
                    text(user, &["name", "username"])
                } else {
                    name
                }
            },
            email: {
                let s = text(value, &["email"]);
                if s.is_empty() {
                    text(user, &["email"])
                } else {
                    s
                }
            },
            expires_at: if expires > 0 {
                expires
            } else {
                timestamp(jwt.get("exp"))
            },
            client_id: text(value, &["oauthClientId"]),
            redirect_uri: text(value, &["oauthRedirectUri"]),
        })
    }
    pub fn expiring(&self) -> bool {
        self.expires_at > 0 && self.expires_at <= crate::server::logging::now_ms() + 30 * 60 * 1000
    }
    pub fn payload(&self) -> Value {
        json!({"accessToken": self.token, "refreshToken": self.refresh_token,
            "userId": self.user_id, "uid": self.user_id, "email": self.email,
            "nickname": self.name, "expiresAt": self.expires_at,
            "oauthClientId": self.client_id, "oauthRedirectUri": self.redirect_uri})
    }
}

pub fn read_desktop() -> Result<Credentials, GatewayError> {
    if !cfg!(windows) {
        return Err(GatewayError::bad_request(
            "QwenWork 客户端导入仅支持 Windows；其他平台请粘贴明文凭据 JSON",
        ));
    }
    let base = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| GatewayError::bad_request("未找到 Windows APPDATA 目录"))?
        .join("QwenWork");
    let blob = std::fs::read(base.join("auth-v2.dat"))
        .map_err(|_| GatewayError::bad_request("未找到 QwenWork 登录态，请先在客户端登录"))?;
    let key = super::super::autoclaw::crypto::os_crypt_aes_key(&base.join("Local State")).map_err(
        |_| {
            GatewayError::bad_request(
                "QwenWork 登录态密钥无法解密，请使用登录客户端的同一 Windows 用户",
            )
        },
    )?;
    let encoded = format!(
        "enc:{}",
        base64::engine::general_purpose::STANDARD.encode(blob)
    );
    let plain =
        super::super::autoclaw::crypto::decrypt_enc_value(&encoded, Some(&key)).map_err(|_| {
            GatewayError::bad_request("QwenWork 登录态解密失败，请重新登录客户端后导入")
        })?;
    let value: Value = serde_json::from_str(&plain)
        .map_err(|_| GatewayError::bad_request("QwenWork 登录态 JSON 无效"))?;
    Credentials::from_value(&value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reference_credential_and_dates() {
        let c = Credentials::from_value(&json!({"token":"test-token", "refreshToken":"refresh-test", "user":{"id":"user-demo","name":"Demo"}, "expiresAt":"2027-01-01T00:00:00Z"})).unwrap();
        assert_eq!(c.user_id, "user-demo");
        assert_eq!(c.expires_at, 1798761600000);
        assert!(Credentials::from_value(&json!({"token":"test-token"})).is_err());
        let round_trip = Credentials::from_value(&c.payload()).unwrap();
        assert_eq!(round_trip.name, "Demo");
        assert_eq!(round_trip.user_id, c.user_id);
        assert_eq!(round_trip.payload(), c.payload());
    }
}
