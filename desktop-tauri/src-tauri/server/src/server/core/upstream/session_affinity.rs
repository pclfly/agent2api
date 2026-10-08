//! 会话粘性（session affinity）与轮询选路的状态层。
//!
//! 参考 `workbuddy2api-hub` 的 `SessionAffinity` + `AccountPool.pick_for_session`
//! （同口径移植，阈值照抄）。解决两个问题：
//!
//! ── 为什么需要会话粘性 ──────────────────────────────────────
//! 上游的 prompt cache 是**账号级**的：同一段前缀只有再次落到**同一个账号**
//! 才命中缓存。hub 的实测（11 个号）：8 次完全相同的前缀请求被轮询分散到
//! 8 个账号，缓存率全部 0%；固定落到同一账号时，第 2 次起缓存率 95.2%。
//! 因此：**同一对话固定同一账号**，不同对话之间才分散。
//!
//! ── 会话键从哪来 ────────────────────────────────────────────
//! 客户端通常不发会话标识（hub 的注释：sub2api / DSH 等客户端都没有），
//! 于是用**对话稳定前缀**派生：取消息列表前两条（system + 首条 user）——
//! 它们在整段对话生命周期内逐字节不变，同一对话的每一轮都会落到同一账号；
//! 不同对话的首条 user 不同，依旧分散。与 hub 的 `derive_affinity_key` 同款。
//!
//! ── 轮询的游标为什么是「账号 id」而不是下标 ─────────────────
//! 候选列表随请求变化（不同模型 = 不同的候选家、排除集也不同），下标会在
//! 列表长度变化时指向漂移。记「上次选中的账号 id」，下一次在候选里找它的
//! 位置、取下一个 —— 列表怎么变，语义都是「接着上次那个往后轮」。
//!
//! ── 进程级、不落盘（有意）───────────────────────────────────
//! 粘性是**性能优化**而不是正确性状态：重启后丢失只会让缓存暂时不命中，
//! 不会错路由。落盘反而引入「绑定到已删除账号」的陈旧数据问题。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::server::logging;

/// 粘性绑定的 TTL（毫秒）：2 小时（与 hub 的 `SessionAffinity(ttl=7200)` 同值）。
/// 命中时**滑动续期**（hub 的 get 同款）—— 一段持续活跃的对话不会中途掉绑定。
const AFFINITY_TTL_MS: i64 = 2 * 60 * 60 * 1000;
/// 绑定表容量上限：超出时清理过期项（hub 的 `max_entries=5000` 同值）。
const AFFINITY_MAX_ENTRIES: usize = 5000;

/// 绑定表：会话键 → (账号 id, 到期毫秒)。
fn affinity_table() -> &'static Mutex<HashMap<String, (String, i64)>> {
    static TABLE: OnceLock<Mutex<HashMap<String, (String, i64)>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 轮询游标：上次选中的账号 id（进程级）。
fn last_picked_slot() -> &'static Mutex<Option<String>> {
    static SLOT: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// 从请求体派生会话键（对照 hub 的 `derive_affinity_key`）：
/// 消息列表**前两条**（system + 首条 user）的稳定 hash。
///
/// 返回 `None`：没有 messages / 空列表（这类请求不参与粘性，走纯轮询）。
pub fn derive_affinity_key(body: &Value) -> Option<String> {
    let messages = body.get("messages")?.as_array()?;
    if messages.is_empty() {
        return None;
    }
    let head: Vec<&Value> = messages.iter().take(2).collect();
    // serde_json 的 Map 是 BTreeMap（未开 preserve_order）→ 键序稳定，
    // 与 hub 的 `json.dumps(..., sort_keys=True)` 同一效果
    let blob = serde_json::to_vec(&head).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&blob);
    let hex: String = hasher
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Some(format!("pfx-{hex}"))
}

/// 查绑定（命中时滑动续期；过期/不存在返回 None）。
pub fn affinity_get(key: &str) -> Option<String> {
    if key.is_empty() {
        return None;
    }
    let now = logging::now_ms();
    let mut table = affinity_table().lock().unwrap_or_else(|error| error.into_inner());
    let (account_id, expiry) = table.get(key)?.clone();
    if now > expiry {
        table.remove(key);
        return None;
    }
    table.insert(key.to_string(), (account_id.clone(), now + AFFINITY_TTL_MS));
    Some(account_id)
}

/// 绑定会话键 → 账号 id（超容量时先清过期项）。
pub fn affinity_bind(key: &str, account_id: &str) {
    if key.is_empty() || account_id.is_empty() {
        return;
    }
    let now = logging::now_ms();
    let mut table = affinity_table().lock().unwrap_or_else(|error| error.into_inner());
    if table.len() >= AFFINITY_MAX_ENTRIES {
        table.retain(|_, (_, expiry)| *expiry > now);
    }
    table.insert(key.to_string(), (account_id.to_string(), now + AFFINITY_TTL_MS));
}

/// 解绑（账号被拒 / 不可用时调用，下一轮重新轮询）。
pub fn affinity_unbind(key: &str) {
    if key.is_empty() {
        return;
    }
    let mut table = affinity_table().lock().unwrap_or_else(|error| error.into_inner());
    table.remove(key);
}

/// 上次轮询选中的账号 id。
pub fn last_picked() -> Option<String> {
    last_picked_slot()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

/// 记录本次轮询选中的账号 id（供下一次「接着往后轮」）。
pub fn note_picked(account_id: &str) {
    if account_id.is_empty() {
        return;
    }
    let mut slot = last_picked_slot().lock().unwrap_or_else(|error| error.into_inner());
    *slot = Some(account_id.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn derives_stable_key_from_message_prefix() {
        let body = json!({
            "messages": [
                {"role": "system", "content": "你是助手"},
                {"role": "user", "content": "你好"},
                {"role": "assistant", "content": "你好！"},
                {"role": "user", "content": "再讲一点"}
            ]
        });
        let key = derive_affinity_key(&body).expect("应派生出键");
        assert!(key.starts_with("pfx-"));
        assert_eq!(key.len(), 4 + 16);
        // 同一前缀（多轮之后）→ 同一个键
        let later = json!({
            "messages": [
                {"role": "system", "content": "你是助手"},
                {"role": "user", "content": "你好"},
                {"role": "assistant", "content": "你好！"},
                {"role": "user", "content": "再讲一点"},
                {"role": "assistant", "content": "好的"}
            ]
        });
        assert_eq!(derive_affinity_key(&later).as_deref(), Some(key.as_str()));
        // 不同对话（首条 user 不同）→ 不同的键
        let other = json!({
            "messages": [
                {"role": "system", "content": "你是助手"},
                {"role": "user", "content": "另一个问题"}
            ]
        });
        assert_ne!(derive_affinity_key(&other).as_deref(), Some(key.as_str()));
    }

    #[test]
    fn rejects_bodies_without_messages() {
        assert!(derive_affinity_key(&json!({})).is_none());
        assert!(derive_affinity_key(&json!({"messages": []})).is_none());
    }

    #[test]
    fn bind_get_unbind_round_trip() {
        let key = "pfx-test-roundtrip";
        affinity_unbind(key);
        assert!(affinity_get(key).is_none());
        affinity_bind(key, "account-1");
        assert_eq!(affinity_get(key).as_deref(), Some("account-1"));
        affinity_unbind(key);
        assert!(affinity_get(key).is_none());
    }
}
