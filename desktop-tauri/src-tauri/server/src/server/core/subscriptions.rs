//! 订阅存储：「订阅」区块维护的机场/节点订阅链接（kv 的 `subscriptions` 键）。
//!
//! ── 存储形态（与 proxyPool 同一类）────────────────────────────
//! 条目数量级是个位数、无查询需求（永远整份读、整份写），与 `proxyPool` /
//! `scheduledTasks` 同一类零散状态 —— 落 `kv` 的固定键（登记在
//! `db::schema::RESERVED_KV_KEYS`，否则 `config::save_raw` 写一次配置就会把
//! 它删掉）。不新增表也就不动 schema 版本。
//!
//! ── 职责边界（刻意只有 KV CRUD）──────────────────────────────
//! 本模块是**纯存储**：不认识内核（`core::sub_kernel`）、不认识代理池。内核
//! 的拉起 / 节点同步由 api 层编排（handler 读完这里 → 构造 `ProviderInput`
//! 清单 → `sub_kernel::request_restart`），依赖方向是 api → subscriptions →
//! sub_kernel 的类型，反向（sub_kernel → subscriptions）不存在 —— worker
//! 命令带全量输入，内核不回读订阅。节点镜像进代理池的写侧在
//! `core::sub_kernel`（它只认识 proxy_pool），本模块不碰。
//!
//! ── 字段 ────────────────────────────────────────────────────
//! `{id, name, kind, url, path, sourcePath, enabled, createdAt, updatedAt}`。
//! id 形如 `sub_<毫秒>_<8 位随机>`（与代理池条目同一手法），它同时是 mihomo
//! proxy-provider 名字的前缀（`sub_<id>`），所以只允许小写字母数字下划线。
//!
//! ── kind：链接订阅 vs 本地配置文件 ─────────────────────────────
//! `kind: "url"`（缺省，存量条目全是这种）= 内核定时拉取的机场订阅链接；
//! `kind: "file"` = 用户导入的**本地配置文件**（Clash / mihomo YAML，内含
//! `proxies` 列表）。两者的差别只有三处，其余链路完全共用：
//!   · 内核配置里的 provider `type` 是 `http` / `file`（见 `sub_kernel`）；
//!   · file 条目**不参与定时刷新** —— 内核的 file provider 没有 interval，
//!     内容只在我们写入时变（导入时写一次，「立即更新」重新导入一次）；
//!   · file 条目的 YAML 内容由 api 层写进内核目录，本模块只存元数据与
//!     原始路径（`sourcePath`，仅用于「从原路径重新导入」与报错文案）。
//! 不把内容存进 kv：那是几十 KB 到几 MB 的文本，与「条目数量级小、整份读写」
//! 的 kv 契约不合；内核目录本来就是这些文件的落点。

use std::sync::OnceLock;

use serde_json::{json, Map, Value};

use crate::server::core::sub_kernel::ProviderInput;
use crate::server::db::Db;
use crate::server::logging;

/// `kv` 表里的键名（保留键，见 `db::schema::RESERVED_KV_KEYS`）
pub const KV_KEY: &str = "subscriptions";

/// 名称 / 链接 / 本地路径的长度上限（名称出现在表格里；链接要进 mihomo 配置）
const MAX_NAME_LENGTH: usize = 60;
const MAX_URL_LENGTH: usize = 2048;
const MAX_PATH_LENGTH: usize = 1024;

/// 订阅来源：内核定时拉取的链接 / 用户导入的本地配置文件
pub const KIND_URL: &str = "url";
pub const KIND_FILE: &str = "file";

/// 进程级库句柄（照 `core::proxy_pool` 的形态：bootstrap 时 install 一次）
static DB: OnceLock<Option<Db>> = OnceLock::new();

/// 接线（`ServerState::bootstrap` 调用）。重复调用无副作用。
pub fn install(db: Option<Db>) {
    let _ = DB.set(db);
}

fn database() -> Result<&'static Db, String> {
    DB.get()
        .and_then(Option::as_ref)
        .ok_or_else(|| "订阅数据库不可用".to_string())
}

// ─── 存取 ───────────────────────────────────────────────────

/// 读全部条目（库不可用/无记录/解析失败都给空表；解析失败按空表处理，
/// 与 proxyPool 同一口径 —— 用户界面上的表现是「订阅空了」，可重新添加）
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

/// 整份写回（增删改都走它；条目数量小，整份重写的开销可忽略）
fn write_items(items: &[Value]) -> Result<(), String> {
    let db = database()?;
    let text = serde_json::to_string(&json!({ "items": items }))
        .map_err(|error| format!("编码订阅失败: {error}"))?;
    db.with(|conn| {
        conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![KV_KEY, text],
        )
        .map(|_| ())
        .map_err(|error| format!("保存订阅失败: {error}"))
    })
    .ok_or_else(|| "订阅数据库不可用".to_string())?
}

// ─── 正规化与校验 ───────────────────────────────────────────

fn clean_string(value: Option<&Value>, max: usize) -> String {
    let Some(Value::String(text)) = value else {
        return String::new();
    };
    text.trim().chars().take(max).collect()
}

fn text_of(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// 订阅链接校验：http/https 开头（mihomo 的 http provider 只认这两种）。
fn valid_url(url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Err("请填写订阅链接".to_string());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("订阅链接必须是 http:// 或 https:// 开头".to_string());
    }
    Ok(())
}

/// 条目的来源类型；缺字段的存量条目一律按链接订阅（它们全是那种）。
pub fn kind_of(item: &Value) -> String {
    let kind = text_of(item, "kind");
    if kind == KIND_FILE { KIND_FILE.to_string() } else { KIND_URL.to_string() }
}

/// 内核 provider 的缓存文件路径（相对于内核目录）。两种来源共用它：
/// http provider 把它当缓存落点，file provider 直接读它 —— 所以「写进去的
/// YAML」与「内核读到的节点」永远是同一份文件。
pub fn provider_cache_path(id: &str) -> String {
    format!("providers/sub_{id}.yaml")
}

/// 新条目的 id：`sub_<毫秒>_<8 位随机>`。随机段与 `proxy_pool::new_id` 同一
/// 手法（RandomState 的随机种子），不引第三方依赖。
fn new_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(logging::now_ms() as u64);
    hasher.write_u64(std::process::id() as u64);
    let suffix = (hasher.finish() & 0xffff_ffff) as u32;
    format!("sub_{}_{suffix:08x}", logging::now_ms())
}

/// 把 API 输入（新增的整份表单）归一成一条订阅条目。
///
/// 新增时 `existing` 传 None；编辑时从旧条目沿用 `id` / `createdAt`。
/// `kind` 决定校验哪一侧：链接订阅查 url，本地配置文件查原始路径
/// （YAML 内容由 api 层校验并写盘，本模块不认识内核目录）。
fn normalize_item(input: &Value, existing: Option<&Value>) -> Result<Value, String> {
    let Some(object) = input.as_object() else {
        return Err("订阅条目必须是对象".to_string());
    };
    let name = clean_string(object.get("name"), MAX_NAME_LENGTH);
    if name.is_empty() {
        return Err("请填写订阅名称".to_string());
    }
    // kind 跟随输入；输入没给时沿用旧条目（编辑旧链接订阅时不必重传 kind）
    let kind_input = clean_string(object.get("kind"), 10);
    let kind = if kind_input == KIND_FILE {
        KIND_FILE.to_string()
    } else if kind_input == KIND_URL {
        KIND_URL.to_string()
    } else {
        existing.map(kind_of).unwrap_or_else(|| KIND_URL.to_string())
    };
    let url = clean_string(object.get("url"), MAX_URL_LENGTH);
    let source_path = clean_string(object.get("sourcePath"), MAX_PATH_LENGTH);
    if kind == KIND_FILE {
        // 本地配置的「链接」就是它的原始路径（表格里那一列要显示点什么，
        // 且「立即更新」需要知道从哪里重新导入）
        if source_path.is_empty() {
            return Err("请选择要导入的本地配置文件".to_string());
        }
    } else {
        valid_url(&url)?;
    }
    let now = logging::now_ms();
    let id = existing
        .map(|item| text_of(item, "id"))
        .filter(|id| !id.is_empty())
        .unwrap_or_else(new_id);
    let created_at = existing
        .and_then(|item| item.get("createdAt"))
        .and_then(Value::as_i64)
        .unwrap_or(now);
    let mut normalized = Map::new();
    normalized.insert("id".to_string(), Value::String(id));
    normalized.insert("name".to_string(), Value::String(name));
    normalized.insert("kind".to_string(), Value::String(kind.clone()));
    if kind == KIND_FILE {
        normalized.insert("url".to_string(), Value::String(String::new()));
        normalized.insert("sourcePath".to_string(), Value::String(source_path));
    } else {
        normalized.insert("url".to_string(), Value::String(url));
        normalized.insert("sourcePath".to_string(), Value::String(String::new()));
    }
    normalized.insert(
        "enabled".to_string(),
        Value::Bool(!matches!(object.get("enabled"), Some(Value::Bool(false)))),
    );
    normalized.insert("createdAt".to_string(), Value::from(created_at));
    normalized.insert("updatedAt".to_string(), Value::from(now));
    Ok(Value::Object(normalized))
}

// ─── 公开形态 ───────────────────────────────────────────────

fn describe_item(item: &Value, node_count: usize) -> Value {
    json!({
        "id": text_of(item, "id"),
        "name": text_of(item, "name"),
        "kind": kind_of(item),
        "url": text_of(item, "url"),
        "sourcePath": text_of(item, "sourcePath"),
        "enabled": !matches!(item.get("enabled"), Some(Value::Bool(false))),
        "createdAt": item.get("createdAt").cloned().unwrap_or(Value::from(0)),
        "updatedAt": item.get("updatedAt").cloned().unwrap_or(Value::from(0)),
        "nodeCount": node_count,
    })
}

/// 最新全量列表；`node_count` 是「订阅 id → 节点数」的映射（api 层从内核
/// 状态里带进来 —— 本模块不认识内核，节点数由调用方注入）。
pub fn list(node_counts: &std::collections::HashMap<String, usize>) -> Vec<Value> {
    read_items()
        .iter()
        .map(|item| {
            let id = text_of(item, "id");
            describe_item(item, node_counts.get(&id).copied().unwrap_or(0))
        })
        .collect()
}

// ─── 写操作 ─────────────────────────────────────────────────

/// 新增订阅；返回最新全量列表（原始形态，条目不带节点数 —— api 层统一注入）。
pub fn create(input: &Value) -> Result<Vec<Value>, String> {
    let item = normalize_item(input, None)?;
    let mut items = read_items();
    // 同一链接只存一份：重复导入是常见误操作（同一订阅改个名再存一遍
    // 会拉两次内容、占两份 provider）。本地配置文件按**原始路径**去重：
    // 同一个文件导入两次同样是白占一份 provider。
    if kind_of(&item) == KIND_FILE {
        let path = text_of(&item, "sourcePath");
        if items.iter().any(|item| {
            kind_of(item) == KIND_FILE && text_of(item, "sourcePath") == path
        }) {
            return Err("该本地配置文件已导入过".to_string());
        }
    } else {
        let url = text_of(&item, "url");
        if items.iter().any(|item| kind_of(item) != KIND_FILE && text_of(item, "url") == url) {
            return Err("该订阅链接已存在".to_string());
        }
    }
    items.insert(0, item.clone());
    write_items(&items)?;
    logging::log(
        "[Subscription]",
        &format!("✅ 新增订阅「{}」", text_of(&item, "name")),
    );
    Ok(items)
}

/// 编辑订阅（id 必带；整份覆盖语义，与代理池条目同口径）。
pub fn update(input: &Value) -> Result<Vec<Value>, String> {
    let id = clean_string(input.get("id"), 100);
    if id.is_empty() {
        return Err("缺少订阅 id".to_string());
    }
    let mut items = read_items();
    let Some(index) = items
        .iter()
        .position(|item| item.get("id").and_then(Value::as_str) == Some(id.as_str()))
    else {
        return Err(format!("订阅「{id}」不存在"));
    };
    let next = normalize_item(input, Some(&items[index]))?;
    // 改链接时查重（排除自己）
    if kind_of(&next) == KIND_FILE {
        let path = text_of(&next, "sourcePath");
        if items.iter().enumerate().any(|(position, item)| {
            position != index && kind_of(item) == KIND_FILE && text_of(item, "sourcePath") == path
        }) {
            return Err("该本地配置文件已导入过".to_string());
        }
    } else {
        let url = text_of(&next, "url");
        if items.iter().enumerate().any(|(position, item)| {
            position != index && kind_of(item) != KIND_FILE && text_of(item, "url") == url
        }) {
            return Err("该订阅链接已存在".to_string());
        }
    }
    items[index] = next.clone();
    write_items(&items)?;
    logging::log(
        "[Subscription]",
        &format!("✅ 更新订阅「{}」", text_of(&next, "name")),
    );
    Ok(items)
}

/// 删除订阅；返回（最新列表, 被删条目的名称 —— 调用方拼日志/提示用）。
pub fn remove(id: &str) -> Result<(Vec<Value>, String), String> {
    let mut items = read_items();
    let Some(index) = items
        .iter()
        .position(|item| item.get("id").and_then(Value::as_str) == Some(id))
    else {
        return Err(format!("订阅「{id}」不存在"));
    };
    let removed = items.remove(index);
    write_items(&items)?;
    let name = text_of(&removed, "name");
    logging::log("[Subscription]", &format!("🗑️ 删除订阅「{name}」"));
    Ok((items, name))
}

/// 是否有启用的订阅（内核要不要拉起 / api 层要不要投重启命令的判据）
pub fn has_enabled() -> bool {
    read_items()
        .iter()
        .any(|item| !matches!(item.get("enabled"), Some(Value::Bool(false))))
}

/// 条目是否存在 / 是否启用三态（`None` = 不存在）—— refresh 的报错分流用。
///
/// 「不存在」与「被禁用」是两种事：前者多半是条目被删（或数据被外部工具
/// 清掉），后者用户自己就能启用 —— 笼统报「不存在或已禁用」会把排查方向
/// 带偏（实测踩过：旧版进程误删存储键时，用户从这句文案里看不出真正原因）。
pub fn find_enabled_state(id: &str) -> Option<bool> {
    read_items()
        .iter()
        .find(|item| text_of(item, "id") == id)
        .map(|item| !matches!(item.get("enabled"), Some(Value::Bool(false))))
}

/// 现存订阅的 id 集合（代理池标注「孤儿订阅节点」用：条目的 providerId
/// 不在这个集合里 = 所属订阅已不存在，可手动清理）。
pub fn ids() -> std::collections::HashSet<String> {
    read_items()
        .iter()
        .map(|item| text_of(item, "id"))
        .collect()
}

/// 启用订阅的输入清单（api 层 / bootstrap 构造 `ProviderInput` 用：
/// 内核的 proxy-providers 只收启用的条目）。
pub fn enabled_provider_inputs() -> Vec<ProviderInput> {
    read_items()
        .iter()
        .filter(|item| !matches!(item.get("enabled"), Some(Value::Bool(false))))
        .map(|item| ProviderInput {
            id: text_of(item, "id"),
            name: text_of(item, "name"),
            url: text_of(item, "url"),
            kind: kind_of(item),
        })
        .collect()
}

/// 是否已安装内核且有启用的订阅（bootstrap 自动拉起的判据）
pub fn should_autostart() -> bool {
    crate::server::core::sub_kernel::installed() && has_enabled()
}

/// 一条订阅的完整形态（api 层写本地配置文件、重新导入时定位条目用）。
pub fn find(id: &str) -> Option<Value> {
    read_items()
        .into_iter()
        .find(|item| text_of(item, "id") == id)
}

/// 该本地文件路径是否已被**另一条**订阅占用（原地替换换位置前的查重）。
///
/// 单独一个只读函数而不是让调用方把列表拆开看：去重口径（只比 file 条目、
/// 排除自己）与 `create` / `update` 里那两处必须一致，写成一处引用。
pub fn file_path_taken(path: &str, except_id: &str) -> bool {
    read_items().iter().any(|item| {
        kind_of(item) == KIND_FILE
            && text_of(item, "id") != except_id
            && text_of(item, "sourcePath") == path
    })
}

#[cfg(test)]
mod kind_tests {
    use super::*;
    use serde_json::json;

    /// 缺字段的存量条目一律按链接订阅 —— 这个兜底决定了升级后老订阅不会
    /// 突然变成「本地文件」而拿不到 url。
    #[test]
    fn missing_kind_defaults_to_a_url_subscription() {
        assert_eq!(kind_of(&json!({"id": "sub_1"})), KIND_URL);
        assert_eq!(kind_of(&json!({"id": "sub_1", "kind": "url"})), KIND_URL);
        assert_eq!(kind_of(&json!({"id": "sub_1", "kind": "file"})), KIND_FILE);
        // 未知取值按链接处理（不把坏数据当成本地文件）
        assert_eq!(kind_of(&json!({"id": "sub_1", "kind": "weird"})), KIND_URL);
    }

    /// 两种来源共用同一个缓存文件路径：导入时写进去的 YAML 就是内核读到的文件。
    ///
    /// 实际落盘是 `sub_sub_<毫秒>_<随机>.yaml`（id 本身带 `sub_` 前缀，而 provider
    /// 名又是 `sub_<id>`）—— 这不是重复，是本仓既有的命名（`data/proxy-kernel/providers/`
    /// 下的存量文件就是这个形状）：改掉前缀会让内核找不到已有缓存、所有订阅重拉一遍。
    #[test]
    fn provider_cache_path_is_shared_by_both_kinds() {
        assert_eq!(provider_cache_path("local"), "providers/sub_local.yaml");
        assert_eq!(
            provider_cache_path("sub_1792052874963_dc1b6bb8"),
            "providers/sub_sub_1792052874963_dc1b6bb8.yaml"
        );
    }

    /// 本地配置文件按**原始路径**校验与去重，链接订阅按 url
    #[test]
    fn a_local_file_needs_a_source_path_not_a_url() {
        let missing = normalize_item(&json!({"name": "本机配置", "kind": "file"}), None).unwrap_err();
        assert!(missing.contains("本地配置文件"), "{missing}");
        let ok = normalize_item(
            &json!({"name": "本机配置", "kind": "file", "sourcePath": "C:/nodes.yaml"}),
            None,
        )
        .unwrap();
        assert_eq!(kind_of(&ok), KIND_FILE);
        assert_eq!(ok["sourcePath"], json!("C:/nodes.yaml"));
        // 本地文件条目的 url 恒为空串：没有远端可拉
        assert_eq!(ok["url"], json!(""));
        // 链接订阅仍然要求合法链接
        let bad = normalize_item(&json!({"name": "机场", "url": "ftp://x"}), None).unwrap_err();
        assert!(bad.contains("http"), "{bad}");
    }
}
