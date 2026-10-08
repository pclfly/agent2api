//! MiniMax 用户资料（`GET /v1/api/user/info`）：手机号 / 昵称 / 用户 id。
//!
//! ── 为什么需要它 ────────────────────────────────────────────
//! MiniMax 的凭证里**没有能认出「这是哪个账号」的展示字段**：
//!   - `accessToken` 不是 JWT 时（`mmoat_`）什么都解不出来；
//!   - 是 JWT 时（`local-runtime` 导入）payload 只有
//!     `user.id` / `user.name` / `user.avatar` / `isAnonymous` ——
//!     `name` 是上游随机昵称（`MiniMax597757` / `探索者8626`），
//!     `id` 是一串 18 位数字，两者都看不出绑的是哪个手机号。
//!
//! `user/info` 返回 `data.userInfo.phone` —— 实测（2026-10-04）**两种凭证
//! 都能拿到**（JWT 走矩阵形态、`mmoat_` 走 Bearer 形态，与签到 / 积分同一条
//! 信封链路）。于是账号列表改用**脱敏手机号**当展示名，与 Loomy / AutoClaw
//! 的「账号名 = 130****1100」同一口径。
//!
//! ── 落库口径 ────────────────────────────────────────────────
//! 手机号明文写进账号记录的 `phone`（只在本地库里，与 Loomy 一致）；
//! 公开形态（`/api/accounts`）只给**脱敏形态**，明文不出网关。
//!
//! ── 拿不到时不阻断 ──────────────────────────────────────────
//! 手机号是**展示信息**，不是转发凭据。查询失败（网络、上游 5xx、账号没绑
//! 手机号）只记一行 verbose 并返回 `None`，账号照常落地 —— 展示名回落到
//! 上游昵称。反过来若把失败上抛，一次上游抖动就会让「登录成功」变成
//! 「账号没加上」，代价远大于少一个手机号。

use serde_json::Value;

use crate::server::errors::GatewayError;
use crate::server::logging;

use super::checkin;
use super::credentials::Credential;
use super::product;

/// `user/info` 里本网关用到的三个字段。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserInfo {
    /// 手机号（中国大陆 11 位；账号没绑手机号时为空串）。
    pub phone: String,
    /// 上游昵称（`MiniMax597757` 这类，仅作展示名兜底）。
    pub name: String,
    /// 上游用户 id（`userID`，短 id，**不是** JWT 里的 `user.id`）。
    pub user_id: String,
}

/// 查询用户资料（失败返回 `Err`，调用方自行决定是否降级）。
pub async fn query_user_info(credential: &Credential) -> Result<UserInfo, GatewayError> {
    let payload = checkin::request_envelope(product::USER_INFO_PATH, credential, "GET", None, &[])
        .await
        .map_err(|(code, message)| {
            // 401/403 = 凭证失效（与余额、签到同一分档），其余归 502
            if code == 401 || code == 403 {
                GatewayError::with_status(401, message)
            } else {
                GatewayError::with_status(502, format!("用户资料查询失败：{message}"))
            }
        })?;
    Ok(parse_user_info(&payload))
}

/// 从响应体解析资料（纯函数，可单测）。
///
/// 实测形态：`{"data":{"userInfo":{"name":"…","phone":"…","userID":"…"}}}`。
/// 同时容忍没有 `data` 信封的平铺形态（`{"userInfo":{…}}`）—— 上游两个域名
/// 的信封层级历史上出现过差异（签到端点就有「有信封 / 平铺」两种）。
pub fn parse_user_info(payload: &Value) -> UserInfo {
    let info = payload
        .get("data")
        .and_then(|data| data.get("userInfo"))
        .or_else(|| payload.get("userInfo"));
    let Some(info) = info else {
        return UserInfo::default();
    };
    let text = |key: &str| {
        info.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    UserInfo {
        phone: text("phone"),
        name: text("name"),
        user_id: text("userID"),
    }
}

/// 手机号脱敏（展示名用）：`18070551324` → `180****1324`。
///
/// 只认「11 位、1 开头、全数字」的大陆号；其它形态原样返回（不猜、不截断
/// 一个可能不是手机号的串）。与 `account_store::loomy_accounts` 的
/// `mask_phone` 同一规则。
pub fn mask_phone(phone: &str) -> String {
    let trimmed = phone.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() == 11 && bytes[0] == b'1' && bytes.iter().all(u8::is_ascii_digit) {
        return format!("{}****{}", &trimmed[..3], &trimmed[7..]);
    }
    trimmed.to_string()
}

/// 「尽力查一次手机号」：失败 / 没绑号一律 `None`，只留一行 verbose。
///
/// 给落账号与余额查询两条链路共用：它们都不该因为拿不到手机号而失败。
pub async fn try_query_phone(credential: &Credential) -> Option<String> {
    match query_user_info(credential).await {
        Ok(info) if !info.phone.is_empty() => Some(info.phone),
        Ok(_) => None,
        Err(error) => {
            logging::verbose(
                "[Accounts]",
                &format!("MiniMax 手机号查询跳过：{}", error.message),
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_enveloped_and_flat_shapes() {
        let enveloped = json!({
            "data": { "userInfo": {
                "name": "MiniMax597757",
                "phone": "18070551324",
                "userID": "1lv297ekG8X1",
                "avatar": "https://example.invalid/a.png"
            }}
        });
        let info = parse_user_info(&enveloped);
        assert_eq!(info.phone, "18070551324");
        assert_eq!(info.name, "MiniMax597757");
        assert_eq!(info.user_id, "1lv297ekG8X1");
        // 平铺形态
        let flat = json!({ "userInfo": { "phone": "13011111100" } });
        assert_eq!(parse_user_info(&flat).phone, "13011111100");
        // 没有 userInfo：全部空串（不 panic、不编造）
        assert_eq!(parse_user_info(&json!({"data": {}})), UserInfo::default());
    }

    #[test]
    fn mask_phone_only_touches_mainland_mobile() {
        assert_eq!(mask_phone("18070551324"), "180****1324");
        assert_eq!(mask_phone(" 13011111100 "), "130****1100");
        // 非手机号形态原样返回
        assert_eq!(mask_phone(""), "");
        assert_eq!(mask_phone("12345"), "12345");
        assert_eq!(mask_phone("180705513241"), "180705513241");
        assert_eq!(mask_phone("1807055132a"), "1807055132a");
    }
}
