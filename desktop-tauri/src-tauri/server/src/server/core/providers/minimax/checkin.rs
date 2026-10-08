//! MiniMax 每日签到（`signin/status` + `signin/claim`）。
//!
//! ── 两个必须记住的点 ────────────────────────────────────────
//! 1. **必须带 `timezone_id`**（IANA 时区名）—— 签到按客户端上报的时区结算；
//! 2. **幂等判据是 `claim_result`**（`1` = 本次真领取，`2` = 已领过），
//!    **不是 HTTP 状态码**（重复领取同样 200）。
//!
//! ── 信封 ────────────────────────────────────────────────────
//! `signin/*` 是**信封**（业务字段在 `data` 下）；业务码在
//! `base_resp.status_code`，且 `invalid timezone_id` 也是 HTTP 200 ——
//! 只看 HTTP 状态码会把它当成成功。
//!
//! ── 时区怎么取（本实现的局限，如实写在注释里）───────────────
//! 参考实现用 `Intl.DateTimeFormat().resolvedOptions().timeZone` 取 IANA 名。
//! Rust 侧没有等价的标准 API，本实现按「`TZ` 环境变量 → UTC 偏移映射常见
//! 时区 → UTC」三级回退。**局限**：非 UTC+8 且未设 `TZ` 的机器会报到 UTC，
//! 签到日界与用户本地日不同 —— 上游按上报时区结算，日界偏移只会影响
//! 「今天算哪一天」的边界时刻，不会让签到失败。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::client;
use super::credentials::Credential;
use super::product;

/// 单日状态（asar `SigninDayStatus`）。
const STATUS_CLAIMABLE: i64 = 2;
const STATUS_CLAIMED: i64 = 3;

/// 领取结果（asar `SigninClaimResult`）。
const CLAIM_RESULT_CLAIMED: i64 = 1;
const CLAIM_RESULT_ALREADY: i64 = 2;

/// 取本机 IANA 时区名（见模块头「时区怎么取」的局限说明）。
pub fn resolve_timezone_id() -> String {
    // 1) `TZ` 环境变量（Linux / macOS 常规，Windows 上也可手工设）
    if let Ok(tz) = std::env::var("TZ") {
        let trimmed = tz.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    // 2) 按当前 UTC 偏移映射常见时区
    let offset_seconds = chrono::Local::now().offset().local_minus_utc();
    let zone = if offset_seconds == 8 * 3600 {
        "Asia/Shanghai"
    } else if offset_seconds == 9 * 3600 {
        "Asia/Tokyo"
    } else if offset_seconds == 5 * 3600 + 1800 {
        "Asia/Kolkata"
    } else if offset_seconds == 0 {
        "UTC"
    } else if offset_seconds == -5 * 3600 {
        "America/New_York"
    } else if offset_seconds == -8 * 3600 {
        "America/Los_Angeles"
    } else if offset_seconds == 3600 {
        "Europe/London"
    } else if offset_seconds == 2 * 3600 {
        "Europe/Paris"
    } else if offset_seconds == 10 * 3600 {
        "Australia/Sydney"
    } else {
        // 3) 兜底 UTC（不编造一个可能错误的 IANA 名）
        "UTC"
    };
    zone.to_string()
}

/// 发一次带信封的请求并拆包。返回 `Ok(data)` 或 `Err((code, message))`。
///
/// `extra_query` 是**业务参数**（如积分明细的 `page_size`），两种认证形态都要带上：
/// 矩阵形态由 `matrix_request` 追加在矩阵参数之后（签名不含 query，故不受影响），
/// Bearer 形态由 `client::append_query` 追加在路径上（签到路径已有 `?timezone_id=`，
/// 那里会自动用 `&`）。
pub(super) async fn request_envelope(
    path: &str,
    credential: &Credential,
    method: &str,
    body: Option<&Value>,
    extra_query: &[(&str, &str)],
) -> Result<Value, (i64, String)> {
    let timeout = std::time::Duration::from_millis(product::REQUEST_TIMEOUT_MS);
    // 两种 token 的认证形态不同（见 `client::matrix_request` 的说明）：
    // JWT（local-runtime 导入）走矩阵形态（token 头 + 签名 + 完整 query）；
    // mmoat_（OAuth 设备码）走 Bearer 形态（现有路径，带 `?timezone_id=`）。
    let reply = if is_jwt_credential(credential) {
        client::matrix_request(method, path, body, credential, None, timeout, extra_query).await
    } else {
        let mut headers = client::business_headers(credential);
        if body.is_some() {
            headers.push(("Content-Type".to_string(), "application/json".to_string()));
        }
        client::request(
            method,
            &format!("{}{}", product::API_HOST, client::append_query(path, extra_query)),
            body,
            &headers,
            None,
            timeout,
        )
        .await
    };
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => return Err((-1, error.message)),
    };
    if reply.status == 401 || reply.status == 403 {
        return Err((
            reply.status as i64,
            format!("凭据已失效（HTTP {}），请重新登录该账号", reply.status),
        ));
    }
    let Some(payload) = reply.json() else {
        return Err((
            reply.status as i64,
            format!("服务端返回了非 JSON 响应（HTTP {}）", reply.status),
        ));
    };
    let base_resp = payload.get("base_resp").cloned().unwrap_or(Value::Null);
    let status_code = base_resp.get("status_code").and_then(Value::as_i64);
    let status_msg = base_resp
        .get("status_msg")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if let Some(code) = status_code {
        if code != 0 {
            return Err((
                code,
                if status_msg.is_empty() { format!("业务错误 {code}") } else { status_msg },
            ));
        }
    }
    Ok(payload)
}

/// 凭证是不是 JWT（`header.payload.signature` 两段点）—— local-runtime 导入
/// 的会话 token 是 JWT（385 字符），OAuth 设备码的 `mmoat_` 不是。
/// `/minimax-cloud/*` 对 JWT 只认矩阵形态（见 `client::matrix_request`）。
fn is_jwt_credential(credential: &Credential) -> bool {
    credential.access_token.matches('.').count() == 2
}

/// 拆信封：有 `data` 对象就用它，否则用顶层（两个端点形状不同，见参考实现）。
fn unwrap_envelope_data(payload: &Value) -> Value {
    match payload.get("data") {
        Some(value) if value.is_object() => value.clone(),
        _ => payload.clone(),
    }
}

/// 该账号的每日签到。
///
/// 返回 `{success, msg, ...}` —— `success` 的口径是「**本次真的领到了积分**」。
/// 今天已领过时 `success: false` 且 `alreadyCompleted: true`。
pub async fn claim_daily_checkin(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let record = store.minimax_account_record(account_id);
    if !account_id.is_empty() && record.is_none() {
        return Err(GatewayError::with_status(404, "账号不存在"));
    }
    let Some(record) = record else {
        return Err(GatewayError::with_status(404, "没有可用的 MiniMax 账号"));
    };
    // desktop 账号优先重读客户端文件（见 `super::credential_of`）
    let credential = super::credential_of(&record);
    if !credential.valid() {
        return Err(GatewayError::with_status(401, "该账号没有可用凭证，无法签到"));
    }
    let timezone = resolve_timezone_id();
    let path = format!(
        "{}?timezone_id={}",
        product::SIGNIN_CLAIM_PATH,
        url::form_urlencoded::byte_serialize(timezone.as_bytes()).collect::<String>()
    );
    let payload = match request_envelope(&path, &credential, "POST", Some(&json!({})), &[]).await {
        Ok(payload) => payload,
        Err((code, message)) => {
            // 凭据失效归 401（红色失败）；其余业务失败归中性提示
            if code == 401 || code == 403 {
                return Err(GatewayError::with_status(401, message));
            }
            return Ok(json!({ "success": false, "msg": message }));
        }
    };
    let data = unwrap_envelope_data(&payload);
    let claim_result = data.get("claim_result").and_then(Value::as_i64);
    if claim_result == Some(CLAIM_RESULT_ALREADY) {
        return Ok(json!({ "success": false, "msg": "今日已签到", "alreadyCompleted": true }));
    }
    if claim_result != Some(CLAIM_RESULT_CLAIMED) {
        return Err(GatewayError::with_status(502, "签到响应缺少有效的 claim_result"));
    }
    // `points` 是总数（已含 bonus_points）
    let points = data.get("points").and_then(Value::as_f64).unwrap_or(0.0);
    Ok(json!({
        "success": true,
        "msg": format!("签到成功，获得 {} 积分", compact(points)),
        "credit": points,
    }))
}

/// 查询签到状态（只读；`None` = 查不到，与「今天没签」区分）。
pub async fn signin_status(store: &AccountStore, account_id: &str) -> Option<Value> {
    let record = store.minimax_account_record(account_id)?;
    let credential = super::credential_of(&record);
    if !credential.valid() {
        return None;
    }
    let timezone = resolve_timezone_id();
    let path = format!(
        "{}?timezone_id={}",
        product::SIGNIN_STATUS_PATH,
        url::form_urlencoded::byte_serialize(timezone.as_bytes()).collect::<String>()
    );
    let payload = request_envelope(&path, &credential, "GET", None, &[]).await.ok()?;
    let data = unwrap_envelope_data(&payload);
    let days = data.get("days").and_then(Value::as_array)?;
    // 今天是否已签：找 is_today 且 status == Claimed
    let claimed_today = days.iter().any(|day| {
        day.get("is_today").and_then(Value::as_bool) == Some(true)
            && day.get("status").and_then(Value::as_i64) == Some(STATUS_CLAIMED)
    });
    let claimable = days
        .iter()
        .any(|day| day.get("status").and_then(Value::as_i64) == Some(STATUS_CLAIMABLE));
    Some(json!({
        "claimedToday": claimed_today,
        "claimable": claimable,
        "days": days,
    }))
}

/// 数值 → 去尾零展示串。
fn compact(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    format!("{value:.2}").trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timezone_resolution_never_empty() {
        let tz = resolve_timezone_id();
        assert!(!tz.trim().is_empty());
    }

    #[test]
    fn unwrap_prefers_nested_data() {
        let nested = json!({"data": {"claim_result": 1}, "base_resp": {"status_code": 0}});
        assert_eq!(unwrap_envelope_data(&nested)["claim_result"], 1);
        // 平铺（余额端点形态）：原样返回顶层
        let flat = json!({"total_count": 0, "base_resp": {"status_code": 0}});
        assert_eq!(unwrap_envelope_data(&flat)["total_count"], 0);
    }
}
