//! LobsterAI 积分余额（`GET /api/user/profile-summary`）。
//!
//! ── 为什么用 profile-summary 而不是 quota ────────────────────
//! `/api/user/quota` 只显示 `freeCreditsTotal=300`，**不含活动积分**
//! （参考实现实测某账号 profile-summary 有 5297.72，quota 只有 300）。
//!
//! ── 面值推断（用户期望显示 100/100，而不是 100/0）────────────
//! 服务端**没有**面值字段（实测响应只有 `creditsRemaining` + `expiresAt`），
//! 但同组包的面值恒定 —— 「每日登录奖励」每天发 100，没用过的包剩余量就等于
//! 面值。取「同组（label/type 相同）有效包的剩余量最大值」当面值：
//! 6 个没用过的包 max=100 → 全部显示 100/100；被用过的包（92.73）也按同组
//! 面值 100 显示 92.73/100。**只对有效包推断** —— 失效包可能当初面值不同，
//! 宁可显示 `?`。
//!
//! ── 三条读数纪律 ────────────────────────────────────────────
//! 1. 失败与「余额为 0」严格区分：`totalCreditsRemaining` 缺失/0 且拿不到明细
//!    时视为**查不到**（Err），而不是显示 0；
//! 2. 负数一律 clamp 到 0（超额扣费/计量回滚等异常下服务端可能下发负值）；
//! 3. 失效包的余额同样 clamp（负值计入「另有 N 已失效」会让它变成负数）。

use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::client;
use super::credentials::{self, Credential};
use super::product;

/// 余额查询的请求超时（毫秒）。
///
/// 必须显式设：`egress` 默认的 read_timeout 是 600 秒（留给 SSE 长连接），
/// 不设总超时会让一个挂住的余额查询把前端的批量查询拖到一直转圈。
const REQUEST_TIMEOUT_MS: u64 = 20_000;

/// 查询某账号的积分余额（归一化形状见 `ProviderAdapter::query_usage` 的文档）。
pub async fn query_usage(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    let record = store.lobsterai_account_record(account_id);
    if !account_id.is_empty() && record.is_none() {
        return Err(GatewayError::with_status(404, "账号不存在"));
    }
    let Some(record) = record else {
        return Err(GatewayError::with_status(404, "没有可用的 LobsterAI 账号"));
    };
    let credential = Credential::from_value(&record);
    if !credential.valid() {
        return Err(GatewayError::with_status(401, "该账号没有可用凭证，无法查询积分"));
    }
    let reply = client::get_api(
        product::PROFILE_SUMMARY_PATH,
        &client::auth_headers(&credential, "application/json"),
        None,
        Duration::from_millis(REQUEST_TIMEOUT_MS),
    )
    .await?;
    if reply.status == 401 || reply.status == 403 {
        return Err(GatewayError::with_status(401, "登录态已过期，无法查询积分"));
    }
    if !(200..300).contains(&reply.status) {
        return Err(GatewayError::with_status(502, format!("上游返回 HTTP {}", reply.status)));
    }
    let Some(body) = reply.json() else {
        return Err(GatewayError::with_status(502, "余额响应不是 JSON"));
    };
    let envelope = credentials::parse_envelope(&body)
        .map_err(|failure| GatewayError::with_status(502, format!("余额查询失败：{}", failure.message)))?;
    let data = &envelope.data;

    // 明细：creditItems[] → 归一 wallets
    let mut packages: Vec<Value> = Vec::new();
    if let Some(items) = data.get("creditItems").and_then(Value::as_array) {
        for item in items {
            let Some(_record) = item.as_object() else { continue };
            let remaining = credentials::read_number(item, "creditsRemaining").unwrap_or(0.0);
            let kind = credentials::read_string(item, "type");
            let label = credentials::read_string(item, "label");
            let expires_at = credentials::read_string(item, "expiresAt");
            // 展示名：label 是给人看的名字（实测「每日登录奖励」），
            // type 是机器分类码（"campaign"）—— label 缺失才回退 type。
            let name = if !label.is_empty() {
                label
            } else if !kind.is_empty() {
                kind.clone()
            } else {
                "积分包".to_string()
            };
            let exp_ms = parse_expiry(&expires_at);
            let active = match exp_ms {
                Some(value) => logging_now_ms() < value,
                None => true,
            };
            let mut wallet = serde_json::Map::new();
            wallet.insert("type".to_string(), Value::String(kind));
            wallet.insert("displayName".to_string(), Value::String(name));
            wallet.insert("balance".to_string(), Value::from(round_credits(remaining.max(0.0))));
            wallet.insert("expired".to_string(), Value::Bool(!active));
            if let Some(value) = exp_ms {
                wallet.insert("deductionEndTime".to_string(), Value::from(value));
            }
            packages.push(Value::Object(wallet));
        }
    }

    // 面值推断（同组有效包的最大剩余量；见模块头）
    let mut face_value: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for wallet in &packages {
        let active = !wallet.get("expired").and_then(Value::as_bool).unwrap_or(false);
        if !active {
            continue;
        }
        let name = wallet.get("displayName").and_then(Value::as_str).unwrap_or("");
        let remaining = wallet.get("balance").and_then(Value::as_f64).unwrap_or(0.0);
        let entry = face_value.entry(name.to_string()).or_insert(0.0);
        if remaining > *entry {
            *entry = remaining;
        }
    }
    for wallet in &mut packages {
        let Some(object) = wallet.as_object_mut() else { continue };
        let name = object.get("displayName").and_then(Value::as_str).unwrap_or("").to_string();
        let Some(face) = face_value.get(&name).copied() else { continue };
        if face <= 0.0 {
            continue;
        }
        let remaining = object.get("balance").and_then(Value::as_f64).unwrap_or(0.0);
        let active = !object.get("expired").and_then(Value::as_bool).unwrap_or(false);
        let face_text = compact(face);
        let remaining_text = compact(remaining);
        let view = if active {
            format!("剩 {remaining_text} / {face_text}")
        } else {
            format!("已失效（剩 {remaining_text} / {face_text}）")
        };
        object.insert("balanceView".to_string(), Value::String(view));
    }

    // 总量：totalCreditsRemaining（clamp 到 0）
    let total = credentials::read_number(data, "totalCreditsRemaining").unwrap_or(0.0);
    let total = round_credits(total.max(0.0));
    // total 为 0 且拿不到明细 → 视为「查不到」而非「余额为 0」
    if total == 0.0 && packages.is_empty() {
        return Err(GatewayError::with_status(502, "上游没有返回积分读数"));
    }

    Ok(json!({
        "available": total,
        "unit": "积分",
        "wallets": packages,
        // LobsterAI 没有订阅概念（没有套餐接口），如实给 null
        "subscription": Value::Null,
        "raw": { "profile": data },
    }))
}

/// 解析 `expiresAt`（实测 ISO 8601，如 `2026-10-23T01:21:23`；兼容空格分隔与
/// 毫秒时间戳字符串）→ 毫秒时间戳。
fn parse_expiry(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        if let Ok(number) = trimmed.parse::<i64>() {
            let parsed = if number > 1_000_000_000_000 { number } else { number * 1000 };
            return (parsed > 0).then_some(parsed);
        }
    }
    // ISO 8601：`2026-10-23T01:21:23` 或 `2026-10-23 01:21:23`
    let normalized = trimmed.replace(' ', "T");
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(&normalized) {
        return Some(parsed.timestamp_millis());
    }
    // 无时区的本地时间：按 UTC 解析（与 JS Date.parse 的宽松口径近似）
    if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M:%S") {
        return Some(parsed.and_utc().timestamp_millis());
    }
    None
}

/// 把额度规整为两位小数（服务端精确值可能带浮点尾数）。
fn round_credits(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// 数值 → 去尾零的展示串（整数不带小数点）。
fn compact(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    let text = format!("{value:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn logging_now_ms() -> i64 {
    crate::server::logging::now_ms()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_parsing_accepts_iso_and_millis() {
        assert_eq!(parse_expiry("2026-10-23T01:21:23Z"), Some(1_792_718_483_000));
        assert_eq!(parse_expiry("2026-10-23 01:21:23"), Some(1_792_718_483_000));
        assert_eq!(parse_expiry("1800000000000"), Some(1_800_000_000_000));
        assert_eq!(parse_expiry("1800000000"), Some(1_800_000_000_000));
        assert_eq!(parse_expiry(""), None);
        assert_eq!(parse_expiry("garbage"), None);
    }

    #[test]
    fn compact_trims_trailing_zeros() {
        assert_eq!(compact(100.0), "100");
        assert_eq!(compact(92.73), "92.73");
        assert_eq!(compact(0.5), "0.5");
    }

    #[test]
    fn round_credits_keeps_two_decimals() {
        assert_eq!(round_credits(55.67000031), 55.67);
        assert_eq!(round_credits(100.0), 100.0);
    }
}
