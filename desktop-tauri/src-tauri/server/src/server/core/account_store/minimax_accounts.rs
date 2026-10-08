//! MiniMax 账号：添加 / 更新（设备码登录与手工粘贴共用）、续期回写、公开形态。
//!
//! ── 身份怎么认 ──────────────────────────────────────────────
//! token 不是 JWT、解不出 `sub`，故去重键按 **token 指纹**（`sha256(accessToken)`）
//! 派生 —— 同一串凭证重复添加仍命中同一条。昵称只影响展示名。
//!
//! ── 展示名：脱敏手机号（2026-10 起）──────────────────────────
//! 上游昵称是随机串（`MiniMax597757` / `探索者8626`），认不出是哪个号；手机号
//! 由 `providers::minimax::profile` 从 `GET /v1/api/user/info` 取（JWT 与
//! `mmoat_` 两种凭证都能拿到），落在记录的 `phone` 上，公开形态只给脱敏形态
//! （`180****1324`）。详见 [`AccountStore::to_minimax_public_account`]。

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::server::core::providers::minimax::credentials::Credential;
use crate::server::core::providers::minimax::PROVIDER_ID;
use crate::server::logging;

use super::priority::next_free_priority;
use super::sql;
use super::state::StoredAccount;
use super::store::{AccountStore, AccountStoreError};
use super::store_util::{max_concurrent_public, token_tail_of, truncate_chars};
use super::CredentialWrite;

impl AccountStore {
    /// 取一条 MiniMax 账号记录（`account_id` 为空 = 这一家的队首可用账号）。
    pub fn minimax_account_record(&self, account_id: &str) -> Option<Value> {
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

    /// 添加 / 更新一条 MiniMax 账号。
    pub fn add_minimax_account(
        &self,
        credential: &Credential,
        name: Option<&str>,
        source: &str,
    ) -> Result<Value, AccountStoreError> {
        if !credential.valid() && !credential.can_refresh() {
            return Err(AccountStoreError::bad_request(
                "MiniMax 凭据里既没有 accessToken 也没有 refreshToken",
            ));
        }
        // 身份：token 指纹（token 非 JWT，没有可用的稳定字段）
        let fingerprint = {
            let digest = Sha256::digest(credential.access_token.as_bytes());
            digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>()
        };
        let id = format!("minimax-{}", &fingerprint[..32.min(fingerprint.len())]);
        let guard = self.guard();
        let existing = self
            .records_for_provider(&guard, PROVIDER_ID)
            .into_iter()
            .find(|record| {
                let same_token = !credential.access_token.is_empty()
                    && record.get("accessToken").and_then(Value::as_str)
                        == Some(credential.access_token.as_str());
                let same_refresh = !credential.refresh_token.is_empty()
                    && record.get("refreshToken").and_then(Value::as_str)
                        == Some(credential.refresh_token.as_str());
                same_token || same_refresh
            });
        if existing.is_none() && self.record_by_id(&guard, &id).is_some() {
            return Err(AccountStoreError::new(
                "MiniMax 账号 ID 已被其它账号占用，请先核对账号记录",
                409,
            ));
        }
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
                (!credential.nickname.trim().is_empty()).then(|| credential.nickname.trim().to_string())
            })
            // 上游昵称拿不到时用**脱敏手机号**（本家的展示名口径，见 `profile.rs`）
            .or_else(|| {
                let masked = crate::server::core::providers::minimax::profile::mask_phone(&credential.phone);
                (!masked.is_empty()).then_some(masked)
            })
            .unwrap_or_else(|| format!("MiniMax {}", truncate_chars(&fingerprint, 8)));
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
            &format!("✅ MiniMax 账号已保存（{}，优先级 {priority}）", record_name),
        );
        Ok(self.public_account(&record))
    }

    /// 续期结果回写（比较-再写：记录里的凭证仍是续期前那份才写）。
    ///
    /// 比较的键里**必须**有 refreshToken：续期会轮换它（一次一换），
    /// 若只比 accessToken，两个并发续期里后完成的那个会把新串覆盖成旧串。
    pub fn update_minimax_credentials_if_current(
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
        for key in ["accessToken", "refreshToken"] {
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

    /// 回填手机号（展示名用，见 `providers::minimax::profile`）。
    ///
    /// ── 为什么单独一个方法，而不是复用「比较-再写」──────────────
    /// 手机号**不是凭据**：它不参与任何请求，写错也不会让转发失败；而
    /// 凭证写回（`update_minimax_credentials_if_current`）比的是 accessToken /
    /// refreshToken —— 余额查询期间凭证完全可能刚好被续期换掉，用同一套
    /// 比较会把这次回填判成 `Stale` 白丢。所以这里只做「记录还在 + 值确实变了」
    /// 两道最小检查。
    ///
    /// 返回是否真的写了（调用方只用于日志，不看也不影响正确性）。
    pub fn set_minimax_phone(
        &self,
        account_id: &str,
        phone: &str,
    ) -> Result<bool, AccountStoreError> {
        let trimmed = phone.trim();
        if account_id.is_empty() || trimmed.is_empty() {
            return Ok(false);
        }
        let guard = self.guard();
        let Some(mut record) = self
            .record_by_id(&guard, account_id)
            .filter(|record| record.provider() == PROVIDER_ID)
        else {
            return Ok(false);
        };
        if record.get("phone").and_then(Value::as_str) == Some(trimmed) {
            return Ok(false);
        }
        record.set("phone", Value::String(trimmed.to_string()));
        record.set_updated_at(logging::now_ms());
        self.with_conn(&guard, |conn| sql::update_in_place(conn, &record))?;
        Ok(true)
    }

    /// 导入 MiniMax 客户端登录态（`~/.minimax/local-runtime.auth.json` 优先，
    /// 兼容旧的 `auth/prod/cn/mcode-public/auth.json`）。
    ///
    /// ── 身份：realUserID（而不是 token 指纹）───────────────────
    /// JWT 会随桌面端续期而变化（10 天一轮），按 token 指纹去重会让
    /// 「重新导入」堆出一串重复账号。因此同 `realUserId` 的记录**原位更新**
    /// （保留 id / 备注名 / 优先级 / 加入时间 —— 与 unigate 的同款口径）；
    /// 只有找不到同一账号时才新增。mcode 来源没有 realUserID，退回
    /// `add_minimax_account` 的 token 指纹去重（旧口径）。
    ///
    /// ── 只读纪律（见 `providers::minimax::import` 的模块头）────
    ///   - 凭据**落盘快照**到账号库（客户端文件永远不动）；
    ///   - 记录带 `desktop: true` 标记：后续使用时**优先重读客户端文件**
    ///     （客户端刷新后网关自动拿到新 token）；
    ///   - **不自动调上游续期**（`credentials_expiring` 对 desktop 账号只在
    ///     「文件比记录新」时触发同步，同步 = 重读文件）。
    ///
    /// ── 为什么是 async ──────────────────────────────────────────
    /// 客户端文件里**没有手机号**（JWT payload 只有 id / name / avatar），
    /// 落账号前要调一次 `GET /v1/api/user/info` 把它取回来当展示名
    /// （见 `providers::minimax::profile`）。查询失败不影响导入。
    pub async fn import_minimax_desktop_account(
        &self,
        name: Option<&str>,
    ) -> Result<Value, AccountStoreError> {
        let mut desktop = crate::server::core::providers::minimax::import::read_desktop_credentials()
            .map_err(|error| AccountStoreError::new(error.message, error.status_code))?
            .ok_or_else(|| {
                AccountStoreError::new(
                    "本机没有找到 MiniMax 登录态（请先在 MiniMax 客户端登录）",
                    400,
                )
            })?;
        if let Some(phone) =
            crate::server::core::providers::minimax::profile::try_query_phone(&desktop.credential).await
        {
            desktop.credential.phone = phone;
        }
        // 同 realUserID 的既有记录 → 原位更新（保留 id / 备注名 / 优先级）
        if !desktop.real_user_id.is_empty() {
            let guard = self.guard();
            let existing = self
                .records_for_provider(&guard, PROVIDER_ID)
                .into_iter()
                .find(|record| {
                    record.get("realUserId").and_then(Value::as_str)
                        == Some(desktop.real_user_id.as_str())
                });
            if let Some(mut record) = existing {
                for (key, value) in desktop.credential.patch_fields() {
                    if value.is_null()
                        || matches!(&value, Value::String(text) if text.is_empty())
                    {
                        continue;
                    }
                    record.fields_mut().insert(key.to_string(), value);
                }
                record.set(
                    "tokenTail",
                    Value::String(token_tail_of(&desktop.credential.access_token)),
                );
                record.set("desktop", Value::Bool(true));
                record.set_updated_at(logging::now_ms());
                self.with_conn(&guard, |conn| sql::update_in_place(conn, &record))?;
                logging::log(
                    "[Accounts]",
                    &format!(
                        "✅ MiniMax 客户端登录态已同步（{}，来源 {}）",
                        record.name(),
                        desktop.source
                    ),
                );
                return Ok(self.public_account(&record));
            }
        }
        // 新账号：走既有添加路径（去重、命名、优先级全复用），再补 desktop 标记
        let account = self.add_minimax_account(&desktop.credential, name, "import")?;
        let id = account
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            return Err(AccountStoreError::new("导入失败：账号 id 缺失", 500));
        }
        let guard = self.guard();
        let Some(mut record) = self.record_by_id(&guard, &id) else {
            return Err(AccountStoreError::new("导入失败：写入后读不回记录", 500));
        };
        record.set("desktop", Value::Bool(true));
        record.set_updated_at(logging::now_ms());
        self.with_conn(&guard, |conn| sql::update_in_place(conn, &record))?;
        logging::log(
            "[Accounts]",
            &format!(
                "✅ MiniMax 客户端登录态已导入（{}，来源 {}）",
                record.name(),
                desktop.source
            ),
        );
        Ok(self.public_account(&record))
    }

    /// 公开形态（进面板账号列表与 `/api/accounts`）。
    ///
    /// ── 展示名为什么是手机号（2026-10 起）───────────────────────
    /// 本家的凭证里没有能认出「这是哪个账号」的字段：上游昵称是随机串
    /// （`MiniMax597757` / `探索者8626`），`userID` 是一串短 id，两者都看不出
    /// 绑的是哪个手机号。界面（`accounts-panels` 的 `accountLabel`）优先显示
    /// `nickname`，所以这里把 `nickname` 换成**脱敏手机号**（`180****1324`，
    /// 与 Loomy / AutoClaw 同一口径）；上游昵称挪到 `upstreamNickname` 保留，
    /// 备注名仍是 `name`（用户在账号页自己填的那个）。
    ///
    /// ⚠️ 手机号**明文不出网关**：这里只给脱敏形态。记录里没手机号时
    /// （老账号、上游没绑号）`nickname` 回落到上游昵称 —— 由余额查询那条链路
    /// 回填（见 `providers::minimax::balance::query_usage`）。
    pub fn to_minimax_public_account(&self, record: &StoredAccount) -> Value {
        let credential = Credential::from_value(&record.to_value());
        let available = credential.valid();
        let can_refresh = credential.can_refresh();
        let phone_display =
            crate::server::core::providers::minimax::profile::mask_phone(&credential.phone);
        let upstream_nickname = record
            .get("nickname")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut public = Map::new();
        for key in [
            "id",
            "provider",
            "name",
            "source",
            "tokenTail",
            "expiresAt",
            "scope",
        ] {
            public.insert(key.to_string(), record.get(key).cloned().unwrap_or(Value::Null));
        }
        // 展示名：脱敏手机号优先，其次上游昵称（见上面的说明）
        public.insert(
            "nickname".to_string(),
            Value::String(if phone_display.is_empty() {
                upstream_nickname.clone()
            } else {
                phone_display.clone()
            }),
        );
        public.insert("phone".to_string(), Value::String(phone_display));
        public.insert("upstreamNickname".to_string(), Value::String(upstream_nickname));
        public.insert(
            "expiresAtMs".to_string(),
            Value::from(
                crate::server::core::providers::minimax::credentials::credential_expires_at_ms(&credential)
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

    fn store(label: &str) -> (AccountStore, crate::server::db::test_temp::TempDb) {
        let (db, guard) = crate::server::db::test_temp::TempDb::open(&format!("minimax-accounts-{label}"));
        (AccountStore::with_db(Some(db)), guard)
    }

    fn credential(access: &str, refresh: &str) -> Credential {
        Credential {
            access_token: access.to_string(),
            refresh_token: refresh.to_string(),
            token_type: "Bearer".to_string(),
            expires_at: "1900000000000".to_string(),
            scope: "agent.default".to_string(),
            nickname: "测试".to_string(),
            real_user_id: String::new(),
            phone: String::new(),
        }
    }

    #[test]
    fn add_and_dedupe_by_token() {
        let (store, _db) = store("dedupe");
        let first = store
            .add_minimax_account(&credential("mmoat_a", "mmort_r"), None, "manual")
            .expect("首次添加应成功");
        let id = first.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let second = store
            .add_minimax_account(&credential("mmoat_a", "mmort_r"), Some("改名"), "manual")
            .expect("重复添加应成功");
        assert_eq!(second.get("id").and_then(Value::as_str), Some(id.as_str()));
        assert_eq!(second.get("name").and_then(Value::as_str), Some("改名"));
        // 反空跑
        assert!(store.minimax_account_record(&id).is_some());
    }

    #[test]
    fn update_if_current_compares_refresh_token() {
        let (store, _db) = store("compare");
        let added = store
            .add_minimax_account(&credential("mmoat_a", "mmort_r"), None, "manual")
            .expect("添加应成功");
        let id = added.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let record = store.minimax_account_record(&id).expect("记录应存在");
        let mut next = Credential::from_value(&record);
        next.access_token = "mmoat_b".to_string();
        next.refresh_token = "mmort_r2".to_string();
        let result = store
            .update_minimax_credentials_if_current(&record, &next)
            .expect("写回应成功");
        assert!(matches!(result, CredentialWrite::Written));
        // 旧 expected 再写 → Stale
        let result = store
            .update_minimax_credentials_if_current(&record, &next)
            .expect("写回应成功");
        assert!(matches!(result, CredentialWrite::Stale));
    }

    #[test]
    fn public_shape() {
        let (store, _db) = store("public");
        let added = store
            .add_minimax_account(&credential("mmoat_a", "mmort_r"), None, "manual")
            .expect("添加应成功");
        assert!(added.get("accessToken").is_none());
        assert_eq!(added.get("hasRefreshToken").and_then(Value::as_bool), Some(true));
        assert_eq!(added.get("available").and_then(Value::as_bool), Some(true));
    }

    /// 回归：**续期回写的期望快照必须带账号 id**。
    ///
    /// 历史 bug：`expected` 只放 `credential.patch_fields()`（7 个凭证字段、
    /// 没有 id），store 用 `expected["id"]` 定位记录 → 取不到 → 每次续期都被
    /// 判 `Stale` 且被调用处 `let _ =` 吞掉 ⇒ 新 token 一次也没落库，
    /// 60 分钟到期的 MiniMax 账号于是「刷新成功但显示已过期、下一轮用旧
    /// refresh_token 被上游拒」。
    #[test]
    fn refresh_writeback_needs_account_id() {
        let (store, _db) = store("writeback-id");
        let added = store
            .add_minimax_account(&credential("mmoat_a", "mmort_r"), None, "manual")
            .expect("添加应成功");
        let id = added.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let record = store.minimax_account_record(&id).expect("记录应存在");
        let current = Credential::from_value(&record);
        let mut next = current.clone();
        next.access_token = "mmoat_new".to_string();
        next.refresh_token = "mmort_new".to_string();

        // 旧形状（无 id）→ Stale：写不进去（这就是当初的 bug）
        let mut legacy = serde_json::Map::new();
        for (key, value) in current.patch_fields() {
            legacy.insert(key.to_string(), value);
        }
        let result = store
            .update_minimax_credentials_if_current(&Value::Object(legacy), &next)
            .expect("写回应成功");
        assert!(
            matches!(result, CredentialWrite::Stale),
            "没有 id 的期望快照必须判 Stale（否则说明 store 的定位口径变了）"
        );
        assert_eq!(
            store
                .minimax_account_record(&id)
                .and_then(|record| record.get("accessToken").and_then(Value::as_str).map(str::to_string)),
            Some("mmoat_a".to_string()),
            "Stale 时不能写进去"
        );

        // 正确形状（带 id）→ Written：新凭证落库
        let result = store
            .update_minimax_credentials_if_current(
                &crate::server::core::providers::minimax::expected_patch(&id, &current),
                &next,
            )
            .expect("写回应成功");
        assert!(matches!(result, CredentialWrite::Written));
        let stored = store.minimax_account_record(&id).expect("记录应存在");
        assert_eq!(
            stored.get("accessToken").and_then(Value::as_str),
            Some("mmoat_new")
        );
        assert_eq!(
            stored.get("refreshToken").and_then(Value::as_str),
            Some("mmort_new")
        );
        assert_eq!(stored.get("expiresAt").and_then(Value::as_str), Some("1900000000000"));
    }

    /// 展示名：手机号进公开形态的是**脱敏形态**（明文只留在记录里）。
    #[test]
    fn public_shape_shows_masked_phone() {
        let (store, _db) = store("phone");
        let mut with_phone = credential("mmoat_p", "mmort_p");
        with_phone.phone = "18070551324".to_string();
        with_phone.nickname = "探索者8626".to_string();
        let added = store
            .add_minimax_account(&with_phone, None, "web")
            .expect("添加应成功");
        // 展示名（前端优先读 nickname）= 脱敏手机号；上游昵称挪到 upstreamNickname
        assert_eq!(added.get("nickname").and_then(Value::as_str), Some("180****1324"));
        assert_eq!(added.get("phone").and_then(Value::as_str), Some("180****1324"));
        assert_eq!(
            added.get("upstreamNickname").and_then(Value::as_str),
            Some("探索者8626")
        );
        // 明文不出网关
        assert!(added.get("accessToken").is_none());
        let serialized = added.to_string();
        assert!(!serialized.contains("18070551324"), "公开形态不能带明文手机号");
    }

    /// 回填：只在值真的变化时写；账号不存在 / 空号一律不写。
    #[test]
    fn set_minimax_phone_backfills() {
        let (store, _db) = store("phone-backfill");
        let added = store
            .add_minimax_account(&credential("mmoat_b", "mmort_b"), None, "manual")
            .expect("添加应成功");
        let id = added.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        assert_eq!(added.get("nickname").and_then(Value::as_str), Some("测试"), "没有手机号时回落上游昵称");

        assert!(store.set_minimax_phone(&id, "13011111100").expect("回填应成功"));
        let stored = store.minimax_account_record(&id).expect("记录应存在");
        assert_eq!(stored.get("phone").and_then(Value::as_str), Some("13011111100"));
        // 同值再写 → 不写（返回 false）
        assert!(!store.set_minimax_phone(&id, "13011111100").expect("回填应成功"));
        // 空号 / 不存在的账号 → 不写
        assert!(!store.set_minimax_phone(&id, "  ").expect("回填应成功"));
        assert!(!store.set_minimax_phone("minimax-none", "13011111100").expect("回填应成功"));
    }
}
