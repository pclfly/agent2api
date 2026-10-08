//! Loomy（讯飞办公助手）凭据结构、业务信封解析与展示名规范化。
//!
//! ── 三个必须记住的坑（参考实现 `loomy.ts` 的模块头）────────────
//! 1. **两套认证头**：`/chat/completions` 只认 `Authorization: Bearer`，
//!    而 `/models`、`/points/*`、`/onboarding/*` 只认小写 `token` 头。
//!    实测交叉验证：带错的那个 → HTTP 200 + `{"code":"100002","desc":"缺少 token"}`。
//! 2. **倍率在 `name` 字符串里**，没有独立字段，且三种括号风格混用
//!    （`MiniMax M3 （x4.0）` / `Qwen 3.8 Max (x12.0)` / `GLM 5.3 Flash(x0.8)`），
//!    必须规范化。
//! 3. **没有 refresh 端点**：`session` 是登录时声明 14 天得来的，过期只能重新
//!    短信登录。故 `can_refresh` 恒 false —— 诚实标记，不是遗漏。
//!
//! ── 业务信封 ────────────────────────────────────────────────
//! Loomy 的业务失败**恒返回 HTTP 200**，成败只能读 body 的 `code`。
//! 只看状态码会把「登录已失效」误判成成功。成功码 `"000000"`；
//! 登录失效码 `"100002"`（收到它**不得重试**，应标记凭据失效）。

use serde_json::Value;

/// 业务成功码。
pub const OK_CODE: &str = "000000";
/// 登录失效码（收到它不得重试，应提示重新登录）。
pub const AUTH_ERROR_CODE: &str = "100002";
/// 参数错误码（如未知 task key）。
pub const BAD_REQUEST_CODE: &str = "100001";

/// 持久化的 Loomy 凭据。
///
/// 字段名用本项目的 camelCase 落盘习惯（`accessToken` / `userId` / …）；
/// 参考实现的 snake_case 字段在 `from_value` 里兼容读取。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credential {
    /// 讯飞 session（32 位小写 hex）。
    pub access_token: String,
    /// 讯飞用户 id（18 位数字串）。
    pub userid: String,
    /// 绑定的手机号（11 位）。
    pub phone: String,
    /// 展示用昵称；Loomy 无昵称接口，缺省时 UI 回退到账号 id。
    pub nickname: String,
    /// 过期时间（**毫秒时间戳字符串**；由登录时刻 + 14 天推算，
    /// 不是服务端下发的值 —— 服务端只接受登录请求里的 `expire` 参数）。
    pub expires_at: String,
}

impl Credential {
    /// 从账号记录形态读回凭据（兼容 camelCase 与参考实现的 snake_case 键）。
    ///
    /// ── 两种形状都要认（转发 401 的根因就在这一条）────────────────
    /// 本函数的调用点手上拿的 JSON **不是同一种形状**：
    ///   - **记录形态**（accounts 表的 `data` 列 / 导出文件）：凭证平铺在顶层
    ///     （`accessToken` / `userId` / `phone` / …）。余额 / 签到 / 模型目录走的
    ///     都是这一种；
    ///   - **会话形态**（`AccountStore::session_from_record` 的返回值）：凭证在
    ///     `auth` 下（`auth.accessToken`）、账号属性在 `account` 下。转发链路的
    ///     `ProviderAdapter::build_chat_request` 拿到的**就是它**
    ///     （见 `upstream::provider_loop` 的 `adapter.build_chat_request(&session, …)`）。
    /// 只认平铺会让会话形态读出空 session，于是**每一次转发都稳定 401**
    /// （「Loomy 账号缺少 session，无法转发」），而账号在界面上一切正常
    /// —— 账号页读的是记录形态，两边看到的不是同一份数据。
    ///
    /// `userid` **刻意不从会话的 `account.uid` 回退**：会话那个字段取自记录的
    /// `uid`（见 `AccountStore::session_from_record`），而本家的记录把用户 id 存在
    /// `userId` 上 —— 会话形态里它就是空的，写一条永远命不中的回退只会让人以为
    /// 这里有兜底。`userid` 只用于展示与身份标识，转发链路的
    /// `build_chat_request` 一个字段都不读它（凭证本体在 `auth`），所以空着无害。
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
            userid: text(&["userId", "userid", "user_id"]),
            phone: text(&["phone"]),
            nickname: text(&["nickname"]),
            // `expiresAt` 的**类型随形状变**：记录形态是毫秒时间戳**字符串**
            // （`patch_fields` 写的是 `Value::String`），会话形态是**数字**
            // （`session_from_record` 把 `record.expires_at()` 的 f64 原样写出）。
            // 只认字符串会让会话形态的过期时间恒为空 ⇒ `is_expired` 恒假
            // （「无过期信息视为不过期」）—— 那是本家最要紧的一处读数，
            // 因为 Loomy 的 session 是登录时声明 14 天、过期只能重登。
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
        push("userId", &self.userid);
        push("phone", &self.phone);
        push("nickname", &self.nickname);
        push("expiresAt", &self.expires_at);
        out
    }

    /// 凭据是否可用（有 session 即视为可用）。
    pub fn valid(&self) -> bool {
        !self.access_token.trim().is_empty()
    }

    /// 凭据是否可静默续期。
    ///
    /// **恒 false** —— Loomy 没有任何 refresh 端点（登录请求里的
    /// `expire: 1209600` 只是向服务端声明有效期，凭据里没有 refresh_token）。
    /// 这是诚实标记：账号卡片据此显示「凭证过期，请重新登录」，
    /// 而不是假装能续期却永远失败。
    pub fn can_refresh(&self) -> bool {
        false
    }
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

/// 业务响应信封。
#[derive(Debug)]
pub struct Envelope {
    pub data: Value,
}

/// 业务信封解析失败。
#[derive(Debug)]
pub struct EnvelopeFailure {
    pub code: String,
    pub message: String,
}

/// 解析 Loomy 的业务信封。
///
/// ⚠️ Loomy 的业务失败**恒返回 HTTP 200**，成败只能读 body 的 `code`。
pub fn parse_envelope(payload: &Value) -> Result<Envelope, EnvelopeFailure> {
    let Some(record) = payload.as_object() else {
        return Err(EnvelopeFailure { code: String::new(), message: "响应不是 JSON 对象".to_string() });
    };
    let code = record
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let message = {
        let desc = record.get("desc").and_then(Value::as_str).unwrap_or("");
        if !desc.is_empty() {
            desc.to_string()
        } else {
            record.get("message").and_then(Value::as_str).unwrap_or("").to_string()
        }
    };
    if code != OK_CODE {
        return Err(EnvelopeFailure {
            code: code.clone(),
            message: if message.is_empty() {
                format!("业务错误 {}", if code.is_empty() { "(缺少 code)" } else { &code })
            } else {
                message
            },
        });
    }
    Ok(Envelope { data: record.get("data").cloned().unwrap_or(Value::Null) })
}

/// 从展示名抽出倍率。
///
/// 认**两种**形态（同时处理「远端原值」与「已规范化的兜底表名」）：
/// 1. 远端原值：倍率在末尾括号里（全角半角都认、括号前后空格可有可无）；
/// 2. 已规范化形态 `{name} · x{n}`。
///
/// ⚠️ 因此本函数**幂等**：`split_rate(display_name(x))` 与 `split_rate(x)`
/// 结果一致。只认**末尾**的倍率（中间的括号属于模型名本身）。
///
/// 返回 `(name, rate)`；`rate` 为空串表示该模型无倍率（如生图模型）。
pub fn split_rate(raw_name: &str) -> (String, String) {
    let original = raw_name.trim();
    if original.is_empty() {
        return (String::new(), String::new());
    }
    // 形态 1：末尾括号 `（x1.0）` / `(x1.0)`
    if let Some(open) = original.rfind(['(', '（']) {
        let tail = &original[open..];
        if let Some(inner) = tail
            .trim_start_matches(['(', '（'])
            .trim_end_matches([')', '）'])
            .strip_prefix(['x', 'X'])
        {
            let rate = inner.trim().to_lowercase();
            if !rate.is_empty() && rate.chars().all(|ch| ch.is_ascii_digit() || ch == '.') {
                let name = original[..open].trim().to_string();
                if !name.is_empty() {
                    return (name, format!("x{rate}"));
                }
            }
        }
    }
    // 形态 2：已规范化的 `{name} · x1.0`
    if let Some(index) = original.rfind('·') {
        let tail = original[index + '·'.len_utf8()..].trim();
        if let Some(inner) = tail.strip_prefix(['x', 'X']) {
            let rate = inner.trim().to_lowercase();
            if !rate.is_empty() && rate.chars().all(|ch| ch.is_ascii_digit() || ch == '.') {
                let name = original[..index].trim().to_string();
                if !name.is_empty() {
                    return (name, format!("x{rate}"));
                }
            }
        }
    }
    (original.to_string(), String::new())
}

/// 最终展示名：`MiniMax M3 · x4.0`。
///
/// ⚠️ 倍率必须拼进 `name`（**不是** `description`）：composer 的模型切换菜单
/// 只渲染 `name`。无倍率时不追加分隔符，避免出现「模型名 · 」这种孤立分隔符。
pub fn display_name(raw_name: &str) -> String {
    let (name, rate) = split_rate(raw_name);
    if rate.is_empty() {
        name
    } else {
        format!("{name} · {rate}")
    }
}

/// 解析 `expires_at`（毫秒时间戳字符串）；缺失或非法返回 None。
pub fn credential_expires_at_ms(credential: &Credential) -> Option<i64> {
    let raw = credential.expires_at.trim();
    if raw.is_empty() {
        return None;
    }
    let parsed = raw.parse::<i64>().ok()?;
    (parsed > 0).then_some(parsed)
}

/// 凭据是否已过期。
///
/// 无 `expires_at` 时视为**不过期**：宁可用一个可能过期的凭据去试
/// （服务端会回 `100002`，我们能识别），也不要凭猜测阻止用户使用。
pub fn is_expired(credential: &Credential, now_ms: i64) -> bool {
    match credential_expires_at_ms(credential) {
        Some(expires_at) => expires_at <= now_ms,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_requires_success_code() {
        // 成功
        let ok = parse_envelope(&json!({"code": "000000", "data": {"session": "s"}}));
        assert!(ok.is_ok());
        // 登录失效码：失败
        let failure = parse_envelope(&json!({"code": "100002", "desc": "缺少 token"}))
            .expect_err("失效码必须失败");
        assert_eq!(failure.code, "100002");
        assert_eq!(failure.message, "缺少 token");
        // 非对象：失败
        assert!(parse_envelope(&json!(null)).is_err());
        // 缺 code：失败
        assert!(parse_envelope(&json!({"data": {}})).is_err());
    }

    #[test]
    fn rate_split_is_idempotent_across_three_styles() {
        // 三种远端风格
        assert_eq!(split_rate("MiniMax M3 （x4.0）"), ("MiniMax M3".to_string(), "x4.0".to_string()));
        assert_eq!(split_rate("Qwen 3.8 Max (x12.0)"), ("Qwen 3.8 Max".to_string(), "x12.0".to_string()));
        assert_eq!(split_rate("GLM 5.3 Flash(x0.8)"), ("GLM 5.3 Flash".to_string(), "x0.8".to_string()));
        // 已规范化形态（幂等）
        let normalized = display_name("MiniMax M3 （x4.0）");
        assert_eq!(normalized, "MiniMax M3 · x4.0");
        assert_eq!(split_rate(&normalized), ("MiniMax M3".to_string(), "x4.0".to_string()));
        // 无倍率
        assert_eq!(split_rate("Spark X2.5"), ("Spark X2.5".to_string(), String::new()));
        assert_eq!(display_name("Spark X2.5"), "Spark X2.5");
    }

    /// 记录形态（凭证平铺）与**会话形态**（凭证在 `auth` 下）都要读出 session。
    ///
    /// 这条用例钉的是「转发稳定 401」那个缺陷：转发链路的
    /// `build_chat_request` 拿到的是 `session_from_record` 的返回值（`auth` 嵌套），
    /// 而只认平铺的取值会让 `valid()` 恒为假 —— 于是每一次转发都在
    /// 「Loomy 账号缺少 session，无法转发」上失败，账号页却一切正常。
    #[test]
    fn from_value_reads_both_record_and_session_shape() {
        // 记录形态：凭证平铺在顶层
        let record = Credential::from_value(&json!({
            "accessToken": "sess-flat",
            "userId": "123456789012345678",
            "phone": "13011111100",
            "expiresAt": "1800000000000",
        }));
        assert_eq!(record.access_token, "sess-flat");
        assert_eq!(record.userid, "123456789012345678");
        assert!(record.valid());

        // 会话形态：凭证在 `auth` 下，`expiresAt` 是**数字**
        let session = Credential::from_value(&json!({
            "endpoint": "https://loomy.xunfei.cn",
            "auth": {
                "accessToken": "sess-nested",
                "tokenType": "Bearer",
                "expiresAt": 1800000000000_i64,
            },
            "account": { "uid": "123456789012345678" },
        }));
        assert_eq!(session.access_token, "sess-nested");
        assert!(session.valid(), "会话形态必须被认出有 session（否则转发恒 401）");
        // 数字型 expiresAt 必须被读成毫秒时间戳（否则 is_expired 恒假）
        assert_eq!(session.expires_at, "1800000000000");

        // `auth` 是空对象：以它为准，不回头取顶层
        let empty_auth = Credential::from_value(&json!({
            "auth": {},
            "accessToken": "sess-top",
        }));
        assert!(!empty_auth.valid(), "auth 是对象就以它为准，顶层值不参与");
    }

    #[test]
    fn credential_parses_snake_case_and_camel_case() {
        let camel = Credential::from_value(&json!({
            "accessToken": "a", "userId": "u", "phone": "13011111100", "expiresAt": "1800000000000"
        }));
        assert_eq!(camel.access_token, "a");
        assert_eq!(camel.userid, "u");
        let snake = Credential::from_value(&json!({
            "access_token": "b", "userid": "v", "phone": "130", "expires_at": "1800000000000"
        }));
        assert_eq!(snake.access_token, "b");
        assert_eq!(snake.userid, "v");
    }

    #[test]
    fn expiry_and_refreshable() {
        let mut credential = Credential { access_token: "a".to_string(), ..Default::default() };
        assert!(!credential.can_refresh(), "Loomy 恒不可刷新（没有 refresh 端点）");
        assert!(!is_expired(&credential, 1_000_000), "无过期时间视为不过期");
        credential.expires_at = "500000".to_string();
        assert!(is_expired(&credential, 600_000));
        assert!(!is_expired(&credential, 400_000));
    }
}
