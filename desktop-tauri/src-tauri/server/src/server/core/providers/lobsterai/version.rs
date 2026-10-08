//! LobsterAI 客户端版本号解析器（进程内缓存 + 兜底）。
//!
//! ── 为什么需要动态版本号 ────────────────────────────────────
//! 版本号是**签到接口的必填 query 参数**，也是 `X-LobsterAI-Client-Version`
//! 头的取值。它是日期式的（如 `2026.9.4`），变更频率极低（客户端发版节奏）。
//! 参考实现从更新接口动态拉取并缓存 12 小时 —— 不缓存会让每次签到多一次
//! 跨域请求；不拉取则要硬编码一个很快过期的值。
//!
//! ── 失败为什么不放弃 ────────────────────────────────────────
//! `sigin.py:73-76` 在取不到版本号时**整个脚本放弃签到**；参考实现比它宽容：
//! 用兜底值继续尝试并记日志 —— 宁可用一个稍旧的版本号试，也别让用户完全
//! 无法签到（后端对 `version` 并不强校验：Go 侧一直发假值 `0.1.0` 也未导致
//! 失败）。**兜底值不写缓存** —— 缓存会让一次瞬时故障在 TTL 内持续影响后续
//! 请求。

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;

use crate::server::core::egress;
use crate::server::logging;

use super::product;

/// 缓存条目：`(版本号, 写入时刻毫秒)`。
static CACHE: OnceLock<Mutex<Option<(String, i64)>>> = OnceLock::new();

fn cache_slot() -> &'static Mutex<Option<(String, i64)>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 校验并解析日期式版本号。
///
/// 正则对齐 `sigin.py:16-18` 的 `version_key`：
/// `^(\d+(?:\.\d+)*)(?:-[0-9A-Za-z.-]+)?$` —— 主干为点分数字，
/// 允许一个可选的预发布后缀（如 `2026.9.4-beta.1`）。
///
/// 之所以要**校验**而不是直接采信：版本号是签到接口的必填 query 参数，
/// 若上游返回 `null` / 空串 / HTML 错误页，把它拼进 URL 会让签到以一个
/// 更费解的错误失败。提前拒绝能给出「版本格式异常」这种可读原因。
pub fn parse_client_version(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (main, suffix) = match trimmed.split_once('-') {
        Some((main, suffix)) => (main, Some(suffix)),
        None => (trimmed, None),
    };
    // 主干：点分数字段，每段至少 1 位数字
    if main.is_empty() || !main.split('.').all(|part| {
        !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return None;
    }
    // 后缀：字母数字与 `.` / `-`（`-` 已在 split_once 处消费，不再出现）
    if let Some(suffix) = suffix {
        if suffix.is_empty()
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.')
        {
            return None;
        }
    }
    Some(trimmed.to_string())
}

/// 从更新接口的响应体里取出 `data.value.version`。
///
/// 该响应的结构与业务接口**不同**：`code`/`msg` 在外层，载荷在 `data.value`。
/// 实测形状：`{data:{value:{version:"2026.9.4", ...}}, code:0, msg:"OK"}`。
pub fn parse_client_version_from_update(body: &Value) -> Option<String> {
    let version = body
        .get("data")?
        .get("value")?
        .get("version")?
        .as_str()?;
    parse_client_version(version)
}

/// 解析当前客户端版本号。
///
/// 顺序：进程内缓存（未过期）→ 请求上游更新接口 → 兜底常量。
/// 返回值带 `source` 标记（`"cache"` / `"remote"` / `"fallback"`），
/// 让调用方能决定是否记日志。
pub async fn resolve() -> (String, &'static str) {
    let now = logging::now_ms();
    if let Ok(guard) = cache_slot().lock() {
        if let Some((version, cached_at)) = guard.as_ref() {
            if now - cached_at < product::VERSION_CACHE_TTL_MS as i64 {
                return (version.clone(), "cache");
            }
        }
    }
    let fetched = fetch_remote().await;
    if let Some(version) = fetched {
        if let Ok(mut guard) = cache_slot().lock() {
            *guard = Some((version.clone(), now));
        }
        return (version, "remote");
    }
    // 失败走兜底，**不写缓存**（见模块头）
    (product::FALLBACK_CLIENT_VERSION.to_string(), "fallback")
}

/// 同步读当前版本号（给同步上下文用，如 `build_chat_request` 拼头）。
///
/// 缓存命中直接返回；未命中/已过期返回兜底值，并**触发一次后台刷新**
/// （不阻塞当前请求 —— 第一次请求用兜底值，后续请求用真值）。
/// 用 CAS 节流，避免高并发下每个请求各 spawn 一个刷新任务。
pub fn current() -> String {
    let now = logging::now_ms();
    if let Ok(guard) = cache_slot().lock() {
        if let Some((version, cached_at)) = guard.as_ref() {
            if now - cached_at < product::VERSION_CACHE_TTL_MS as i64 {
                return version.clone();
            }
        }
    }
    spawn_refresh();
    product::FALLBACK_CLIENT_VERSION.to_string()
}

/// 后台刷新（节流：同一时刻只允许一个在飞）。
fn spawn_refresh() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static REFRESHING: AtomicBool = AtomicBool::new(false);
    if REFRESHING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    tokio::spawn(async {
        let _ = resolve().await;
        REFRESHING.store(false, Ordering::SeqCst);
    });
}

/// 请求上游更新接口取版本号；任何失败都返回 None（不抛错）。
async fn fetch_remote() -> Option<String> {
    let client = egress::client_for(None);
    let response = client
        .get(product::CLIENT_VERSION_API)
        .header("Accept", "application/json")
        .header("User-Agent", product::USER_AGENT)
        .timeout(Duration::from_millis(product::REQUEST_TIMEOUT_MS))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let text = response.text().await.ok()?;
    let body: Value = serde_json::from_str(&text).ok()?;
    parse_client_version_from_update(&body)
}

/// 清空缓存（测试用）。
#[cfg(test)]
pub fn clear_cache() {
    if let Ok(mut guard) = cache_slot().lock() {
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn version_validation_accepts_date_style() {
        assert_eq!(parse_client_version("2026.9.4").as_deref(), Some("2026.9.4"));
        assert_eq!(parse_client_version(" 2026.9.4 ").as_deref(), Some("2026.9.4"));
        assert_eq!(
            parse_client_version("2026.9.4-beta.1").as_deref(),
            Some("2026.9.4-beta.1")
        );
        assert_eq!(parse_client_version("1").as_deref(), Some("1"));
    }

    #[test]
    fn version_validation_rejects_garbage() {
        assert_eq!(parse_client_version(""), None);
        assert_eq!(parse_client_version("v2026.9.4"), None);
        assert_eq!(parse_client_version("2026..4"), None);
        assert_eq!(parse_client_version("2026.9.4-"), None);
        assert_eq!(parse_client_version("<html>"), None);
        assert_eq!(parse_client_version("2026.9.4-beta_1"), None);
    }

    #[test]
    fn update_payload_extracts_nested_version() {
        let body = json!({"data": {"value": {"version": "2026.9.4"}}, "code": 0, "msg": "OK"});
        assert_eq!(parse_client_version_from_update(&body).as_deref(), Some("2026.9.4"));
        // 形状不对 → None
        assert_eq!(parse_client_version_from_update(&json!({"data": {}})), None);
        assert_eq!(parse_client_version_from_update(&json!(null)), None);
    }
}
