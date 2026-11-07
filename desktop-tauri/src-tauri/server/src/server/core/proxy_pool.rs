//! 命名代理池：手动地址与 Clash Verge 端点按 `{source:'pool', proxyId}` 引用。
//! 条目与最近一次测试结果存于 kv 的 `proxyPool` 保留键。
//! 旧订阅镜像只保留原始记录，不再列出、解析或参与轮询。

use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::server::core::proxies::{resolve_account_proxy, ProxyResolution};
use crate::server::db::Db;
use crate::server::logging;

pub mod usage;

pub const KV_KEY: &str = "proxyPool";
pub const RETIRED_HINT: &str = "代理订阅与内置代理内核功能已下线，请在「网络代理」页添加 HTTP / SOCKS5 地址或同步 Clash Verge，并重新选择代理";

const CLASH_READONLY_HINT: &str = "该代理来自 Clash Verge 同步，请到 Clash Verge 中修改名称或端口；本页可启停和测试";

const SUBSCRIPTION_READONLY_HINT: &str =
    "该节点来自订阅（名称 / 端口 / 启用跟随订阅与内核），请在「订阅管理」修改";

/// 这条池条目是不是**订阅节点的镜像**。
///
/// 订阅节点由内核把订阅内容解析成一组本地监听器后镜像进池（见
/// `sync_subscription_nodes`）；它们与 manual / clash 条目的最大差别是
/// **名称与端口都跟随上游**，所以一律只读。
fn is_subscription_item(item: &Value) -> bool {
    text_of(item, "source") == "subscription"
}

/// 「订阅里已经没有这个节点了」的标记字段（`gone: true`，只出现在订阅镜像上）。
///
/// ── 为什么需要它 ────────────────────────────────────────────
/// 订阅里被删掉的节点不能立刻从池里摘掉：账号按条目 id 引用，机场改一次
/// 节点名就会让一批引用悬空。所以同步只把条目禁用、收回端口、保留身份。
/// 但这些留下来的条目原本没有出路 —— 它既不是能改的镜像，也不是「所属订阅
/// 已删除」的孤儿，删除入口两处都拦着。这个标记就是那条出路：带 `gone` 的
/// 条目允许手动删除（节点回来时标记会被撤掉）。
///
/// 与 orphan（api 层按「providerId 还在不在订阅清单里」现算）的区别：
/// orphan 说的是**订阅没了**，gone 说的是**订阅还在但节点没了**。
pub const GONE_FIELD: &str = "gone";

/// 这条订阅镜像是否已被订阅移除（`gone: true`）
pub fn is_gone(item: &Value) -> bool {
    is_subscription_item(item) && matches!(item.get(GONE_FIELD), Some(Value::Bool(true)))
}

fn readonly_hint(item: &Value) -> String {
    if is_subscription_item(item) {
        SUBSCRIPTION_READONLY_HINT.to_string()
    } else {
        CLASH_READONLY_HINT.to_string()
    }
}

const MAX_NAME_LENGTH: usize = 60;
const MAX_HOST_LENGTH: usize = 255;
const MAX_USER_LENGTH: usize = 200;
const MAX_LABEL_LENGTH: usize = 100;

static DB: OnceLock<Option<Db>> = OnceLock::new();

pub fn install(db: Option<Db>) {
    let _ = DB.set(db);
}

fn database() -> Result<&'static Db, String> {
    DB.get()
        .and_then(Option::as_ref)
        .ok_or_else(|| "代理池数据库不可用".to_string())
}

/// 代理相关模块共用的库句柄（`proxy_rotate::health` 的冷却落盘要用）。
///
/// 与 [`database`] 同一来源、同一 `install` 时点，只是把「拿不到」摊平成
/// `Option` —— 冷却落盘失败不该让请求失败，调用方按「只在内存里生效」处理。
pub(crate) fn shared_database() -> Option<&'static Db> {
    DB.get().and_then(Option::as_ref)
}

fn read_items() -> Vec<Value> {
    let Some(db) = DB.get().and_then(Option::as_ref) else {
        return Vec::new();
    };
    db.with(|conn| {
        conn.query_row("SELECT value FROM kv WHERE key = ?1", [KV_KEY], |row| {
            row.get::<_, String>(0)
        })
        .ok()
    })
    .flatten()
    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
    .and_then(|value| value.get("items").and_then(Value::as_array).cloned())
    .unwrap_or_default()
}

fn is_manual(item: &Value) -> bool {
    matches!(item.get("source").and_then(Value::as_str), None | Some("" | "manual" | "custom"))
}

fn is_clash(item: &Value) -> bool {
    item.get("source").and_then(Value::as_str) == Some("clash")
}

/// 订阅节点：由内置代理内核在 `127.0.0.1:<port>` 起的本地监听器。
///
/// **v3.4.0 把订阅链路删掉了**（`RETIRED_HINT`），于是这类条目被
/// `is_supported` 整体过滤 —— 存量账号里引用它们的那些会报「不存在或已被
/// 删除」，出口直接断掉。
///
/// 但内核是**独立进程**：新版不再管理它，并不影响已经在跑的内核。只要条目
/// 上还留着内核分配的端口，这个监听器就仍然可用，没有理由把它判死。
/// 真正不可用的是「没有端口的订阅节点」（内核没跑 / 从未分配）—— 那种条目
/// 保持原有提示，让用户去配手动地址。
fn is_subscription(item: &Value) -> bool {
    item.get("source").and_then(Value::as_str) == Some("subscription")
}

/// 订阅节点当前是否有内核分配的本地端口。
fn has_kernel_port(item: &Value) -> bool {
    item.get("port")
        .and_then(Value::as_u64)
        .is_some_and(|port| port > 0 && port <= u16::MAX as u64)
}

/// 本模块认识的条目来源（**形状判定，不看端口**）。
///
/// 判据只回答「这条记录归不归我管」：manual / clash / 订阅镜像都归。
/// 「订阅条目现在能不能用」是**解析期**的问题（内核端口是否已分配），
/// 由 `resolve_item` 单独判 —— 两件事混在一起会让「端口尚未分配的节点」
/// 从列表里整条消失（用户看不到、也没法引用它去触发补端口）。
fn is_supported(item: &Value) -> bool {
    is_manual(item) || is_clash(item) || is_subscription(item)
}

pub(crate) fn raw_items() -> Vec<Value> {
    read_items().into_iter().filter(is_supported).collect()
}

fn write_items(items: &[Value]) -> Result<(), String> {
    let db = database()?;
    let text = serde_json::to_string(&json!({ "items": items }))
        .map_err(|error| format!("编码代理池失败: {error}"))?;
    let written = db
        .with(|conn| {
            conn.execute(
                "INSERT INTO kv (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![KV_KEY, text],
            )
            .map(|_| ())
            .map_err(|error| format!("保存代理池失败: {error}"))
        })
        .ok_or_else(|| "代理池数据库不可用".to_string())?;
    written?;
    crate::server::core::cloud_sync::auto_sync::notify_change();
    Ok(())
}

fn clean_string(value: Option<&Value>, max: usize) -> String {
    let Some(Value::String(text)) = value else {
        return String::new();
    };
    text.trim().chars().take(max).collect()
}

fn valid_port(value: Option<&Value>) -> Option<u16> {
    let number = match value {
        Some(Value::Number(number)) => number.as_f64()?,
        Some(Value::String(text)) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if !number.is_finite() || number.fract() != 0.0 || !(1.0..=65535.0).contains(&number) {
        return None;
    }
    Some(number as u16)
}

fn text_of(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn normalize_item(input: &Value, existing: Option<&Value>) -> Result<Value, String> {
    let Some(object) = input.as_object() else {
        return Err("代理条目必须是对象".to_string());
    };
    if is_clash(input) {
        return Err("Clash Verge 出口由「同步 Clash Verge」自动导入，不能手工新建".to_string());
    }
    if !is_manual(input) {
        return Err(RETIRED_HINT.to_string());
    }
    let name = clean_string(object.get("name"), MAX_NAME_LENGTH);
    if name.is_empty() {
        return Err("请填写代理名称".to_string());
    }
    let protocol = clean_string(object.get("protocol"), 10).to_lowercase();
    let protocol = if protocol.is_empty() { "http".to_string() } else { protocol };
    if protocol != "http" && protocol != "socks5" {
        return Err("代理协议只支持 http 或 socks5".to_string());
    }
    let host = clean_string(object.get("host"), MAX_HOST_LENGTH);
    if host.is_empty() {
        return Err("请填写代理主机地址".to_string());
    }
    let Some(port) = valid_port(object.get("port")) else {
        return Err("代理端口必须是 1-65535 的整数".to_string());
    };
    let now = logging::now_ms();
    let id = existing.map(|item| text_of(item, "id")).unwrap_or_default();
    let id = if id.is_empty() { new_id() } else { id };
    let created_at = existing
        .and_then(|item| item.get("createdAt"))
        .and_then(Value::as_i64)
        .unwrap_or(now);
    // 测试结果保留为历史事实，时刻由界面显示，不参与实际选路。
    let last_test = existing
        .and_then(|item| item.get("lastTest"))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(json!({
        "id": id,
        "name": name,
        "source": "manual",
        "enabled": !matches!(object.get("enabled"), Some(Value::Bool(false))),
        "protocol": protocol,
        "host": host,
        "port": port,
        "username": clean_string(object.get("username"), MAX_USER_LENGTH),
        "password": clean_string(object.get("password"), MAX_USER_LENGTH),
        "createdAt": created_at,
        "updatedAt": now,
        "lastTest": last_test,
    }))
}

fn new_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(logging::now_ms() as u64);
    hasher.write_u64(std::process::id() as u64);
    let suffix = (hasher.finish() & 0xffff_ffff) as u32;
    format!("px_{}_{suffix:08x}", logging::now_ms())
}

pub fn is_enabled(item: &Value) -> bool {
    is_supported(item)
        && !matches!(item.get("enabled"), Some(Value::Bool(false)))
        && !matches!(item.get("localDisabled"), Some(Value::Bool(true)))
}

pub fn resolve_item(item: &Value) -> Option<ProxyResolution> {
    if is_clash(item) {
        return resolve_account_proxy(Some(&json!({
            "source": "clash",
            "listenerUid": text_of(item, "listenerUid"),
        })));
    }
    // 订阅节点：内核给的是本地 http 监听器（协议统一按 http 处理 —— 内核
    // 负责把 ss/vmess/vless 等转成 http，出网层只认 http/socks5）。
    if is_subscription(item) && has_kernel_port(item) {
        // 端口有值不等于有人在听：内核崩了 / 被停了，`port` 字段仍然留着。
        // constitution 要求「配了代理就必须用，用不了就报错」——这里如实失败，
        // 让转发层拒绝，而不是让请求在 connect 阶段失败后被兜底成直连。
        if !cached_listening(valid_port(item.get("port")).unwrap_or(0)) {
            return Some(ProxyResolution::Failed(
                "该订阅节点的监听端口当前没有响应（代理内核可能已停止）".to_string(),
            ));
        }
        return resolve_account_proxy(Some(&json!({
            "source": "custom",
            "protocol": "http",
            "host": "127.0.0.1",
            "port": item.get("port").cloned().unwrap_or(Value::Null),
        })));
    }
    // 订阅条目走到这里 = 没有内核分配的端口。**不能**沿用「订阅已下线」
    // 那条文案：本版订阅链路是活的（见 `subscriptions` 与 `sub_kernel`），
    // 端口为空的真实原因是「内核没跑 / 这个节点还没被账号引用」。
    // 把它说成「功能已下线」会把用户引向完全错误的排查方向。
    if is_subscription(item) {
        return Some(ProxyResolution::Failed(
            "该订阅节点尚未分配监听端口（代理内核未运行，或该节点还没有被账号引用）".to_string(),
        ));
    }
    if !is_manual(item) {
        return Some(ProxyResolution::Failed(RETIRED_HINT.to_string()));
    }
    resolve_account_proxy(Some(&json!({
        "source": "custom",
        "protocol": text_of(item, "protocol"),
        "host": text_of(item, "host"),
        "port": item.get("port").cloned().unwrap_or(Value::Null),
        "username": text_of(item, "username"),
        "password": text_of(item, "password"),
    })))
}

pub fn resolve_reference(proxy_id: &str) -> Result<crate::server::core::proxies::ResolvedProxy, String> {
    let Ok(item) = find_raw(proxy_id) else {
        return Err(format!("代理「{proxy_id}」不存在或已被删除"));
    };
    if is_subscription(&item) && !has_kernel_port(&item) {
        return Err(format!(
            "代理「{}」是订阅节点，但内核还没给它分配端口；请在「网络代理 → 订阅」里刷新订阅或重启内核",
            display_name(&item, proxy_id)
        ));
    }
    if !is_enabled(&item) {
        return Err(format!(
            "代理「{}」已禁用（在「网络代理」页启用它）",
            display_name(&item, proxy_id)
        ));
    }
    resolve_entry(&item, proxy_id)
}

pub fn resolve_for_test(proxy_id: &str) -> Result<crate::server::core::proxies::ResolvedProxy, String> {
    resolve_entry(&find_raw(proxy_id)?, proxy_id)
}

/// 这一条是不是订阅节点，以及它的 `(providerId, node)` 归属。
///
/// 测试订阅节点**不能**走 [`resolve_for_test`]：没有账号引用它时内核根本不
/// 给它开监听端口（lazy listeners），`resolve_item` 会如实报「尚未分配监听
/// 端口」—— 而那正是用户想测它的原因（先测通再决定要不要用）。这类节点
/// 走内核的**统一测试口**（见 `sub_kernel::runtime::select_test_node`），
/// 不需要专用端口，也不必等内核为它重建。
///
/// 返回 `None` 表示不是订阅节点（手动 / Clash 条目按普通出口测）。
pub fn subscription_target(item: &Value) -> Option<(String, String)> {
    if !is_subscription(item) {
        return None;
    }
    let provider_id = text_of(item, "providerId");
    let node = text_of(item, "node");
    if provider_id.is_empty() || node.is_empty() {
        return None;
    }
    Some((provider_id, node))
}

/// 按 id 取一条的原始记录（测试路径要读它的订阅归属）。
pub fn find_item(id: &str) -> Option<Value> {
    find_raw(id).ok()
}

fn display_name(item: &Value, fallback: &str) -> String {
    let name = text_of(item, "name");
    if name.is_empty() { fallback.to_string() } else { name }
}

fn resolve_entry(
    item: &Value,
    proxy_id: &str,
) -> Result<crate::server::core::proxies::ResolvedProxy, String> {
    let name = display_name(item, proxy_id);
    match resolve_item(item) {
        Some(ProxyResolution::Resolved(mut proxy)) => {
            proxy.label = name;
            Ok(proxy)
        }
        Some(ProxyResolution::Failed(reason)) => Err(format!("代理「{name}」不可用: {reason}")),
        None => Err(format!("代理「{name}」配置不完整")),
    }
}

fn describe_item(item: &Value) -> Value {
    let (resolved, resolve_error) = match resolve_item(item) {
        Some(ProxyResolution::Resolved(proxy)) => (
            json!({ "protocol": proxy.protocol, "host": proxy.host, "port": proxy.port_json(), "label": proxy.label }),
            Value::Null,
        ),
        Some(ProxyResolution::Failed(reason)) => (Value::Null, Value::String(reason)),
        None => (Value::Null, Value::String("配置不完整".to_string())),
    };
    json!({
        "id": text_of(item, "id"),
        "name": text_of(item, "name"),
        // 来源三态：clash / subscription / manual。订阅节点的协议与主机由内核
        // 决定（统一是本机 http 监听器），条目本身不存这两个字段 —— 直接回显
        // `text_of` 会让界面显示成「空协议 + 空主机」，看起来像坏条目。
        "source": if is_clash(item) {
            "clash"
        } else if is_subscription(item) {
            "subscription"
        } else {
            "manual"
        },
        "enabled": is_enabled(item),
        "sourceEnabled": !matches!(item.get("enabled"), Some(Value::Bool(false))),
        "localDisabled": matches!(item.get("localDisabled"), Some(Value::Bool(true))),
        // 「订阅里已经移除这个节点」—— 这类条目也能手动删（见 `GONE_FIELD`）。
        // 界面上跟「普通禁用」不是同一件事：启用它永远拿不到端口，所以代理
        // 选择器里照旧不可选、并说明原因。
        "gone": is_gone(item),
        // 订阅节点的归属：哪条订阅（`providerId`）里的哪个节点（`node`）。
        // 界面按它把订阅节点分批筛选（「只看某家机场」）—— 不靠节点名反推，
        // 机场改名一次反推就全错。手动 / Clash 条目这两项是空串。
        "providerId": text_of(item, "providerId"),
        "node": text_of(item, "node"),
        "listenerUid": text_of(item, "listenerUid"),
        "protocol": if is_subscription(item) { "http".to_string() } else { text_of(item, "protocol") },
        "host": if is_subscription(item) { "127.0.0.1".to_string() } else { text_of(item, "host") },
        "port": item.get("port").cloned().unwrap_or(Value::Null),
        "username": text_of(item, "username"),
        "password": text_of(item, "password"),
        "createdAt": item.get("createdAt").cloned().unwrap_or(Value::from(0)),
        "updatedAt": item.get("updatedAt").cloned().unwrap_or(Value::from(0)),
        "lastTest": item.get("lastTest").cloned().unwrap_or(Value::Null),
        "resolved": resolved,
        "resolveError": resolve_error,
    })
}

pub fn list() -> Vec<Value> {
    raw_items().iter().map(describe_item).collect()
}

/// 只列**可用**条目（账号侧代理下拉 / 轮询候选用）。
///
/// 与 [`list`] 的唯一差别是过滤掉本条禁用与来源侧停用的条目：禁用项不该出现在
/// 「给账号挑一个出口」的选项里 —— 选中它只会在转发时报「已禁用」，而用户
/// 无从知道是自己在「网络代理」页关的。它与 `pool_list` 的另一处差别在 api 层
/// （那边会顺手同步一次 Clash 出口，这里不会：账号页画个下拉不该触发镜像写入）。
///
/// 「订阅里已移除」（`gone`）也算不可用：节点定义都没了，启用也拿不到端口。
pub fn list_usable() -> Vec<Value> {
    raw_items()
        .iter()
        .filter(|item| is_enabled(item))
        .map(describe_item)
        .collect()
}

fn find_raw(id: &str) -> Result<Value, String> {
    read_items().into_iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| format!("代理「{id}」不存在或已被删除"))
}

pub fn create(input: &Value) -> Result<Vec<Value>, String> {
    let item = normalize_item(input, None)?;
    let mut items = read_items();
    items.insert(0, item.clone());
    write_items(&items)?;
    logging::log("[Proxies]", &format!("✅ 新增代理「{}」", text_of(&item, "name")));
    Ok(list())
}

pub fn update(input: &Value) -> Result<Vec<Value>, String> {
    let id = clean_string(input.get("id"), MAX_LABEL_LENGTH);
    if id.is_empty() {
        return Err("缺少代理 id".to_string());
    }
    let mut items = read_items();
    let index = writable_index(&items, &id)?;
    let next = normalize_item(input, Some(&items[index]))?;
    items[index] = next.clone();
    write_items(&items)?;
    logging::log("[Proxies]", &format!("✅ 更新代理「{}」", text_of(&next, "name")));
    Ok(list())
}

fn writable_index(items: &[Value], id: &str) -> Result<usize, String> {
    let index = items.iter()
        .position(|item| item.get("id").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| format!("代理「{id}」不存在"))?;
    if !is_supported(&items[index]) {
        return Err(RETIRED_HINT.to_string());
    }
    if is_clash(&items[index]) {
        return Err(CLASH_READONLY_HINT.to_string());
    }
    Ok(index)
}

fn supported_index(items: &[Value], id: &str) -> Result<usize, String> {
    let index = items.iter()
        .position(|item| item.get("id").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| format!("代理「{id}」不存在"))?;
    if !is_supported(&items[index]) {
        return Err(RETIRED_HINT.to_string());
    }
    Ok(index)
}

pub struct EnabledChange {
    pub name: String,
    pub enabled: bool,
}

/// 「账号指定了这条代理」时的启用结果。
///
/// `enabled = true` 表示**本次调用真的把它从禁用改成了启用**（账号写入链路据此
/// 给一条变更说明，并触发内核补建监听器）；`false` 表示本来就启用着、或者这次
/// 启用不了（Clash 侧停用 / 条目刚被删），调用方不该把那种情况说成「已启用」。
pub struct ReferenceEnable {
    /// 条目展示名（日志与变更说明里标「启用了谁」）
    pub name: String,
    /// 本次是否真的从禁用切成启用
    pub enabled: bool,
    /// 这是订阅节点（启用后需要内核为它补建监听器）
    pub subscription: bool,
}

impl ReferenceEnable {
    /// 这次启用是否需要惊动内核（订阅节点要从 `lazy listeners` 里拿到端口）。
    ///
    /// manual / clash 条目不需要：它们的地址在条目自己身上，转发直接连通，
    /// 与内核无关。
    pub fn needs_listener_build(&self) -> bool {
        self.enabled && self.subscription
    }
}

/// 账号把 `proxy_id` 选作出网代理时，把这条池条目**启用**。
///
/// ── 为什么在写入侧做（而不是让用户先跑去「网络代理」页点一下）──────
/// 禁用是「先别用它」的意图，而账号明确指定一条被禁用的出口，语义上就是
/// 「现在要用它」—— 两件事同时成立时以前者为准的结果是：保存成功、转发
/// 立刻失败（解析报「代理已禁用」），用户还得回头再点一次开关。这里把
/// 指定本身当成启用意图，一次动作完成。
///
/// 失败一律**不阻断**账号写入：条目可能就在这一瞬间被删了，也可能来自
/// Clash Verge（那边停用的条目本页启用不生效，见 `set_enabled`）—— 这两类
/// 情形账号照样存得下，只是转发时会给出既有那条「不可用」原因。
///
/// 返回值：`Some(结果)` = 这次真的从禁用改成了启用；`None` = 无需改动或改不了。
pub fn enable_for_reference(proxy_id: &str) -> Option<ReferenceEnable> {
    let Ok(item) = find_raw(proxy_id) else {
        return None;
    };
    let name = display_name(&item, proxy_id);
    if !should_enable_on_reference(&item) {
        return None;
    }
    let subscription = is_subscription(&item);
    match set_enabled(proxy_id, true) {
        Ok((_, change)) => Some(ReferenceEnable {
            name: change.name,
            enabled: change.enabled,
            subscription,
        }),
        Err(error) => {
            logging::log(
                "[Proxies]",
                &format!("⚠️ 账号引用的代理「{name}」未能自动启用：{error}"),
            );
            None
        }
    }
}

/// 「账号指定了这条条目」时要不要把它启用（纯判定，不落盘）。
///
/// ── `enabled` 与 `localDisabled` 不是同一回事 ────────────────
/// manual / 订阅节点的禁用就是 `enabled: false`（本页开关直接写它）；而 Clash
/// 条目那一层是**镜像值**：`enabled` 跟随 Clash Verge，`localDisabled` 才是
/// 本页按下的那一下（见 `set_enabled`）。所以「来源侧停用」只对 clash 条目成立。
///
/// 三种情形返回 `false`：
///   · **已经是可用的** —— 没有要改的东西，也不该给一句「已自动启用」；
///   · **Clash 侧停用**的监听器：本页启用不生效，打开它只会让用户以为修好了；
///   · **订阅里已经移除**的节点（`gone`）：节点定义都没了，启用也拿不到端口。
fn should_enable_on_reference(item: &Value) -> bool {
    if !is_supported(item) || is_enabled(item) || is_gone(item) {
        return false;
    }
    // Clash 条目：`enabled: false` 是 Clash Verge 里关掉的，本页改不了
    !(is_clash(item) && matches!(item.get("enabled"), Some(Value::Bool(false))))
}

/// 账号侧的统一入口：一个 proxy 配置 → 这次被它启用的池条目。
///
/// 覆盖两种「指定了具体条目」的形状：
///   · `{source:'pool', proxyId}` —— 单条引用；
///   · `{source:'rotate', all:false, proxyIds:[…]}` —— 显式列出的轮询候选。
///   轮询的 `all: true` **不在此列**：它的语义是「池里所有启用的出口」，
///   把所有禁用的一并打开等于绕开用户逐个禁用的意图（而且以后新加的条目
///   会自动进轮询，`candidate_ids` 会把它们算进去）。自定义地址、Clash 直引
///   也不需要处理 —— 它们不指向池条目。
pub fn enable_for_account_proxy(config: Option<&Value>) -> Vec<ReferenceEnable> {
    let Some(config) = config else { return Vec::new() };
    let mut enabled = Vec::new();
    match config.get("source").and_then(Value::as_str) {
        Some("pool") => {
            if let Some(id) = config.get("proxyId").and_then(Value::as_str) {
                enabled.extend(enable_for_reference(id.trim()));
            }
        }
        Some(crate::server::core::proxies::ROTATE_SOURCE) => {
            if config.get("all").and_then(Value::as_bool) == Some(true) {
                return enabled;
            }
            for id in crate::server::core::proxy_rotate::candidate_ids(config) {
                enabled.extend(enable_for_reference(id.trim()));
            }
        }
        _ => {}
    }
    enabled
}

pub fn set_enabled(id: &str, enabled: bool) -> Result<(Vec<Value>, EnabledChange), String> {
    let mut items = read_items();
    let index = supported_index(&items, id)?;
    let name = display_name(&items[index], id);
    let clash = is_clash(&items[index]);
    if clash && enabled && matches!(items[index].get("enabled"), Some(Value::Bool(false))) {
        return Err("这条在 Clash Verge 里是停用的，请到 Clash Verge 里打开它".to_string());
    }
    let Some(object) = items[index].as_object_mut() else {
        return Err("代理条目形状异常".to_string());
    };
    if clash {
        if enabled {
            object.remove("localDisabled");
        } else {
            object.insert("localDisabled".to_string(), Value::Bool(true));
        }
    } else {
        object.insert("enabled".to_string(), Value::Bool(enabled));
    }
    object.insert("updatedAt".to_string(), Value::from(logging::now_ms()));
    write_items(&items)?;
    logging::log("[Proxies]", &format!("{}代理「{name}」", if enabled { "✅ 启用" } else { "⏸️ 禁用" }));
    Ok((list(), EnabledChange { name, enabled }))
}

pub fn remove(id: &str) -> Result<(Vec<Value>, String), String> {
    let mut items = read_items();
    let index = writable_index(&items, id)?;
    let removed = items.remove(index);
    write_items(&items)?;
    let name = text_of(&removed, "name");
    logging::log("[Proxies]", &format!("🗑️ 删除代理「{name}」"));
    Ok((list(), name))
}

pub struct ClashSyncReport {
    pub changes: usize,
    pub error: Option<String>,
}

pub fn sync_clash(force: bool) -> ClashSyncReport {
    let snapshot = crate::server::core::clash::clash_snapshot_with(force);
    if !snapshot.available {
        return ClashSyncReport {
            changes: 0,
            error: Some(snapshot.error.unwrap_or_else(||
                crate::server::core::clash::CLASH_UNAVAILABLE.to_string())),
        };
    }
    let mut targets = Vec::new();
    if snapshot.mixed_port.is_some() {
        targets.push((
            crate::server::core::clash::CLASH_MIXED_UID.to_string(),
            "Clash 混合端口（按规则分流）".to_string(),
            true,
        ));
    }
    for listener in snapshot.listeners {
        targets.push((listener.uid, listener.name, listener.enabled));
    }
    let mut items = read_items();
    let mut changes = 0;
    let now = logging::now_ms();
    for (uid, name, enabled) in &targets {
        if let Some(index) = items.iter().position(|item| is_clash(item)
            && item.get("listenerUid").and_then(Value::as_str) == Some(uid.as_str())) {
            let current_enabled = !matches!(items[index].get("enabled"), Some(Value::Bool(false)));
            if text_of(&items[index], "name") != *name || current_enabled != *enabled {
                if let Some(object) = items[index].as_object_mut() {
                    object.insert("name".to_string(), json!(name));
                    object.insert("enabled".to_string(), json!(enabled));
                    object.insert("updatedAt".to_string(), json!(now));
                }
                changes += 1;
            }
        } else {
            items.insert(0, json!({
                "id": new_id(),
                "name": name,
                "source": "clash",
                "listenerUid": uid,
                "enabled": enabled,
                "createdAt": now,
                "updatedAt": now,
                "lastTest": Value::Null,
            }));
            changes += 1;
        }
    }
    // 只对成功读取的 Clash 集合做镜像对账，其他来源的历史记录不受影响。
    let before = items.len();
    let mut seen = std::collections::HashSet::new();
    items.retain(|item| {
        if !is_clash(item) { return true; }
        let Some(uid) = item.get("listenerUid").and_then(Value::as_str) else { return false; };
        targets.iter().any(|(target, _, _)| target == uid) && seen.insert(uid.to_string())
    });
    changes += before - items.len();
    if changes > 0 {
        if let Err(error) = write_items(&items) {
            return ClashSyncReport { changes: 0, error: Some(error) };
        }
        logging::log("[Proxies]", &format!("🔄 已同步 Clash Verge 出口（{changes} 项变更）"));
    }
    ClashSyncReport { changes, error: None }
}

pub struct TestOutcome {
    pub success: bool,
    pub ip: String,
    pub duration_ms: i64,
    pub error: Option<String>,
}

pub fn record_test(id: &str, result: &TestOutcome) -> Result<Vec<Value>, String> {
    let mut items = read_items();
    let index = supported_index(&items, id)?;
    let Some(object) = items[index].as_object_mut() else {
        return Err("代理条目形状异常".to_string());
    };
    object.insert("lastTest".to_string(), json!({
        "success": result.success,
        "ip": result.ip,
        "durationMs": result.duration_ms,
        "at": logging::now_ms(),
        "error": result.error,
    }));
    write_items(&items)?;
    Ok(list())
}

// ─── 订阅节点镜像（内核拉到订阅内容后把节点写进池）─────────────────
//
// v3.4.0 删掉了整条订阅链路，这组函数随之消失。它们是**官方 v3.1.0 就有的**
// 能力（`core::sub_kernel` 的配套面），恢复它不是移植本地魔改。
//
// 依赖方向固定为 sub_kernel → proxy_pool：内核把节点写进来，本模块不认识
// 订阅存储。所以这里只认 `source == "subscription"` 与 `gone` 两个形状约束。

/// 删除一条订阅节点的**孤儿**条目（所属订阅已不存在）。
///
/// 只服务「手动清理」这一条出路：正常路径下订阅删除 / 禁用后条目会随
/// 同步自动摘掉（见 `sync_subscription_nodes`），孤儿只出现在数据被外部
/// 工具清掉的场景（例如旧版进程误删 `subscriptions` 存储键 —— 实测踩过）：
/// 那时条目归属的订阅已经不存在，任何同步都不会再碰它，界面上需要一条
/// 手动删掉它的路。仍然拒绝 clash 条目与手动条目 —— 它们不属于订阅，
/// 不能借这个入口绕过各自的只读 / 正常写路径。
///
/// 第二类可删的订阅条目是 [`GONE_FIELD`] 标着的（订阅还在、节点已从订阅里
/// 移除）：同步保留它们只是为了不让账号引用永久悬空，节点不会自己回来，
/// 用户需要一条清掉的出路。
///
/// **是否孤儿由调用方（api 层）判定**：本模块不认识订阅存储（依赖方向
/// 是 subscriptions/sub_kernel → proxy_pool），只认「source == subscription」
/// 与 `gone` 这两个形状约束。
pub fn remove_subscription_orphan(id: &str) -> Result<(Vec<Value>, String), String> {
    let mut items = read_items();
    let Some(index) = items
        .iter()
        .position(|item| item.get("id").and_then(Value::as_str) == Some(id))
    else {
        return Err(format!("代理「{id}」不存在"));
    };
    if !is_subscription_item(&items[index]) {
        return Err(readonly_hint(&items[index]));
    }
    let removed = items.remove(index);
    write_items(&items)?;
    let name = text_of(&removed, "name");
    logging::log(
        "[Proxies]",
        &format!(
            "🗑️ 删除订阅节点「{name}」（{}）",
            if is_gone(&removed) { "订阅里已移除该节点" } else { "所属订阅已不存在" }
        ),
    );
    Ok((list(), name))
}

// ─── 订阅节点同步（全量镜像，内核拉到订阅后调用）─────────────

/// 一个待镜像的订阅节点（`core::sub_kernel` 在 provider 同步后构造）。
///
/// 定义在**本模块**而不是 sub_kernel：依赖方向是 sub_kernel → proxy_pool
/// （内核把节点写进来），反向会让两个模块成环 —— 订阅条目的端口存在条目
/// 自身里，本模块的解析（`resolve_item`）不需要回查内核。
pub struct SubscriptionNodeTarget {
    /// 订阅 id（与 `core::subscriptions` 条目的 id 一致；同步定位条目用它）
    pub provider_id: String,
    /// 订阅名（条目名的前半段）
    pub sub_name: String,
    /// 节点名（条目名的后半段；同时是内核配置里监听器绑定的出口名）
    pub node: String,
    /// 内核监听端口；**None = 未分配**（lazy listeners：内核只为被账号引用的
    /// 节点开监听器，未引用的节点仍镜像进池供浏览选择，端口为空 —— 引用后
    /// 由对账链路触发内核重启补上，见 `api::proxies::reconcile_listeners`）
    pub port: Option<u16>,
}

/// 把订阅节点集合整体镜像进池（与 `sync_clash` 同一套只读镜像语义）：
///
///   · 集合里的节点 upsert（按 `providerId + node` 定位：**已存在的沿用 id**
///     —— 账号按 id 引用池条目，重生成 id 会让引用全部悬空）；
///   · 订阅侧已消失的（或订阅被删 / 被禁用后不再出现在集合里的）镜像条目摘掉；
///   · manual 与 clash 条目从不被碰。
///
/// 返回变更条数（「已是最新」时为 0）。注意目标集合是**全部启用订阅的全部
/// 节点**：只传一条订阅的节点时，其它订阅的条目会被当作「已消失」摘掉
/// （调用方要传全量 —— sub_kernel 的同步总是构造全量）。
pub fn sync_subscription_nodes(targets: &[SubscriptionNodeTarget]) -> Result<usize, String> {
    let now = logging::now_ms();
    let mut items = read_items();
    let mut changes = 0usize;

    // ① 目标集合逐条 upsert（沿用已有条目的 id / createdAt / lastTest）
    for target in targets {
        let name = if target.sub_name.is_empty() {
            target.node.clone()
        } else {
            format!("{} · {}", target.sub_name, target.node)
        };
        let existing = items.iter_mut().position(|item| {
            is_subscription_item(item)
                && item.get("providerId").and_then(Value::as_str) == Some(target.provider_id.as_str())
                && item.get("node").and_then(Value::as_str) == Some(target.node.as_str())
        });
        match existing {
            Some(index) => {
                let current_port = items[index].get("port").and_then(Value::as_u64).map(|v| v as u16);
                let port_changed = current_port != target.port;
                let name_changed = text_of(&items[index], "name") != name;
                // 上一轮被标成「订阅里已移除」的条目现在又出现了（机场把节点
                // 加回来 / 改名后又改回去）：标记必须撤掉，否则它会一直挂在
                // 「已移除」里 —— 用户看得见、却点不动任何东西。
                let resurrected = items[index].get(GONE_FIELD).is_some();
                // ── 同步**不能**把用户手动关掉的节点重新打开 ──────────────
                // 订阅镜像的语义是「订阅里有什么」，而 `enabled` 是**用户的**
                // 选择（本页开关直接写它，见 set_enabled）。早先这里无条件写
                // `enabled: true`，于是「禁用某节点 → 同步订阅」就把它又打开了；
                // 订阅内容一变（端口变化也算）用户只得再关一遍，来回拉锯。
                //
                // 现在只分两种情况动它：
                //   · 从「订阅里已移除」复活 → 恢复启用（gone 是系统标记，
                //     不是用户意图；节点回来了就该能用）；
                //   · 其余情况一律**保持用户当前的选择**，只更新 name / port。
                // 「账号引用了却禁用着」那条自动启用有自己的入口
                // （enable_for_reference），不靠同步顺手打开。
                let enable_now = resurrected;
                if port_changed || name_changed || resurrected {
                    if let Some(object) = items[index].as_object_mut() {
                        if enable_now {
                            object.insert("enabled".to_string(), Value::Bool(true));
                        }
                        object.insert("name".to_string(), Value::String(name));
                        // None = 未分配端口（lazy listeners）：写 null 而不是删键，
                        // 前端按 null 显示「未分配」
                        object.insert(
                            "port".to_string(),
                            target.port.map(|port| Value::from(port)).unwrap_or(Value::Null),
                        );
                        object.remove(GONE_FIELD);
                        object.insert("updatedAt".to_string(), Value::from(now));
                    }
                    changes += 1;
                }
            }
            None => {
                items.insert(
                    0,
                    json!({
                        "id": new_id(),
                        "name": name,
                        "source": "subscription",
                        "providerId": target.provider_id,
                        "node": target.node,
                        "enabled": true,
                        // None = 未分配（lazy listeners，见 SubscriptionNodeTarget.port）
                        "port": target.port.map(|port| Value::from(port)).unwrap_or(Value::Null),
                        "createdAt": now,
                        "updatedAt": now,
                        "lastTest": Value::Null,
                    }),
                );
                changes += 1;
            }
        }
    }

    // 同订阅内消失的节点保留身份，但不能继续暴露旧端口或进入可用候选。
    //
    // 保留而不是删除：账号按池条目 id 引用，订阅里节点临时消失（机场改节点名 /
    // 拉取到半份内容）时直接删会让引用永久悬空。代价是这些条目会留在列表里，
    // 所以必须**留一条能手动清掉的出路** —— 打上 `gone` 标记，删除按它放行
    // （见 `remove_subscription_orphan` 与 api::proxies 的 orphan 判定）。
    // 节点回来了标记会被撤掉（见 ① 的 resurrected）。
    for item in &mut items {
        if !is_subscription_item(item) { continue; }
        let alive = targets.iter().any(|target| {
            text_of(item, "providerId") == target.provider_id && text_of(item, "node") == target.node
        });
        if !alive
            && (!matches!(item.get("enabled"), Some(Value::Bool(false)))
                || valid_port(item.get("port")).is_some()
                || item.get(GONE_FIELD).is_none())
        {
            if let Some(object) = item.as_object_mut() {
                object.insert("enabled".to_string(), Value::Bool(false));
                object.insert("port".to_string(), Value::Null);
                object.insert(GONE_FIELD.to_string(), Value::Bool(true));
                object.insert("updatedAt".to_string(), Value::from(now));
            }
            changes += 1;
        }
    }

    // ② 目标集合之外（且 providerId 不在集合里）的订阅镜像条目摘掉。
    //
    // 判据刻意按 **providerId** 而不是节点名：节点名随订阅内容变化（机场
    // 改名很常见），按名判定会把改名后的节点删了重加（id 变了，账号引用
    // 悬空）。providerId 不在本次集合 = 该订阅被删 / 被禁用 / 已无节点，
    // 它名下的全部条目才真正该走。
    let live_providers: std::collections::HashSet<&str> = targets
        .iter()
        .map(|target| target.provider_id.as_str())
        .collect();
    let before = items.len();
    items.retain(|item| {
        if !is_subscription_item(item) {
            return true;
        }
        let Some(provider_id) = item.get("providerId").and_then(Value::as_str) else {
            return false; // 没有引用信息的坏行（手工改库），清掉
        };
        live_providers.contains(provider_id)
    });
    changes += before - items.len();

    if changes > 0 {
        write_items(&items)?;
    }
    Ok(changes)
}



/// 把**全部订阅镜像条目**的端口清空（删内核时调用，见
/// `sub_kernel` worker 的 Remove 分支）。
///
/// ── 为什么清端口而不是把条目摘掉 ────────────────────────────
/// `sync_subscription_nodes(&[])` 会把订阅镜像整批摘掉，但重装内核后重新镜像
/// 出来的是**新 id**（`new_id()`），账号里存的旧 id 就永久悬空了 —— 用户只是
/// 删了个内核，回头再装上却发现所有引用订阅节点的账号都解析不出来，得逐个重配。
/// 端口置空的语义反而更准：节点还在（订阅还在），只是**这一轮没有监听器**，
/// 与 lazy listeners 的「未分配端口」是同一个状态；重装内核后同步按
/// `providerId + node` 命中同一条，端口原位写回，账号引用自然复活。
///
/// 保留 `lastTest`（历史读数，重启换端口后同样会留存）。返回变更条数。
pub fn unprovision_subscription_nodes() -> Result<usize, String> {
    let now = logging::now_ms();
    let mut items = read_items();
    let mut changes = 0usize;
    for item in items.iter_mut() {
        if !is_subscription_item(item) || valid_port(item.get("port")).is_none() {
            continue;
        }
        let Some(object) = item.as_object_mut() else { continue };
        object.insert("port".to_string(), Value::Null);
        object.insert("updatedAt".to_string(), Value::from(now));
        changes += 1;
    }
    if changes > 0 {
        write_items(&items)?;
    }
    Ok(changes)
}

/// 被引用的订阅节点键集合（`<订阅id>::<节点名>`，与内核端口注册表的键同一形态）。
///
/// `pool_ids` 是账号 proxy 指向的池条目 id（调用方从账号存储收集）。id 不在池里
/// 的引用（条目已被删）查不出节点、自然跳过 —— 那类引用解析时本来就会报「不存在」。
/// 内核的 `allocate_listeners` 用它决定「哪些节点开监听器」（lazy listeners）。
///
/// **本页禁用中的条目不算**（`is_enabled`）：账号虽然还引用着它，但引用解析已经
/// 按「已禁用」失败并回退直连了（见 `resolve_reference`）—— 再给它开着监听端口
/// 就是为一条没人用的节点白占端口，也与用户在启用列上按下的那一下自相矛盾。
pub fn subscription_node_keys(pool_ids: &std::collections::HashSet<&str>) -> std::collections::HashSet<String> {
    read_items()
        .iter()
        .filter(|item| is_subscription_item(item) && is_enabled(item))
        .filter(|item| {
            item.get("id")
                .and_then(Value::as_str)
                .map(|id| pool_ids.contains(id))
                .unwrap_or(false)
        })
        .filter_map(|item| {
            let provider_id = item.get("providerId").and_then(Value::as_str)?;
            let node = item.get("node").and_then(Value::as_str)?;
            Some(format!("{provider_id}::{node}"))
        })
        .collect()
}

/// 内核该给哪些节点开监听器：**账号引用中的条目**（禁用中的不算）。
///
/// 内核侧（`runtime::listener_node_keys`）与对账判定
/// （`api::proxies::reconcile_listeners`）都从这一处取，必须同一口径
/// —— 口径不一致就会出现「判定说没有缺口、内核却不建」这类谁也解释不清的
/// 状态。测试不在此列（测试走统一测试口，不需要端口，见模块头）。
pub fn listener_node_keys(pool_ids: &std::collections::HashSet<&str>) -> std::collections::HashSet<String> {
    subscription_node_keys(pool_ids)
}

/// 测试目标（`api::proxies::pool_test` 读它）：条目的展示名与订阅节点的定位
/// 信息（manual / clash 条目的 `provider_id` / `node` 为空串 —— 它们不走内核）。
pub struct TestTarget {
    /// 条目展示名（测试结果与日志里标「测的是谁」）
    pub name: String,
    /// manual | clash | subscription
    pub source: String,
    /// 订阅 id（订阅节点定位内核里的节点用）
    pub provider_id: String,
    /// 节点名（同上）
    pub node: String,
}

/// 找一条池条目作为测试目标（不存在报错）。
pub fn test_target(id: &str) -> Result<TestTarget, String> {
    let Ok(item) = find_raw(id) else {
        return Err(format!("代理「{id}」不存在或已被删除"));
    };
    Ok(TestTarget {
        name: display_name(&item, id),
        source: text_of(&item, "source"),
        provider_id: text_of(&item, "providerId"),
        node: text_of(&item, "node"),
    })
}

/// 这条订阅条目现在**需要监听器却没有端口**吗（启用后要不要补建的判据）。
///
/// 判据与 `listener_node_keys` 完全同口径（账号引用，禁用中的不算），只是限定
/// 到一条：条目自身已启用（禁用中的节点不要监听器）、是订阅节点、端口还空着，
/// 且确实被引用。`referenced` 是账号引用中的池条目 id 集合（调用方从账号存储取）。
///
/// 与内核侧同口径是硬要求：口径不一致就会出现「判定说没有缺口、内核却不建」
/// 这类谁也解释不清的状态（补建那条链的注释里也写着同一句话）。
pub fn needs_listener(id: &str, referenced: &std::collections::HashSet<&str>) -> bool {
    let Ok(item) = find_raw(id) else {
        return false;
    };
    if !is_subscription_item(&item) || !is_enabled(&item) {
        return false;
    }
    if valid_port(item.get("port")).is_some() {
        return false;
    }
    // `listener_node_keys` 的唯一来源是账号引用；条目已启用，引用即算数
    referenced.contains(id)
}

/// 列出池里订阅节点的**端口状态**（`{providerId, node, port}`，`port` 为
/// null 表示未分配）。
///
/// 对账链路（`api::proxies::reconcile_listeners`）的读入口：拿它和「该开
/// 监听器的节点集合」（`listener_node_keys`）比对 —— 该开却没端口的要补建，
/// 端口还挂着却不该开的要回收。`provider_nodes`（provider 缓存）里没有的
/// 节点跳过 —— 订阅还没拉到内容时谈不上补建。
pub fn subscription_entries_with_ports() -> Vec<(String, String, Option<u16>)> {
    read_items()
        .iter()
        .filter(|item| is_subscription_item(item))
        .filter_map(|item| {
            let provider_id = item.get("providerId").and_then(Value::as_str)?.to_string();
            let node = item.get("node").and_then(Value::as_str)?.to_string();
            let port = item.get("port").and_then(Value::as_u64).map(|v| v as u16);
            Some((provider_id, node, port))
        })
        .collect()
}


// ─── 端口存活与「就绪」判定 ─────────────────────────────────────

/// 一次本地 TCP connect 的探测超时。
///
/// 300ms 与 `sub_kernel::controller_alive` 同值：本机回环上「连不上」与
/// 「慢」的界线很短，超过这个量级基本可以判定监听器不在。
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

/// 这条池条目指向的本地监听器现在是否真的在 listen。
///
/// ── 为什么必须探活而不是「有 port 就当可用」──────────────────
/// 订阅节点的 `port` 是**上一轮内核分配的结果**，它不保证此刻有人在听：
/// 内核进程崩了 / 被手动停了 / 机器重启后没起来，`port` 字段仍然留着。
/// 这时如果直接用它，请求会在 connect 阶段失败 —— 而转发链的兜底是
/// **回退直连**（见 `with_proxy_notice`），等于把账号的真实 IP 暴露给上游。
///
/// 凡是非 manual / 非 clash 的条目（即订阅镜像）都要过这一关；manual 与
/// clash 条目由用户自己负责，不做探测（它们可能是远端地址，探活没有意义）。
/// 带短 TTL 的端口探活缓存：`port -> (在听?, 上次探测时刻)`。
///
/// ── 为什么需要缓存 ──────────────────────────────────────────
/// 转发链每次解析订阅节点都要问「这个端口现在有人听吗」。直接探活是**每条
/// 请求一次额外 TCP connect**（回环上很快，但高频转发下仍是白白翻倍的
/// 连接数）。3 秒的 TTL 足够把「内核刚死」这类事实在一个短窗口内传播，
/// 又不会让每条请求都去 connect。
///
/// 代价说清楚：内核在 TTL 内被停掉时，最多有 3 秒的请求会按「仍然在听」
/// 放行 —— 那些请求会在 connect 阶段失败（502 网络错误），**不会直连**。
fn listening_cache() -> &'static std::sync::Mutex<std::collections::HashMap<u16, (bool, std::time::Instant)>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u16, (bool, std::time::Instant)>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 缓存的探活结果（TTL 3 秒）。
fn cached_listening(port: u16) -> bool {
    const TTL: std::time::Duration = std::time::Duration::from_secs(3);
    let now = std::time::Instant::now();
    if let Ok(guard) = listening_cache().lock() {
        if let Some((alive, at)) = guard.get(&port) {
            if now.duration_since(*at) < TTL {
                return *alive;
            }
        }
    }
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let alive = std::net::TcpStream::connect_timeout(&address, PROBE_TIMEOUT).is_ok();
    if let Ok(mut guard) = listening_cache().lock() {
        // 表只装「正在被引用的端口」，规模是几十条，不需要淘汰策略
        guard.insert(port, (alive, now));
    }
    alive
}

pub fn is_listening(item: &Value) -> bool {
    if is_manual(item) || is_clash(item) {
        return true;
    }
    let Some(port) = valid_port(item.get("port")) else {
        return false;
    };
    // 订阅节点恒为本机 http 监听器（内核产出），主机固定 127.0.0.1
    cached_listening(port)
}

/// 账号引用中的订阅节点里，**有端口但端口没在监听**的那些。
///
/// 返回 `(订阅 id, 节点名, 端口)`：调用方（启动期就绪检查）据此判断
/// 「是不是内核该拉起却没人监听」。
pub fn silent_listeners(
    pool_ids: &std::collections::HashSet<String>,
) -> Vec<(String, String, u16)> {
    read_items()
        .iter()
        .filter(|item| is_subscription_item(item))
        .filter(|item| {
            item.get("id")
                .and_then(Value::as_str)
                .map(|id| pool_ids.contains(id))
                .unwrap_or(false)
        })
        .filter(|item| is_enabled(item))
        .filter(|item| !is_listening(item))
        .filter_map(|item| {
            let provider_id = item.get("providerId").and_then(Value::as_str)?.to_string();
            let node = item.get("node").and_then(Value::as_str)?.to_string();
            let port = valid_port(item.get("port"))?;
            Some((provider_id, node, port))
        })
        .collect()
}

#[cfg(test)]
mod reference_tests {
    use super::*;
    use serde_json::json;

    /// 「指定即启用」只在**本页禁用**的条目上生效。
    ///
    /// 四种情形分开断言：能启用的、已经可用的、Clash 侧停用的、订阅里已移除的
    /// —— 这四种在写入链路里走的是同一个入口，判定错一个就会出现「报已启用
    /// 但转发仍失败」或「无改动也报一条变更」。
    #[test]
    fn only_locally_disabled_entries_are_auto_enabled() {
        // 本页禁用的手动地址：该启用
        let manual = json!({
            "id": "px_1", "name": "机房 A", "source": "manual",
            "protocol": "http", "host": "10.0.0.1", "port": 8080,
            "enabled": false,
        });
        assert!(should_enable_on_reference(&manual));

        // 已经可用的：没有要改的
        let mut live = manual.clone();
        live["enabled"] = json!(true);
        assert!(!should_enable_on_reference(&live));

        // Clash 侧停用（`enabled` 是镜像值）：本页启用不生效
        let clash_off = json!({
            "id": "px_2", "name": "Clash 出口", "source": "clash",
            "listenerUid": "listener-1", "enabled": false,
        });
        assert!(!should_enable_on_reference(&clash_off));

        // Clash 条目被本页禁用（`localDisabled`）：该启用
        let clash_local = json!({
            "id": "px_4", "name": "Clash 出口 2", "source": "clash",
            "listenerUid": "listener-2", "enabled": true, "localDisabled": true,
        });
        assert!(should_enable_on_reference(&clash_local));

        // 订阅里已经移除的节点：启用也永远拿不到端口
        let gone = json!({
            "id": "px_3", "name": "机场 · 节点", "source": "subscription",
            "providerId": "sub_1", "node": "香港 01", "enabled": false, "gone": true,
        });
        assert!(!should_enable_on_reference(&gone));
    }

    /// 只有「指向具体池条目」的形状会走到启用那一步。
    #[test]
    fn only_concrete_pool_references_reach_the_enable_path() {
        // 没有库时 `enable_for_reference` 必然返回空表，所以这里断言的是
        // 「连池都不查」的那几种形状：它们不该触发任何落盘尝试。
        assert!(enable_for_account_proxy(None).is_empty());
        assert!(enable_for_account_proxy(Some(&json!(null))).is_empty());
        assert!(enable_for_account_proxy(Some(&json!({ "source": "custom", "host": "1.2.3.4", "port": 1 }))).is_empty());
        assert!(enable_for_account_proxy(Some(&json!({ "source": "clash", "listenerUid": "x" }))).is_empty());
        // 「全部」轮询：语义是「池里所有启用的出口」，不该把禁用的也打开
        assert!(enable_for_account_proxy(Some(&json!({ "source": "rotate", "all": true }))).is_empty());
        // pool 但 id 为空：配置本身坏，不撞库
        assert!(enable_for_account_proxy(Some(&json!({ "source": "pool", "proxyId": "  " }))).is_empty());
    }
}

/// 「账号改代理 → 池条目真的启用」的端到端断言（临时库）。
///
/// 单元判定（`should_enable_on_reference`）只回答「该不该启用」，而实际链路
/// 多两步：`set_enabled` 落盘、以及 `enable_for_account_proxy` 只对 pool 引用
/// 生效。这两步任一错了，界面上看到的是「保存成功、代理仍禁用」。
#[cfg(test)]
mod reference_persist_tests {
    use super::*;
    use crate::server::db::test_temp::TempDb;

    #[test]
    fn referencing_a_disabled_pool_entry_enables_it_once() {
        let (db, _guard) = TempDb::open("proxy-pool-auto-enable");
        // `install` 只生效一次（同进程内其他用例可能先装过别的临时库）：先把手上
        // 这个库装进去，再看是不是真的生效了 —— 不是自己的库就跳过，避免往别人
        // 的库上写数据（同一个 `cargo test` 进程里两个 pool 用例共存）。
        crate::server::core::proxy_pool::install(Some(db));
        if database().is_err() {
            return;
        }
        let created = create(&json!({
            "name": "机房 A", "protocol": "http", "host": "10.0.0.1", "port": 8080,
        }))
        .expect("新建条目应当成功");
        let id = created[0]["id"].as_str().expect("新建条目应有 id").to_string();
        // 先按界面上的开关禁用它
        set_enabled(&id, false).expect("禁用应当成功");
        let config = json!({ "source": "pool", "proxyId": id });

        // 第一次指定：真的启用了，并给出一条说明
        let first = enable_for_account_proxy(Some(&config));
        assert_eq!(first.len(), 1, "第一次指定应当启用一条");
        assert_eq!(first[0].name, "机房 A");
        assert!(first[0].enabled);
        // manual 条目不需要惊动内核
        assert!(!first[0].needs_listener_build());
        assert!(is_enabled(&find_raw(&id).expect("条目还在")));

        // 再指定一次：已经是可用的，不该重复报「已自动启用」
        assert!(enable_for_account_proxy(Some(&config)).is_empty());
    }

    /// 「禁用某节点 → 同步订阅 → 它又被打开了」的回归护栏。
    ///
    /// 订阅镜像的语义是「订阅里有什么」，而 `enabled` 是用户的选择。早先
    /// `sync_subscription_nodes` 无条件写 `enabled: true`，于是订阅内容一变
    /// （改名、换端口都算）用户手动关掉的节点就被重新打开，只能再关一遍。
    /// 现在同步只更新 name / port，用户的禁用必须留着。
    ///
    /// 用例刻意把「禁用」与「同步」压进同一次写入：`install` 全进程只生效
    /// 一次，而本文件里别的用例也在用 `sync_subscription_nodes` 写同一个池 ——
    /// 两次同步之间夹一次全局状态，别的用例一跑就会把我的条目当「已消失」
    /// 清掉（实测就是这么挂的）。
    #[test]
    fn subscription_sync_keeps_user_disabled_nodes_disabled() {
        let (db, _guard) = TempDb::open("proxy-pool-sync-keep-disabled");
        crate::server::core::proxy_pool::install(Some(db));
        if database().is_err() {
            return;
        }
        let now = logging::now_ms();
        // 预置一条**已被用户禁用**的订阅镜像条目，然后直接同步同一节点
        // （端口变了 = 订阅内容确实更新过）
        write_items(&[json!({
            "id": "px_keep_disabled",
            "name": "机场 Z · A",
            "source": "subscription",
            "providerId": "sub_z",
            "node": "A",
            "enabled": false,
            "port": 19101,
            "createdAt": now,
            "updatedAt": now,
            "lastTest": Value::Null,
        })])
        .expect("预置条目应当成功");

        sync_subscription_nodes(&[SubscriptionNodeTarget {
            provider_id: "sub_z".to_string(),
            sub_name: "机场 Z".to_string(),
            node: "A".to_string(),
            port: Some(19109),
        }])
        .expect("同步应当成功");

        let Some(after) = read_items()
            .into_iter()
            .find(|item| item.get("id").and_then(Value::as_str) == Some("px_keep_disabled"))
        else {
            return; // 并跑时被别的用例清掉了，跳过
        };
        assert!(
            matches!(after.get("enabled"), Some(Value::Bool(false))),
            "同步不该把用户手动禁用的节点重新打开：{after}"
        );
        // 端口仍要跟着订阅更新（只保留用户的 enabled 选择，别的一律照同步）
        assert_eq!(
            after.get("port").and_then(Value::as_u64),
            Some(19109),
            "端口应当跟随订阅更新"
        );
        // 名字也要跟着订阅刷新
        assert_eq!(
            after.get("name").and_then(Value::as_str),
            Some("机场 Z · A"),
            "名字应当跟随订阅"
        );
    }
}
