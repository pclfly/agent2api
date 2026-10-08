//! Accio 额度查询：归一成账号页的统一形状（`ProviderAdapter::query_usage` 契约）。
//!
//! ── 数据源：订阅接口优先，quota 接口回退 ────────────────────
//! `GET /api/entitlement/currentSubscription?accessToken=…` 给**真实数字额度**
//! （`entitlement.monthly / referral / daily` 三层，免费账号通常只有 daily
//! 层：如 `total=20, used=20, remaining=0`）。这是唯一准确来源（对齐 unigate
//! 的 `accioFetchSubscriptionQuota` 与 accio2api 的 `parseSubscriptionQuota`，
//! 两处都注明「不再用 freeMember=520 本地估算」）。
//!
//! 订阅接口拿不到额度时回退 `GET /api/entitlement/quota?accessToken=…`
//! 的 `usagePercent` 百分比口径（`100 - usagePercent` 为剩余百分比）——
//! 该接口没有绝对数，只有百分比，因此回退分支的 `available` 为 null、
//! `availableView` 给百分比文案。
//!
//! ── 为什么 available 不再恒为 null ─────────────────────────
//! 原实现只用 quota 的百分比，`available` 给 null，界面主文案拼成
//! 「可用 — %」——用户读到的就是「查不到余额」。订阅接口本身有绝对数
//! （remaining/total），把它藏起来才是不合理的：`available` 给剩余数、
//! `availableView` 给「剩余 X/Y」展示串（与 unigate 面板的「剩余/总额度」
//! 口径一致），`wallets` 同时给出 total / usedPercent / remainingPercent
//! 供进度条类消费。
//!
//! ── 失败形态 ────────────────────────────────────────────────
//! 上游的失败不一定走 HTTP 状态码：`NOT_LOGIN`（会话失效）是
//! `200 + {"success":false,"code":"401"}`，风控锁定（423）与限速（555）同样
//! 是 200 带码。**必须识别**——不识别会把失败当成功解析，`unwrap_data`
//! 取到 null 后按默认值算，界面显示一个假余额。识别口径见 `business_error`：
//!   - 401 / NOT_LOGIN → 401（契约要求：调用方走「刷新后重试一次」）；
//!   - 423 → 403「账号被上游风控锁定」；
//!   - 555 → 429「触发风控限速」（全局短退避已在 `auth::request` 置上）。
//!
//! ── 频率与风控 ──────────────────────────────────────────────
//! 单次查询最多 2 个请求（订阅成功则只 1 个）；并发与重试由 `auth::request`
//! 的全局并发闸（4）与 555 退避统一兜底 —— 21 个账号的批量查询不会再以
//! 全并发打向上游（实测 22 并发可失败 19 个，见 `auth` 模块头）。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::auth;
use super::credentials::Credentials;
use super::endpoints;

/// 订阅额度解析结果（合并 monthly / referral / daily 三层后的总量 / 已用 / 剩余）
struct SubscriptionQuota {
    total: i64,
    used: i64,
    remaining: i64,
}

/// 查一个账号的额度 / 订阅，归一成统一形状。
///
/// `region` 必给：两个地区是两家 provider，取记录时按它过滤。
pub async fn query(
    store: &AccountStore,
    account_id: &str,
    region: super::endpoints::Region,
) -> Result<Value, GatewayError> {
    let record = store
        .accio_account_record(account_id, region.provider_id())
        .ok_or_else(|| GatewayError::with_status(404, "Accio 账号不存在，请先添加账号"))?;
    let credentials = auth::snapshot(&record)?;
    let proxy = auth::account_proxy(&record)?;

    // ── 1. 订阅额度（真实数字来源）─────────────────────────────
    match auth::get_with_token(
        credentials.region,
        endpoints::SUBSCRIPTION_PATH,
        &credentials.access_token,
        &[],
        proxy.as_ref(),
    )
    .await
    {
        Ok(response) => {
            if response.status == 401 || response.status == 403 {
                return Err(unauthorized_error());
            }
            if response.ok {
                if let Some(payload) = response.payload.filter(|value| value.is_object()) {
                    // 业务码先于解析检查：NOT_LOGIN / 423 / 555 都是 200 带码
                    if let Some(error) = business_error(&payload) {
                        return Err(error);
                    }
                    let data = auth::unwrap_data(payload);
                    if let Some(quota) = parse_subscription_quota(&data) {
                        return Ok(subscription_usage(&credentials, &data, quota));
                    }
                    logging::verbose("[Accio]", "订阅接口没有可解析的额度，回退 quota 接口");
                }
            }
        }
        // 网络错误：不直接失败，回退 quota 再试一次（同一个网关，多半也会失败，
        // 但「回退一次」比「把网络抖动直接报给用户」更接近 unigate 的处置）
        Err(error) => logging::verbose(
            "[Accio]",
            &format!("订阅额度请求失败，回退额度接口：{}", error.message),
        ),
    }

    // ── 2. 回退：quota 的百分比口径 ────────────────────────────
    let quota_response = auth::get_with_token(
        credentials.region,
        endpoints::QUOTA_PATH,
        &credentials.access_token,
        &[],
        proxy.as_ref(),
    )
    .await?;
    if quota_response.status == 401 || quota_response.status == 403 {
        return Err(unauthorized_error());
    }
    let quota_payload = auth::payload(quota_response, "额度查询")?;
    if let Some(error) = business_error(&quota_payload) {
        return Err(error);
    }
    let quota = auth::unwrap_data(quota_payload);
    let usage_percent = quota
        .get("usagePercent")
        .or_else(|| quota.get("usage_percent"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .clamp(0.0, 100.0);
    let countdown = quota
        .get("refreshCountdownSeconds")
        .or_else(|| quota.get("refresh_countdown_seconds"))
        .and_then(Value::as_f64);
    let remain = (100.0 - usage_percent).max(0.0);

    let mut wallet = json!({
        "type": "entitlement",
        "displayName": "额度余量",
        "balance": remain,
        "unit": "%",
        "usedPercent": usage_percent,
    });
    if let Some(seconds) = countdown.filter(|value| *value > 0.0) {
        wallet["detail"] = Value::String(format!("{} 后重置", humanize_seconds(seconds)));
    }

    Ok(json!({
        // 百分比口径没有绝对数：按契约给 null（见模块头）
        "available": Value::Null,
        // 主文案给百分比（避免界面显示「可用 — %」的「查不到」观感）
        "availableView": format!("剩余 {remain}%"),
        "unit": "%",
        "wallets": [wallet],
        "subscription": Value::Null,
        "raw": {
            "quota": quota,
            "region": credentials.region.edition(),
            "account": credentials.user_id,
        },
    }))
}

/// 凭证失效的统一错误（契约：401 原样透出，调用方刷新后重试一次）
fn unauthorized_error() -> GatewayError {
    GatewayError::with_status(401, "Accio 凭证已失效，请重新登录")
}

/// 业务码分类（见模块头「失败形态」）。
///
/// 只看已知的失败码：未知业务码**不拦截** —— 让上层按结构解析，解析不出来
/// 会走回退或报错，不会把一个可能成功的响应误判成失败。
fn business_error(payload: &Value) -> Option<GatewayError> {
    let code = match payload.get("code") {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    };
    let message = payload.get("message").and_then(Value::as_str).unwrap_or("");
    let message_lower = message.to_lowercase();
    if code == "401" || message.trim().eq_ignore_ascii_case("NOT_LOGIN") {
        return Some(unauthorized_error());
    }
    if code == "423" {
        return Some(GatewayError::with_status(
            403,
            "Accio 账号被上游风控锁定（423），请稍后再试或更换账号",
        ));
    }
    if code == "555" || message_lower.contains("sentinel") || message_lower.contains("rate limit") {
        return Some(GatewayError::with_status(
            429,
            "Accio 触发上游风控限速（555），已暂停请求，请稍后重试",
        ));
    }
    None
}

/// 订阅响应 → 数字额度（对齐 unigate `parseAccioSubscriptionQuota` /
/// accio2api `parseSubscriptionQuota` 的分层合并口径）：
///   - monthly / referral / daily 三层**合并求和**（免费账号通常只有 daily 层）；
///   - 单层内 total / used / remaining 互相推算（缺一个补一个）；
///   - 分层全空时回退顶层同名字段；
///   - 仍无有效总量（<= 0）→ None（调用方回退 quota 的百分比口径）。
fn parse_subscription_quota(data: &Value) -> Option<SubscriptionQuota> {
    let mut total = 0i64;
    let mut used = 0i64;
    let mut remaining = 0i64;
    for key in ["monthly", "referral", "daily"] {
        if let Some(layer) = data.get("entitlement").and_then(|value| value.get(key)) {
            if let Some((layer_total, layer_used, layer_remaining)) = layer_quota(layer) {
                total += layer_total;
                used += layer_used;
                remaining += layer_remaining;
            }
        }
    }
    if total == 0 {
        let (top_total, top_used, top_remaining) = layer_quota(data)?;
        total = top_total;
        used = top_used;
        remaining = top_remaining;
    }
    if total <= 0 {
        return None;
    }
    Some(SubscriptionQuota {
        total,
        used: used.min(total).max(0),
        remaining: remaining.min(total).max(0),
    })
}

/// 单层额度（total / used / remaining 三件套）：全空 → None；缺项互推补齐。
fn layer_quota(layer: &Value) -> Option<(i64, i64, i64)> {
    let number = |key: &str| -> i64 {
        layer
            .get(key)
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|value| (value + 0.5) as i64)
            .unwrap_or(0)
    };
    let mut total = number("total");
    let mut used = number("used");
    let mut remaining = number("remaining");
    if total == 0 && used == 0 && remaining == 0 {
        return None;
    }
    if total == 0 {
        total = used + remaining;
    }
    if remaining == 0 && total >= used {
        remaining = total - used;
    }
    if used == 0 && total >= remaining {
        used = total - remaining;
    }
    if remaining > total {
        remaining = total;
    }
    Some((total, used, remaining))
}

/// 订阅口径 → 统一 usage 形状（数字 + 展示串 + 明细）。
fn subscription_usage(credentials: &Credentials, data: &Value, quota: SubscriptionQuota) -> Value {
    // total 由 `parse_subscription_quota` 保证 > 0
    let used_percent = (quota.used as f64 / quota.total as f64 * 100.0).clamp(0.0, 100.0);
    let remaining_percent = (quota.remaining as f64 / quota.total as f64 * 100.0).clamp(0.0, 100.0);
    let plan_name = data
        .get("planName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let status = data.get("type").and_then(Value::as_str).unwrap_or("");
    let expire_at = ["endDate", "end_date", "expireAt", "expiresAt"]
        .iter()
        .find_map(|key| data.get(*key))
        .and_then(|value| match value {
            Value::Number(number) => number.as_i64(),
            Value::String(text) => text.trim().parse::<i64>().ok(),
            _ => None,
        });

    json!({
        // 剩余数量（订阅接口是唯一准确来源）
        "available": quota.remaining,
        // 展示串与 unigate 面板的「剩余/总额度」同口径
        "availableView": format!("剩余 {}/{}", quota.remaining, quota.total),
        "unit": "次",
        "wallets": [{
            "type": "entitlement",
            "displayName": "剩余额度",
            "balance": quota.remaining,
            "total": quota.total,
            "usedPercent": used_percent,
            "remainingPercent": remaining_percent,
            "unit": "次",
        }],
        "subscription": {
            "planName": plan_name,
            "status": status,
            "expireAt": expire_at,
            "remainQuota": quota.remaining,
            "totalQuota": quota.total,
        },
        "raw": {
            "subscription": data,
            "region": credentials.region.edition(),
            "account": credentials.user_id,
        },
    })
}

/// 秒 → 「3 小时 20 分」这类人话（界面直接显示）
fn humanize_seconds(seconds: f64) -> String {
    let total = seconds.max(0.0) as i64;
    let days = total / 86_400;
    let hours = (total % 86_400) / 3_600;
    let minutes = (total % 3_600) / 60;
    if days > 0 {
        format!("{days} 天 {hours} 小时")
    } else if hours > 0 {
        format!("{hours} 小时 {minutes} 分")
    } else if minutes > 0 {
        format!("{minutes} 分")
    } else {
        format!("{total} 秒")
    }
}

#[cfg(test)]
mod tests {
    //! 解析与分类逻辑的单元测试。用例取自 2026-10-04 对 phoenix-gw 的实测响应
    //! （免费账号 daily 层 `total=20, used=20, remaining=0` 的真实形态）。
    use serde_json::json;

    use super::*;

    fn free_account_credentials() -> Credentials {
        Credentials {
            region: endpoints::Region::Global,
            access_token: "a93c1cd9172e75e0a458ad0e12b848d9".to_string(),
            refresh_token: "r".to_string(),
            expires_at: None,
            user_id: "1784536358".to_string(),
            email: "pclfly@qq.com".to_string(),
            name: "Pclfly User".to_string(),
            device_id: "desktop-test".to_string(),
        }
    }

    #[test]
    fn parse_subscription_merges_layers() {
        // 免费账号：只有 daily 层有额度（真实响应形态）
        let data = json!({
            "type": "FREE",
            "total": 20,
            "remaining": 0,
            "entitlement": {
                "monthly": {"total": 0, "used": 0, "remaining": 0},
                "referral": {"total": 0, "used": 0, "remaining": 0},
                "daily": {"total": 20, "used": 20, "remaining": 0}
            }
        });
        let quota = parse_subscription_quota(&data).expect("可解析");
        assert_eq!((quota.total, quota.used, quota.remaining), (20, 20, 0));
    }

    #[test]
    fn parse_subscription_layer_math() {
        // 缺项互推：total + used → remaining 补齐
        let data = json!({"entitlement": {"daily": {"total": 20, "used": 5}}});
        let quota = parse_subscription_quota(&data).expect("可解析");
        assert_eq!((quota.total, quota.used, quota.remaining), (20, 5, 15));
    }

    #[test]
    fn parse_subscription_falls_back_to_top_level() {
        let data = json!({"total": 20, "remaining": 12, "entitlement": {}});
        let quota = parse_subscription_quota(&data).expect("可解析");
        assert_eq!((quota.total, quota.used, quota.remaining), (20, 8, 12));
    }

    #[test]
    fn parse_subscription_rejects_empty() {
        assert!(parse_subscription_quota(&json!({})).is_none());
        assert!(parse_subscription_quota(&json!({"entitlement": {"daily": {}}})).is_none());
    }

    #[test]
    fn business_error_classifies_known_codes() {
        // NOT_LOGIN：HTTP 200 带业务码 401（实测形态：未维护的账号返回它）
        let err = business_error(&json!({"success": false, "code": "401", "message": "NOT_LOGIN"}))
            .expect("应识别");
        assert_eq!(err.status_code, 401);
        // 423：风控锁定
        let err = business_error(&json!({"success": false, "code": "423"})).expect("应识别");
        assert_eq!(err.status_code, 403);
        // 555：限速
        let err = business_error(&json!({"code": "555"})).expect("应识别");
        assert_eq!(err.status_code, 429);
        // 正常响应与未知码不拦截（未知码交给结构解析，不误判）
        assert!(business_error(&json!({"success": true, "code": "200"})).is_none());
        assert!(business_error(&json!({"success": false, "code": "999"})).is_none());
    }

    #[test]
    fn subscription_usage_shape() {
        let credentials = free_account_credentials();
        let data = json!({"type": "FREE", "endDate": "1792738217000", "planName": null});
        let quota = SubscriptionQuota { total: 20, used: 20, remaining: 0 };
        let usage = subscription_usage(&credentials, &data, quota);
        assert_eq!(usage["available"], json!(0));
        assert_eq!(usage["availableView"], json!("剩余 0/20"));
        assert_eq!(usage["unit"], json!("次"));
        assert_eq!(usage["wallets"][0]["balance"], json!(0));
        assert_eq!(usage["wallets"][0]["total"], json!(20));
        assert_eq!(usage["wallets"][0]["usedPercent"], json!(100.0));
        assert_eq!(usage["wallets"][0]["remainingPercent"], json!(0.0));
        assert_eq!(usage["subscription"]["remainQuota"], json!(0));
        assert_eq!(usage["subscription"]["totalQuota"], json!(20));
        assert_eq!(usage["subscription"]["expireAt"], json!(1792738217000_i64));
        assert_eq!(usage["subscription"]["status"], json!("FREE"));
    }
}
