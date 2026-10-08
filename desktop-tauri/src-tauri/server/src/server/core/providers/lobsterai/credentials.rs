//! LobsterAI 凭据结构、统一信封解析与续期载荷（纯函数，可完整单测）。
//!
//! ── 与 `BuddyCredential` 的关键结构差异（最容易漏的地方）────────
//! LobsterAI 的 **refresh 请求体不是只带 refreshToken**，还要带
//! `firstKeyfrom` / `latestKeyfrom` / `uuid`（`auth.go:37-50` +
//! `client.go:112-147`）。这三个字段必须随凭据一起持久化 ——
//! 丢了就续期失败、只能让用户重新登录。
//!
//! ── `latest_keyfrom` 刻意不随刷新更新 ────────────────────────
//! 虽然字段名叫「最近活动」，但严格对齐 Go：`RefreshToken`
//! （`client.go:137-145`）只改 token 与过期时间，`LatestKeyfrom` 永久停留在
//! 登录时那一刻，续期时原样回发。语义上「刷新即活动、理应更新」是更直觉的
//! 读法，但 Go 是唯一在生产验证过的实现；若服务端对该字段有校验，
//! 自作聪明地更新会让续期失败，而这不是能从代码推导出来的。
//!
//! ── 统一信封 `{code, msg, data}` ─────────────────────────────
//! `code !== 0` 即失败。**`data` 必须是非数组对象** —— 上游在凭据失效时
//! 倾向于返回 `code:0` 但 `data:null`，只看 code 会把这种情况当成成功，
//! 随后在解引用时崩在更远的地方（`sigin.py:46-47` 的双重校验）。

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// 持久化的 LobsterAI 凭据（`snake_case` 与参考实现一致）。
///
/// 字段名刻意保持 `snake_case`：账号池侧对非 codearts 的 provider 统一取
/// `access_token` 作身份标识，命名一致才能直接复用。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credential {
    /// 访问令牌（`Authorization: Bearer <access_token>`）。
    pub access_token: String,
    /// 刷新令牌（exchange/refresh 返回；允许为空串 —— Go 侧按「可缺失」处理）。
    pub refresh_token: String,
    /// 过期时间（**毫秒时间戳字符串**；与既有约定一致）。
    pub expires_at: String,
    /// 用户唯一 ID（账号池去重、候选排序的标识）。
    pub uid: String,
    /// 有道 yid（用于 refresh 请求体，与 uid 不一定相同）。
    pub user_id: String,
    /// 昵称（UI 展示）。
    pub nickname: String,
    /// 安装 UUID（exchange/refresh 必带；**不能每次刷新都重新生成**）。
    pub uuid: String,
    /// 首次登录时间戳（毫秒字符串，exchange/refresh 必带）。
    pub first_keyfrom: String,
    /// 最近活动时间戳（毫秒字符串；**不随刷新更新**，见模块头）。
    pub latest_keyfrom: String,
}

impl Credential {
    /// 从账号记录形态（`Value`）读回凭据。
    ///
    /// 字段名与落盘键名一致（camelCase）；同时兼容参考实现的 snake_case
    /// 形态（手工粘贴的凭据可能来自 `lobsterai2api` 的 `auths/*.json`），
    /// 缺失字段回落空串（与 `patch_fields` 同口径）。
    ///
    /// ── 两种形状都要认（转发 401 的根因就在这一条）────────────────
    /// 本函数的调用点手上拿的 JSON **不是同一种形状**：
    ///   - **记录形态**（accounts 表的 `data` 列 / 导出文件）：凭证平铺在顶层
    ///     （`accessToken` / `uid` / `firstKeyfrom` / …）。凭证维护、续期回写、
    ///     余额 / 签到 / 模型目录走的都是这一种；
    ///   - **会话形态**（`AccountStore::session_from_record` 的返回值）：凭证在
    ///     `auth` 下（`auth.accessToken`）、账号属性在 `account` 下。转发链路的
    ///     `ProviderAdapter::build_chat_request` 拿到的**就是它**
    ///     （见 `upstream::provider_loop` 的 `adapter.build_chat_request(&session, …)`）。
    /// 只认平铺会让会话形态读出空 token，于是**每一次转发都稳定 401**
    /// （「LobsterAI 账号缺少 accessToken，无法转发」），而账号在界面上一切正常
    /// —— 账号页读的是记录形态，两边看到的不是同一份数据。
    ///
    /// `uid` 从 `account.uid` 回退：会话形态把它放在 `account` 下
    /// （与 workbuddy / 小浣熊的会话约定一致）。它是**展示与日志**用的身份，
    /// 缺失不影响转发（凭证本体在 `auth`）。
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
        Self {
            access_token: text(&["accessToken", "access_token"]),
            refresh_token: text(&["refreshToken", "refresh_token"]),
            // `expiresAt` 的**类型随形状变**：记录形态是毫秒时间戳**字符串**
            // （`patch_fields` 写的是 `Value::String`），会话形态是**数字**
            // （`session_from_record` 把 `record.expires_at()` 的 f64 原样写出）。
            // 只认字符串会让会话形态的过期时间恒为空 —— 本家 token 是 JWT，
            // 于是静默回落到解 `exp`（当前结论相同，但那是巧合而不是契约）。
            // 两种都读，`credential_expires_at_ms` 的「秒 / 毫秒」归一照旧生效。
            expires_at: {
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
            },
            uid: {
                let flat = text(&["uid"]);
                if !flat.is_empty() {
                    flat
                } else {
                    value
                        .get("account")
                        .and_then(|account| account.get("uid"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string()
                }
            },
            user_id: text(&["userId", "user_id"]),
            nickname: text(&["nickname"]),
            uuid: text(&["uuid"]),
            first_keyfrom: text(&["firstKeyfrom", "first_keyfrom"]),
            latest_keyfrom: text(&["latestKeyfrom", "latest_keyfrom"]),
        }
    }

    /// 转成账号记录的字段补丁（**只含非空值**，调用方决定合并策略）。
    ///
    /// 键名：`accessToken` / `refreshToken` / `expiresAt` / `uid` / `userId` /
    /// `nickname` / `uuid` / `firstKeyfrom` / `latestKeyfrom`。
    /// 与参考实现的落盘字段逐字对应（前四个是既有通用键，后五个是本家专有）。
    pub fn patch_fields(&self) -> Vec<(&'static str, Value)> {
        let mut out: Vec<(&'static str, Value)> = Vec::new();
        let mut push = |key: &'static str, value: &str| {
            if !value.is_empty() {
                out.push((key, Value::String(value.to_string())));
            }
        };
        push("accessToken", &self.access_token);
        push("refreshToken", &self.refresh_token);
        push("expiresAt", &self.expires_at);
        push("uid", &self.uid);
        push("userId", &self.user_id);
        push("nickname", &self.nickname);
        push("uuid", &self.uuid);
        push("firstKeyfrom", &self.first_keyfrom);
        push("latestKeyfrom", &self.latest_keyfrom);
        out
    }

    /// 凭据是否可用（有 access_token 即视为可用）。
    pub fn valid(&self) -> bool {
        !self.access_token.trim().is_empty()
    }

    /// 凭据是否携带可静默续期的 refresh_token。
    pub fn can_refresh(&self) -> bool {
        !self.refresh_token.trim().is_empty()
    }
}

// ── 统一信封 ──

/// 信封解析成功。
#[derive(Clone, Debug)]
pub struct EnvelopeSuccess {
    pub data: Value,
}

/// 信封解析失败（业务码非 0，或 data 缺失/非对象）。
#[derive(Clone, Debug)]
pub struct EnvelopeFailure {
    /// 业务码；无法读出时为 -1。
    pub code: i64,
    /// 可读原因（优先服务端 msg，缺失时给出本地的结构性说明）。
    pub message: String,
}

/// 从 JSON 安全读取字符串字段（兼容后端把数字返回成 number）。
pub fn read_string(source: &Value, key: &str) -> String {
    match source.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// 从 JSON 安全读取数字字段（兼容字符串形态的数字）。
pub fn read_number(source: &Value, key: &str) -> Option<f64> {
    match source.get(key) {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            trimmed.parse::<f64>().ok()
        }
        _ => None,
    }
}

/// 解析 `{code, msg, data}` 信封。
///
/// 三条判定（对齐 `main.go:143-150` 与 `sigin.py:44-48` 的双重校验）：
/// 1. 响应体必须是对象；
/// 2. `code` 必须为 `0`；
/// 3. `data` 必须是**对象**（非数组）—— 见模块头说明。
pub fn parse_envelope(body: &Value) -> Result<EnvelopeSuccess, EnvelopeFailure> {
    let Some(record) = body.as_object() else {
        return Err(EnvelopeFailure { code: -1, message: "响应不是 JSON 对象".to_string() });
    };
    let code = record
        .get("code")
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_f64().map(|number| number as i64))
                .or_else(|| value.as_str().and_then(|text| text.trim().parse::<i64>().ok()))
        })
        .unwrap_or(-1);
    let message = {
        let msg = record.get("msg").and_then(Value::as_str).unwrap_or("");
        if !msg.is_empty() {
            msg.to_string()
        } else {
            record.get("message").and_then(Value::as_str).unwrap_or("").to_string()
        }
    };
    if code != 0 {
        return Err(EnvelopeFailure {
            code,
            message: if message.is_empty() { format!("code={code}") } else { message },
        });
    }
    let data = record.get("data");
    match data {
        Some(value) if value.is_object() => Ok(EnvelopeSuccess { data: value.clone() }),
        _ => Err(EnvelopeFailure {
            code,
            message: if message.is_empty() {
                "data 为空（accessToken 可能已失效）".to_string()
            } else {
                message
            },
        }),
    }
}

// ── 身份载荷（exchange / refresh / models 共用） ──

/// 构造 `keyfrom` 身份载荷。
///
/// 对应 Go 的 `Auth.KeyfromBody()`（`auth.go:37-50`）。三个字段的含义：
///   - `firstKeyfrom` —— 首次登录时间戳，标识「这个账号是什么时候开始用的」；
///   - `latestKeyfrom` —— 最近活动时间戳（用**凭据里存储的原值**，不取当前时刻）；
///   - `version` —— 客户端版本号（动态真值，由调用方传入）；
///   - `uuid` / `userId` —— 可选，缺失时**不带该键**（而非带空串）。
pub fn keyfrom_body(credential: &Credential, client_version: &str) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("firstKeyfrom".to_string(), Value::String(credential.first_keyfrom.clone()));
    body.insert("latestKeyfrom".to_string(), Value::String(credential.latest_keyfrom.clone()));
    body.insert("version".to_string(), Value::String(client_version.to_string()));
    if !credential.uuid.is_empty() {
        body.insert("uuid".to_string(), Value::String(credential.uuid.clone()));
    }
    if !credential.user_id.is_empty() {
        body.insert("userId".to_string(), Value::String(credential.user_id.clone()));
    }
    Value::Object(body)
}

/// 构造续期请求体 = keyfrom 载荷 + `refreshToken`。
pub fn refresh_body(credential: &Credential, client_version: &str) -> Value {
    let mut body = keyfrom_body(credential, client_version);
    if let Value::Object(map) = &mut body {
        map.insert("refreshToken".to_string(), Value::String(credential.refresh_token.clone()));
    }
    body
}

/// 构造 exchange 请求体（5 个字段，对齐 `main.go:264-270`）。
///
/// `authCode` / `firstKeyfrom` / `latestKeyfrom` / `uuid` / `version`。
pub fn exchange_body(
    auth_code: &str,
    uuid: &str,
    first_keyfrom: &str,
    latest_keyfrom: &str,
    client_version: &str,
) -> Value {
    json!({
        "authCode": auth_code,
        "firstKeyfrom": first_keyfrom,
        "latestKeyfrom": latest_keyfrom,
        "uuid": uuid,
        "version": client_version,
    })
}

// ── 登录 / 续期响应解析 ──

/// exchange / refresh 响应的令牌部分。
#[derive(Clone, Debug, Default)]
pub struct TokenPayload {
    pub access_token: String,
    pub refresh_token: String,
    /// 相对过期秒数；缺失时 None（由 JWT exp 兜底）。
    pub expires_in: Option<f64>,
    /// 用户 ID 候选（`user.id`）。
    pub user_id: Option<String>,
    /// 有道 yid（`user.yid`）。
    pub yid: Option<String>,
    /// 账号 userId（`user.userId`）。
    pub account_user_id: Option<String>,
    /// 昵称。
    pub nickname: Option<String>,
}

/// 解析 exchange / refresh 响应里的令牌与用户信息。
pub fn parse_token_payload(data: &Value) -> TokenPayload {
    let user = data.get("user").cloned().unwrap_or(Value::Null);
    let text_opt = |source: &Value, key: &str| {
        let value = read_string(source, key);
        if value.is_empty() { None } else { Some(value) }
    };
    TokenPayload {
        access_token: read_string(data, "accessToken"),
        refresh_token: read_string(data, "refreshToken"),
        expires_in: read_number(data, "expiresIn"),
        user_id: text_opt(&user, "id"),
        yid: text_opt(&user, "yid"),
        account_user_id: text_opt(&user, "userId"),
        nickname: text_opt(&user, "nickname"),
    }
}

/// 解析账号唯一 ID，按**四级**回退（严格对齐 `main.go:297-306`）：
///
/// `user.id` → `user.userId` → `user.yid` → `sha256(accessToken)` 前 16 位。
///
/// 前三者是服务端字段，不同账号形态下哪个非空并不固定（个人号与企业号不同）；
/// 末级哈希兜底保证**任何情况下都能得到一个稳定 ID** —— 否则空 uid 会让
/// 账号池里多个账号互相覆盖（按 id 去重）。
///
/// ⚠️ **刻意不在 `yid` 与哈希之间插入 JWT `sub` 回退**：Go 没有这一级，
/// 插进去会让「服务端三个 user 字段皆空」的账号在本项目得到 `sub`、
/// 而在 Go 得到 16 位哈希 —— 同一账号两种 uid，破坏与参考实现的对照能力。
pub fn resolve_uid(payload: &TokenPayload) -> String {
    for candidate in [&payload.user_id, &payload.account_user_id, &payload.yid] {
        if let Some(value) = candidate {
            if !value.is_empty() {
                return value.clone();
            }
        }
    }
    let digest = Sha256::digest(payload.access_token.as_bytes());
    // hex 前 16 字符（与 Go 的 `fmt.Sprintf("%x", sha256.Sum256(...))[:16]` 一致）
    let hex = digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    hex.chars().take(16).collect()
}

/// 由令牌载荷组装可持久化的凭据。
///
/// `expires_at` 的取值顺序（**与 Go 一致**，`main.go:313-319`）：
/// 1. `expiresIn`（相对秒数）→ 以**当前时刻**为基准换算；
/// 2. 缺失时用 access token 的 JWT `exp`；
/// 3. 都拿不到则留空（`credential_expires_at_ms` 会再尝试 JWT，
///    仍失败则「不判定过期」）。
///
/// ⚠️ 基准取当前时刻而非 JWT `iat`：Go 用的是 `time.Now()`。
pub fn build_credential(
    payload: &TokenPayload,
    session: (&str, &str, &str),
    now_ms: i64,
) -> Credential {
    let (uuid, first_keyfrom, latest_keyfrom) = session;
    let expires_at = match payload.expires_in {
        Some(seconds) if seconds > 0.0 => ((now_ms as f64 + seconds * 1000.0) as i64).to_string(),
        _ => jwt_expires_at_ms(&payload.access_token)
            .map(|value| value.to_string())
            .unwrap_or_default(),
    };
    Credential {
        access_token: payload.access_token.clone(),
        refresh_token: payload.refresh_token.clone(),
        expires_at,
        uid: resolve_uid(payload),
        user_id: payload
            .account_user_id
            .clone()
            .filter(|value| !value.is_empty())
            .or_else(|| payload.yid.clone())
            .unwrap_or_default(),
        nickname: payload.nickname.clone().unwrap_or_default(),
        uuid: uuid.to_string(),
        first_keyfrom: first_keyfrom.to_string(),
        latest_keyfrom: latest_keyfrom.to_string(),
    }
}

/// 用续期结果更新凭据（保留服务端未返回的字段）。
///
/// **所有身份字段一律沿用旧值**（`uuid` / `first_keyfrom` / `latest_keyfrom`
/// / `uid` / `user_id` / `nickname`）：LobsterAI 的 refresh 响应只带令牌，
/// 不含 account 对象。`latest_keyfrom` 也刻意不更新（见模块头）。
pub fn apply_refresh(previous: &Credential, payload: &TokenPayload, now_ms: i64) -> Credential {
    let expires_at = match payload.expires_in {
        Some(seconds) if seconds > 0.0 => ((now_ms as f64 + seconds * 1000.0) as i64).to_string(),
        _ => jwt_expires_at_ms(&payload.access_token)
            .map(|value| value.to_string())
            .unwrap_or_else(|| previous.expires_at.clone()),
    };
    Credential {
        access_token: payload.access_token.clone(),
        // refresh 响应可能不返回新 refreshToken（沿用旧的），不能覆盖成空串。
        refresh_token: if payload.refresh_token.is_empty() {
            previous.refresh_token.clone()
        } else {
            payload.refresh_token.clone()
        },
        expires_at,
        ..previous.clone()
    }
}

// ── 过期时间与可刷新判定 ──

/// 从 JWT 解出 `exp`（毫秒）。三段点分、payload 是 base64url JSON。
///
/// LobsterAI 的 access token 是 HS512 JWT（`main.go:317` 注释「实测 30 天」），
/// 因此 `exp` 是权威过期时刻 —— `expires_at` 缺失时的后备来源。
pub fn jwt_expires_at_ms(token: &str) -> Option<i64> {
    let mut segments = token.split('.');
    let _header = segments.next()?;
    let payload = segments.next()?;
    if segments.next().is_none() {
        return None;
    }
    let decoded = base64url_decode(payload)?;
    let parsed: Value = serde_json::from_slice(&decoded).ok()?;
    let exp = parsed.get("exp")?.as_f64()?;
    let ms = exp * 1000.0;
    // 溢出保护：`exp` 极大时（如 1e308）乘 1000 会变 `inf`，
    // 那会被下游当成「永不过期」的有效时刻 —— 宁可返回 None。
    if ms.is_finite() && ms > 0.0 {
        Some(ms as i64)
    } else {
        None
    }
}

/// base64url（无填充）解码。手写而不是拉依赖：JWT payload 的解码只需这一处，
/// 且 base64 crate 的 URL_SAFE_NO_PAD 引擎等价（见 Cargo 依赖里的 `base64`）。
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(input).ok()
}

/// 从凭据的 `expires_at` 解析毫秒时间戳。
///
/// 兼容毫秒时间戳 / 秒级时间戳 / ISO 8601 三种形态。后备来源：
/// `expires_at` 为空时回退解析 `access_token` 这个 JWT 的 `exp`。
///
/// ⚠️ 纯数字判定用**原串**（不 trim）—— 与参考实现的 `/^\d+$/` 同口径，
/// `" 123 "` 一律视为非法（`Number(' 123 ')` 会静默变 123，不能放行）。
pub fn credential_expires_at_ms(credential: &Credential) -> Option<i64> {
    let raw = credential.expires_at.as_str();
    if !raw.is_empty() {
        if raw.chars().all(|ch| ch.is_ascii_digit()) {
            if let Ok(value) = raw.parse::<i64>() {
                // > 1e12 视为毫秒，否则视为秒（既有约定）
                let parsed = if value > 1_000_000_000_000 { value } else { value * 1000 };
                if parsed > 0 {
                    return Some(parsed);
                }
            }
        } else if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) {
            return Some(parsed.timestamp_millis());
        }
    }
    jwt_expires_at_ms(&credential.access_token)
}

/// 凭据是否已过期；无法解析过期时间时**不**判定过期（与参考实现一致）。
pub fn is_expired(credential: &Credential, now_ms: i64) -> bool {
    match credential_expires_at_ms(credential) {
        Some(expires_at) => now_ms >= expires_at,
        None => false,
    }
}

// ── 手机号脱敏（账号展示名）──

/// 把手机号（完整或已脱敏）统一掩码为**只露末 2 位**的形态。
///
/// 服务端把**手机号本身**当昵称下发，故统一收敛到「只露末 2 位」；
/// 非手机号形态的昵称原样保留。
///
/// 判据是 11 位纯数字（`1` 开头），或 `3 位数字 + 星号 + 数字` 的脱敏形态。
/// 绝不能泛化到任意字符串 —— 那会把真实昵称也掩码掉。
/// 星号个数按**原串总长**推算，故对非 11 位的号码也自洽。
pub fn mask_phone_tail(value: &str, visible_tail: usize) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return trimmed.to_string();
    }
    let rebuild = |prefix: &str, total: usize, tail: &str| -> String {
        let star_count = total as i64 - prefix.len() as i64 - tail.len() as i64;
        if star_count < 0 {
            return trimmed.to_string();
        }
        format!("{prefix}{}{tail}", "*".repeat(star_count as usize))
    };
    // 形态 1：完整手机号（11 位纯数字，1 开头）
    let bytes = trimmed.as_bytes();
    if bytes.len() == 11 && bytes[0] == b'1' && bytes.iter().all(u8::is_ascii_digit) {
        let tail_start = trimmed.len().saturating_sub(visible_tail);
        return rebuild(&trimmed[..3], trimmed.len(), &trimmed[tail_start..]);
    }
    // 形态 2：服务端已脱敏 —— 3 位数字 + 星号 + 若干位后缀
    let mut prefix_len = 0;
    for byte in bytes {
        if byte.is_ascii_digit() {
            prefix_len += 1;
        } else {
            break;
        }
    }
    if prefix_len == 3 {
        let rest = &trimmed[3..];
        let star_count = rest.chars().take_while(|ch| *ch == '*').count();
        let suffix: String = rest.chars().skip(star_count).collect();
        if star_count > 0 && !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()) {
            let suffix_tail: String = {
                let chars: Vec<char> = suffix.chars().collect();
                let start = chars.len().saturating_sub(visible_tail);
                chars[start..].iter().collect()
            };
            let total = 3 + star_count + suffix.chars().count();
            return rebuild(&trimmed[..3], total, &suffix_tail);
        }
    }
    trimmed.to_string()
}

/// LobsterAI 账号的**展示名**：昵称经手机号掩码归一化。
/// 昵称为空时退回账号 id（与登录路径既有行为一致）。
pub fn display_nickname(credential: &Credential, fallback_id: &str) -> String {
    let nickname = credential.nickname.trim();
    if nickname.is_empty() {
        return fallback_id.to_string();
    }
    mask_phone_tail(nickname, 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_credential() -> Credential {
        Credential {
            access_token: "header.payload.sig".to_string(),
            refresh_token: "rt-1".to_string(),
            expires_at: "1800000000000".to_string(),
            uid: "u-1".to_string(),
            user_id: "yid-1".to_string(),
            nickname: "13011111100".to_string(),
            uuid: "uuid-1".to_string(),
            first_keyfrom: "1700000000000".to_string(),
            latest_keyfrom: "1700000000001".to_string(),
        }
    }

    /// 记录形态（凭证平铺）与**会话形态**（凭证在 `auth` 下）都要读出 token。
    ///
    /// 这条用例钉的是「转发稳定 401」那个缺陷：转发链路的
    /// `build_chat_request` 拿到的是 `session_from_record` 的返回值（`auth` 嵌套），
    /// 而只认平铺的取值会让 `valid()` 恒为假 —— 于是每一次转发都在
    /// 「LobsterAI 账号缺少 accessToken，无法转发」上失败，账号页却一切正常。
    #[test]
    fn from_value_reads_both_record_and_session_shape() {
        // 记录形态：凭证平铺在顶层（含本家专有的 uuid / firstKeyfrom）
        let record = Credential::from_value(&json!({
            "accessToken": "at-flat",
            "refreshToken": "rt-flat",
            "uid": "u-1",
            "uuid": "uuid-1",
            "firstKeyfrom": "1700000000000",
            "latestKeyfrom": "1700000000001",
            "expiresAt": "1800000000000",
        }));
        assert_eq!(record.access_token, "at-flat");
        assert_eq!(record.uid, "u-1");
        assert_eq!(record.uuid, "uuid-1");
        assert!(record.valid() && record.can_refresh());

        // 会话形态：凭证在 `auth` 下、账号属性在 `account` 下，`expiresAt` 是数字
        let session = Credential::from_value(&json!({
            "endpoint": "https://api.lobsterai.com",
            "auth": {
                "accessToken": "at-sess",
                "refreshToken": "rt-sess",
                "tokenType": "Bearer",
                "expiresAt": 1800000000000_i64,
            },
            "account": { "uid": "u-9", "nickname": "130******00" },
        }));
        assert_eq!(session.access_token, "at-sess");
        assert_eq!(session.refresh_token, "rt-sess");
        assert!(session.valid(), "会话形态必须被认出有 token（否则转发恒 401）");
        // uid 从 `account.uid` 回退（会话形态把它放在那儿）
        assert_eq!(session.uid, "u-9");
        assert_eq!(session.expires_at, "1800000000000");

        // 本家专有字段不在会话形态里（续期要用的 uuid / keyfrom）——
        // 如实为空，不编造：续期链路的 `can_refresh` 只认 refreshToken，
        // 而 refresh 请求体的这几个字段由**记录形态**那条路径提供
        assert!(session.uuid.is_empty());
        assert!(session.first_keyfrom.is_empty());

        // `auth` 是空对象：以它为准，不回头取顶层
        let empty_auth = Credential::from_value(&json!({
            "auth": {},
            "accessToken": "at-top",
        }));
        assert!(!empty_auth.valid(), "auth 是对象就以它为准，顶层值不参与");
    }

    #[test]
    fn envelope_requires_object_data() {
        // code=0 但 data=null 是「凭据失效」的伪装形态，必须判失败
        let body = json!({"code": 0, "msg": "OK", "data": null});
        let failure = parse_envelope(&body).expect_err("data 为空必须失败");
        assert_eq!(failure.code, 0);
        // 无 msg 时给出本地结构性说明（有 msg 时透传服务端文案）
        let body = json!({"code": 0, "data": null});
        let failure = parse_envelope(&body).expect_err("data 为空必须失败");
        assert!(failure.message.contains("data 为空"));
        // data 是数组同样失败
        let body = json!({"code": 0, "data": [1, 2]});
        assert!(parse_envelope(&body).is_err());
        // 正常形态
        let body = json!({"code": 0, "data": {"accessToken": "a"}});
        assert!(parse_envelope(&body).is_ok());
        // 业务码非 0 优先回服务端文案
        let body = json!({"code": 40001, "msg": "令牌已失效", "data": {}});
        let failure = parse_envelope(&body).expect_err("业务码非 0 必须失败");
        assert_eq!(failure.code, 40001);
        assert_eq!(failure.message, "令牌已失效");
    }

    #[test]
    fn keyfrom_body_omits_empty_optionals() {
        let mut credential = sample_credential();
        credential.uuid = String::new();
        credential.user_id = String::new();
        let body = keyfrom_body(&credential, "2026.9.4");
        assert!(body.get("uuid").is_none(), "空 uuid 不该带键");
        assert!(body.get("userId").is_none(), "空 userId 不该带键");
        assert_eq!(body.get("firstKeyfrom").and_then(Value::as_str), Some("1700000000000"));
        assert_eq!(body.get("latestKeyfrom").and_then(Value::as_str), Some("1700000000001"));
        assert_eq!(body.get("version").and_then(Value::as_str), Some("2026.9.4"));
    }

    #[test]
    fn refresh_body_carries_refresh_token() {
        let credential = sample_credential();
        let body = refresh_body(&credential, "2026.9.4");
        assert_eq!(body.get("refreshToken").and_then(Value::as_str), Some("rt-1"));
        assert_eq!(body.get("uuid").and_then(Value::as_str), Some("uuid-1"));
    }

    #[test]
    fn uid_falls_back_four_levels() {
        // 一级：user.id
        let payload = TokenPayload {
            access_token: "token-a".to_string(),
            user_id: Some("id-1".to_string()),
            account_user_id: Some("acc-1".to_string()),
            yid: Some("yid-1".to_string()),
            ..Default::default()
        };
        assert_eq!(resolve_uid(&payload), "id-1");
        // 二级：user.userId
        let payload = TokenPayload {
            access_token: "token-a".to_string(),
            user_id: None,
            account_user_id: Some("acc-1".to_string()),
            yid: Some("yid-1".to_string()),
            ..Default::default()
        };
        assert_eq!(resolve_uid(&payload), "acc-1");
        // 三级：user.yid
        let payload = TokenPayload {
            access_token: "token-a".to_string(),
            user_id: None,
            account_user_id: None,
            yid: Some("yid-1".to_string()),
            ..Default::default()
        };
        assert_eq!(resolve_uid(&payload), "yid-1");
        // 四级：sha256(accessToken) 前 16 位 hex
        let payload = TokenPayload { access_token: "token-a".to_string(), ..Default::default() };
        let uid = resolve_uid(&payload);
        assert_eq!(uid.len(), 16);
        assert!(uid.chars().all(|ch| ch.is_ascii_hexdigit()));
        // 稳定性：同一 token 两次结果一致
        assert_eq!(uid, resolve_uid(&payload));
    }

    #[test]
    fn build_credential_uses_expires_in_first() {
        let payload = TokenPayload {
            access_token: "a.b.c".to_string(),
            refresh_token: "rt".to_string(),
            expires_in: Some(3600.0),
            ..Default::default()
        };
        let credential = build_credential(&payload, ("uuid", "f", "l"), 1_000_000);
        // 1000000 + 3600*1000 = 4600000
        assert_eq!(credential.expires_at, "4600000");
        assert_eq!(credential.uuid, "uuid");
    }

    #[test]
    fn apply_refresh_keeps_identity_and_old_refresh_token() {
        let previous = sample_credential();
        let payload = TokenPayload {
            access_token: "new-token".to_string(),
            refresh_token: String::new(), // 响应不带新 refreshToken
            expires_in: Some(100.0),
            ..Default::default()
        };
        let next = apply_refresh(&previous, &payload, 5_000);
        assert_eq!(next.access_token, "new-token");
        assert_eq!(next.refresh_token, "rt-1", "旧 refreshToken 必须保留");
        assert_eq!(next.uid, "u-1", "身份字段沿用旧值");
        assert_eq!(next.latest_keyfrom, "1700000000001", "latest_keyfrom 不更新");
        assert_eq!(next.expires_at, "105000");
    }

    #[test]
    fn mask_phone_tail_is_idempotent() {
        assert_eq!(mask_phone_tail("13011111100", 2), "130******00");
        assert_eq!(mask_phone_tail("130****1100", 2), "130******00");
        assert_eq!(mask_phone_tail("130******00", 2), "130******00");
        // 非手机号形态不误伤
        assert_eq!(mask_phone_tail("测试账号", 2), "测试账号");
        assert_eq!(mask_phone_tail("用户26815487395", 2), "用户26815487395");
        assert_eq!(mask_phone_tail("", 2), "");
    }

    #[test]
    fn credential_expiry_reads_seconds_and_millis() {
        let mut credential = sample_credential();
        credential.expires_at = "1800000000000".to_string();
        assert_eq!(credential_expires_at_ms(&credential), Some(1_800_000_000_000));
        credential.expires_at = "1800000000".to_string();
        assert_eq!(credential_expires_at_ms(&credential), Some(1_800_000_000_000));
        // 非数字非 ISO：回退 JWT（这里 token 不是合法 JWT，故 None）
        credential.expires_at = "not-a-time".to_string();
        assert_eq!(credential_expires_at_ms(&credential), None);
    }
}
