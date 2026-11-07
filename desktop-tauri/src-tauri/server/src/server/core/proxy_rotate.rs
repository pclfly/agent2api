//! 账号出网代理轮询，候选来自手动地址与 Clash Verge 端点。
//! 游标按账号存于进程内；展示只读取候选，不推进游标。
//!
//! ── 冷却（`health` 子模块）──────────────────────────────────
//! 轮询的意义是「相邻两次出网落在不同出口」，所以一条刚失败的出口必须被
//! 跳过一段时间 —— 否则「换一条」会变成「又换回刚坏的那条」。冷却表在
//! `health` 里：按 `(账号, 出口身份, 目标 origin)` 记账，只落哈希，重启不丢。
//!
//! 失败不算「上游业务错误」：模型不存在、参数不对与出口无关，记进去只会让
//! 好出口被误跳过。判据在 [`is_egress_failure`]。

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

use crate::server::core::proxies::ResolvedProxy;
// 条目的「有效启用」（来源侧启用 ∧ 非本地禁用）直接复用池的判定，不再另写一份
// —— 只读 `enabled` 的判据会让「在「网络代理」页关掉的节点照样被轮询选中」：
// 用户按下开关却看不出任何效果，且每次选中都解析失败再顺延。
use crate::server::core::proxy_pool::is_enabled;

pub mod admission;
pub mod health;
pub mod probe;

pub use admission::{admit, try_admit, AdmissionError, Outcome as AdmissionOutcome, Permit};
pub use health::{clear_cooldowns, summary as cooldown_summary, ClearCooldowns};

/// 被冷却唤醒的一方（预热 worker / 排队中的请求）。
///
/// 现阶段唯一的消费者是「清空冷却」这个管理动作：清空之后必须让等着的请求
/// 立刻重试一轮，而不是等下一次自然超时。实现成一个计数信号即可 —— 缺一个
/// 订阅者不影响正确性，只是那次清空的效果要等下一轮。
static WAKE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 通知「冷却表变了，可以重试了」。
pub(crate) fn wake() {
    WAKE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// 当前唤醒代次（调用方比对前后值判断「期间有没有人改过冷却」）。
pub fn wake_generation() -> u64 {
    WAKE.load(std::sync::atomic::Ordering::SeqCst)
}

/// 一次失败**是否与出口有关**（决定要不要给它记冷却）。
///
/// ── 为什么必须分开 ──────────────────────────────────────────
/// 上游的业务错误（模型不存在、参数不合法、内容被拦）换一条出口**照样失败**
/// —— 给它记冷却等于把一条好出口无故冻住几分钟，还会让真正的病因埋在
/// 「换了 8 个出口都失败」的噪声里。
///
/// 判据按「出口相关」的三类来：
///   · 传输层失败（连接 / TLS / 超时）—— 出口本身有问题，最典型；
///   · 429 且文案指向 IP / 地区（`ip rate limit` / `unsupported_country` …）
///     —— 按 IP 计的额度，换出口确实能过；
///   · 403 且文案同上 —— 地区封锁的表现形式。
///
/// 纯 429（没有 IP/地区字样）**不**算出口问题：那是账号级/模型级的限流，
/// 换出口没用（上游看的是账号），该走账号冷却那条路。
pub fn is_egress_failure(status: Option<u16>, message: &str) -> bool {
    // 没有状态码 = 没拿到 HTTP 响应 = 传输层
    let Some(status) = status else {
        return true;
    };
    let text = message.to_lowercase();
    let exit_restricted = [
        "ip_rate_limit",
        "ip rate limit",
        "rate limit per ip",
        "rate limit exceeded for ip",
        "ip address blocked",
        "ip_address_blocked",
        "unsupported_country",
        "country not supported",
        "region not supported",
        "geo_blocked",
        "地区不支持",
        "ip限流",
        "ip 限流",
    ]
    .iter()
    .any(|signal| text.contains(signal));
    match status {
        403 | 429 => exit_restricted,
        // 5xx：网关/上游自身坏了，换出口通常也没用（同一条上游）
        _ => false,
    }
}

/// 从 `Retry-After` 头或文案里解析「多久之后再来」。
///
/// 支持三种形态（上游各家都用过）：纯秒数、RFC 2822 日期、以及 `reset_at`
/// 那类 ISO 时间（由 `errors::parse_quota_reset_at` 兜）。解析不出来给
/// `Duration::ZERO` —— 让退避曲线自己决定，不编一个时长。
pub fn retry_after_delay(headers: Option<&reqwest::header::HeaderMap>, message: &str) -> std::time::Duration {
    let now = chrono::Utc::now();
    if let Some(value) = headers
        .and_then(|headers| headers.get(reqwest::header::RETRY_AFTER))
        .and_then(|value| value.to_str().ok())
    {
        let text = value.trim();
        if let Ok(seconds) = text.parse::<u64>() {
            // 夹到一天：上游偶尔给一个荒谬的大数，冻一天已经足够表达「别来」
            return std::time::Duration::from_secs(seconds.min(86_400));
        }
        if let Ok(date) = chrono::DateTime::parse_from_rfc2822(text) {
            let seconds = (date.timestamp() - now.timestamp()).clamp(0, 86_400) as u64;
            return std::time::Duration::from_secs(seconds);
        }
    }
    // 文案里的恢复时刻（`errors::parse_quota_reset_at` 认 `YYYY-MM-DD HH:MM:SS UTC+8`）
    let reset_at = crate::server::errors::parse_quota_reset_at(message);
    if reset_at > now.timestamp_millis() {
        let seconds = ((reset_at - now.timestamp_millis()) / 1000).clamp(0, 86_400) as u64;
        return std::time::Duration::from_secs(seconds);
    }
    std::time::Duration::ZERO
}

/// 指定轮询时**配置里最多留多少条 id**：与 `core::proxies` 的归一上限同源
/// （两处都写 64 会被改歪一处，所以只在这里定义，归一那边引用它）。
pub const MAX_IDS: usize = 64;

/// 轮询游标表的大小上限（超出时整表清空）。
///
/// 键是账号 id，正常规模在几十个以内 —— 这个上限只防「账号被反复重建」这类
/// 病态增长（每条记录一个新 id）。清空是安全的：游标只是一段进度，丢了从头转。
const MAX_CURSORS: usize = 512;

/// 账号 id → 下一次从第几个候选开始（进程级，见模块头）
static CURSORS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

fn lock_cursors() -> std::sync::MutexGuard<'static, Option<HashMap<String, u64>>> {
    match CURSORS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 取号：返回本次从候选列表的第几位开始，并把游标推到下一位。
///
/// `len` 必须非 0（调用方已挡）；条目数变化时取模自然收敛到新列表长度。
fn take_cursor(key: &str, len: usize) -> usize {
    let mut guard = lock_cursors();
    let cursors = guard.get_or_insert_with(HashMap::new);
    if cursors.len() >= MAX_CURSORS && !cursors.contains_key(key) {
        cursors.clear();
    }
    let entry = cursors.entry(key.to_string()).or_insert(0);
    let start = (*entry as usize) % len;
    *entry = entry.wrapping_add(1);
    start
}

// ─── 配置读取 ───────────────────────────────────────────────

/// `all` 的读法：缺键 / null 当**真**。
///
/// 与写入侧（归一恒写显式布尔）配起来，「缺键」只可能来自手工改库；那时按
/// 「全部」解释比按「一条都不轮」解释安全 —— 前者是这条配置的默认语义，
/// 后者会让转发直接回退直连。
fn is_all(config: &Value) -> bool {
    match config.get("all") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(flag)) => *flag,
        // 脏值（字符串 / 数字）按「不是 false 即为真」的 JS 口径：只认显式 false
        Some(_) => true,
    }
}

/// 配置里显式列出的 id（`all: false` 那一支；去空、去重、保序、截断到 [`MAX_IDS`]）
fn configured_ids(config: &Value) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    let Some(items) = config.get("proxyIds").and_then(Value::as_array) else {
        return ids;
    };
    for item in items {
        let Some(text) = item.as_str() else { continue };
        let id = text.trim();
        if id.is_empty() || ids.iter().any(|existing| existing == id) {
            continue;
        }
        ids.push(id.to_string());
        if ids.len() >= MAX_IDS {
            break;
        }
    }
    ids
}

fn text_of(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// 轮询候选的条目 id（按配置取，见模块头）。
///
/// `all: true` → 池里**全部启用**条目（禁用项不参与：它在池里就是「先别用它」
/// 的意思，而轮询是逐请求选路，没有「这次先跳过」的余地 —— 硬选中它只会每次
/// 都解析失败再顺延，多绕一圈）。顺序即池的存储顺序（新加的在前）。
pub fn candidate_ids(config: &Value) -> Vec<String> {
    if !is_all(config) {
        return configured_ids(config);
    }
    crate::server::core::proxy_pool::raw_items()
        .iter()
        .filter(|item| is_enabled(item))
        .filter_map(|item| {
            let id = text_of(item, "id");
            if id.is_empty() { None } else { Some(id) }
        })
        .collect()
}

/// 条目的**廉价**可用性（不发网络）：用于展示摘要里的
/// 「可用 N 条」。真正的取用判定在解析那一步（`proxy_pool::resolve_reference`）。
fn entry_ready(item: &Value) -> Result<(), String> {
    if item.get("source").and_then(Value::as_str) == Some("clash") {
        return match crate::server::core::proxy_pool::resolve_item(item) {
            Some(crate::server::core::proxies::ProxyResolution::Resolved(_)) => Ok(()),
            Some(crate::server::core::proxies::ProxyResolution::Failed(reason)) => Err(reason),
            None => Err("条目地址不完整".to_string()),
        };
    }
    let host = text_of(item, "host");
    let port = item.get("port").and_then(Value::as_u64).unwrap_or(0);
    if host.is_empty() || !(1..=65535).contains(&port) {
        return Err("条目地址不完整".to_string());
    }
    Ok(())
}

// ─── 取用 ───────────────────────────────────────────────────

/// 这条轮询配置要求在什么失败上换出口（`failureMode`，见 `proxies` 的归一）。
///
/// 缺键 = `smart`：与写入侧的默认值一致。这里不做字符串校验（归一那一侧已经
/// 保证了只有两个合法值），认不出的值一律按 `smart` 处理 —— 那是安全的一侧。
pub fn requires_all_errors(config: &Value) -> bool {
    config.get("failureMode").and_then(Value::as_str) == Some("all_errors")
}

/// 这次失败要不要换出口。
///
/// `all_errors` 档：任何失败都换（调用方只在「可能是出口的锅」的地方问它，
/// 例如 429/5xx；业务 4xx 不该走到这里）。
/// `smart` 档：只换与出口有关的（[`is_egress_failure`]）。
pub fn should_rotate_on(config: &Value, status: Option<u16>, message: &str) -> bool {
    if requires_all_errors(config) {
        return true;
    }
    is_egress_failure(status, message)
}

/// 从 `start` 起绕一圈找第一条能解析的候选；一条都解析不出来时给出**第一条的
/// 原因**（它最有代表性：坏在哪，用户第一条就该看到那句话）+ 一句总起。
///
/// `ids` 由调用方算好传进来（候选要读一次池，一条请求只该读一次）。
///
/// ── 冷却怎么参与（`cooldown` 三态）──────────────────────────
///   · `None`：调用方没有目标 origin（展示、余额这类非转发链路），不看冷却；
///   · `Some((account, origin))`：跳过冷却中的条目，**但冷却全中时回落**到
///     「忽略冷却再挑一遍」。
///
/// 为什么要回落：整个池子都在冷却里（一次网络抖动会把所有出口各失败一次）
/// 时，报「都不可用」会让请求直接失败；而挑一条再试通常能过 —— 冷却的意义
/// 是「优先换一条」，不是「宁可失败也不用」。回落时仍然只挑一条，不额外放大。
fn pick_at(
    ids: &[String],
    start: usize,
    all: bool,
    cooldown: Option<(&str, &str)>,
) -> Result<ResolvedProxy, String> {
    if ids.is_empty() {
        return Err(if all {
            "轮询没有可用出口：「网络代理」页还没有启用的代理".to_string()
        } else {
            "轮询没有可用出口：账号设置里选中的代理都不在了（可能已被删除）".to_string()
        });
    }
    let mut first_error: Option<String> = None;
    let mut cooling: Vec<ResolvedProxy> = Vec::new();
    for offset in 0..ids.len() {
        let id = &ids[(start + offset) % ids.len()];
        match crate::server::core::proxy_pool::resolve_reference(id) {
            Ok(proxy) => {
                let allowed = match cooldown {
                    Some((account, origin)) => {
                        health::available(account, &proxy, origin)
                    }
                    None => true,
                };
                if allowed {
                    return Ok(proxy);
                }
                // 记下第一条冷却中的，作为「全在冷却」时的回落选项
                if cooling.is_empty() {
                    cooling.push(proxy);
                }
            }
            Err(reason) => {
                if first_error.is_none() {
                    first_error = Some(reason);
                }
            }
        }
    }
    // 全部解析成功但都在冷却里 → 回落挑一条（见函数头的说明）
    if let Some(proxy) = cooling.into_iter().next() {
        return Ok(proxy);
    }
    Err(format!(
        "轮询的出口都不可用：{}",
        first_error.unwrap_or_else(|| "没有可用的代理".to_string())
    ))
}

/// 轮询取下一个出口（游标按 `key` 前进；`key` = 账号 id）。
///
/// `key` 为空串时退化为「第一条可用」：没有可归属的账号身份就谈不上轮询进度，
/// 此时确定性优先（同一个调用方每次拿到同一条，便于复现与排障）。
pub fn next(config: &Value, key: &str) -> Result<ResolvedProxy, String> {
    next_for(config, key, None)
}

/// 带目标 origin 的轮询取用：跳过对**这个上游**正在冷却的出口。
///
/// 冷却按 `(账号, 出口, origin)` 记账 —— 同一条出口对 A 家被限流不代表对 B 家
/// 也不通，所以 origin 必须参与判定（见 `health` 的模块头）。
pub fn next_for(
    config: &Value,
    key: &str,
    origin: Option<&str>,
) -> Result<ResolvedProxy, String> {
    let ids = candidate_ids(config);
    // 空候选时不必取号（`take_cursor` 要求 len 非 0）——直接由 pick_at 报空
    let start = if ids.is_empty() || key.is_empty() {
        0
    } else {
        take_cursor(key, ids.len())
    };
    let cooldown = match (key.is_empty(), origin) {
        (false, Some(origin)) => Some((key, origin)),
        _ => None,
    };
    pick_at(&ids, start, is_all(config), cooldown)
}

/// 不前进游标的静态解析：给**展示**与**非转发**链路用（描述账号代理、
/// 余额查询这类「一次调用不代表一次转发」的地方）。
///
/// 取第一条可用条目 —— 它回答的是「这条配置现在能不能用、大概走哪儿」，
/// 而不是「这次该走哪儿」。**不看冷却**：展示要回答的是「配置本身能不能用」，
/// 而冷却「此刻先别用」。把冷却混进展示会让账号页在正常情况下也显示
/// 「不可用」。
pub fn first_usable(config: &Value) -> Result<ResolvedProxy, String> {
    let ids = candidate_ids(config);
    pick_at(&ids, 0, is_all(config), None)
}

// ─── 展示摘要（账号列表 / 日志文案用，不动游标）───────────────

/// 摘要读数：候选条数与其中可用的条数（判据见 [`entry_ready`]）
pub struct Summary {
    /// 是否「全部出口」档
    pub all: bool,
    /// 候选条数（all=false 时只数**还存在**的条目）
    pub total: usize,
    /// 其中看起来可用的条数（廉价判据，见 [`entry_ready`]）
    pub usable: usize,
    /// 一条候选都不可用时的原因（可用时为 None）
    pub error: Option<String>,
}

/// 算一份摘要（读一次池，不做网络请求）。
pub fn summary(config: &Value) -> Summary {
    let all = is_all(config);
    let items = crate::server::core::proxy_pool::raw_items();
    let mut total = 0usize;
    let mut usable = 0usize;
    let mut consider = |item: &Value| {
        total += 1;
        if is_enabled(item) && entry_ready(item).is_ok() {
            usable += 1;
        }
    };
    if all {
        for item in items.iter().filter(|item| is_enabled(item)) {
            consider(item);
        }
    } else {
        for id in configured_ids(config) {
            if let Some(item) = items
                .iter()
                .find(|item| text_of(item, "id") == id)
            {
                consider(item);
            }
        }
    }
    let error = if total == 0 {
        Some(if all {
            "「网络代理」页还没有启用的代理".to_string()
        } else {
            "轮询里选中的代理都不在了（可能已被删除）".to_string()
        })
    } else if usable == 0 {
        Some("轮询里的代理都不可用（已禁用或未就绪）".to_string())
    } else {
        None
    };
    Summary { all, total, usable, error }
}

#[cfg(test)]
mod failure_mode_tests {
    use super::*;
    use serde_json::json;

    fn rotating(all_errors: bool) -> Value {
        let mut config = json!({
            "source": "rotate",
            "all": true,
            "failureMode": if all_errors { "all_errors" } else { "smart" },
        });
        if !all_errors {
            config["failureMode"] = json!("smart");
        }
        config
    }

    /// `smart`：只换与出口有关的失败。业务错误换出口照样失败。
    #[test]
    fn smart_mode_ignores_business_errors() {
        let config = rotating(false);
        // 传输层（没有状态码）
        assert!(should_rotate_on(&config, None, "connection reset"));
        // 429 + IP 限流：按 IP 计的额度，换出口有意义
        assert!(should_rotate_on(
            &config,
            Some(429),
            "rate limit exceeded for ip 1.2.3.4"
        ));
        // 403 + 地区封锁
        assert!(should_rotate_on(
            &config,
            Some(403),
            "unsupported_country: CN is not supported"
        ));
        // 纯 429（没有 IP/地区字样）：账号级限流，换出口没用
        assert!(!should_rotate_on(&config, Some(429), "too many requests"));
        // 400 业务错误
        assert!(!should_rotate_on(
            &config,
            Some(400),
            "model gpt-x does not exist"
        ));
        // 5xx：同一条上游坏了，换出口也没用
        assert!(!should_rotate_on(&config, Some(502), "bad gateway"));
    }

    /// `all_errors`：任何失败都换（判据交给账号配置）。
    #[test]
    fn all_errors_mode_rotates_on_anything() {
        let config = rotating(true);
        assert!(should_rotate_on(&config, Some(400), "model does not exist"));
        assert!(should_rotate_on(&config, Some(502), "bad gateway"));
        assert!(should_rotate_on(&config, None, "connection reset"));
    }

    /// 缺键 = `smart`（安全的那一侧）。
    #[test]
    fn missing_failure_mode_defaults_to_smart() {
        let config = json!({ "source": "rotate", "all": true });
        assert!(!requires_all_errors(&config));
        assert!(!should_rotate_on(&config, Some(400), "bad model"));
    }

    /// `Retry-After` 的三种形态都要认，且都要夹到一天。
    #[test]
    fn retry_after_parsing_covers_seconds_dates_and_text() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("120"));
        assert_eq!(
            retry_after_delay(Some(&headers), ""),
            std::time::Duration::from_secs(120)
        );

        // 荒谬的大数被夹到一天
        let mut huge = HeaderMap::new();
        huge.insert(RETRY_AFTER, HeaderValue::from_static("99999999"));
        assert_eq!(
            retry_after_delay(Some(&huge), ""),
            std::time::Duration::from_secs(86_400)
        );

        // HTTP 日期
        let future = (chrono::Utc::now() + chrono::Duration::seconds(60))
            .to_rfc2822();
        let mut dated = HeaderMap::new();
        dated.insert(RETRY_AFTER, HeaderValue::from_str(&future).unwrap());
        let parsed = retry_after_delay(Some(&dated), "");
        assert!(
            parsed >= std::time::Duration::from_secs(30) && parsed <= std::time::Duration::from_secs(90),
            "HTTP 日期应解析成 30..90 秒，实际 {parsed:?}"
        );

        // 没有头时从文案里读（`classify_custom_error` 会把恢复时刻并进 message）
        let reset_at = crate::server::errors::parse_quota_reset_at("2099-01-01 00:00:00 UTC+8");
        assert!(reset_at > 0, "文案形态应当能被 parse_quota_reset_at 认出来");
        let text = "上游返回 429: too many requests（2099-01-01 00:00:00 UTC+8 恢复）";
        let from_text = retry_after_delay(None, text);
        assert_eq!(
            from_text,
            std::time::Duration::from_secs(86_400),
            "文案里的恢复时刻很远，应被夹到一天"
        );

        // 什么都没有：零（让退避曲线自己决定）
        assert_eq!(retry_after_delay(None, ""), std::time::Duration::ZERO);
    }
}
