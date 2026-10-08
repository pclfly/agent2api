//! LobsterAI 账号：添加 / 更新（网页登录与手工粘贴共用）、续期回写、公开形态。
//!
//! ── 身份怎么认 ──────────────────────────────────────────────
//! 去重键是 `uid`（四级回退：`user.id` → `user.userId` → `user.yid` →
//! `sha256(accessToken)[:16]`，见 `credentials::resolve_uid`）—— 任何情况下
//! 都能得到稳定 ID，重复登录必然命中同一条记录并覆盖它，而不是堆出第二个账号。
//!
//! ── 写回为什么是「字段补丁」而不是重建记录 ────────────────────
//! `update_lobsterai_credentials_if_current` 只并 `patch_fields()` 给出的
//! 非空键（`accessToken` / `refreshToken` / `expiresAt` / `uid` / `userId` /
//! `nickname` / `uuid` / `firstKeyfrom` / `latestKeyfrom`）。
//! **身份三件套（uuid / firstKeyfrom / latestKeyfrom）必须随凭据持久化** ——
//! 它们不在服务端响应里，丢了续期请求体就不完整，只能让用户重新登录。
//!
//! ── 空值不覆盖 ──────────────────────────────────────────────
//! 手工粘贴的凭据常常只有一把 accessToken。这时 `refreshToken` /
//! `expiresAt` 之类的空值**不能**写进已有记录（等于把上次登录拿到的续期串
//! 洗掉）。与 accio / zcode / trae 同一口径。

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::server::core::providers::lobsterai::credentials::Credential;
use crate::server::core::providers::lobsterai::PROVIDER_ID;
use crate::server::logging;

use super::priority::next_free_priority;
use super::sql;
use super::state::StoredAccount;
use super::store::{AccountStore, AccountStoreError};
use super::store_util::{max_concurrent_public, token_tail_of, truncate_chars};
use super::CredentialWrite;

impl AccountStore {
    /// 取一条 LobsterAI 账号记录（`account_id` 为空 = 这一家的队首可用账号）。
    pub fn lobsterai_account_record(&self, account_id: &str) -> Option<Value> {
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

    /// 添加 / 更新一条 LobsterAI 账号。
    ///
    /// `source` 是「这条怎么来的」（`web` / `manual`），只进记录不做判据。
    pub fn add_lobsterai_account(
        &self,
        credential: &Credential,
        name: Option<&str>,
        source: &str,
    ) -> Result<Value, AccountStoreError> {
        if !credential.valid() && !credential.can_refresh() {
            return Err(AccountStoreError::bad_request(
                "LobsterAI 凭据里既没有 accessToken 也没有 refreshToken",
            ));
        }
        // 身份：uid（四级回退保证非空）；uid 意外为空时按令牌指纹兜底
        let uid = credential.uid.trim().to_string();
        let seed = if uid.is_empty() {
            format!("token-{}", credential.access_token)
        } else {
            uid.clone()
        };
        let id = format!("lobsterai-{:x}", Sha256::digest(seed.as_bytes()));
        let guard = self.guard();
        let existing = self
            .records_for_provider(&guard, PROVIDER_ID)
            .into_iter()
            .find(|record| {
                let same_uid = !uid.is_empty()
                    && record.get("uid").and_then(Value::as_str) == Some(uid.as_str());
                let same_token = !credential.access_token.is_empty()
                    && record.get("accessToken").and_then(Value::as_str)
                        == Some(credential.access_token.as_str());
                same_uid || same_token
            });
        if existing.is_none() && self.record_by_id(&guard, &id).is_some() {
            return Err(AccountStoreError::new(
                "LobsterAI 账号 ID 已被其它账号占用，请先核对账号记录",
                409,
            ));
        }
        // 展示名：用户显式填的优先 → 记录里的旧名 → 昵称（手机号掩码归一化）→ uid
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
            .or_else(|| {
                let nickname = crate::server::core::providers::lobsterai::credentials::display_nickname(
                    credential,
                    "",
                );
                (!nickname.is_empty()).then_some(nickname)
            })
            .unwrap_or_else(|| format!("LobsterAI {}", truncate_chars(&uid, 20)));
        let mut fields = existing
            .as_ref()
            .map(|record| record.fields().clone())
            .unwrap_or_default();
        // 只并**非空**字段（见模块头「空值不覆盖」）
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
        // 本家没有「桌面端登录态导入」这条来源，恒 false
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
            &format!("✅ LobsterAI 账号已保存（{}，优先级 {priority}）", record_name),
        );
        Ok(self.public_account(&record))
    }

    /// 续期结果回写（比较-再写：记录里的凭证仍是续期前那份才写）。
    ///
    /// 比较的键里**必须**有 refreshToken：续期可能轮换它，若只比 accessToken，
    /// 两个并发续期里后完成的那个会把前一个的新串覆盖成旧串。
    /// 身份三件套（uuid / firstKeyfrom / latestKeyfrom）同样参与比较 ——
    /// 它们与凭据同生共死。
    pub fn update_lobsterai_credentials_if_current(
        &self,
        expected: &Value,
        credential: &Credential,
    ) -> Result<CredentialWrite, AccountStoreError> {
        let id = expected.get("id").and_then(Value::as_str).unwrap_or("");
        let guard = self.guard();
        let Some(mut record) = self
            .record_by_id(&guard, id)
            .filter(|record| record.provider() == PROVIDER_ID)
        else {
            return Ok(CredentialWrite::Stale);
        };
        for key in ["accessToken", "refreshToken", "uuid", "firstKeyfrom", "latestKeyfrom"] {
            if record.get(key) != expected.get(key) {
                return Ok(CredentialWrite::Stale);
            }
        }
        for (key, value) in credential.patch_fields() {
            if value.is_null() || matches!(&value, Value::String(text) if text.is_empty()) {
                continue;
            }
            record.fields_mut().insert(key.to_string(), value);
        }
        record.set("tokenTail", Value::String(token_tail_of(&credential.access_token)));
        record.set_updated_at(logging::now_ms());
        self.with_conn(&guard, |conn| sql::update_in_place(conn, &record))?;
        Ok(CredentialWrite::Written)
    }

    /// 公开形态（进面板账号列表与 `/api/accounts`）。
    pub fn to_lobsterai_public_account(&self, record: &StoredAccount) -> Value {
        let credential = Credential::from_value(&record.to_value());
        let available = credential.valid();
        let can_refresh = credential.can_refresh();
        let mut public = Map::new();
        for key in [
            "id",
            "provider",
            "name",
            "uid",
            "userId",
            "nickname",
            "source",
            "tokenTail",
            "expiresAt",
        ] {
            public.insert(key.to_string(), record.get(key).cloned().unwrap_or(Value::Null));
        }
        // 到期读数统一成**毫秒**（面板与别家共用同一列）
        public.insert(
            "expiresAtMs".to_string(),
            Value::from(
                crate::server::core::providers::lobsterai::credentials::credential_expires_at_ms(
                    &credential,
                )
                .unwrap_or(0),
            ),
        );
        public.insert("hasRefreshToken".to_string(), Value::Bool(can_refresh));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 每条用例一个独立的临时库（账号层测试碰的是真 SQLite）。
    fn store(label: &str) -> (AccountStore, crate::server::db::test_temp::TempDb) {
        let (db, guard) = crate::server::db::test_temp::TempDb::open(&format!("lobsterai-accounts-{label}"));
        (AccountStore::with_db(Some(db)), guard)
    }

    fn credential(uid: &str, access: &str) -> Credential {
        Credential {
            access_token: access.to_string(),
            refresh_token: "rt-1".to_string(),
            expires_at: "1900000000000".to_string(),
            uid: uid.to_string(),
            user_id: "yid-1".to_string(),
            nickname: "13011111100".to_string(),
            uuid: "uuid-1".to_string(),
            first_keyfrom: "1700000000000".to_string(),
            latest_keyfrom: "1700000000001".to_string(),
        }
    }

    #[test]
    fn add_and_dedupe_by_uid() {
        let (store, _db) = store("dedupe");
        let first = store
            .add_lobsterai_account(&credential("u-1", "at-1"), None, "manual")
            .expect("首次添加应成功");
        let id = first.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        assert!(!id.is_empty());
        // 同一 uid 再添加：命中同一条记录（覆盖，不堆第二条）
        let second = store
            .add_lobsterai_account(&credential("u-1", "at-2"), Some("改名"), "manual")
            .expect("重复添加应成功");
        assert_eq!(second.get("id").and_then(Value::as_str), Some(id.as_str()));
        assert_eq!(second.get("name").and_then(Value::as_str), Some("改名"));
        // 列表只有一条
        let list = store.list_accounts();
        let accounts = list.get("accounts").and_then(Value::as_array).cloned().unwrap_or_default();
        assert_eq!(accounts.len(), 1);
        // 反空跑：按 id 查得回记录
        assert!(store.lobsterai_account_record(&id).is_some());
    }

    #[test]
    fn empty_fields_do_not_overwrite() {
        let (store, _db) = store("no-overwrite");
        let first = store
            .add_lobsterai_account(&credential("u-2", "at-1"), None, "manual")
            .expect("首次添加应成功");
        let id = first.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        // 只带 accessToken 的第二次添加：不能把 refreshToken 洗掉
        let mut bare = Credential::default();
        bare.access_token = "at-3".to_string();
        bare.uid = "u-2".to_string();
        store
            .add_lobsterai_account(&bare, None, "manual")
            .expect("二次添加应成功");
        let record = store.lobsterai_account_record(&id).expect("记录应存在");
        assert_eq!(record.get("refreshToken").and_then(Value::as_str), Some("rt-1"));
        assert_eq!(record.get("accessToken").and_then(Value::as_str), Some("at-3"));
    }

    #[test]
    fn update_if_current_writes_only_when_unchanged() {
        let (store, _db) = store("compare-write");
        let first = store
            .add_lobsterai_account(&credential("u-3", "at-1"), None, "manual")
            .expect("添加应成功");
        let id = first.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let record = store.lobsterai_account_record(&id).expect("记录应存在");
        // expected 与记录一致 → 写回成功
        let mut next = Credential::from_value(&record);
        next.access_token = "at-new".to_string();
        let result = store
            .update_lobsterai_credentials_if_current(&record, &next)
            .expect("写回应成功");
        assert!(matches!(result, CredentialWrite::Written));
        let updated = store.lobsterai_account_record(&id).expect("记录应存在");
        assert_eq!(updated.get("accessToken").and_then(Value::as_str), Some("at-new"));
        // 旧 expected 再写 → Stale（不覆盖）
        let result = store
            .update_lobsterai_credentials_if_current(&record, &next)
            .expect("写回应成功");
        assert!(matches!(result, CredentialWrite::Stale));
    }

    #[test]
    fn public_account_shape() {
        let (store, _db) = store("public");
        let added = store
            .add_lobsterai_account(&credential("u-4", "at-1"), None, "manual")
            .expect("添加应成功");
        // 公开形态：不含 accessToken 本体，含 tokenTail 与 hasRefreshToken
        assert!(added.get("accessToken").is_none(), "公开形态不能含 accessToken");
        assert!(added.get("tokenTail").is_some());
        assert_eq!(added.get("hasRefreshToken").and_then(Value::as_bool), Some(true));
        assert_eq!(added.get("uid").and_then(Value::as_str), Some("u-4"));
        assert_eq!(added.get("desktop").and_then(Value::as_bool), Some(false));
        assert_eq!(added.get("available").and_then(Value::as_bool), Some(true));
    }
}
