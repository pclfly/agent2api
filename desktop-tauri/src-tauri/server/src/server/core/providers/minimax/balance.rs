//! MiniMax 积分余额（`GET /minimax-cloud/api/v1/credit/details`）。
//!
//! ── 一个已被生产数据推翻的字段误读（照参考实现的记录）────────
//! 初版把 `total_count` 当作**积分余额**。这是**错的** —— `total_count` 是
//! `details[]` 的**记录条数**。实测：领取 800 积分后
//! `{"details":[{"remaining_amount":"800.00",...}],"total_count":1}` ——
//! 真实余额是 `remaining_amount`（800），`total_count` 是 1。
//!
//! ⚠️ 为什么初版没被发现：账号余额为 0 时 `details` 整个缺失、`total_count`
//! 恰好也是 0 —— 「条数 0」与「余额 0」偶然重合。领取后才分叉。
//!
//! ── 第二个已被生产数据推翻的误读：**默认分页**（2026-10-04 实测）──
//! 不传 `page_size` 时上游**只回 20 条**：同一账号
//! `total_count=31` 而 `details.len()=20`，求和只得到 15600，
//! 真实余额 23399.99 —— **少算 7799.99**。这就是「余额显示不正确」的根因。
//! 修复：显式传 `page_size`（见 `product::CREDIT_PAGE_SIZE`，上限 100）。
//!
//! ⚠️ `page` / `page_num` 参数实测**无效**（`page=1/2/3` 返回同一页数据），
//! 所以拿不到「第 101 条之后」的包；`total_count` 超出返回条数时如实把
//! `truncated: true` 报给上层，**不假装完整**。
//!
//! ── 取值口径 ────────────────────────────────────────────────
//!   - **余额 = Σ `details[].remaining_amount`**，且只计**未过期**的包
//!     （`expire_at_ms > now`）：过期的包 remaining 仍非 0，但它已经不能用了，
//!     计进去只会让用户看到一个对不上实际可用的数；
//!   - `remaining_amount` 实测是**字符串**（`"800.00"`），宽容解析；
//!   - `details` 缺失 ⇒ **空数组 ⇒ 余额 0**（「真的为 0」），**不是**查询失败；
//!   - `base_resp.status_code` 非 0 ⇒ 失败（信封在 `checkin::request_envelope`）。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::checkin;
use super::product;
use super::profile;

/// 查询某账号的积分余额（归一化形状见 `ProviderAdapter::query_usage` 的文档）。
pub async fn query_usage(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
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
        return Err(GatewayError::with_status(401, "该账号没有可用凭证，无法查询积分"));
    }
    let page_size = product::CREDIT_PAGE_SIZE.to_string();
    let payload = checkin::request_envelope(
        product::CREDIT_DETAILS_PATH,
        &credential,
        "GET",
        None,
        &[("page_size", page_size.as_str())],
    )
    .await
    .map_err(|(code, message)| {
        if code == 401 || code == 403 {
            GatewayError::with_status(401, message)
        } else {
            GatewayError::with_status(502, format!("余额查询失败：{message}"))
        }
    })?;
    // 顺手补手机号（账号展示名用）：只在记录里还没有时查一次。
    // 放在余额这条链路上是因为它本来就是「用户主动/定时打上游」的动作 ——
    // 不给凭证维护那条批量链路再叠一次网络调用；失败不影响余额结果。
    backfill_phone(store, &record, &credential).await;
    // 余额响应是**平铺**的（`details` / `total_count` 与 `base_resp` 同级）
    let data = payload;
    let details = data.get("details").and_then(Value::as_array);
    let total_count = data
        .get("total_count")
        .and_then(Value::as_f64)
        .map(|value| value.max(0.0) as usize);
    let has_shape = details.is_some() || total_count.is_some();
    if !has_shape {
        // 形状不对：不编造 0
        return Err(GatewayError::with_status(502, "上游没有返回积分读数"));
    }
    let now_ms = crate::server::logging::now_ms();
    let mut total = 0.0_f64;
    let mut expired_credits = 0.0_f64;
    let mut packages: Vec<Value> = Vec::new();
    for entry in details.map(Vec::as_slice).unwrap_or_default() {
        let remaining = entry
            .get("remaining_amount")
            .and_then(loose_amount)
            .unwrap_or(0.0);
        // 过期判定：`expire_at_ms` 缺失时按**未过期**处理（上游没说过期，
        // 不能凭猜测把它排除掉 —— 那会让余额凭空变小）
        let expire_ms = entry.get("expire_at_ms").and_then(Value::as_i64);
        let expired = expire_ms.map(|value| value <= now_ms).unwrap_or(false);
        if expired {
            expired_credits += remaining;
        } else {
            total += remaining;
        }
        // 明细行：上游只给剩余/消耗/发放三个数与到期时间（`credit_type` 语义
        // 未实测，不凭猜测分类 —— 与参考实现同一处置）
        let mut wallet = serde_json::Map::new();
        wallet.insert("type".to_string(), Value::String("credit".to_string()));
        wallet.insert(
            "displayName".to_string(),
            Value::String(
                entry
                    .get("credit_type")
                    .and_then(Value::as_i64)
                    .map(|kind| format!("积分包 #{kind}"))
                    .unwrap_or_else(|| "积分包".to_string()),
            ),
        );
        wallet.insert("balance".to_string(), Value::from(round_credits(remaining)));
        wallet.insert(
            "balanceView".to_string(),
            Value::String(if expired {
                format!("已过期 {}", compact(remaining))
            } else {
                format!("剩 {}", compact(remaining))
            }),
        );
        if let Some(expire_ms) = expire_ms {
            wallet.insert("deductionEndTime".to_string(), Value::from(expire_ms));
        }
        if expired {
            wallet.insert("expired".to_string(), Value::Bool(true));
        }
        packages.push(Value::Object(wallet));
    }
    let returned = details.map(Vec::len).unwrap_or(0);
    // `page` 参数无效 ⇒ 一次最多 `CREDIT_PAGE_SIZE` 条；上游说还有更多时如实标注
    let truncated = total_count.map(|count| count > returned).unwrap_or(false);
    Ok(json!({
        "available": round_credits(total),
        "unit": "积分",
        "wallets": packages,
        // MiniMax 的订阅信息不在这个端点（没有可读的套餐接口），如实给 null
        "subscription": Value::Null,
        // 上游口径的辅助读数：积分包总数 / 是否被分页截断 / 已过期积分合计
        "totalCount": total_count,
        "truncated": truncated,
        "expiredCredits": round_credits(expired_credits),
        "raw": { "details": data },
    }))
}

/// 记录里没有手机号时查一次并回填（展示名用；失败静默）。
async fn backfill_phone(
    store: &AccountStore,
    record: &Value,
    credential: &super::credentials::Credential,
) {
    let existing = record
        .get("phone")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if !existing.is_empty() {
        return;
    }
    let id = record.get("id").and_then(Value::as_str).unwrap_or("");
    if id.is_empty() {
        return;
    }
    if let Some(phone) = profile::try_query_phone(credential).await {
        let _ = store.set_minimax_phone(id, &phone);
    }
}

/// 宽容读金额：数字与字符串形态都接受（`remaining_amount` 实测是字符串）。
///
/// ⚠️ `Number('')` 是 0，会把「空字符串」误读成「0 积分」—— 先挡掉空串。
fn loose_amount(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            trimmed.parse::<f64>().ok()
        }
        _ => None,
    }
}

/// 把额度规整为两位小数。
fn round_credits(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
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
    use serde_json::json;

    #[test]
    fn loose_amount_accepts_strings_and_numbers() {
        assert_eq!(loose_amount(&json!("800.00")), Some(800.0));
        assert_eq!(loose_amount(&json!(800)), Some(800.0));
        assert_eq!(loose_amount(&json!("")), None, "空串不能读成 0");
        assert_eq!(loose_amount(&json!("abc")), None);
        assert_eq!(loose_amount(&json!(null)), None);
    }

    #[test]
    fn compact_formats() {
        assert_eq!(compact(800.0), "800");
        assert_eq!(compact(799.5), "799.5");
    }

    /// 余额口径：只累加**未过期**的包（过期包单列，不进 available）。
    #[test]
    fn expired_packages_are_excluded_from_available() {
        let now = crate::server::logging::now_ms();
        let fresh = json!({"remaining_amount": "800.00", "expire_at_ms": now + 86_400_000_i64});
        let stale = json!({"remaining_amount": "200.00", "expire_at_ms": now - 1000_i64});
        let no_expiry = json!({"remaining_amount": "10.50"});
        let mut total = 0.0_f64;
        let mut expired = 0.0_f64;
        for entry in [&fresh, &stale, &no_expiry] {
            let remaining = entry.get("remaining_amount").and_then(loose_amount).unwrap_or(0.0);
            let expire_ms = entry.get("expire_at_ms").and_then(Value::as_i64);
            if expire_ms.map(|value| value <= now).unwrap_or(false) {
                expired += remaining;
            } else {
                total += remaining;
            }
        }
        assert_eq!(round_credits(total), 810.5);
        assert_eq!(round_credits(expired), 200.0);
    }

    /// 分页参数：必须显式带上 `page_size`（上游默认只回 20 条）。
    #[test]
    fn credit_page_size_within_upstream_limit() {
        // 上限 100：101 上游直接 500（见 product::CREDIT_PAGE_SIZE 的说明）
        assert_eq!(product::CREDIT_PAGE_SIZE, 100);
        assert!(product::CREDIT_PAGE_SIZE <= 100);
    }
}
