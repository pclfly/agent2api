//! LobsterAI 模型目录：远程拉取（`GET /api/models/available`）+ 兜底表。
//!
//! ── 两个客户端头是必需的（不是可选的元数据）──────────────────
//! 服务端按 `X-LobsterAI-Client-Capabilities` 声明的能力**过滤模型集合**：
//! 不带该头时 `kimi-k3` 不会出现在返回里（25 个模型），带上才 26 个。
//! 头构造见 `client::models_headers`（与对话共用同一份）。
//!
//! ── 响应形状 ────────────────────────────────────────────────
//! `{code:0, data:[...]}` 或 `{code:0, data:{data:[...]}}` 两种嵌套都认
//! （`readLobsteraiModelArray` 的逐条对齐）；`code !== 0` 或结构不符回空数组，
//! 由调用方回退兜底目录。
//!
//! ── 缓存 ────────────────────────────────────────────────────
//! 进程内：远程清单 + 上次成功时刻；持久化：`catalog_cache` 的
//! `SCOPE_LOBSTERAI` 一条（重启后先恢复上次成功清单，再等刷新）。
//! 失败**保留上一份成功结果**不清空 —— 清空会让「网络抖了一下」变成
//! 「模型全没了」。

use std::sync::{OnceLock, RwLock};

use serde_json::{json, Value};

use crate::server::core::proxies::ResolvedProxy;
use crate::server::core::providers::catalog_cache;
use crate::server::logging;

use super::client;
use super::credentials::{self, Credential};
use super::product;

/// 目录请求超时（毫秒）。
///
/// 与参考实现一致（`LOBSTERAI_REQUEST_TIMEOUT_MS = 30_000`）；模型目录是
/// 用户手动点「获取模型」会等的请求，30 秒给了跨域拉取足够余量。
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 目录状态：**原始条目**（上游形态）+ 上次成功刷新时刻（0 = 从未成功过）。
///
/// 存原始形态而不是归一形态：思考档位映射（`thinking_options`）要读上游的
/// `thinkingConfig`，而归一形态（给聚合层的 `list_item` 字段）会把它丢掉。
/// 归一很便宜（26 条以内），`list()` 每次现算即可。
#[derive(Default, Clone)]
struct CatalogState {
    raw: Vec<Value>,
    fetched_at: i64,
}

/// 进程级目录缓存（单家，无地区分叉）。
fn catalog() -> &'static RwLock<CatalogState> {
    static SLOT: OnceLock<RwLock<CatalogState>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(restored_state()))
}

/// 首次初始化读一次持久化缓存（见 `providers::catalog_cache` 的模块头）。
fn restored_state() -> CatalogState {
    match catalog_cache::load(catalog_cache::SCOPE_LOBSTERAI) {
        Some(cached) => CatalogState { raw: cached.models, fetched_at: cached.fetched_at },
        None => CatalogState::default(),
    }
}

fn read_state() -> CatalogState {
    match catalog().read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// 本家清单（远程优先，否则兜底表）。
///
/// 返回形态与聚合层 `models::list_item` 的读取字段对齐：
/// `id` / `name` / `maxInputTokens` / `maxOutputTokens` / `supportsImages` /
/// `supportsReasoning` / `credits`。
pub fn list() -> Vec<Value> {
    let state = read_state();
    if !state.raw.is_empty() {
        return normalize_entries(&state.raw);
    }
    fallback_models()
}

/// 最近一次成功刷新时刻（毫秒；0 = 从未成功过）。
pub fn last_refreshed_at() -> i64 {
    read_state().fetched_at
}

/// 当前清单是不是远程拉来的（`true` = 至少成功拉过一次）。
pub fn remote_refreshed() -> bool {
    !read_state().raw.is_empty()
}

/// 一条思考档位（`thinkingConfig.options[]` 的一项）。
///
/// **两个字段语义不同，不可混用**：
///   - `level`：**产品侧档位名**（`off`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`），
///     用于展示与 `defaultLevel` 引用；
///   - `openclaw_level`：**发给服务端的 wire 值**（**没有 `max`**），
///     即 `reasoning_effort` 的取值。
///
/// 实测（2026-09-17，真实凭据）：远端把 `level: 'max'` 映射到
/// `openclawLevel: 'xhigh'`。直接发 `reasoning_effort: 'max'` 与不带参数
/// 无差异（走服务端默认），发 `'xhigh'` 才真正触发最高档 —— 因此必须用
/// `openclaw_level` 作为注入值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThinkingOption {
    /// 产品侧档位名（`default_level` 引用的是这个值）。
    pub level: String,
    /// 发给服务端的 `reasoning_effort` 取值。
    pub openclaw_level: String,
}

/// 某模型的思考档位配置（`thinkingConfig`）。
pub struct ThinkingConfig {
    pub options: Vec<ThinkingOption>,
    /// 默认档（引用的是 `level` 而不是 wire 值）。
    pub default_level: Option<String>,
}

impl ThinkingConfig {
    /// 把产品侧 `level` 翻译成 wire 值（`openclawLevel`）。
    ///
    /// 找不到返回 `None`（调用方据此 Skip，不注入未知取值）。
    pub fn wire_for(&self, level: &str) -> Option<&str> {
        self.options
            .iter()
            .find(|option| option.level == level)
            .map(|option| option.openclaw_level.as_str())
    }
}

/// 从原始条目解析 `thinkingConfig`。
pub fn parse_thinking_config(record: &Value) -> Option<ThinkingConfig> {
    let config = record.get("thinkingConfig")?;
    if !config.is_object() {
        return None;
    }
    let options = config
        .get("options")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let level = credentials::read_string(item, "level");
                    let openclaw = credentials::read_string(item, "openclawLevel");
                    if level.is_empty() || openclaw.is_empty() {
                        return None;
                    }
                    Some(ThinkingOption { level, openclaw_level: openclaw })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if options.is_empty() {
        return None;
    }
    let default_level = {
        let value = credentials::read_string(config, "defaultLevel");
        (!value.is_empty()).then_some(value)
    };
    Some(ThinkingConfig { options, default_level })
}

/// 某模型的思考档位配置（远程清单里查；兜底表没有该元数据）。
pub fn thinking_config_of(model: &str) -> Option<ThinkingConfig> {
    let state = read_state();
    state
        .raw
        .iter()
        .find(|item| credentials::read_string(item, "modelId") == model)
        .and_then(parse_thinking_config)
}

/// 兜底模型表 → 聚合层形态（19 条，统一 131072 窗口）。
pub fn fallback_models() -> Vec<Value> {
    product::FALLBACK_MODELS
        .iter()
        .map(|model| {
            json!({
                "id": model.id,
                "name": model.name,
                "maxInputTokens": model.context_window,
                // 兜底表没有 maxTokens / 能力位 / 倍率（远端整体失败时的
                // 保守读数）—— 缺失键在聚合层不输出，而不是编一个假值。
            })
        })
        .collect()
}

/// 读模型数组：`{code:0, data:[...]}` 或 `{code:0, data:{data:[...]}}`。
pub fn read_model_array(body: &Value) -> Vec<Value> {
    let code = body
        .get("code")
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_f64().map(|number| number as i64))
                .or_else(|| value.as_str().and_then(|text| text.trim().parse::<i64>().ok()))
        })
        .unwrap_or(-1);
    if code != 0 {
        return Vec::new();
    }
    let Some(data) = body.get("data") else {
        return Vec::new();
    };
    if let Some(array) = data.as_array() {
        return array.clone();
    }
    if let Some(nested) = data.get("data").and_then(Value::as_array) {
        return nested.clone();
    }
    Vec::new()
}

/// 解析模型条目（`modelId` 必填，其余可选不编造）。
///
/// `provider`/`apiFormat`/`runtimeProfile` 等字段不取：前者是上游内部字段，
/// 后者是 IDE 内置 agent 内核的编排概念，对 OpenAI 兼容转发没有意义。
pub fn parse_models(body: &Value) -> Vec<Value> {
    normalize_entries(&read_model_array(body))
}

/// 原始条目 → 聚合层形态（`list_item` 的读取字段）。
///
/// 从原始 JSON 现归一（成本可忽略），因为 `thinking_config_of` 需要原始
/// `thinkingConfig`，归一时会把它丢掉。
fn normalize_entries(raw: &[Value]) -> Vec<Value> {
    let mut models = Vec::new();
    for item in raw {
        let Some(record) = item.as_object() else { continue };
        let id = credentials::read_string(item, "modelId");
        if id.is_empty() {
            continue;
        }
        let name = credentials::read_string(item, "modelName");
        let name = if name.is_empty() { id.clone() } else { name };
        let mut entry = serde_json::Map::new();
        entry.insert("id".to_string(), Value::String(id));
        entry.insert("name".to_string(), Value::String(name));
        // 可选字段：只在远端确实给出合法值时才带上（「不编造值」约定）
        if let Some(window) = credentials::read_number(item, "contextWindow") {
            if window > 0.0 {
                entry.insert("maxInputTokens".to_string(), Value::from(window as i64));
            }
        }
        if let Some(max_tokens) = credentials::read_number(item, "maxTokens") {
            if max_tokens > 0.0 {
                entry.insert("maxOutputTokens".to_string(), Value::from(max_tokens as i64));
            }
        }
        if let Some(flag) = item.get("supportsImage").and_then(Value::as_bool) {
            entry.insert("supportsImages".to_string(), Value::Bool(flag));
        }
        // 思考能力：`supportsThinking` 布尔或 `thinkingConfig` 存在都算
        let thinking = item
            .get("supportsThinking")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || record.get("thinkingConfig").is_some_and(|value| !value.is_null());
        if thinking {
            entry.insert("supportsReasoning".to_string(), Value::Bool(true));
        }
        // 计费倍率：**裸数字**（实测 0.05 / 1.08 / 20），格式化成 `x{n}` 文案
        // （与别家的 `credits` 列同口径）；只在为正数时带上。
        if let Some(multiplier) = credentials::read_number(item, "costMultiplier") {
            if multiplier > 0.0 {
                entry.insert("credits".to_string(), Value::String(format_multiplier(multiplier)));
            }
        }
        models.push(Value::Object(entry));
    }
    models
}

/// 倍率数字 → 展示文案（`0.05` → `x0.05`；`20` → `x20`）。
///
/// 去尾零但至少保留一位小数判断：`1.08` → `x1.08`、`1.0` → `x1`。
fn format_multiplier(value: f64) -> String {
    let text = format!("{value}");
    // Rust 的 f64 Display 已经给出最短往返表示（1.0 → "1"、0.05 → "0.05"）
    format!("x{text}")
}

/// 拉取远程目录并落地（`force = false` 时仍会真拉 —— 调用方已经做过
/// TTL/缓存判断的语义见 `ProviderAdapter::refresh_models`；本函数不早退）。
///
/// 返回 `(条数, 失败原因)`：成功时 `(n, None)`；失败时 `(0, Some(原因))`。
/// **失败不清空既有清单**（保留上一份成功结果）。
pub async fn refresh(
    credential: &Credential,
    client_version: &str,
    proxy: Option<&ResolvedProxy>,
) -> (usize, Option<String>) {
    if !credential.valid() {
        return (0, Some("缺少登录态".to_string()));
    }
    // query 串：keyfrom 身份载荷（**不含 refreshToken**，见参考实现说明）。
    // 构造放进块作用域：`url::form_urlencoded::Serializer` 不是 Send，
    // 不能跨 await 存活（块结束即 drop）。
    let body = credentials::keyfrom_body(credential, client_version);
    let path = {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        if let Value::Object(map) = &body {
            for (key, value) in map {
                if let Some(text) = value.as_str() {
                    if !text.is_empty() {
                        serializer.append_pair(key, text);
                    }
                }
            }
        }
        let query = serializer.finish();
        if query.is_empty() {
            product::MODELS_PATH.to_string()
        } else {
            format!("{}?{query}", product::MODELS_PATH)
        }
    };
    let headers = client::models_headers(credential, client_version);
    let reply = match client::get_api(
        &path,
        &headers,
        proxy,
        std::time::Duration::from_millis(REQUEST_TIMEOUT_MS),
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
    let models = parse_models(&body);
    if models.is_empty() {
        return (0, Some("上游没有返回可用模型".to_string()));
    }
    // 原始条目（供 `thinking_config_of` 读 `thinkingConfig`）；持久化缓存
    // 存原始形态（重启后两个查询都能工作）。
    let raw = read_model_array(&body);
    let count = models.len();
    let fetched_at = logging::now_ms();
    // 锁中毒（持锁 panic）时用 into_inner 继续写 —— 缓存是纯数据，
    // 没有「不变量被破坏」的风险（与仓库其它目录模块同一处置）。
    match catalog().write() {
        Ok(mut guard) => {
            guard.raw = raw.clone();
            guard.fetched_at = fetched_at;
        }
        Err(poisoned) => {
            let mut guard = poisoned.into_inner();
            guard.raw = raw.clone();
            guard.fetched_at = fetched_at;
        }
    }
    catalog_cache::save(catalog_cache::SCOPE_LOBSTERAI, &raw, fetched_at);
    logging::log("[Models]", &format!("LobsterAI 模型目录已刷新：{count} 个模型"));
    (count, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_model_array_accepts_both_shapes() {
        let flat = json!({"code": 0, "data": [{"modelId": "a"}]});
        assert_eq!(read_model_array(&flat).len(), 1);
        let nested = json!({"code": 0, "data": {"data": [{"modelId": "a"}, {"modelId": "b"}]}});
        assert_eq!(read_model_array(&nested).len(), 2);
        // code 非 0 / 结构不符 → 空
        assert!(read_model_array(&json!({"code": 1, "data": []})).is_empty());
        assert!(read_model_array(&json!({"code": 0, "data": null})).is_empty());
    }

    #[test]
    fn parse_models_keeps_optional_fields_only_when_present() {
        let body = json!({
            "code": 0,
            "data": [
                {
                    "modelId": "kimi-k3",
                    "modelName": "Kimi K3",
                    "contextWindow": 1000000,
                    "maxTokens": 32768,
                    "supportsImage": true,
                    "supportsThinking": true,
                    "costMultiplier": 1.08
                },
                { "modelId": "bare" }
            ]
        });
        let models = parse_models(&body);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], "kimi-k3");
        assert_eq!(models[0]["name"], "Kimi K3");
        assert_eq!(models[0]["maxInputTokens"], 1_000_000);
        assert_eq!(models[0]["maxOutputTokens"], 32768);
        assert_eq!(models[0]["supportsImages"], true);
        assert_eq!(models[0]["supportsReasoning"], true);
        assert_eq!(models[0]["credits"], "x1.08");
        // 裸条目：不编造字段
        assert_eq!(models[1]["id"], "bare");
        assert_eq!(models[1]["name"], "bare");
        assert!(models[1].get("maxInputTokens").is_none());
        assert!(models[1].get("credits").is_none());
    }

    #[test]
    fn parse_models_skips_entries_without_id() {
        let body = json!({"code": 0, "data": [{"modelName": "no id"}, {"modelId": "ok"}]});
        let models = parse_models(&body);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], "ok");
    }

    #[test]
    fn fallback_table_has_19_entries() {
        assert_eq!(fallback_models().len(), 19);
        assert_eq!(fallback_models()[0]["id"], "deepseek-v4-flash");
    }
}
