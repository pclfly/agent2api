//! MiniMax Code（中国版）凭据结构、过期判定与令牌响应解析。
//!
//! ── token 不是 JWT（本家最大的坑）────────────────────────────
//! 实测：`access_token` 前缀 `mmoat_`、**60 字符、0 个点**；`refresh_token`
//! 前缀 `mmort_` 同样非 JWT。⇒ **不能**从 JWT 解过期时间，
//! 登录/续期时**必须**由响应 `expires_in` 自算 `expires_at` 写入，
//! 否则过期时间彻底丢失、账号永远显示「未知」。
//!
//! ── 令牌响应的硬校验（照 asar `parseTokenGrant`）────────────
//!   - `access_token` 非空；
//!   - `refresh_token` 非空（缺失时回退上一个）；
//!   - `token_type.toLowerCase() === 'bearer'`；
//!   - `expires_in` 是正数；
//!   - **`scope` 必须含产品声明的 scope**（默认 `agent.default`），
//!     否则报 `invalid_token_response`。

use serde_json::Value;

/// 持久化的 MiniMax 凭据。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credential {
    /// OAuth access token（前缀 `mmoat_`，实测 60 字符；JWT 来源则为 385 字符的 JWT）。
    pub access_token: String,
    /// refresh token（实测 60 字符；**不是 JWT**；JWT 来源为空）。
    pub refresh_token: String,
    /// 恒 `"Bearer"`。
    pub token_type: String,
    /// 毫秒时间戳字符串（OAuth 来源由 `expires_in` 自算；JWT 来源从 `exp` 解析）。
    pub expires_at: String,
    /// 空格分隔的 scope 字符串。
    pub scope: String,
    /// 展示名。
    pub nickname: String,
    /// 矩阵接口的 `user_id`（`realUserID`）。
    ///
    /// 只有「本地登录态导入」的账号有（JWT 来源）；OAuth 设备码登录拿不到它。
    /// 签到 / 积分走矩阵请求形态时（见 `client::matrix_*`）必须带上它 ——
    /// 没有 `user_id` 的请求会被矩阵网关拒（`NOT_LOGIN`）。
    pub real_user_id: String,
    /// 绑定手机号（`user/info` 的 `data.userInfo.phone`，见 `profile.rs`）。
    ///
    /// **不是凭据**：只用于账号展示名（脱敏后进公开形态）。落库时明文存本地，
    /// 拿不到就是空串 —— 展示名回落到昵称。
    pub phone: String,
}

impl Credential {
    /// 从账号记录形态读回凭据（兼容 camelCase 与 snake_case 键）。
    ///
    /// ── 两种形状都要认（转发 401 的根因就在这一条）────────────────
    /// 本函数的调用点手上拿的 JSON **不是同一种形状**：
    ///   - **记录形态**（accounts 表的 `data` 列 / 导出文件）：凭证平铺在顶层
    ///     （`accessToken` / `refreshToken` / …）。`credential_of`（余额 / 签到 /
    ///     模型目录）、凭证维护、续期回写走的都是这一种；
    ///   - **会话形态**（`AccountStore::session_from_record` 的返回值）：凭证在
    ///     `auth` 下（`auth.accessToken`）、账号属性在 `account` 下。转发链路的
    ///     `ProviderAdapter::build_chat_request` 拿到的**就是它**
    ///     （见 `upstream::provider_loop` 的 `adapter.build_chat_request(&session, …)`）。
    /// 只认平铺会让会话形态读出空 token，于是**每一次转发都稳定 401**
    /// （「MiniMax 账号缺少 accessToken，无法转发」），而账号在界面上一切正常
    /// —— 账号页读的是记录形态，两边看到的不是同一份数据。两种形状都认，
    /// 这条静默失败才不可能发生。
    pub fn from_value(value: &Value) -> Self {
        // 会话形态把凭证收在 `auth` 下；记录形态没有这个键，此时 auth 即本体
        // （于是「平铺」这条既有语义逐字不变）。
        let auth = value
            .get("auth")
            .filter(|item| item.is_object())
            .unwrap_or(value);
        let text = |keys: &[&str]| {
            for key in keys {
                if let Some(found) = auth.get(*key).and_then(Value::as_str) {
                    if !found.is_empty() {
                        return found.to_string();
                    }
                }
            }
            String::new()
        };
        // `expiresAt` 的**类型随形状变**：记录形态是毫秒时间戳**字符串**
        // （`patch_fields` 写的是 `Value::String`），会话形态是**数字**
        // （`session_from_record` 把 `record.expires_at()` 的 f64 原样写出）。
        // 只认字符串会让会话形态的过期时间恒为空 ⇒ `credential_expires_at_ms`
        // 回落 JWT（本家 token 非 JWT ⇒ None）⇒ 永远判成「不会过期」。
        // 两种都读，`credential_expires_at_ms` 的「秒 / 毫秒」归一照旧生效。
        let expires_at = {
            let as_text = text(&["expiresAt", "expires_at"]);
            if !as_text.is_empty() {
                as_text
            } else {
                auth.get("expiresAt")
                    .and_then(Value::as_f64)
                    .filter(|value| *value > 0.0)
                    .map(|value| (value as i64).to_string())
                    .unwrap_or_default()
            }
        };
        Self {
            access_token: text(&["accessToken", "access_token"]),
            refresh_token: text(&["refreshToken", "refresh_token"]),
            token_type: text(&["tokenType", "token_type"]),
            expires_at,
            scope: text(&["scope"]),
            nickname: text(&["nickname"]),
            real_user_id: text(&["realUserId", "real_user_id"]),
            phone: text(&["phone"]),
        }
    }

    /// 转成账号记录的字段补丁（**只含非空值**）。
    pub fn patch_fields(&self) -> Vec<(&'static str, Value)> {
        let mut out: Vec<(&'static str, Value)> = Vec::new();
        let mut push = |key: &'static str, value: &str| {
            if !value.is_empty() {
                out.push((key, Value::String(value.to_string())));
            }
        };
        push("accessToken", &self.access_token);
        push("refreshToken", &self.refresh_token);
        push("tokenType", &self.token_type);
        push("expiresAt", &self.expires_at);
        push("scope", &self.scope);
        push("nickname", &self.nickname);
        push("realUserId", &self.real_user_id);
        push("phone", &self.phone);
        out
    }

    /// 凭据是否可用（有 access_token 即视为可用）。
    pub fn valid(&self) -> bool {
        !self.access_token.trim().is_empty()
    }

    /// 凭据是否可续期（必须有 refresh_token）。
    pub fn can_refresh(&self) -> bool {
        !self.refresh_token.trim().is_empty()
    }
}

/// 从 JWT 解出 `exp`（毫秒）。
///
/// ⚠️ **实测：MiniMax 的 token 不是 JWT**（前缀 `mmoat_`、60 字符、0 个点），
/// 故本函数对真实凭据**恒返回 `None`**，`credential_expires_at_ms` 实际
/// **永远**走 `expires_at` 回退。保留本函数仅为兼容上游将来改发 JWT ——
/// **不要**据此省略 `expires_at` 的写入。
pub fn decode_jwt_exp_ms(token: &str) -> Option<i64> {
    let mut segments = token.split('.');
    let _header = segments.next()?;
    let payload = segments.next()?;
    if segments.next().is_none() {
        return None;
    }
    use base64::Engine;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    let parsed: Value = serde_json::from_slice(&decoded).ok()?;
    let exp = parsed.get("exp")?.as_f64()?;
    let ms = exp * 1000.0;
    // 溢出保护：`exp` 极大时乘 1000 会变 `inf`，那会被下游当成「永不过期」
    if ms.is_finite() && ms > 0.0 {
        Some(ms as i64)
    } else {
        None
    }
}

/// 凭据过期时间（毫秒）。
///
/// ⚠️ **优先 `expires_at`**（真实 token 不是 JWT，JWT 路径恒不命中）。
/// ⚠️ **单位兼容**：`expires_at` 是**秒**时按秒换算（判据 `> 1e12` 视为毫秒）。
/// 不兼容会让秒级值被当成 1970 年 ⇒ **恒判已过期**、每次使用都触发无谓续期。
/// ⚠️ 只认**纯数字**串（`'1e12'` / `' 123 '` / `'123abc'` 一律非法并回退 JWT）。
pub fn credential_expires_at_ms(credential: &Credential) -> Option<i64> {
    let raw = credential.expires_at.as_str();
    if !raw.is_empty() && raw.chars().all(|ch| ch.is_ascii_digit()) {
        if let Ok(value) = raw.parse::<i64>() {
            let parsed = if value > 1_000_000_000_000 { value } else { value.saturating_mul(1000) };
            if parsed > 0 {
                return Some(parsed);
            }
        }
    }
    decode_jwt_exp_ms(&credential.access_token)
}

/// 凭据是否已过期（无过期信息时保守视为未过期）。
pub fn is_expired(credential: &Credential, now_ms: i64) -> bool {
    match credential_expires_at_ms(credential) {
        Some(expires_at) => now_ms >= expires_at,
        None => false,
    }
}

/// 令牌响应的解析结果。
#[derive(Clone, Debug, Default)]
pub struct TokenGrant {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: Option<f64>,
    pub scope: String,
}

/// 解析令牌响应（轮询与续期共用）。
///
/// 硬校验（照 asar `parseTokenGrant`，不满足即报错）：
/// `access_token` 非空 / `refresh_token` 非空（缺失时回退 `previous_refresh`）/
/// `token_type` 是 Bearer / `expires_in` 是正数 / `scope` 含声明的 scope。
pub fn parse_token_grant(
    payload: &Value,
    previous_refresh: Option<&str>,
    required_scope: &str,
) -> Result<TokenGrant, String> {
    let Some(record) = payload.as_object() else {
        return Err("MiniMax 令牌响应不是对象".to_string());
    };
    let text = |key: &str| record.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let access_token = text("access_token");
    let refresh_token = {
        let current = text("refresh_token");
        if current.is_empty() {
            previous_refresh.unwrap_or("").to_string()
        } else {
            current
        }
    };
    let token_type = text("token_type");
    let expires_in = record
        .get("expires_in")
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.trim().parse::<f64>().ok()))
        })
        .filter(|value| *value > 0.0);
    let scope = text("scope");
    if access_token.is_empty() {
        return Err("MiniMax 令牌响应缺少 access_token".to_string());
    }
    if refresh_token.is_empty() {
        return Err("MiniMax 令牌响应缺少 refresh_token".to_string());
    }
    if !token_type.eq_ignore_ascii_case("bearer") {
        return Err("MiniMax 令牌响应的 token_type 不是 Bearer".to_string());
    }
    let Some(expires_in) = expires_in else {
        return Err("MiniMax 令牌响应缺少 expires_in".to_string());
    };
    // scope 必须含声明的 scope（否则上游后续会报 invalid_token_response）
    if !required_scope.is_empty()
        && !scope
            .split_whitespace()
            .any(|item| item == required_scope)
    {
        return Err(format!(
            "MiniMax 令牌响应的 scope 不含 {required_scope}（实际：{scope}）"
        ));
    }
    Ok(TokenGrant {
        access_token,
        refresh_token,
        token_type,
        expires_in: Some(expires_in),
        scope,
    })
}

/// 由令牌响应组装可持久化的凭据（`expires_at` **必须**在这里写入）。
pub fn build_credential(
    grant: &TokenGrant,
    now_ms: i64,
    previous: Option<&Credential>,
) -> Credential {
    let expires_at = match grant.expires_in {
        Some(seconds) if seconds > 0.0 => {
            ((now_ms as f64) + seconds * 1000.0).round() as i64
        }
        _ => previous
            .and_then(credential_expires_at_ms)
            .unwrap_or(0),
    };
    Credential {
        access_token: grant.access_token.clone(),
        refresh_token: if grant.refresh_token.is_empty() {
            previous.map(|item| item.refresh_token.clone()).unwrap_or_default()
        } else {
            grant.refresh_token.clone()
        },
        token_type: if grant.token_type.is_empty() {
            "Bearer".to_string()
        } else {
            grant.token_type.clone()
        },
        expires_at: if expires_at > 0 { expires_at.to_string() } else { String::new() },
        scope: grant.scope.clone(),
        nickname: previous.map(|item| item.nickname.clone()).unwrap_or_default(),
        // 续期不改变账号身份：realUserID 从上一份凭据继承（OAuth 来源本来就是空）
        real_user_id: previous.map(|item| item.real_user_id.clone()).unwrap_or_default(),
        // 手机号同理：它是账号属性，续期响应里没有，必须从上一份继承
        phone: previous.map(|item| item.phone.clone()).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 记录形态（凭证平铺）与**会话形态**（凭证在 `auth` 下）都要读出 token。
    ///
    /// 这条用例钉的是「转发稳定 401」那个缺陷：转发链路的
    /// `build_chat_request` 拿到的是 `session_from_record` 的返回值（`auth` 嵌套），
    /// 而只认平铺的取值会让 `valid()` 恒为假 —— 于是每一次转发都在
    /// 「MiniMax 账号缺少 accessToken，无法转发」上失败，账号页却一切正常。
    #[test]
    fn from_value_reads_both_record_and_session_shape() {
        // 记录形态：凭证平铺在顶层（accounts 表的 data 列）
        let record = Credential::from_value(&json!({
            "accessToken": "mmoat_flat",
            "refreshToken": "mmort_flat",
            "expiresAt": "1800000000000",
        }));
        assert_eq!(record.access_token, "mmoat_flat");
        assert_eq!(record.refresh_token, "mmort_flat");
        assert!(record.valid());
        assert!(record.can_refresh());

        // 会话形态：凭证在 `auth` 下（session_from_record 的返回值）
        let session = Credential::from_value(&json!({
            "endpoint": "https://agent.minimax.cn",
            "auth": {
                "accessToken": "mmoat_sess",
                "refreshToken": "mmort_sess",
                "tokenType": "Bearer",
                "expiresAt": 1800000000000_i64,
            },
            "account": { "uid": "548212364582207496", "nickname": "MiniMax597757" },
        }));
        assert_eq!(session.access_token, "mmoat_sess");
        assert_eq!(session.refresh_token, "mmort_sess");
        assert!(session.valid(), "会话形态必须被认出有 token（否则转发恒 401）");

        // `auth` 存在但不是对象（脏数据）：回落顶层，不 panic、不误判
        let weird = Credential::from_value(&json!({
            "auth": "not-an-object",
            "accessToken": "mmoat_top",
        }));
        assert_eq!(weird.access_token, "mmoat_top");
        // `auth` 是空对象：不得从顶层再取（那会让形状判定变成「哪里有一把就用哪把」）
        let empty_auth = Credential::from_value(&json!({
            "auth": {},
            "accessToken": "mmoat_top",
        }));
        assert!(!empty_auth.valid(), "auth 是对象就以它为准，顶层值不参与");
    }

    #[test]
    fn expiry_prefers_expires_at_and_accepts_seconds() {
        let mut credential = Credential {
            access_token: "mmoat_x".to_string(),
            expires_at: "1800000000000".to_string(),
            ..Default::default()
        };
        assert_eq!(credential_expires_at_ms(&credential), Some(1_800_000_000_000));
        credential.expires_at = "1800000000".to_string();
        assert_eq!(credential_expires_at_ms(&credential), Some(1_800_000_000_000));
        // 非法串：回退 JWT（非 JWT → None）
        credential.expires_at = "1e12".to_string();
        assert_eq!(credential_expires_at_ms(&credential), None);
        credential.expires_at = " 123 ".to_string();
        assert_eq!(credential_expires_at_ms(&credential), None);
    }

    #[test]
    fn token_grant_validates_scope_and_type() {
        let ok = json!({
            "access_token": "mmoat_a",
            "refresh_token": "mmort_r",
            "token_type": "Bearer",
            "expires_in": 3600,
            "scope": "agent.default other.scope"
        });
        let grant = parse_token_grant(&ok, None, "agent.default").expect("应解析成功");
        assert_eq!(grant.access_token, "mmoat_a");
        assert_eq!(grant.expires_in, Some(3600.0));
        // scope 不含声明的 scope → 报错
        let bad_scope = json!({
            "access_token": "a", "refresh_token": "r", "token_type": "Bearer",
            "expires_in": 1, "scope": "other"
        });
        assert!(parse_token_grant(&bad_scope, None, "agent.default").is_err());
        // token_type 不是 Bearer → 报错
        let bad_type = json!({
            "access_token": "a", "refresh_token": "r", "token_type": "mac",
            "expires_in": 1, "scope": "agent.default"
        });
        assert!(parse_token_grant(&bad_type, None, "agent.default").is_err());
        // 缺 expires_in → 报错
        let no_expiry = json!({
            "access_token": "a", "refresh_token": "r", "token_type": "Bearer", "scope": "agent.default"
        });
        assert!(parse_token_grant(&no_expiry, None, "agent.default").is_err());
        // refresh_token 缺失时回退上一个
        let fallback = json!({
            "access_token": "a", "token_type": "Bearer", "expires_in": 1, "scope": "agent.default"
        });
        let grant = parse_token_grant(&fallback, Some("old-r"), "agent.default").expect("应回退");
        assert_eq!(grant.refresh_token, "old-r");
    }

    #[test]
    fn build_credential_writes_expires_at() {
        let grant = TokenGrant {
            access_token: "a".to_string(),
            refresh_token: "r".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: Some(3600.0),
            scope: "agent.default".to_string(),
        };
        let credential = build_credential(&grant, 1_000_000, None);
        assert_eq!(credential.expires_at, "4600000");
        assert!(credential.can_refresh());
    }
}
