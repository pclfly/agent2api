//! QwenWork daily check-in, based on the client's static credits-growth-card contract.
//!
//! Evidence: international QwenWork 1.0.4-26090501 out/main/main.js,
//! parseDailyCheckIn*Response, getHeaders and getQwenWorkClientHeaders, checked
//! against the installed ASAR on 2026-10-15. CN 1.3.1 was the earlier evidence.
//! GET status, then POST claim with NO body and Bearer auth (not COSY/campaigns).
//! Only explicit CLAIMED / ALREADY_CLAIMED results complete the local ledger.
//!
//! ── 线上实测（2026-10-15，国际版账号）────────────────────────
//! 国际版 `gateway.qwenwork.ai` **不提供 `/sash` 家族接口**：
//!   GET /sash/api/v1/me/daily-check-in/status   → 404（无响应体）
//!   GET /sash/api/v1/me/invitationCode          → 404
//!   GET /sash/api/v1/me/pro-upgrade/eligibility → 404
//!   POST /sash/api/v1/me/daily-check-in/claim   → 404
//! 同一 host 上的 `/api/v1/adapter/user/account-context`、`/api/v2/quota/usage`、
//! `/api/v3/user/status` 都是 200 —— 所以不是令牌或路径拼接错，而是这一族端点
//! 在该站点根本没有部署（客户端自己取签到配置时也是 404，回落到 DISABLED）。
//! 客户端 bundle 里那个 `daily.gateway.qwenwork.ai`（`$` 常量）**不解析 DNS**，
//! 且 `getSashApiBaseUrl()` 没有分支用到它，因此不是「换 host 就能通」的问题。
//!
//! 于是 404 在这里按「该站点未开放签到」**终态**处理（409，与 DISABLED /
//! NOT_STARTED / ENDED 同一档），不再报成「未确认领取」——那句话会让用户以为
//! 领取请求发出去了、结果未知，实际是**根本没发出**（status GET 就 404 了）。
//! 客户端在别处签到的账号在这个站点同样签不了：签到账在服务端，与客户端无关。

use super::{auth, credentials::Credentials, refresh};
use crate::server::{
    core::{account_store::AccountStore, egress, proxies::ResolvedProxy},
    errors::GatewayError,
};
use serde_json::{json, Value};
use std::future::Future;

const STATUS_PATH: &str = "/sash/api/v1/me/daily-check-in/status";
const CLAIM_PATH: &str = "/sash/api/v1/me/daily-check-in/claim";

fn payload(value: &Value) -> Result<&Value, GatewayError> {
    let data = value.get("data").filter(|v| v.is_object()).unwrap_or(value);
    if !value.is_object() || !auth::business_ok(value) || !auth::business_ok(data) {
        return Err(GatewayError::with_status(
            502,
            "QwenWork 签到返回业务错误或无效数据",
        ));
    }
    Ok(data)
}

fn already() -> Value {
    // No rewardPoints: a replay must not count yesterday's/today's prior grant again.
    json!({"success":false,"alreadyCompleted":true,"msg":"今日已签到","unit":"积分"})
}

/// None means the server explicitly permits a claim; everything else is terminal.
fn status_result(value: &Value) -> Result<Option<Value>, GatewayError> {
    let data = payload(value)?;
    let status = data.get("status").and_then(Value::as_str).unwrap_or("");
    let message = match status {
        "CLAIMABLE" => return Ok(None),
        "CLAIMED_TODAY" => return Ok(Some(already())),
        "DISABLED" => "QwenWork 当前未开放签到活动",
        "NOT_STARTED" => "QwenWork 签到活动尚未开始",
        "ENDED" => "QwenWork 签到活动已结束",
        _ => {
            return Err(GatewayError::with_status(
                502,
                "QwenWork 签到状态未知，未发起领取",
            ))
        }
    };
    // An unavailable activity is an unsuccessful attempt with an explicit reason,
    // not a completed check-in or a retryable window. Return a row-level error so
    // the detail ledger and schedule/history summaries report the same outcome.
    Err(GatewayError::with_status(409, message))
}

fn claim_result(value: &Value) -> Result<Value, GatewayError> {
    let data = payload(value)?;
    match data.get("result").and_then(Value::as_str) {
        Some("ALREADY_CLAIMED") => Ok(already()),
        Some("CLAIMED") => {
            let reward = data
                .get("rewardCredits")
                .and_then(Value::as_f64)
                .filter(|n| n.is_finite() && *n >= 0.0);
            let message = reward
                .map(|n| format!("签到成功，获得 {n} 积分"))
                .unwrap_or_else(|| "签到成功，上游未提供奖励金额".into());
            let mut result = json!({"success":true,"msg":message,"unit":"积分"});
            if let Some(reward) = reward {
                result["rewardPoints"] = json!(reward);
            }
            // Keep the upstream integer as-is; no unverified seconds/milliseconds conversion.
            if let Some(expires) = data.get("expiresAt").and_then(Value::as_u64) {
                result["expiresAt"] = json!(expires);
            }
            Ok(result)
        }
        _ => Err(GatewayError::with_status(
            502,
            "QwenWork 未确认签到领取结果，请稍后查询状态",
        )),
    }
}

async fn run<F, R>(mut request: F) -> Result<Value, GatewayError>
where
    F: FnMut(bool) -> R,
    R: Future<Output = Result<Value, GatewayError>>,
{
    let status = request(false).await?;
    if let Some(result) = status_result(&status)? {
        return Ok(result);
    }
    claim_result(&request(true).await?)
}

fn build_request(
    client: &reqwest::Client,
    credentials: &Credentials,
    claim: bool,
) -> reqwest::RequestBuilder {
    let method = if claim {
        reqwest::Method::POST
    } else {
        reqwest::Method::GET
    };
    let path = if claim { CLAIM_PATH } else { STATUS_PATH };
    auth::with_client_headers(client
        .request(method, format!("{}{path}", auth::BASE))
        .bearer_auth(&credentials.token)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("User-Agent", "QoderWork")
        .timeout(std::time::Duration::from_secs(8)))
    // Intentionally no .json({}) / .body(): the client sends a bodyless POST.
    // No CN-only app-mode or unrelated campaign/risk headers.
}

async fn request(
    credentials: &Credentials,
    proxy: Option<&ResolvedProxy>,
    claim: bool,
) -> Result<Value, GatewayError> {
    let response = build_request(&egress::client_for(proxy), credentials, claim)
        .send()
        .await
        .map_err(|_| {
            GatewayError::with_status(
                502,
                "QwenWork 签到请求失败或超时；未确认领取，请稍后查询状态",
            )
        })?;
    json_response(response, claim).await
}

/// 404 的两种说法：status GET 时**根本没发出领取请求**，claim POST 时才是
/// 「发出去了但端点不存在」。分开报是为了不把「未开放」说成「结果未知」。
const NOT_OFFERED_STATUS: &str =
    "QwenWork 该站点未开放每日签到（上游不提供 /sash 签到接口，HTTP 404）";
const NOT_OFFERED_CLAIM: &str =
    "QwenWork 该站点未开放每日签到（领取接口不存在，HTTP 404）";

async fn json_response(
    response: reqwest::Response,
    claim: bool,
) -> Result<Value, GatewayError> {
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        // 404 = 该站点没有这一族端点（见模块头的线上实测）。这是**终态**：
        // 与 DISABLED / NOT_STARTED / ENDED 同一档报 409，不进「未确认领取」
        // 那类含糊说法（那会让用户以为领取请求已经发出、结果未知）。
        if status == 404 {
            return Err(GatewayError::with_status(
                409,
                if claim { NOT_OFFERED_CLAIM } else { NOT_OFFERED_STATUS },
            ));
        }
        // Preserve the transport status. The generic COSY classifier also maps
        // response text to 401, which is unsafe for an outcome-unknown claim POST.
        return Err(GatewayError::with_status(
            if status >= 400 {
                i32::from(status)
            } else {
                502
            },
            if claim {
                format!("QwenWork 签到请求失败（HTTP {status}，未确认领取）")
            } else {
                format!("QwenWork 签到状态查询失败（HTTP {status}，未发起领取）")
            },
        ));
    }
    response
        .json()
        .await
        .map_err(|_| GatewayError::with_status(502, "QwenWork 签到返回无效 JSON，未确认领取"))
}

async fn retry_after_auth<F, R>(
    first: Result<Value, GatewayError>,
    retry: F,
) -> Result<Value, GatewayError>
where
    F: FnOnce() -> R,
    R: Future<Output = Result<Value, GatewayError>>,
{
    match first {
        Err(error) if error.status_code == 401 => retry().await,
        other => other,
    }
}

async fn request_with_refresh(
    store: &AccountStore,
    id: &str,
    claim: bool,
) -> Result<Value, GatewayError> {
    let record = store
        .qwenwork_account_record(id)
        .ok_or_else(|| GatewayError::with_status(404, "QwenWork 账号不存在"))?;
    let proxy = auth::proxy(&record)?;
    let credentials = refresh::ensure(store, id, false).await?;
    let first = request(&credentials, proxy.as_ref(), claim).await;
    retry_after_auth(first, || async {
        // Only a raw HTTP 401 permits one retry. Snapshot credentials remain
        // read-only; refresh failure or a second rejection is returned unchanged.
        let credentials = refresh::ensure(store, id, true).await?;
        request(&credentials, proxy.as_ref(), claim).await
    })
    .await
}

pub async fn claim_daily_checkin(store: &AccountStore, id: &str) -> Result<Value, GatewayError> {
    run(|claim| request_with_refresh(store, id, claim)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::core::billing::checkin::checkin_completed_today;

    #[test]
    fn statuses_are_explicit_and_do_not_fabricate_completion() {
        assert!(status_result(&json!({"status":"CLAIMABLE"}))
            .unwrap()
            .is_none());
        let result = status_result(&json!({"data":{"status":"CLAIMED_TODAY","rewardCredits":100}}))
            .unwrap()
            .unwrap();
        assert!(checkin_completed_today(&result));
        assert!(result.get("rewardPoints").is_none());
        for state in ["DISABLED", "NOT_STARTED", "ENDED"] {
            let error = status_result(&json!({"status":state})).unwrap_err();
            assert_eq!(error.status_code, 409);
            assert!(error.message.contains("QwenWork"));
        }
        for value in [
            json!({}),
            json!(null),
            json!({"status":"NEW_STATUS"}),
            json!({"code":"error","data":{"status":"CLAIMABLE"}}),
            json!({"data":{"success":false,"status":"CLAIMABLE"}}),
        ] {
            assert!(status_result(&value).is_err());
        }
    }

    #[test]
    fn rewards_use_only_explicit_nonnegative_claim_amounts() {
        let result = claim_result(
            &json!({"code":"ok","data":{"result":"CLAIMED","rewardCredits":12.5,"expiresAt":123}}),
        )
        .unwrap();
        assert_eq!(result["rewardPoints"], 12.5);
        assert_eq!(result["expiresAt"], 123);
        assert!(checkin_completed_today(&result));
        let zero = claim_result(&json!({"result":"CLAIMED","rewardCredits":0})).unwrap();
        assert_eq!(zero["rewardPoints"], 0.0);
        for amount in [Value::Null, json!(-1), json!("100")] {
            let result = claim_result(&json!({"result":"CLAIMED","rewardCredits":amount})).unwrap();
            assert!(result.get("rewardPoints").is_none());
        }
        let replay =
            claim_result(&json!({"result":"ALREADY_CLAIMED","rewardCredits":100})).unwrap();
        assert_eq!(replay["success"], false);
        assert!(replay.get("rewardPoints").is_none());
        assert!(checkin_completed_today(&replay));
        for value in [
            json!({}),
            json!({"status":"CLAIMED"}),
            json!({"result":"PENDING"}),
            json!({"code":"error","result":"CLAIMED"}),
        ] {
            assert!(claim_result(&value).is_err());
        }
    }

    #[tokio::test]
    async fn only_claimable_sends_a_post() {
        for status in [
            "CLAIMABLE",
            "CLAIMED_TODAY",
            "DISABLED",
            "NOT_STARTED",
            "ENDED",
            "UNKNOWN",
        ] {
            let mut calls = Vec::new();
            let result = run(|claim| {
                calls.push(claim);
                std::future::ready(Ok(if claim {
                    json!({"result":"CLAIMED","rewardCredits":2})
                } else {
                    json!({"status":status})
                }))
            })
            .await;
            assert_eq!(
                calls,
                if status == "CLAIMABLE" {
                    vec![false, true]
                } else {
                    vec![false]
                }
            );
            assert_eq!(
                result.is_err(),
                !matches!(status, "CLAIMABLE" | "CLAIMED_TODAY")
            );
        }
    }

    #[tokio::test]
    async fn request_failures_never_turn_into_success_or_blind_reposts() {
        for fail_on_claim in [false, true] {
            let mut calls = Vec::new();
            let result = run(|claim| {
                calls.push(claim);
                std::future::ready(if claim || !fail_on_claim {
                    Err(GatewayError::with_status(409, "synthetic conflict"))
                } else {
                    Ok(json!({"status":"CLAIMABLE"}))
                })
            })
            .await;
            assert!(result.is_err());
            assert_eq!(
                calls,
                if fail_on_claim {
                    vec![false, true]
                } else {
                    vec![false]
                }
            );
        }
    }

    #[tokio::test]
    async fn only_raw_http_401_permits_one_retry() {
        fn response(status: u16, body: &str) -> reqwest::Response {
            axum::http::Response::builder()
                .status(status)
                .body(body.to_owned())
                .unwrap()
                .into()
        }
        for status in [401, 403, 409, 429, 500, 502, 503] {
            let first = json_response(response(status, r#"{"message":"invalid token"}"#), true).await;
            assert_eq!(first.as_ref().unwrap_err().status_code, i32::from(status));
            let mut retries = 0;
            let result = retry_after_auth(first, || {
                retries += 1;
                std::future::ready(Ok(json!({"result":"CLAIMED"})))
            })
            .await;
            assert_eq!(retries, usize::from(status == 401));
            assert_eq!(result.is_ok(), status == 401);
        }
        // A malformed success response, business failure, or successful request
        // must not refresh/replay the claim based on text in the response.
        for body in [
            "not json",
            r#"{"success":false,"message":"invalid token"}"#,
            r#"{"result":"CLAIMED"}"#,
        ] {
            let first = json_response(response(200, body), true).await;
            let mut retries = 0;
            let result = retry_after_auth(first, || {
                retries += 1;
                std::future::ready(Ok(json!({})))
            })
            .await;
            assert_eq!(retries, 0);
            if body.contains("\"success\":false") {
                assert!(claim_result(&result.unwrap()).is_err());
            }
        }
        // Refresh failure and a second HTTP 401 both stop after the one retry.
        for status in [401, 502] {
            let first = json_response(response(401, "not json"), true).await;
            let mut retries = 0;
            let error = retry_after_auth(first, || {
                retries += 1;
                std::future::ready(Err(GatewayError::with_status(
                    status,
                    "synthetic refresh/retry failure",
                )))
            })
            .await
            .unwrap_err();
            assert_eq!(error.status_code, status);
            assert_eq!(retries, 1);
        }
    }

    /// 404 = 该站点没有这一族端点（国际版实测如此）：终态、明确原因、
    /// **不重试**（重试同一 404 只会再撞一次），且不把它说成「未确认领取」。
    #[tokio::test]
    async fn a_missing_endpoint_is_reported_as_not_offered_not_as_unknown() {
        fn response(status: u16, body: &str) -> reqwest::Response {
            axum::http::Response::builder()
                .status(status)
                .body(body.to_owned())
                .unwrap()
                .into()
        }
        for claim in [false, true] {
            let error = json_response(response(404, ""), claim).await.unwrap_err();
            assert_eq!(error.status_code, 409);
            assert!(error.message.contains("未开放"), "{}", error.message);
            assert!(!error.message.contains("未确认领取"), "{}", error.message);
            // 404 不走 401 的刷新重试
            let mut retries = 0;
            let result = retry_after_auth(Err(error), || {
                retries += 1;
                std::future::ready(Ok(json!({"result":"CLAIMED"})))
            })
            .await;
            assert_eq!(retries, 0);
            assert!(result.is_err());
        }
        // 非 404 的失败仍保留传输状态码，且 status GET 的文案不谎称「未确认领取」
        let error = json_response(response(500, ""), false).await.unwrap_err();
        assert_eq!(error.status_code, 500);
        assert!(error.message.contains("未发起领取"), "{}", error.message);
        let error = json_response(response(500, ""), true).await.unwrap_err();
        assert!(error.message.contains("未确认领取"), "{}", error.message);
    }

    /// 国际版实测（2026-10-15/16）：`/sash` 家族在该站点不存在。这条钉住
    /// 404 的报法 —— 它是**终态**（409 + 「未开放」），不是「未确认领取」
    /// （那句话会让用户以为领取请求已经发出去了，实际一个都没发），
    /// 也不进 401 的刷新重试。
    #[tokio::test]
    async fn a_missing_sash_family_is_terminal_and_never_claims_an_unknown_outcome() {
        fn response(status: u16, body: &str) -> reqwest::Response {
            axum::http::Response::builder()
                .status(status)
                .body(body.to_owned())
                .unwrap()
                .into()
        }
        for claim in [false, true] {
            let error = json_response(response(404, ""), claim).await.unwrap_err();
            assert_eq!(error.status_code, 409);
            assert!(error.message.contains("未开放"), "{}", error.message);
            assert!(!error.message.contains("未确认领取"), "{}", error.message);
            let mut retries = 0;
            let result = retry_after_auth(Err(error), || {
                retries += 1;
                std::future::ready(Ok(json!({"result":"CLAIMED"})))
            })
            .await;
            assert_eq!(retries, 0, "404 不得触发刷新重试");
            assert!(result.is_err());
        }
        // 非 404 的失败仍保留传输状态码，且 status 查询不谎称「未确认领取」
        let error = json_response(response(500, ""), false).await.unwrap_err();
        assert_eq!(error.status_code, 500);
        assert!(error.message.contains("未发起领取"), "{}", error.message);
        let error = json_response(response(500, ""), true).await.unwrap_err();
        assert!(error.message.contains("未确认领取"), "{}", error.message);
    }

    #[test]
    fn registration_and_schedule_upgrade_include_qwenwork() {
        use crate::server::core::{auto_checkin, billing::checkin};
        assert!(auto_checkin::CHECKIN_PROVIDERS.contains(&"qwenwork"));
        assert!(checkin::supports_checkin(
            &json!({"provider":"qwenwork","edition":"intl","enabled":false})
        ));
        assert!(auto_checkin::providers_added_after(2).contains(&"qwenwork".to_string()));
        assert!(auto_checkin::providers_added_after(auto_checkin::PROVIDERS_VERSION).is_empty());
    }

    #[test]
    fn request_contract_is_bearer_and_bodyless() {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let credentials =
            Credentials::from_value(&json!({"token":"synthetic-token","userId":"synthetic-user"}))
                .unwrap();
        for claim in [false, true] {
            let req = build_request(&client, &credentials, claim).build().unwrap();
            assert_eq!(
                req.method(),
                if claim {
                    reqwest::Method::POST
                } else {
                    reqwest::Method::GET
                }
            );
            assert_eq!(
                req.url().path(),
                if claim { CLAIM_PATH } else { STATUS_PATH }
            );
            assert_eq!(req.url().host_str(), Some("gateway.qwenwork.ai"));
            assert_eq!(req.headers()["authorization"], "Bearer synthetic-token");
            assert_eq!(req.headers()["content-type"], "application/json");
            assert_eq!(req.headers()["accept"], "application/json");
            assert_eq!(req.headers()["user-agent"], "QoderWork");
            for (name, value) in [
                ("x-qwenwork-version", "1.0.4"),
                ("x-qwenwork-release-version", "1.0.4-26090501"),
                ("x-qwenwork-build", "26090501"),
                ("x-qwenwork-platform", "win32"),
                ("x-qwenwork-arch", "x64"),
                ("x-qwenwork-channel", "stable"),
            ] {
                assert_eq!(
                    req.headers().get(name).and_then(|v| v.to_str().ok()),
                    Some(value),
                    "{name}"
                );
            }
            assert_eq!(req.timeout(), Some(&std::time::Duration::from_secs(8)));
            assert!(req.body().is_none());
        }
    }
}
