//! Loomy 积分余额（`GET /points/records`，**只读**）。
//!
//! ── 为什么用 `points/records` 而不是 `first-login` ─────────────
//! 后者是**写**端点（触发每日额度初始化），在「打开面板」这种高频路径上调用
//! 会意外触发签到。读与写严格分离。
//!
//! ── 两个积分池分开显示（用户明确要求的展示方式）──────────────
//! 实测（2026-09-26）：
//! ```text
//! balance: 15000        ← 永久
//! dailyBalance: 4992    ← 每日池余额 = dailyQuota(5000) - dailyConsumed(8)
//! availableBalance: 19992  ← 两者之和
//! ```
//! 两个池各作一个 wallet，`available` 用 `availableBalance`。
//!
//! ── 失败与「余额为 0」严格区分 ──────────────────────────────
//! `balance` 是核心字段：没有它就说明响应形状不对，**不编造数字**。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::client;
use super::credentials::{self, Credential};
use super::product;

/// 查询某账号的积分余额（归一化形状见 `ProviderAdapter::query_usage` 的文档）。
pub async fn query_usage(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    let record = store.loomy_account_record(account_id);
    if !account_id.is_empty() && record.is_none() {
        return Err(GatewayError::with_status(404, "账号不存在"));
    }
    let Some(record) = record else {
        return Err(GatewayError::with_status(404, "没有可用的 Loomy 账号"));
    };
    let credential = Credential::from_value(&record);
    if !credential.valid() {
        return Err(GatewayError::with_status(401, "该账号没有可用凭证，无法查询积分"));
    }
    let path = format!("{}?pageNo=1&pageSize=1&recordType=all", product::POINTS_RECORDS_PATH);
    let reply = client::get_business(&path, &client::business_headers(&credential.access_token), None).await?;
    if reply.status == 401 || reply.status == 403 {
        return Err(GatewayError::with_status(401, "登录态已过期，无法查询积分"));
    }
    if !(200..300).contains(&reply.status) {
        return Err(GatewayError::with_status(502, format!("上游返回 HTTP {}", reply.status)));
    }
    let Some(body) = reply.json() else {
        return Err(GatewayError::with_status(502, "余额响应不是 JSON"));
    };
    let envelope = match credentials::parse_envelope(&body) {
        Ok(envelope) => envelope,
        Err(failure) => {
            if failure.code == credentials::AUTH_ERROR_CODE {
                return Err(GatewayError::with_status(401, "登录态已失效：请重新短信登录"));
            }
            return Err(GatewayError::with_status(502, format!("余额查询失败：{}", failure.message)));
        }
    };
    let data = &envelope.data;
    let permanent = credentials::read_number(data, "balance");
    let daily = credentials::read_number(data, "dailyBalance");
    let available = credentials::read_number(data, "availableBalance");
    // `balance` 是核心字段：没有它就说明响应形状不对，不编造数字。
    let Some(permanent) = permanent else {
        return Err(GatewayError::with_status(502, "上游没有返回积分读数"));
    };
    let daily = daily.unwrap_or(0.0);
    let total = available.unwrap_or(permanent + daily);
    let daily_quota = credentials::read_number(data, "dailyQuota");
    let daily_consumed = credentials::read_number(data, "dailyConsumed");

    // 两个池各作一个 wallet（见模块头）；每日池带展示串说明「消耗后不回补」
    let mut daily_view = format!("剩 {}", compact(daily));
    if let Some(quota) = daily_quota {
        daily_view = format!("剩 {} / {}", compact(daily), compact(quota));
    }
    if let Some(consumed) = daily_consumed {
        if consumed > 0.0 {
            daily_view = format!("{daily_view}（今日已用 {}）", compact(consumed));
        }
    }
    let wallets = vec![
        json!({
            "type": "permanent",
            "displayName": "永久积分",
            "balance": permanent,
            "balanceView": format!("剩 {}", compact(permanent)),
        }),
        json!({
            "type": "daily",
            "displayName": "每日赠送（消耗后不回补）",
            "balance": daily,
            "balanceView": daily_view,
        }),
    ];

    Ok(json!({
        "available": total,
        "unit": "积分",
        "wallets": wallets,
        // Loomy 没有订阅概念，如实给 null
        "subscription": Value::Null,
        "raw": { "points": data },
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
        assert_eq!(compact(15000.0), "15000");
        assert_eq!(compact(4992.0), "4992");
        assert_eq!(compact(8.5), "8.5");
    }
}
