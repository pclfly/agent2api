//! MiniMax 模型目录：远程拉取（`GET /mavis/api/v1/models?region=cn&buildEnv=prod`）
//! + 兜底表 + 思考档位元数据。
//!
//! ── 远端响应结构 ────────────────────────────────────────────
//! `{providers:[{providerId:"minimax", config:{models:{…}, model_order:[…]}}]}`
//! —— 校验（照 asar `parseSnapshot`）：`providers[]` 含 `providerId === 'minimax'`、
//! `config.models` 是非空对象。
//!
//! ── 长名 / 短名（最容易搞错的一处）──────────────────────────
//! `models` 对象的 **key 是长名**（`MiniMax-M3.1-Flash-Preview`），条目自带的
//! `name` 字段是**短名**（`M3.1-Flash-Preview`，官方 IDE 就显示它）。两者都要
//! 保留 —— 合并会丢失官方展示名。
//!
//! ── 归一口径 ────────────────────────────────────────────────
//!   - 窗口取 `context_window_options` **最大档**（不是 `limit.context`：
//!     M3.1 的 limit.context 是 512000，但档位表有 1M 档）；
//!   - `limit.output` 只放行**安全正整数**（0/负数/NaN 会让客户端崩）；
//!   - `modalities.input` 含 `image`；
//!   - `effort_options`（去重保序）；`default_effort` **必须落在档位表内**；
//!   - `thinking_config.mode`（`forced_on` / `switchable`）。
//!
//! ── 缓存 ────────────────────────────────────────────────────
//! 进程内 + `catalog_cache` 的 `SCOPE_MINIMAX` 一条；失败保留上一份成功结果。

use std::sync::{OnceLock, RwLock};

use serde_json::Value;

use crate::server::core::providers::catalog_cache;
use crate::server::logging;

use super::client;
use super::credentials::Credential;
use super::product;

/// 目录状态：**归一后的条目**（含档位元数据）+ 上次成功刷新时刻。
#[derive(Default, Clone)]
struct CatalogState {
    entries: Vec<Value>,
    fetched_at: i64,
}

fn catalog() -> &'static RwLock<CatalogState> {
    static SLOT: OnceLock<RwLock<CatalogState>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(restored_state()))
}

fn restored_state() -> CatalogState {
    match catalog_cache::load(catalog_cache::SCOPE_MINIMAX) {
        Some(cached) => CatalogState { entries: cached.models, fetched_at: cached.fetched_at },
        None => CatalogState::default(),
    }
}

fn read_state() -> CatalogState {
    match catalog().read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// 本家清单（远程优先，否则兜底表），形态与聚合层 `list_item` 的读取字段对齐。
pub fn list() -> Vec<Value> {
    let state = read_state();
    let entries = if state.entries.is_empty() {
        fallback_entries()
    } else {
        state.entries
    };
    entries.iter().map(to_public_item).collect()
}

/// 最近一次成功刷新时刻（毫秒；0 = 从未成功过）。
pub fn last_refreshed_at() -> i64 {
    read_state().fetched_at
}

/// 当前清单是不是远程拉来的。
pub fn remote_refreshed() -> bool {
    !read_state().entries.is_empty()
}

/// 某模型的可用思考档位（远程清单里查；兜底表同源）。
///
/// 返回 `None` 表示**该模型没有档位选择**（M3 / M2.7 系 —— 远端没有
/// `effort_options` 字段，给它们编档位就是凭空猜测）。
pub fn efforts_of(model: &str) -> Option<Vec<String>> {
    let state = read_state();
    let entries = if state.entries.is_empty() { fallback_entries() } else { state.entries };
    entries
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(model))
        .and_then(|entry| entry.get("efforts").and_then(Value::as_array))
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .filter(|items: &Vec<String>| !items.is_empty())
}

/// 某模型的思考开关模式（`forced_on` / `switchable` / None = 未知）。
pub fn thinking_mode_of(model: &str) -> Option<String> {
    let state = read_state();
    let entries = if state.entries.is_empty() { fallback_entries() } else { state.entries };
    entries
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(model))
        .and_then(|entry| entry.get("thinkingMode").and_then(Value::as_str))
        .map(str::to_string)
}

/// 兜底条目（4 条，与 `product::FALLBACK_MODELS` 同源）。
pub fn fallback_entries() -> Vec<Value> {
    product::FALLBACK_MODELS
        .iter()
        .map(|model| {
            let mut entry = serde_json::Map::new();
            entry.insert("id".to_string(), Value::String(model.id.to_string()));
            entry.insert("name".to_string(), Value::String(model.name.to_string()));
            entry.insert("contextWindow".to_string(), Value::from(model.context_window));
            entry.insert("maxTokens".to_string(), Value::from(model.max_tokens));
            entry.insert("supportsImage".to_string(), Value::Bool(model.supports_image));
            if let Some(efforts) = model.effort_options {
                entry.insert(
                    "efforts".to_string(),
                    Value::Array(efforts.iter().map(|item| Value::String(item.to_string())).collect()),
                );
            }
            if let Some(default) = model.default_effort {
                entry.insert("defaultEffort".to_string(), Value::String(default.to_string()));
            }
            if let Some(mode) = model.thinking_mode {
                entry.insert("thinkingMode".to_string(), Value::String(mode.to_string()));
            }
            Value::Object(entry)
        })
        .collect()
}

/// 归一后的条目 → 聚合层形态（`list_item` 的读取字段）。
fn to_public_item(entry: &Value) -> Value {
    let mut item = serde_json::Map::new();
    if let Some(id) = entry.get("id").cloned() {
        item.insert("id".to_string(), id);
    }
    if let Some(name) = entry.get("name").cloned() {
        item.insert("name".to_string(), name);
    }
    // 窗口未知时**不编造**（0 是哨兵，不声明）
    if let Some(window) = entry.get("contextWindow").and_then(Value::as_i64) {
        if window > 0 {
            item.insert("maxInputTokens".to_string(), Value::from(window));
        }
    }
    if let Some(max_tokens) = entry.get("maxTokens").and_then(Value::as_i64) {
        if max_tokens > 0 {
            item.insert("maxOutputTokens".to_string(), Value::from(max_tokens));
        }
    }
    if entry.get("supportsImage").and_then(Value::as_bool) == Some(true) {
        item.insert("supportsImages".to_string(), Value::Bool(true));
    }
    // 思考能力：有档位或有开关模式都算
    let has_reasoning = entry
        .get("efforts")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty())
        || entry.get("thinkingMode").and_then(Value::as_str).is_some();
    if has_reasoning {
        item.insert("supportsReasoning".to_string(), Value::Bool(true));
    }
    Value::Object(item)
}

/// 解析远端目录响应 → 归一后的条目数组。
pub fn parse_payload(payload: &Value) -> Vec<Value> {
    let Some(providers) = payload.get("providers").and_then(Value::as_array) else {
        return Vec::new();
    };
    let Some(minimax) = providers.iter().find(|provider| {
        provider.get("providerId").and_then(Value::as_str) == Some("minimax")
    }) else {
        return Vec::new();
    };
    let Some(config) = minimax.get("config") else {
        return Vec::new();
    };
    let Some(models) = config.get("models").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut out: Vec<Value> = Vec::new();
    for (id, raw) in models {
        let mut record = raw.as_object().cloned().unwrap_or_default();
        // ⚠️ 把对象的 **key** 注入为 `id`（长名）；条目自带的 `name` 是短名
        record.insert("id".to_string(), Value::String(id.clone()));
        if let Some(entry) = normalize_model(&Value::Object(record)) {
            out.push(entry);
        }
    }
    // 按远端 `model_order` 排序（若提供），否则保持对象插入序
    if let Some(order) = config.get("model_order").and_then(Value::as_array) {
        let index: std::collections::HashMap<&str, usize> = order
            .iter()
            .enumerate()
            .filter_map(|(position, item)| item.as_str().map(|text| (text, position)))
            .collect();
        out.sort_by_key(|entry| {
            entry
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| index.get(id).copied())
                .unwrap_or(usize::MAX)
        });
    }
    out
}

/// 单条归一（照 asar `parseModel` 的口径）。
fn normalize_model(raw: &Value) -> Option<Value> {
    let id = raw.get("id").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if id.is_empty() {
        return None;
    }
    let name = raw
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(&id)
        .to_string();
    let limit = raw.get("limit").cloned().unwrap_or(Value::Null);
    // 窗口：档位表最大档优先（不是 limit.context）
    let mut context_window: u64 = 0;
    if let Some(options) = raw.get("context_window_options").and_then(Value::as_array) {
        let mut max: u64 = 0;
        for item in options {
            if let Some(value) = positive_int(item) {
                if value > max {
                    max = value;
                }
            }
        }
        context_window = max;
    }
    if context_window == 0 {
        if let Some(value) = limit.get("context").and_then(positive_int) {
            context_window = value;
        }
    }
    let max_tokens = limit.get("output").and_then(positive_int);
    let supports_image = raw
        .get("modalities")
        .and_then(|value| value.get("input"))
        .and_then(Value::as_array)
        .map(|items| items.iter().any(|item| item.as_str() == Some("image")))
        .unwrap_or(false);
    let efforts: Option<Vec<String>> = raw
        .get("effort_options")
        .and_then(Value::as_array)
        .map(|items| {
            let mut seen = std::collections::HashSet::new();
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .filter(|text| seen.insert(text.to_string()))
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|items| !items.is_empty());
    // 默认档必须落在档位表内，否则丢弃（不编造）
    let default_effort = raw
        .get("default_effort")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .filter(|text| efforts.as_ref().is_some_and(|items| items.iter().any(|item| item == text)))
        .map(str::to_string);
    let thinking_mode = raw
        .get("thinking_config")
        .and_then(|value| value.get("mode"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);

    let mut entry = serde_json::Map::new();
    entry.insert("id".to_string(), Value::String(id));
    entry.insert("name".to_string(), Value::String(name));
    entry.insert("contextWindow".to_string(), Value::from(context_window));
    if let Some(max_tokens) = max_tokens {
        entry.insert("maxTokens".to_string(), Value::from(max_tokens));
    }
    entry.insert("supportsImage".to_string(), Value::Bool(supports_image));
    if let Some(efforts) = efforts {
        entry.insert(
            "efforts".to_string(),
            Value::Array(efforts.into_iter().map(Value::String).collect()),
        );
    }
    if let Some(default) = default_effort {
        entry.insert("defaultEffort".to_string(), Value::String(default));
    }
    if let Some(mode) = thinking_mode {
        entry.insert("thinkingMode".to_string(), Value::String(mode));
    }
    Some(Value::Object(entry))
}

/// 只放行**安全正整数**。
fn positive_int(value: &Value) -> Option<u64> {
    let number = value.as_i64()?;
    (number > 0).then_some(number as u64)
}

/// 拉取远程目录并落地。返回 `(条数, 失败原因)`；失败**不清空既有清单**。
pub async fn refresh(credential: &Credential) -> (usize, Option<String>) {
    if !credential.valid() {
        return (0, Some("缺少登录态".to_string()));
    }
    let path = format!(
        "{}?region={}&buildEnv={}",
        product::MODELS_PATH,
        product::REGION,
        product::BUILD_ENV
    );
    let reply = match client::get_api(
        &path,
        &client::business_headers(credential),
        None,
        std::time::Duration::from_millis(product::REQUEST_TIMEOUT_MS),
    )
    .await
    {
        Ok(reply) => reply,
        Err(error) => return (0, Some(error.message)),
    };
    if reply.status == 401 || reply.status == 403 {
        return (0, Some(format!("登录态已失效（HTTP {}）", reply.status)));
    }
    if !(200..300).contains(&reply.status) {
        return (0, Some(format!("上游返回 HTTP {}", reply.status)));
    }
    let Some(body) = reply.json() else {
        return (0, Some("响应不是 JSON".to_string()));
    };
    let entries = parse_payload(&body);
    if entries.is_empty() {
        return (0, Some("上游没有返回可用模型".to_string()));
    }
    let count = entries.len();
    let fetched_at = logging::now_ms();
    match catalog().write() {
        Ok(mut guard) => {
            guard.entries = entries.clone();
            guard.fetched_at = fetched_at;
        }
        Err(poisoned) => {
            let mut guard = poisoned.into_inner();
            guard.entries = entries.clone();
            guard.fetched_at = fetched_at;
        }
    }
    catalog_cache::save(catalog_cache::SCOPE_MINIMAX, &entries, fetched_at);
    logging::log("[Models]", &format!("MiniMax 模型目录已刷新：{count} 个模型"));
    (count, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_payload_reads_long_name_and_short_name() {
        let payload = json!({
            "providers": [{
                "providerId": "minimax",
                "config": {
                    "models": {
                        "MiniMax-M3.1-Flash-Preview": {
                            "name": "M3.1-Flash-Preview",
                            "context_window_options": [512000, 1000000],
                            "limit": {"context": 512000, "output": 128000},
                            "modalities": {"input": ["text", "image"]},
                            "effort_options": ["default", "low", "high", "xhigh", "max"],
                            "default_effort": "default",
                            "thinking_config": {"mode": "forced_on"}
                        }
                    },
                    "model_order": ["MiniMax-M3.1-Flash-Preview"]
                }
            }]
        });
        let entries = parse_payload(&payload);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], "MiniMax-M3.1-Flash-Preview");
        assert_eq!(entries[0]["name"], "M3.1-Flash-Preview");
        // 窗口取档位表最大档（不是 limit.context）
        assert_eq!(entries[0]["contextWindow"], 1_000_000);
        assert_eq!(entries[0]["maxTokens"], 128_000);
        assert_eq!(entries[0]["supportsImage"], true);
        assert_eq!(entries[0]["thinkingMode"], "forced_on");
    }

    #[test]
    fn default_effort_must_be_in_options() {
        let raw = json!({
            "id": "m", "name": "M",
            "effort_options": ["low", "high"],
            "default_effort": "max"
        });
        let entry = normalize_model(&raw).expect("应归一成功");
        assert!(entry.get("defaultEffort").is_none(), "表外的默认档必须丢弃");
    }

    #[test]
    fn parse_payload_rejects_missing_provider() {
        assert!(parse_payload(&json!({"providers": []})).is_empty());
        assert!(parse_payload(&json!({"providers": [{"providerId": "other"}]})).is_empty());
    }

    #[test]
    fn fallback_has_4_entries() {
        let entries = fallback_entries();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0]["id"], "MiniMax-M3.1-Flash-Preview");
        // 只有 M3.1 有档位
        assert!(entries[0].get("efforts").is_some());
        assert!(entries[1].get("efforts").is_none());
    }
}
