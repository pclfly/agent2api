//! 网络代理设置，存于 kv 的 `proxySettings` 保留键。
//!
//! 四个键：
//!   · `testUrl`：连通性测试目标，空时沿用网关默认目标；
//!   · `listenerPortBase` / `listenerPortEnd`：订阅节点的本地监听端口区间
//!     （见 [`listener_port_range`]）；
//!   · `rotateMaxConcurrentPerIp` / `rotateMaxQueueSize` /
//!     `rotateQueueTimeoutSeconds`：轮询出口的并发与排队预算。
//!
//! 读的是**生效值**（非法 / 倒挂一律回落默认），写的是校验过的整数 ——
//! 界面上展示的就是下一个请求 / 下一次内核分配实际会用的那一组。

use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::server::db::Db;

pub const KV_KEY: &str = "proxySettings";
pub const DEFAULT_TEST_URL: &str = "https://copilot.tencent.com/v2/config";
/// 订阅节点监听端口的默认区间（与历史版本一致）。
pub const DEFAULT_LISTENER_PORT_BASE: u16 = 19100;
pub const DEFAULT_LISTENER_PORT_END: u16 = 19200;
const MAX_URL_LENGTH: usize = 2048;
static DB: OnceLock<Option<Db>> = OnceLock::new();

pub fn install(db: Option<Db>) {
    let _ = DB.set(db);
}

fn read_raw() -> Value {
    let Some(db) = DB.get().and_then(Option::as_ref) else {
        return json!({});
    };
    db.with(|conn| {
        conn.query_row("SELECT value FROM kv WHERE key = ?1", [KV_KEY], |row| {
            row.get::<_, String>(0)
        }).ok()
    })
    .flatten()
    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
    .filter(Value::is_object)
    .unwrap_or_else(|| json!({}))
}

fn write_raw(value: &Value) -> Result<(), String> {
    let Some(db) = DB.get().and_then(Option::as_ref) else {
        return Err("设置数据库不可用".to_string());
    };
    let text = serde_json::to_string(value).map_err(|error| format!("编码代理设置失败: {error}"))?;
    db.with(|conn| {
        conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![KV_KEY, text],
        )
        .map(|_| ())
        .map_err(|error| format!("保存代理设置失败: {error}"))
    })
    .ok_or_else(|| "设置数据库不可用".to_string())??;
    crate::server::core::cloud_sync::auto_sync::notify_change();
    Ok(())
}

pub fn test_url() -> Option<String> {
    let text = read_raw().get("testUrl").and_then(Value::as_str)
        .unwrap_or("").trim().to_string();
    (!text.is_empty()).then_some(text)
}

// ─── 轮询出口的并发预算（#34 出口准入）─────────────────────────

/// 轮询档的出口并发预算。
///
/// ── 为什么需要它 ────────────────────────────────────────────
/// 轮询的每条出口都可能是「按 IP 计额度」的免费档（OpenCode Zen 就是）。
/// 同一个 IP 上并发打十几个请求，上游会把它们一起判成超额 —— 于是「轮询
/// 换 IP」的收益被自己的并发吃掉了。这里按**出口 IP** 限并发，让同一 IP 上
/// 同时在飞的请求数保持在额度能容纳的量级。
///
/// ── 三个键 ──────────────────────────────────────────────────
///   · `rotateMaxConcurrentPerIp`（1..=32）：同一出口 IP 最多几个在飞；
///   · `rotateMaxQueueSize`（0..=1024）：排队上限，0 = 满载立即拒绝；
///   · `rotateQueueTimeoutSeconds`（1..=300）：排队等多久放弃。
///
/// 缺省值来自历史实现（1 / 64 / 30）：单条出口 IP 一次只放一个请求，其余排队。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RotateBalanceSettings {
    pub max_concurrent_per_ip: usize,
    pub max_queue_size: usize,
    pub queue_timeout_seconds: u64,
}

impl Default for RotateBalanceSettings {
    fn default() -> Self {
        Self {
            max_concurrent_per_ip: 1,
            max_queue_size: 64,
            queue_timeout_seconds: 30,
        }
    }
}

impl RotateBalanceSettings {
    /// 从设置原文里读三个键（缺失 / 非法一律回落默认，见模块头）。
    fn from_raw(raw: &Value) -> Self {
        let default = Self::default();
        let bounded = |key: &str, low: usize, high: usize, fallback: usize| -> usize {
            raw.get(key)
                .and_then(|value| {
                    value
                        .as_u64()
                        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
                })
                .and_then(|number| usize::try_from(number).ok())
                .filter(|number| (low..=high).contains(number))
                .unwrap_or(fallback)
        };
        Self {
            max_concurrent_per_ip: bounded(
                "rotateMaxConcurrentPerIp",
                1,
                32,
                default.max_concurrent_per_ip,
            ),
            max_queue_size: bounded("rotateMaxQueueSize", 0, 1024, default.max_queue_size),
            queue_timeout_seconds: bounded(
                "rotateQueueTimeoutSeconds",
                1,
                300,
                default.queue_timeout_seconds as usize,
            ) as u64,
        }
    }
}

/// 当前生效的轮询并发预算（每个请求读一次 —— 保存后下一个请求生效）。
pub fn rotate_balance() -> RotateBalanceSettings {
    RotateBalanceSettings::from_raw(&read_raw())
}

pub fn get() -> Value {
    let raw = read_raw();
    let balance = RotateBalanceSettings::from_raw(&raw);
    // 端口区间回**生效值**（非法 / 倒挂已回落默认）：界面上要能直接编辑
    // 「现在实际在用的那一段」，而不是一份可能已被改坏的原文
    let (base, end) = listener_port_range();
    let defaults = RotateBalanceSettings::default();
    json!({
        "listenerPortBase": base,
        "listenerPortEnd": end,
        "testUrl": raw.get("testUrl").and_then(Value::as_str).unwrap_or(""),
        // 轮询并发预算：读回的是**生效值**（非法/缺失已回落默认），
        // 界面上展示的就是下一个请求实际会用的那一组
        "rotateMaxConcurrentPerIp": balance.max_concurrent_per_ip,
        "rotateMaxQueueSize": balance.max_queue_size,
        "rotateQueueTimeoutSeconds": balance.queue_timeout_seconds,
        "defaults": {
            "listenerPortBase": DEFAULT_LISTENER_PORT_BASE,
            "listenerPortEnd": DEFAULT_LISTENER_PORT_END,
            "testUrl": DEFAULT_TEST_URL,
            "rotateMaxConcurrentPerIp": defaults.max_concurrent_per_ip,
            "rotateMaxQueueSize": defaults.max_queue_size,
            "rotateQueueTimeoutSeconds": defaults.queue_timeout_seconds,
        },
    })
}

/// 解析一个端口字面量：只收用户可用的端口（`>= 1024`，低于它是系统保留段），
/// 数字字符串也收（界面把输入框的值当文本传回）；小数、超大值、非数字一律 `None`。
///
/// 下界与 [`listener_port_range_of`] 取同一个数：写进去的必须是读得出来的，
/// 否则「保存成 80 → 生效值回落 19100」会让人以为保存失败。
fn parse_port(value: &Value) -> Option<u16> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        .and_then(|number| u16::try_from(number).ok())
        .filter(|port| *port >= 1024)
}

/// 把请求里的 `listenerPortBase` / `listenerPortEnd` 合并进 `next`。
///
/// ── 为什么两端都写进去之后才比较 ────────────────────────────
/// 先拿「新起点」和「旧终点」比，会在**同时改两端**时误报：把区间整体
/// 19100-19200 平移到 21001-21999 是合法的，但单独看「新起点 21001 > 旧终点
/// 19200」就会被拒。所以顺序是「先各自落进 next，再拿合并后的两端比较」。
///
/// 只提交一端时，另一端沿用 `next` 里的现值；现值为空则按默认值参与，
/// 与 [`listener_port_range`] 的回落口径一致 —— 否则「只改起始端口」会
/// 因为另一端缺失而被判成非法。
fn merge_listener_ports(next: &mut Value, input: &Value) -> Result<(), String> {
    let mut touched = false;
    for (key, field) in [("listenerPortBase", "起始端口"), ("listenerPortEnd", "结束端口")] {
        let Some(value) = input.get(key) else { continue };
        let port = parse_port(value).ok_or_else(|| format!("{field}必须是 1024-65535 的整数"))?;
        next[key] = json!(port);
        touched = true;
    }
    if !touched {
        return Ok(());
    }
    let base = next.get("listenerPortBase").and_then(parse_port)
        .unwrap_or(DEFAULT_LISTENER_PORT_BASE);
    let end = next.get("listenerPortEnd").and_then(parse_port)
        .unwrap_or(DEFAULT_LISTENER_PORT_END);
    if base > end {
        return Err("起始端口不能大于结束端口".to_string());
    }
    Ok(())
}

pub fn save(input: &Value) -> Result<Value, String> {
    if !input.is_object() {
        return Err("代理设置必须是对象".to_string());
    }
    let mut next = read_raw();
    // ── 节点监听端口区间 ────────────────────────────────────────
    // 校验与合并抽在 `merge_listener_ports` 里（纯函数，可单测）
    merge_listener_ports(&mut next, input)?;
    if let Some(value) = input.get("testUrl") {
        let text = value.as_str().ok_or_else(|| "测试网址必须是字符串".to_string())?.trim();
        if !text.is_empty() {
            let url = url::Url::parse(text).map_err(|_| "测试网址无效".to_string())?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                return Err("测试网址要以 http:// 或 https:// 开头".to_string());
            }
            if text.chars().count() > MAX_URL_LENGTH {
                return Err("测试网址过长".to_string());
            }
        }
        next["testUrl"] = json!(text);
    }
    // ── 轮询并发预算（#34）：校验通过才写 ──────────────────────
    // 逐键只认 JSON 整数（数字字符串也收，界面有时把它当文本传回来），
    // 范围外的值报错而不是静默夹逼 —— 静默夹逼会让用户以为「设成 100 生效了」。
    let mut bounded = |key: &str, field: &str, low: u64, high: u64| -> Result<(), String> {
        let Some(value) = input.get(key) else { return Ok(()) };
        let number = value
            .as_u64()
            .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
            .ok_or_else(|| format!("{field} 必须是整数"))?;
        if !(low..=high).contains(&number) {
            return Err(format!("{field} 必须在 {low}..={high} 之间"));
        }
        next[key] = json!(number);
        Ok(())
    };
    bounded("rotateMaxConcurrentPerIp", "同一出口 IP 的最大并发", 1, 32)?;
    bounded("rotateMaxQueueSize", "出口排队上限", 0, 1024)?;
    bounded("rotateQueueTimeoutSeconds", "出口排队超时（秒）", 1, 300)?;
    write_raw(&next)?;
    crate::server::logging::log("[Proxies]", "✅ 已更新网络代理设置");
    Ok(get())
}

/// 内核监听器的端口区间（lazy listener 分配用）。
///
/// ── 为什么要有区间而不是随机端口 ────────────────────────────
/// 内核给每个被引用的订阅节点开一个本地 http 监听器。端口如果每次重启都
/// 随机分配，已落进代理池的 `port` 会全部失效，账号引用跟着一起断 ——
/// 所以这一段的分配必须是**稳定区间内找第一个空闲端口**（见
/// `sub_kernel::runtime::alloc_port`）。
///
/// 取值来自配置里的 `listenerPortBase` / `listenerPortEnd`（用户可在设置页
/// 改，给需要避开本机其它服务的人留出口）。缺省区间与历史版本一致。
pub fn listener_port_range() -> (u16, u16) {
    listener_port_range_of(&read_raw())
}

/// [`listener_port_range`] 的纯函数形态（同一份口径，便于单测）。
///
/// 只认 1024..=65535：低于 1024 是系统保留端口，让内核去占它只会失败 ——
/// 落在这个区间之外的存量按「没配过」处理，回落到默认段。
/// 区间倒挂（end ≤ base）同样回落默认，而不是报错：内核照常能起，
/// 用户界面上的表现是「回到默认范围」。
fn listener_port_range_of(raw: &Value) -> (u16, u16) {
    let bounded = |value: Option<&Value>, fallback: u16| -> u16 {
        value
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|t| t.trim().parse().ok())))
            .and_then(|n| u16::try_from(n).ok())
            .filter(|p| (1024..=65535).contains(p))
            .unwrap_or(fallback)
    };
    let base = bounded(raw.get("listenerPortBase"), DEFAULT_LISTENER_PORT_BASE);
    let end = bounded(raw.get("listenerPortEnd"), DEFAULT_LISTENER_PORT_END);
    if end <= base {
        return (DEFAULT_LISTENER_PORT_BASE, DEFAULT_LISTENER_PORT_END);
    }
    (base, end)
}

#[cfg(test)]
mod listener_port_tests {
    use super::*;

    fn merged(raw: Value, input: Value) -> Result<Value, String> {
        let mut next = raw;
        merge_listener_ports(&mut next, &input)?;
        Ok(next)
    }

    /// 同时改两端是**合法**的整段平移：先拿新起点比旧终点会误报，这条用例
    /// 就是那个陷阱的回归。
    #[test]
    fn shifting_both_ends_at_once_is_allowed() {
        let raw = json!({ "listenerPortBase": 19100, "listenerPortEnd": 19200 });
        let result = merged(raw, json!({ "listenerPortBase": 21001, "listenerPortEnd": 21999 }))
            .expect("整段平移应当通过");
        assert_eq!(result["listenerPortBase"], json!(21001));
        assert_eq!(result["listenerPortEnd"], json!(21999));
    }

    /// 只提交一端时，另一端沿用现值 —— 不能因为「另一端没出现在本次请求里」
    /// 就被当成缺失。
    #[test]
    fn single_end_update_keeps_the_other_end() {
        let raw = json!({ "listenerPortBase": 20000, "listenerPortEnd": 20100 });
        let result = merged(raw, json!({ "listenerPortEnd": 20200 })).expect("只改终点应当通过");
        assert_eq!(result["listenerPortBase"], json!(20000));
        assert_eq!(result["listenerPortEnd"], json!(20200));
    }

    /// 只改起始到超过现终点：必须拒绝（否则会配出倒挂区间）。
    #[test]
    fn single_end_overflow_is_rejected() {
        let raw = json!({ "listenerPortBase": 20000, "listenerPortEnd": 20100 });
        let error = merged(raw, json!({ "listenerPortBase": 20200 })).unwrap_err();
        assert!(error.contains("起始端口不能大于结束端口"), "{error}");
    }

    /// 两端都非法时逐字拒绝：0 / 65536 / 小数 / 非数字都不许静默夹逼。
    #[test]
    fn out_of_range_and_non_integer_ports_are_rejected() {
        for input in [
            json!({ "listenerPortBase": 0 }),
            json!({ "listenerPortBase": -1 }),
            // 低于 1024 是系统保留段：与生效值读取的下界同一个数
            json!({ "listenerPortBase": 80 }),
            json!({ "listenerPortEnd": 65536 }),
            json!({ "listenerPortBase": 1.5 }),
            json!({ "listenerPortBase": "abc" }),
        ] {
            assert!(
                merged(json!({}), input.clone()).is_err(),
                "{input} 应当被拒绝"
            );
        }
    }

    /// 数字字符串照收（界面把输入框的值当文本传回）。
    #[test]
    fn numeric_strings_are_accepted() {
        let result = merged(json!({}), json!({
            "listenerPortBase": " 21001 ",
            "listenerPortEnd": "21999",
        }))
        .expect("数字字符串应当通过");
        assert_eq!(result["listenerPortBase"], json!(21001));
        assert_eq!(result["listenerPortEnd"], json!(21999));
    }

    /// 没带端口字段时什么都不改（部分更新的其余字段照常走）。
    #[test]
    fn absent_fields_are_left_untouched() {
        let raw = json!({ "listenerPortBase": 19100, "listenerPortEnd": 19200 });
        let result = merged(raw.clone(), json!({ "testUrl": "https://x.test/" })).expect("应当通过");
        assert_eq!(result, raw);
    }

    /// 生效值读取：非法 / 倒挂一律回落默认（界面拿到的是能直接编辑的一段）。
    #[test]
    fn effective_range_falls_back_to_defaults() {
        assert_eq!(listener_port_range_of(&json!({})), (DEFAULT_LISTENER_PORT_BASE, DEFAULT_LISTENER_PORT_END));
        assert_eq!(
            listener_port_range_of(&json!({ "listenerPortBase": 21001, "listenerPortEnd": 21999 })),
            (21001, 21999)
        );
        // 倒挂
        assert_eq!(
            listener_port_range_of(&json!({ "listenerPortBase": 21999, "listenerPortEnd": 21001 })),
            (DEFAULT_LISTENER_PORT_BASE, DEFAULT_LISTENER_PORT_END)
        );
        // 越界
        assert_eq!(
            listener_port_range_of(&json!({ "listenerPortBase": 0, "listenerPortEnd": 70000 })),
            (DEFAULT_LISTENER_PORT_BASE, DEFAULT_LISTENER_PORT_END)
        );
    }
}
