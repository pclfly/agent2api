//! LobsterAI 每日签到（三步链路，逐条对齐参考实现 `lobsterai-credits.ts`）。
//!
//! ```text
//! 0) 版本号（必填参数）：GET {clientVersionApi} → data.value.version
//! 1) 查活动槽位  GET  /api/client-activities/slot?placement=…&clientVersion=…
//! 2) 查活动上下文 GET  /api/client-activities/{activityCode}/context?configRevision=…
//! 3) 签到        POST /api/client-activities/{activityCode}/actions/check_in
//! ```
//!
//! 认证是**纯 Bearer，无签名** —— 与 CodeBuddy 那套 `X-Domain` / `X-Product` /
//! `X-Enterprise-Id` 头族毫无关系。
//!
//! ── 幂等 ────────────────────────────────────────────────────
//! 与 CodeBuddy 靠服务端 `code:10001` 不同，LobsterAI 是**客户端幂等**：
//! 请求带 `idempotencyKey`（UUID4），且签到前先查 `context` 的
//! `state.claimedToday` 与 `actions` 是否含 `check_in`。
//! **两步预检查都要做** —— 只看 `claimedToday` 会漏掉「活动有但今天不该领」。
//!
//! ── claim 形状与其他家对齐 ─────────────────────────────────
//! `billing::checkin` 的汇总只认 `{success, msg}` 两个字段；今天已签到时带
//! `alreadyCompleted: true`（它决定要不要落 `checkinAt`，见那里的
//! `checkin_completed_today`）。三种「正常业务状态」与「真失败」严格分开：
//!   - 无活动 → `{success:false, msg:"当前没有可领取的签到活动"}`（中性）；
//!   - 已签到 → `{success:false, msg:"今天已签到", alreadyCompleted:true}`；
//!   - 异常（网络/结构）→ `Err`（红色失败，用户该看到）。

use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::client;
use super::credentials::{self, Credential};
use super::product;
use super::version;

/// 签到链路的请求超时（毫秒）。
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 活动槽位（`slot` 接口的 `data`）。
struct ActivitySlot {
    slot_state: String,
    activity_code: String,
    config_revision: i64,
}

/// 活动上下文（`context` 接口的 `data`）。
struct ActivityContext {
    claimed_today: bool,
    actions: Vec<String>,
}

/// 该账号的每日签到。
///
/// 返回 `{success, msg, ...}` —— `success` 的口径是「**本次真的领到了积分**」，
/// 而不是「HTTP 通了」。今天已领过时 `success: false` 且 `alreadyCompleted: true`。
pub async fn claim_daily_checkin(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let record = store.lobsterai_account_record(account_id);
    if !account_id.is_empty() && record.is_none() {
        return Err(GatewayError::with_status(404, "账号不存在"));
    }
    let Some(record) = record else {
        return Err(GatewayError::with_status(404, "没有可用的 LobsterAI 账号"));
    };
    let credential = Credential::from_value(&record);
    if !credential.valid() {
        return Err(GatewayError::with_status(401, "该账号没有可用凭证，无法签到"));
    }
    // 版本号：动态真值（12 小时缓存 + 兜底），是槽位查询的必填参数
    let (client_version, _source) = version::resolve().await;

    // ① 活动槽位
    let slot = fetch_slot(&credential, &client_version).await?;
    if slot.slot_state != "available" || slot.activity_code.is_empty() {
        return Ok(json!({
            "success": false,
            "msg": format!("当前没有可领取的签到活动（slotState={}）", slot.slot_state),
        }));
    }

    // ② 活动上下文（今天领了没、有哪些可用动作）
    let context = fetch_context(&credential, &slot).await?;
    if context.claimed_today {
        return Ok(json!({
            "success": false,
            "msg": "今天已签到",
            "alreadyCompleted": true,
        }));
    }
    if !context.actions.iter().any(|action| action == "check_in") {
        // 活动存在但当前不可签到（未开始 / 已结束 / 无资格）。
        return Ok(json!({ "success": false, "msg": "当前不可签到" }));
    }

    // ③ 签到（客户端幂等键）
    let idempotency_key = uuid_v4();
    let body = json!({
        "configRevision": slot.config_revision,
        "idempotencyKey": idempotency_key,
        "payload": {},
    });
    let path = format!(
        "{}/{}/actions/check_in",
        product::ACTIVITY_CONTEXT_PATH,
        urlencoding_segment(&slot.activity_code)
    );
    let reply = client::post_api(
        &path,
        &body,
        &client::auth_headers(&credential, "application/json"),
        None,
        Duration::from_millis(REQUEST_TIMEOUT_MS),
    )
    .await?;
    check_auth(reply.status, None)?;
    let Some(envelope_body) = reply.json() else {
        return Err(GatewayError::with_status(502, "签到响应不是 JSON"));
    };
    let envelope = match credentials::parse_envelope(&envelope_body) {
        Ok(envelope) => envelope,
        Err(failure) => {
            // 业务码非 0：今天已领过时上游可能以业务码表达（不同活动/版本
            // 行为不一致），统一归「失败」让用户看到原因。
            return Err(GatewayError::with_status(
                502,
                format!("签到失败：{}", failure.message),
            ));
        }
    };
    // 积分字段三级回退：creditsGranted → rewardCredits → credits
    let result = envelope.data.get("result").cloned().unwrap_or(Value::Null);
    let credit = ["creditsGranted", "rewardCredits", "credits"]
        .iter()
        .find_map(|key| credentials::read_number(&result, key))
        .unwrap_or(0.0);
    let message = credentials::read_string(&result, "message");
    let msg = if message.is_empty() {
        format!("签到成功，获得 {} 积分", format_credit(credit))
    } else {
        message
    };
    Ok(json!({ "success": true, "msg": msg, "credit": credit }))
}

/// ① 查询活动槽位。
async fn fetch_slot(
    credential: &Credential,
    client_version: &str,
) -> Result<ActivitySlot, GatewayError> {
    // query 构造放进块作用域：`url::form_urlencoded::Serializer` 不是 Send，
    // 不能跨 await 存活（块结束即 drop）。
    let path = {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("placement", product::SLOT_PLACEMENT);
        query.append_pair("clientVersion", client_version);
        query.append_pair("containerApiVersion", product::SLOT_CONTAINER_API_VERSION);
        query.append_pair("platform", product::SLOT_PLATFORM);
        format!("{}?{}", product::ACTIVITY_SLOT_PATH, query.finish())
    };
    let reply = client::get_api(
        &path,
        &client::auth_headers(credential, "application/json"),
        None,
        Duration::from_millis(REQUEST_TIMEOUT_MS),
    )
    .await?;
    check_auth(reply.status, None)?;
    let Some(body) = reply.json() else {
        return Err(GatewayError::with_status(502, "活动槽位响应不是 JSON"));
    };
    let envelope = credentials::parse_envelope(&body)
        .map_err(|failure| GatewayError::with_status(502, format!("活动槽位查询失败：{}", failure.message)))?;
    let data = &envelope.data;
    let activity = data.get("activity").cloned().unwrap_or(Value::Null);
    Ok(ActivitySlot {
        slot_state: credentials::read_string(data, "slotState"),
        activity_code: credentials::read_string(&activity, "activityCode"),
        config_revision: credentials::read_number(&activity, "configRevision")
            .map(|value| value as i64)
            .unwrap_or(0),
    })
}

/// ② 查询活动上下文。
async fn fetch_context(
    credential: &Credential,
    slot: &ActivitySlot,
) -> Result<ActivityContext, GatewayError> {
    let path = format!(
        "{}/{}/context?configRevision={}",
        product::ACTIVITY_CONTEXT_PATH,
        urlencoding_segment(&slot.activity_code),
        slot.config_revision
    );
    let reply = client::get_api(
        &path,
        &client::auth_headers(credential, "application/json"),
        None,
        Duration::from_millis(REQUEST_TIMEOUT_MS),
    )
    .await?;
    check_auth(reply.status, None)?;
    let Some(body) = reply.json() else {
        return Err(GatewayError::with_status(502, "活动上下文响应不是 JSON"));
    };
    let envelope = credentials::parse_envelope(&body)
        .map_err(|failure| GatewayError::with_status(502, format!("活动上下文查询失败：{}", failure.message)))?;
    let data = &envelope.data;
    let state = data.get("state").cloned().unwrap_or(Value::Null);
    let actions = data
        .get("actions")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(ActivityContext {
        claimed_today: state.get("claimedToday").and_then(Value::as_bool).unwrap_or(false),
        actions,
    })
}

/// HTTP 状态判定：401/403 明确报「登录态失效」（交给刷新链路/用户重登）。
fn check_auth(status: u16, _code: Option<i64>) -> Result<(), GatewayError> {
    if status == 401 || status == 403 {
        return Err(GatewayError::with_status(401, "登录态已过期，无法签到"));
    }
    if !(200..300).contains(&status) {
        return Err(GatewayError::with_status(502, format!("上游返回 HTTP {status}")));
    }
    Ok(())
}

/// 路径段百分号编码（`activityCode` 是上游给的字符串，可能含特殊字符）。
fn urlencoding_segment(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// 生成一个 UUID v4（`idempotencyKey`；登录的 `state` 与 `uuid` 也用它）。
///
/// 用 `getrandom`（仓库既有依赖）而不是引入 uuid crate：
/// 只要 16 字节随机数 + 版本/变体位，代码量比多一个依赖小。
pub(crate) fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        // 随机源失败时退化为全零（服务端只把它当去重键，不是安全凭据；
        // 退化值的最坏后果是同一毫秒内两次调用可能撞键，概率可忽略）
        bytes = [0u8; 16];
    }
    // 版本 4（高 4 位 = 0100）与变体位（高 2 位 = 10）
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// 积分数字的展示形态（整数不带小数点，小数保留两位）。
fn format_credit(value: f64) -> String {
    if (value - value.round()).abs() < f64::EPSILON {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_v4_shape_is_stable() {
        let uuid = uuid_v4();
        assert_eq!(uuid.len(), 36);
        assert_eq!(uuid.chars().filter(|ch| *ch == '-').count(), 4);
        // 版本位与变体位
        assert_eq!(uuid.as_bytes()[14], b'4');
        let variant = uuid.as_bytes()[19];
        assert!(variant == b'8' || variant == b'9' || variant == b'a' || variant == b'b');
        // 两次调用不同
        assert_ne!(uuid, uuid_v4());
    }

    #[test]
    fn credit_formatting() {
        assert_eq!(format_credit(100.0), "100");
        assert_eq!(format_credit(88.5), "88.50");
    }
}
