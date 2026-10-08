//! Loomy 账号：添加 / 更新（短信登录与手工粘贴共用）、公开形态。
//!
//! ── 身份怎么认 ──────────────────────────────────────────────
//! 去重键是 `userid`（讯飞用户 id，18 位数字串；登录响应必带）。
//! userid 意外为空时按 session 指纹兜底 —— 同一串凭证重复粘贴仍命中同一条。
//!
//! ── 没有续期写回 ────────────────────────────────────────────
//! Loomy **没有 refresh 端点**，因此没有 `update_*_credentials_if_current`
//! 那条链路（与 CatPaw 同一处境：过期只能重新登录）。凭据的 `expires_at`
//! 是登录时刻 + 14 天的本地推算值。

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::server::core::providers::loomy::credentials::Credential;
use crate::server::core::providers::loomy::PROVIDER_ID;
use crate::server::logging;

use super::priority::next_free_priority;
use super::sql;
use super::state::StoredAccount;
use super::store::{AccountStore, AccountStoreError};
use super::store_util::{max_concurrent_public, token_tail_of, truncate_chars};

impl AccountStore {
    /// 取一条 Loomy 账号记录（`account_id` 为空 = 这一家的队首可用账号）。
    pub fn loomy_account_record(&self, account_id: &str) -> Option<Value> {
        let guard = self.guard();
        if !account_id.is_empty() {
            let record = self.record_by_id(&guard, account_id)?;
            return (record.provider() == PROVIDER_ID).then(|| record.to_value());
        }
        self.records_for_provider(&guard, PROVIDER_ID)
            .into_iter()
            .filter(|record| record.enabled() && record.has_token())
            .min_by_key(|record| record.order_key())
            .map(|record| record.to_value())
    }

    /// 添加 / 更新一条 Loomy 账号。
    pub fn add_loomy_account(
        &self,
        credential: &Credential,
        name: Option<&str>,
        source: &str,
    ) -> Result<Value, AccountStoreError> {
        if !credential.valid() {
            return Err(AccountStoreError::bad_request("Loomy 凭据里没有 session（accessToken）"));
        }
        let userid = credential.userid.trim().to_string();
        let seed = if userid.is_empty() {
            format!("token-{}", credential.access_token)
        } else {
            userid.clone()
        };
        let id = format!("loomy-{:x}", Sha256::digest(seed.as_bytes()));
        let guard = self.guard();
        let existing = self
            .records_for_provider(&guard, PROVIDER_ID)
            .into_iter()
            .find(|record| {
                let same_userid = !userid.is_empty()
                    && record.get("userId").and_then(Value::as_str) == Some(userid.as_str());
                let same_token = !credential.access_token.is_empty()
                    && record.get("accessToken").and_then(Value::as_str)
                        == Some(credential.access_token.as_str());
                same_userid || same_token
            });
        if existing.is_none() && self.record_by_id(&guard, &id).is_some() {
            return Err(AccountStoreError::new(
                "Loomy 账号 ID 已被其它账号占用，请先核对账号记录",
                409,
            ));
        }
        // 展示名：用户填的优先 → 旧名 → 手机号（脱敏）→ userid
        let phone_display = mask_phone(&credential.phone);
        let record_name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                existing
                    .as_ref()
                    .map(StoredAccount::name)
                    .filter(|value| !value.is_empty())
            })
            .or_else(|| (!phone_display.is_empty()).then_some(phone_display.clone()))
            .unwrap_or_else(|| format!("Loomy {}", truncate_chars(&userid, 20)));
        let mut fields = existing
            .as_ref()
            .map(|record| record.fields().clone())
            .unwrap_or_default();
        for (key, value) in credential.patch_fields() {
            if value.is_null() || matches!(&value, Value::String(text) if text.is_empty()) {
                continue;
            }
            fields.insert(key.to_string(), value);
        }
        let priority = match existing.as_ref() {
            Some(record) => record.priority(),
            None => {
                let used = self
                    .with_conn(&guard, |conn| sql::priorities_all(conn))
                    .unwrap_or_default();
                next_free_priority(&used)
            }
        };
        fields.insert("id".to_string(), Value::String(id.clone()));
        fields.insert("provider".to_string(), Value::String(PROVIDER_ID.to_string()));
        fields.insert("name".to_string(), Value::String(truncate_chars(&record_name, 100)));
        fields.insert(
            "tokenTail".to_string(),
            Value::String(token_tail_of(&credential.access_token)),
        );
        fields.insert("priority".to_string(), Value::from(priority));
        fields.insert(
            "enabled".to_string(),
            Value::Bool(existing.as_ref().map(StoredAccount::enabled).unwrap_or(true)),
        );
        fields.insert("desktop".to_string(), Value::Bool(false));
        fields.insert("source".to_string(), Value::String(source.to_string()));
        fields.insert(
            "addedAt".to_string(),
            Value::from(
                existing
                    .as_ref()
                    .map(StoredAccount::added_at)
                    .unwrap_or_else(logging::now_ms),
            ),
        );
        fields.insert("updatedAt".to_string(), Value::from(logging::now_ms()));
        fields.insert("rateLimits".to_string(), json!({}));
        let record = StoredAccount::from_map(fields);
        self.with_conn(&guard, |conn| sql::put(conn, &record))?;
        logging::log(
            "[Accounts]",
            &format!("✅ Loomy 账号已保存（{}，优先级 {priority}）", record_name),
        );
        Ok(self.public_account(&record))
    }

    /// 公开形态（进面板账号列表与 `/api/accounts`）。
    pub fn to_loomy_public_account(&self, record: &StoredAccount) -> Value {
        let credential = Credential::from_value(&record.to_value());
        let available = credential.valid();
        let mut public = Map::new();
        for key in [
            "id",
            "provider",
            "name",
            "userId",
            "phone",
            "nickname",
            "source",
            "tokenTail",
            "expiresAt",
        ] {
            public.insert(key.to_string(), record.get(key).cloned().unwrap_or(Value::Null));
        }
        public.insert(
            "expiresAtMs".to_string(),
            Value::from(
                crate::server::core::providers::loomy::credentials::credential_expires_at_ms(&credential)
                    .unwrap_or(0),
            ),
        );
        // 本家恒不可刷新（没有 refresh 端点）—— 界面据此显示「过期后重新登录」
        public.insert("hasRefreshToken".to_string(), Value::Bool(false));
        public.insert("priority".to_string(), Value::from(record.priority()));
        public.insert("enabled".to_string(), Value::Bool(record.enabled()));
        public.insert("addedAt".to_string(), Value::from(record.added_at()));
        public.insert("updatedAt".to_string(), Value::from(record.updated_at()));
        public.insert(
            "proxy".to_string(),
            crate::server::core::proxies::describe_account_proxy(Some(&record.proxy())),
        );
        public.insert(
            "rateLimits".to_string(),
            record.get("rateLimits").cloned().unwrap_or_else(|| json!({})),
        );
        public.insert("desktop".to_string(), Value::Bool(false));
        public.insert("available".to_string(), Value::Bool(available));
        public.insert(
            "maxConcurrent".to_string(),
            Value::from(max_concurrent_public(record.get("maxConcurrent"))),
        );
        Value::Object(public)
    }
}

/// 手机号脱敏（展示名用）：`13011111100` → `130****1100`。
fn mask_phone(phone: &str) -> String {
    let trimmed = phone.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() == 11 && bytes[0] == b'1' && bytes.iter().all(u8::is_ascii_digit) {
        return format!("{}****{}", &trimmed[..3], &trimmed[7..]);
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(label: &str) -> (AccountStore, crate::server::db::test_temp::TempDb) {
        let (db, guard) = crate::server::db::test_temp::TempDb::open(&format!("loomy-accounts-{label}"));
        (AccountStore::with_db(Some(db)), guard)
    }

    fn credential(userid: &str, session: &str) -> Credential {
        Credential {
            access_token: session.to_string(),
            userid: userid.to_string(),
            phone: "13011111100".to_string(),
            nickname: String::new(),
            expires_at: "1900000000000".to_string(),
        }
    }

    #[test]
    fn add_and_dedupe_by_userid() {
        let (store, _db) = store("dedupe");
        let first = store
            .add_loomy_account(&credential("123456789012345678", "s1"), None, "manual")
            .expect("首次添加应成功");
        let id = first.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let second = store
            .add_loomy_account(&credential("123456789012345678", "s2"), Some("改名"), "manual")
            .expect("重复添加应成功");
        assert_eq!(second.get("id").and_then(Value::as_str), Some(id.as_str()));
        assert_eq!(second.get("name").and_then(Value::as_str), Some("改名"));
        // 反空跑
        assert!(store.loomy_account_record(&id).is_some());
    }

    #[test]
    fn default_name_uses_masked_phone() {
        let (store, _db) = store("name");
        let added = store
            .add_loomy_account(&credential("u-1", "s1"), None, "manual")
            .expect("添加应成功");
        assert_eq!(added.get("name").and_then(Value::as_str), Some("130****1100"));
    }

    #[test]
    fn public_shape_has_no_refresh() {
        let (store, _db) = store("public");
        let added = store
            .add_loomy_account(&credential("u-2", "s1"), None, "manual")
            .expect("添加应成功");
        assert!(added.get("accessToken").is_none());
        assert_eq!(added.get("hasRefreshToken").and_then(Value::as_bool), Some(false));
        assert_eq!(added.get("available").and_then(Value::as_bool), Some(true));
    }
}
