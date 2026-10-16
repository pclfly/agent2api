//! Authoritative account balance, separate from per-request token-based estimates.
use super::{auth, credentials::text, refresh};
use crate::server::{core::account_store::AccountStore, errors::GatewayError};
use serde_json::{json, Value};
pub fn normalize(value: &Value) -> Result<Value, GatewayError> {
    if !auth::business_ok(value) {
        return Err(GatewayError::with_status(
            502,
            "QwenWork 余额查询返回业务错误",
        ));
    }
    let data = value
        .get("data")
        .ok_or_else(|| GatewayError::with_status(502, "QwenWork 余额响应缺少 data"))?;
    let quota = data
        .get("quota")
        .ok_or_else(|| GatewayError::with_status(502, "QwenWork 余额响应缺少 quota"))?;
    let available = quota
        .get("remaining")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
        .ok_or_else(|| GatewayError::with_status(502, "QwenWork 未提供可用积分读数"))?;
    Ok(json!({"available":available,"unit":"积分","wallets":[],
        "subscription":{"planName":data.pointer("/plan/name"),"userType":data.pointer("/plan/user_type")},
        "raw":{"quota":quota},"queriedAt":crate::server::logging::now_ms()}))
}
pub async fn query(store: &AccountStore, id: &str) -> Result<Value, GatewayError> {
    let record = store
        .qwenwork_account_record(id)
        .ok_or_else(|| GatewayError::with_status(404, "QwenWork 账号不存在"))?;
    let id = text(&record, &["id"]);
    let proxy = auth::proxy(&record)?;
    let c = refresh::ensure(store, &id, false).await?;
    let value = match auth::get(&c, auth::ACCOUNT, false, proxy.as_ref()).await {
        Err(e) if e.status_code == 401 => {
            let c = refresh::ensure(store, &id, true).await?;
            auth::get(&c, auth::ACCOUNT, false, proxy.as_ref()).await?
        }
        other => other?,
    };
    normalize(&value)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_is_not_zero() {
        assert!(normalize(&json!({"code":"ok","data":{"quota":{"remaining":null}}})).is_err());
        assert!(normalize(&json!({"code":"error","data":{"quota":{"remaining":0}}})).is_err());
        assert_eq!(
            normalize(&json!({"data":{"quota":{"remaining":0,"total":null}}})).unwrap()
                ["available"],
            0.0
        );
    }
}
