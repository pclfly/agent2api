//! QwenWork account storage. Only canonical credential fields enter storage; public views use an allowlist.
use super::{
    priority::next_free_priority,
    sql,
    state::{mark_name_custom, StoredAccount},
};
use super::{AccountStore, AccountStoreError, CredentialWrite};
use crate::server::core::providers::qwenwork::credentials::Credentials;
use crate::server::logging;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
const PROVIDER: &str = "qwenwork";

impl AccountStore {
    pub fn qwenwork_account_record(&self, id: &str) -> Option<Value> {
        let guard = self.guard();
        if !id.is_empty() {
            return self
                .record_by_id(&guard, id)
                .filter(|r| r.provider() == PROVIDER)
                .map(|r| r.to_value());
        }
        let mut all = self.records_for_provider(&guard, PROVIDER);
        all.retain(StoredAccount::enabled);
        all.sort_by_key(StoredAccount::order_key);
        all.first().map(StoredAccount::to_value)
    }
    pub fn add_qwenwork_account(
        &self,
        payload: &Value,
        name: Option<&str>,
        source: &str,
    ) -> Result<Value, AccountStoreError> {
        let c = Credentials::from_value(payload)
            .map_err(|e| AccountStoreError::new(e.message, e.status_code))?;
        let hash = format!(
            "{:x}",
            Sha256::digest(format!("qwenwork:account:v1\0{}", c.user_id))
        );
        let id = format!("qwenwork-{}", hash.get(..16).unwrap_or(&hash));
        let guard = self.guard();
        let existing = self.record_by_id(&guard, &id).or_else(|| {
            self.records_for_provider(&guard, PROVIDER)
                .into_iter()
                .find(|r| r.get("userId").and_then(Value::as_str) == Some(c.user_id.as_str()))
        });
        let id = existing.as_ref().map(|r| r.id().to_string()).unwrap_or(id);
        if existing.as_ref().is_some_and(|r| r.provider() != PROVIDER) {
            return Err(AccountStoreError::new(
                "QwenWork 账号 ID 与其他提供商冲突",
                400,
            ));
        }
        let priority = if let Some(r) = &existing {
            r.priority()
        } else {
            next_free_priority(&self.with_conn(&guard, sql::priorities_all)?)
        };
        let mut fields = existing
            .as_ref()
            .map(|r| r.fields().clone())
            .unwrap_or_default();
        let fresh = c.payload();
        if let Some(object) = fresh.as_object() {
            for (k, v) in object {
                // Partial manual re-import must not erase an existing rotating refresh token.
                if c.refresh_token.is_empty()
                    && existing.is_some()
                    && matches!(
                        k.as_str(),
                        "refreshToken" | "oauthClientId" | "oauthRedirectUri"
                    )
                {
                    continue;
                }
                fields.insert(k.clone(), v.clone());
            }
        }
        let name = name.map(str::trim).filter(|s| !s.is_empty());
        let display = name
            .map(str::to_string)
            .or_else(|| existing.as_ref().map(StoredAccount::name))
            .unwrap_or_else(|| {
                if !c.name.is_empty() {
                    c.name.clone()
                } else if !c.email.is_empty() {
                    c.email.clone()
                } else {
                    format!("QwenWork {}", c.user_id)
                }
            });
        fields.insert("id".into(), json!(id));
        fields.insert("provider".into(), json!(PROVIDER));
        fields.insert(
            "name".into(),
            json!(display.chars().take(100).collect::<String>()),
        );
        mark_name_custom(&mut fields, name.is_some(), existing.as_ref());
        fields.insert("source".into(), json!(source));
        fields.insert("desktop".into(), json!(false));
        fields.insert("priority".into(), json!(priority));
        fields.insert(
            "enabled".into(),
            json!(existing
                .as_ref()
                .map(StoredAccount::enabled)
                .unwrap_or(true)),
        );
        fields.insert(
            "addedAt".into(),
            json!(existing
                .as_ref()
                .map(StoredAccount::added_at)
                .unwrap_or_else(logging::now_ms)),
        );
        fields.insert("updatedAt".into(), json!(logging::now_ms()));
        // Desktop token is shared with the official client; don't race its rolling refresh.
        let managed = if source == "desktop-import" {
            false
        } else if c.refresh_token.is_empty() {
            existing
                .as_ref()
                .and_then(|r| r.get("refreshManaged"))
                .and_then(Value::as_bool)
                .unwrap_or(true)
        } else {
            true
        };
        fields.insert("refreshManaged".into(), json!(managed));
        fields.insert(
            "credentialRevision".into(),
            json!(crate::server::core::providers::qoder::cosy::random_uuid()
                .map_err(|e| AccountStoreError::new(e.message, e.status_code))?),
        );
        let record = StoredAccount::from_map(fields);
        self.with_conn(&guard, |conn| sql::put(conn, &record))?;
        Ok(self.public_account(&record))
    }
    pub fn update_qwenwork_credentials(
        &self,
        id: &str,
        expected: &Value,
        c: &Credentials,
    ) -> Result<CredentialWrite, String> {
        let guard = self.guard();
        let Some(mut r) = self
            .record_by_id(&guard, id)
            .filter(|r| r.provider() == PROVIDER)
        else {
            return Ok(CredentialWrite::Stale);
        };
        for key in [
            "accessToken",
            "refreshToken",
            "refreshManaged",
            "oauthClientId",
            "oauthRedirectUri",
            "credentialRevision",
        ] {
            if r.get(key) != expected.get(key) {
                return Ok(CredentialWrite::Stale);
            }
        }
        r.set("accessToken", json!(c.token));
        r.set("refreshToken", json!(c.refresh_token));
        r.set("expiresAt", json!(c.expires_at));
        r.set(
            "credentialRevision",
            json!(
                crate::server::core::providers::qoder::cosy::random_uuid().map_err(|e| e.message)?
            ),
        );
        r.set_updated_at(logging::now_ms());
        self.with_conn(&guard, |conn| sql::update_in_place(conn, &r))
            .map_err(|e| e.message)?;
        Ok(CredentialWrite::Written)
    }
    pub fn to_qwenwork_public_account(&self, r: &StoredAccount) -> Value {
        let mut out = Map::new();
        for k in [
            "id",
            "provider",
            "name",
            "uid",
            "userId",
            "email",
            "nickname",
            "expiresAt",
            "source",
            "priority",
            "enabled",
            "addedAt",
            "updatedAt",
            "rateLimits",
        ] {
            if let Some(v) = r.get(k) {
                out.insert(k.into(), v.clone());
            }
        }
        let token = r.access_token();
        out.insert(
            "tokenTail".into(),
            json!(token
                .chars()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<String>()),
        );
        out.insert("available".into(), json!(!token.is_empty()));
        out.insert(
            "hasRefreshToken".into(),
            json!(
                !r.refresh_token().is_empty()
                    && r.get("refreshManaged") != Some(&Value::Bool(false))
            ),
        );
        out.insert(
            "proxy".into(),
            crate::server::core::proxies::describe_account_proxy(Some(&r.proxy())),
        );
        Value::Object(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::core::providers::{adapter::ProviderAdapter, qwenwork::QWENWORK_ADAPTER};
    #[test]
    fn qwenwork_backup_round_trip_preserves_refresh_ownership() {
        let (db, _guard) = crate::server::db::test_temp::TempDb::open("qwenwork-backup");
        let store = AccountStore::with_db(Some(db));
        let public=store.add_qwenwork_account(&json!({"token":"synthetic-access","refreshToken":"synthetic-refresh","userId":"backup-user"}),None,"desktop-import").unwrap();
        let id = public["id"].as_str().unwrap();
        let before = store.qwenwork_account_record(id).unwrap();
        let result = crate::server::core::account_transfer::import_accounts(
            &store,
            &json!({"accounts":[before.clone()]}),
        )
        .unwrap();
        assert_eq!(result["updated"], 1);
        assert_eq!(result["failed"], 0);
        let after = store.qwenwork_account_record(id).unwrap();
        assert_eq!(after["refreshManaged"], false);
        assert_eq!(after["accessToken"], before["accessToken"]);
        assert_ne!(after["credentialRevision"], before["credentialRevision"]);
    }
    #[test]
    fn qwenwork_public_session_and_stale_refresh() {
        let (db, _guard) = crate::server::db::test_temp::TempDb::open("qwenwork-accounts");
        let store = AccountStore::with_db(Some(db));
        let payload = json!({"token":"synthetic-access-token","refreshToken":"synthetic-refresh-token","user":{"id":"synthetic-user","name":"Demo","email":"demo@example.invalid"}});
        let public = store
            .add_qwenwork_account(&payload, Some("Custom"), "manual")
            .unwrap();
        let id = public["id"].as_str().unwrap();
        let rendered = public.to_string();
        assert!(!rendered.contains("synthetic-access-token"));
        assert!(!rendered.contains("synthetic-refresh-token"));
        assert_eq!(public["provider"], "qwenwork");
        assert_eq!(public["hasRefreshToken"], true);
        assert!(crate::server::core::billing::checkin::supports_checkin(
            &public
        ));
        let session = store.get_session_by_id(id).unwrap().session;
        assert_eq!(session["auth"]["accessToken"], "synthetic-access-token");
        assert_eq!(session["account"]["uid"], "synthetic-user");
        let plan=QWENWORK_ADAPTER.build_chat_request(&session,&json!({"model":"qwork-auto","messages":[{"role":"user","content":"Synthetic test"}]}),&axum::http::HeaderMap::new()).unwrap();
        let header = |name: &str| {
            plan.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .unwrap()
                .1
                .clone()
        };
        assert_eq!(header("Content-Type"), "application/json");
        assert_eq!(header("Accept"), "text/event-stream");
        let authorization = header("Authorization");
        let pieces: Vec<_> = authorization
            .strip_prefix("Bearer COSY.")
            .unwrap()
            .split('.')
            .collect();
        assert_eq!(
            pieces[1],
            crate::server::core::providers::qwenwork::auth::signature(
                pieces[0],
                &header("Cosy-Key"),
                &header("Cosy-Date"),
                &serde_json::to_string(&plan.body).unwrap(),
                crate::server::core::providers::qwenwork::auth::CHAT
            )
        );
        let mut next = Credentials::from_value(&payload).unwrap();
        next.token = "new-access".into();
        next.refresh_token = "new-refresh".into();
        let snapshot = store.qwenwork_account_record(id).unwrap();
        assert!(matches!(
            store
                .update_qwenwork_credentials(id, &json!({}), &next)
                .unwrap(),
            CredentialWrite::Stale
        ));
        assert!(matches!(
            store
                .update_qwenwork_credentials(id, &snapshot, &next)
                .unwrap(),
            CredentialWrite::Written
        ));
        assert!(matches!(
            store
                .update_qwenwork_credentials(id, &snapshot, &next)
                .unwrap(),
            CredentialWrite::Stale
        ));
        let imported = store
            .add_qwenwork_account(&next.payload(), None, "desktop-import")
            .unwrap();
        assert_eq!(imported["id"], id);
        assert_eq!(imported["name"], "Custom");
        assert_eq!(imported["hasRefreshToken"], false);
        let prior = store.qwenwork_account_record(id).unwrap();
        let partial = store
            .add_qwenwork_account(
                &json!({"token":"partial-access","userId":"synthetic-user"}),
                None,
                "manual",
            )
            .unwrap();
        assert_eq!(partial["hasRefreshToken"], false);
        assert!(matches!(
            store
                .update_qwenwork_credentials(id, &prior, &next)
                .unwrap(),
            CredentialWrite::Stale
        ));
        next.client_id = "oauth-client".into();
        next.redirect_uri = "https://example.invalid/callback".into();
        store
            .add_qwenwork_account(&next.payload(), None, "manual")
            .unwrap();
        let prior = store.qwenwork_account_record(id).unwrap();
        store
            .add_qwenwork_account(
                &json!({"token":"partial-access-2","userId":"synthetic-user"}),
                None,
                "manual",
            )
            .unwrap();
        let record = store.qwenwork_account_record(id).unwrap();
        assert_eq!(record["oauthClientId"], "oauth-client");
        assert_eq!(
            record["oauthRedirectUri"],
            "https://example.invalid/callback"
        );
        assert!(matches!(
            store
                .update_qwenwork_credentials(id, &prior, &next)
                .unwrap(),
            CredentialWrite::Stale
        ));
    }
}
