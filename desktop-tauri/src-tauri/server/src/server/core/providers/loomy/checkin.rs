//! Loomy 每日额度签到（`POST /points/first-login`）。
//!
//! ── 语义是「触发每日额度重置」，不是「+5000 积分」──────────────
//! `dailyBalance = dailyQuota - dailyConsumed`，消耗后不回补。
//! UI 文案必须准确（参考实现 `LOOMY_DAILY_QUOTA_DESCRIPTION`：
//! 「每日赠送额度（消耗后不回补）」）。
//!
//! ── 幂等判据是响应体的 `alreadyProcessed` ─────────────────────
//! 重复调用同样返回 HTTP 200，故已处理的情况映射成 `alreadyCompleted: true`
//! 而**不是** `claimed` —— 后者会让用户以为每天都真的加了额度。
//!
//! ── 两个积分池（用户补充确认的机制）─────────────────────────
//!   - **永久积分**：注册奖励 + 新手任务（`balance`）；
//!   - **每日赠送池**：每天发放，消耗后不回补（`dailyBalance`）。
//! `dailyQuota` **只在 `first-login` 的响应里**，`points/records` 不返回它 ——
//! 故未签到时该字段缺省，**不要硬编码 5000**（额度可能随活动变化）。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::client;
use super::credentials::{self, Credential};
use super::product;

/// 一键签到：触发每日赠送额度。
///
/// 返回 `{success, msg, ...}` —— `success` 的口径是「**本次真的初始化了额度**」。
/// 今天已初始化时 `success: false` 且 `alreadyCompleted: true`。
/// 本函数**不抛业务错**（失败也返回 `{success:false}`），保证批量领取不会因
/// 单个账号中断；网络与凭据失效归 `Err`（红色失败，用户该看到）。
pub async fn claim_daily_quota(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let record = store.loomy_account_record(account_id);
    if !account_id.is_empty() && record.is_none() {
        return Err(GatewayError::with_status(404, "账号不存在"));
    }
    let Some(record) = record else {
        return Err(GatewayError::with_status(404, "没有可用的 Loomy 账号"));
    };
    let credential = Credential::from_value(&record);
    if !credential.valid() {
        return Err(GatewayError::with_status(401, "该账号没有可用凭证，无法签到"));
    }
    let reply = client::post_business(
        product::FIRST_LOGIN_PATH,
        &json!({}),
        &client::business_headers(&credential.access_token),
        None,
    )
    .await?;
    if reply.status == 401 || reply.status == 403 {
        return Err(GatewayError::with_status(401, "登录态已过期，无法签到"));
    }
    if !(200..300).contains(&reply.status) {
        return Err(GatewayError::with_status(502, format!("上游返回 HTTP {}", reply.status)));
    }
    let Some(body) = reply.json() else {
        return Err(GatewayError::with_status(502, "签到响应不是 JSON"));
    };
    let envelope = match credentials::parse_envelope(&body) {
        Ok(envelope) => envelope,
        Err(failure) => {
            // 登录失效码（100002）：明确报「重新登录」
            if failure.code == credentials::AUTH_ERROR_CODE {
                return Err(GatewayError::with_status(401, "登录态已失效：请重新短信登录"));
            }
            // 其余业务失败：归中性提示（批量领取不中断）
            return Ok(json!({ "success": false, "msg": failure.message }));
        }
    };
    let data = &envelope.data;
    let daily_quota = credentials::read_number(data, "dailyQuota");
    let daily_balance = credentials::read_number(data, "dailyBalance");
    let daily_consumed = credentials::read_number(data, "dailyConsumed");
    let already = data.get("alreadyProcessed").and_then(Value::as_bool).unwrap_or(false);
    if already {
        let msg = match (daily_balance, daily_quota) {
            (Some(balance), Some(quota)) => format!(
                "今日额度已初始化（每日 {}/{}）",
                compact(balance),
                compact(quota)
            ),
            _ => "今日额度已初始化".to_string(),
        };
        return Ok(json!({ "success": false, "msg": msg, "alreadyCompleted": true }));
    }
    // 首次处理：本次新发放的额度 = dailyQuota - dailyConsumed（推算）；
    // 两者都缺时回 0（不编造）。
    let granted = match (daily_quota, daily_consumed) {
        (Some(quota), Some(consumed)) => (quota - consumed).max(0.0),
        (Some(quota), None) => quota,
        _ => 0.0,
    };
    Ok(json!({
        "success": true,
        "msg": format!("每日赠送额度已初始化（本次 {}）", compact(granted)),
        "credit": granted,
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
    fn compact_formats() {
        assert_eq!(compact(5000.0), "5000");
        assert_eq!(compact(4992.5), "4992.5");
    }
}
