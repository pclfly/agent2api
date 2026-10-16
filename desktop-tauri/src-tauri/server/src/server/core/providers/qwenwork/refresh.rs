//! Rolling refresh uses the shared single-flight primitive and compare-before-write persistence.
use super::super::refresh_flight::{fingerprint, Join, Table};
use super::{
    auth,
    credentials::{text, timestamp, Credentials},
};
use crate::server::core::{
    account_store::{AccountStore, CredentialWrite},
    egress,
};
use crate::server::errors::GatewayError;
use serde_json::{json, Value};
use std::sync::OnceLock;

fn table() -> &'static Table<Credentials> {
    static T: OnceLock<Table<Credentials>> = OnceLock::new();
    T.get_or_init(Table::new)
}
fn read(store: &AccountStore, id: &str) -> Result<(Value, Credentials), GatewayError> {
    let r = store
        .qwenwork_account_record(id)
        .ok_or_else(|| GatewayError::with_status(401, "没有可用的 QwenWork 账号"))?;
    let c = Credentials::from_value(&r)?;
    Ok((r, c))
}
pub async fn ensure(
    store: &AccountStore,
    id: &str,
    force: bool,
) -> Result<Credentials, GatewayError> {
    let (r, old) = read(store, id)?;
    let old_revision = r.get("credentialRevision").cloned();
    if !force && !old.expiring() {
        return Ok(old);
    }
    if old.refresh_token.is_empty() || r.get("refreshManaged") == Some(&Value::Bool(false)) {
        if !force && (old.expires_at == 0 || old.expires_at > crate::server::logging::now_ms()) {
            return Ok(old);
        }
        return Err(GatewayError::with_status(
            401,
            "QwenWork 凭据已失效；请重新登录并导入（客户端导入账号不代刷新）",
        ));
    }
    let id = text(&r, &["id"]);
    let key = format!("qwenwork:{id}:{}", fingerprint(&old.refresh_token));
    match table().join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(leader) => {
            let result = async {
                let (r, current) = read(store, &id)?;
                if current.token != old.token
                    || current.refresh_token != old.refresh_token
                    || r.get("credentialRevision").cloned() != old_revision
                    || r.get("refreshManaged") == Some(&Value::Bool(false))
                {
                    return Ok(current);
                }
                let proxy = auth::proxy(&r)?;
                let mut body = json!({"refresh_token":current.refresh_token,"target":"c"});
                if !current.client_id.is_empty() && !current.redirect_uri.is_empty() {
                    body["client_id"] = json!(current.client_id);
                    body["redirect_uri"] = json!(current.redirect_uri);
                }
                let response = egress::client_for(proxy.as_ref())
                    .post(format!("{}{}", auth::BASE, auth::REFRESH))
                    .header("User-Agent", auth::UA)
                    .timeout(std::time::Duration::from_secs(25))
                    .json(&body)
                    .send()
                    .await
                    .map_err(|_| GatewayError::with_status(502, "QwenWork 刷新请求失败或超时"))?;
                let response = auth::json_response(response).await?;
                let data = response.get("data").unwrap_or(&response);
                let mut next = current.clone();
                next.token = text(data, &["device_token", "token", "access_token"]);
                next.refresh_token = text(data, &["refresh_token"]);
                if next.token.is_empty() || next.refresh_token.is_empty() {
                    return Err(GatewayError::with_status(
                        502,
                        "QwenWork 刷新响应缺少新令牌",
                    ));
                }
                next.expires_at = timestamp(data.get("expires_at"));
                if next.expires_at <= 0 {
                    next.expires_at = data
                        .get("expires_in")
                        .and_then(Value::as_i64)
                        .filter(|n| *n > 0)
                        .map(|s| {
                            crate::server::logging::now_ms().saturating_add(s.saturating_mul(1000))
                        })
                        .unwrap_or(0);
                }
                if next.expires_at <= 0 {
                    next.expires_at = Credentials::from_value(&next.payload())?.expires_at;
                }
                match store
                    .update_qwenwork_credentials(&id, &r, &next)
                    .map_err(|_| {
                        GatewayError::with_status(500, "QwenWork 新凭据保存失败，请重新导入账号")
                    })? {
                    CredentialWrite::Written => Ok(next),
                    CredentialWrite::Stale => read(store, &id).map(|(_, c)| c),
                }
            }
            .await;
            leader.finish(result.clone());
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn snapshot_and_token_only_lifecycle_never_refreshes() {
        let (db, _guard) = crate::server::db::test_temp::TempDb::open("qwenwork-lifecycle");
        let store = AccountStore::with_db(Some(db));
        let account = store.add_qwenwork_account(&json!({"token":"snapshot-access", "refreshToken":"shared-refresh", "userId":"snapshot", "expiresAt":crate::server::logging::now_ms()+60_000}),None,"desktop-import").unwrap();
        let id = account["id"].as_str().unwrap();
        assert_eq!(
            ensure(&store, id, false).await.unwrap().token,
            "snapshot-access"
        );
        assert!(ensure(&store, id, true).await.is_err());
        store
            .add_qwenwork_account(
                &json!({"token":"expired-access","userId":"snapshot","expiresAt":1}),
                None,
                "manual",
            )
            .unwrap();
        assert!(ensure(&store, id, false).await.is_err());
        let account = store
            .add_qwenwork_account(
                &json!({"token":"token-only","userId":"manual"}),
                None,
                "manual",
            )
            .unwrap();
        let id = account["id"].as_str().unwrap();
        assert_eq!(ensure(&store, id, false).await.unwrap().token, "token-only");
        assert!(ensure(&store, id, true).await.is_err());
    }
}
