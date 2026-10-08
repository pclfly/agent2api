//! Loomy 模型目录：远程拉取（`GET /models`）+ 兜底表。
//!
//! ── 过滤判据是 `type === 'chat'` ─────────────────────────────
//! **不能**看 `input_modalities`：实测 5 个 chat 模型的输入模态含 `image`
//! （能看图），那是输入多模态，与「是不是生图模型」无关。
//!
//! ── 展示名规范化 ────────────────────────────────────────────
//! 远端原值是三种括号风格混用（`MiniMax M3 （x4.0）` / `Qwen 3.8 Max (x12.0)`
//! / `GLM 5.3 Flash(x0.8)`），统一经 `credentials::display_name` 规范成
//! `{name} · x{n}` —— 倍率写进 `name`（不是 description），
//! 因为客户端的模型切换菜单只渲染 `name`。
//!
//! ── 缓存 ────────────────────────────────────────────────────
//! 进程内：原始条目 + 上次成功时刻；持久化：`catalog_cache` 的
//! `SCOPE_LOOMY` 一条。失败**保留上一份成功结果**不清空。

use std::sync::{OnceLock, RwLock};

use serde_json::Value;

use crate::server::core::providers::catalog_cache;
use crate::server::logging;

use super::client;
use super::credentials;
use super::product;

/// 目录状态：**原始条目**（上游形态）+ 上次成功刷新时刻。
#[derive(Default, Clone)]
struct CatalogState {
    raw: Vec<Value>,
    fetched_at: i64,
}

fn catalog() -> &'static RwLock<CatalogState> {
    static SLOT: OnceLock<RwLock<CatalogState>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(restored_state()))
}

fn restored_state() -> CatalogState {
    match catalog_cache::load(catalog_cache::SCOPE_LOOMY) {
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

/// 兜底模型表 → 聚合层形态（8 条）。
pub fn fallback_models() -> Vec<Value> {
    product::FALLBACK_MODELS
        .iter()
        .map(|model| {
            let mut entry = serde_json::Map::new();
            entry.insert("id".to_string(), Value::String(model.id.to_string()));
            entry.insert("name".to_string(), Value::String(model.name.to_string()));
            entry.insert(
                "maxInputTokens".to_string(),
                Value::from(model.context_window),
            );
            // 兜底表声明思考可用（实测 8 个模型全有档位）
            entry.insert("supportsReasoning".to_string(), Value::Bool(true));
            insert_credits(&mut entry, model.name);
            Value::Object(entry)
        })
        .collect()
}

/// 把展示名末尾的倍率拆出来，**另存到 `credits`**（管理页的倍率列只认它）。
///
/// ── 为什么倍率要存两份（真实缺陷）────────────────────────────
/// 本家的远端把倍率写在 `name` 字符串里（`DeepSeek V4 Flash 0731 · x3.0`），
/// 没有独立字段 —— 于是 `display_name` 把它规范进 `name` 之后，倍率**只**以
/// 文本尾巴的形式存在。而管理页的倍率列读的是 `credits`
/// （见 `providers::catalog::view::manage_view`：`credits` → `usageMultiple`
/// → 都没有就如实给 null），本家从来没写过这个键，那一列因此恒为空 `—`。
///
/// 症状极具迷惑性：名字里明明写着 `· x3.0`，旁边那列却是空的 —— 看起来像
/// 「上游没给倍率」，实际是**我们自己没把它从名字里取出来**。
///
/// 两份都留是有意的：`name` 里的那份是给客户端的模型切换菜单看的
/// （它只渲染 `name`，见 `display_name` 的说明），`credits` 这份是给管理页的
/// 倍率列看的。`split_rate` 幂等，两处取的是同一个值，不会漂移。
fn insert_credits(entry: &mut serde_json::Map<String, Value>, raw_name: &str) {
    let (_, rate) = credentials::split_rate(raw_name);
    // 无倍率（如生图模型）不写这个键：`list_item` 会把缺失读成空串、
    // `manage_view` 读成 null，两处都渲染成 `—`，与「0 倍」区分得开
    //（与 codearts / qoder 的「非空才写 credits」同一口径）
    if rate.is_empty() {
        return;
    }
    entry.insert("credits".to_string(), Value::String(rate));
}

/// 某模型的可用思考档位（远程清单里查；兜底表给统一的 5 档）。
///
/// 返回 `None` 表示**该模型不提供档位选择**（此时不注入 —— 发一个
/// 上游不认的档位比不发更糟）。
pub fn efforts_of(model: &str) -> Option<Vec<String>> {
    let state = read_state();
    if state.raw.is_empty() {
        // 兜底路径：统一 5 档（实测 8 个模型完全一致）
        return product::FALLBACK_MODELS
            .iter()
            .any(|item| item.id == model)
            .then(|| product::FALLBACK_EFFORTS.iter().map(|item| item.to_string()).collect());
    }
    state
        .raw
        .iter()
        .find(|item| credentials::read_string(item, "id") == model)
        .and_then(|item| {
            let efforts = item.get("reasoning_efforts").and_then(Value::as_array)?;
            let list: Vec<String> = efforts
                .iter()
                .filter_map(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .collect();
            (!list.is_empty()).then_some(list)
        })
}

/// 是否为可对话的 chat 模型（`type === 'chat'`）。
fn is_chat_model(entry: &Value) -> bool {
    let id = credentials::read_string(entry, "id");
    !id.trim().is_empty() && entry.get("type").and_then(Value::as_str) == Some("chat")
}

/// 原始条目 → 聚合层形态。
fn normalize_entries(raw: &[Value]) -> Vec<Value> {
    let mut models = Vec::new();
    for item in raw {
        if !is_chat_model(item) {
            continue;
        }
        let id = credentials::read_string(item, "id");
        let raw_name = credentials::read_string(item, "name");
        let raw_name = if raw_name.is_empty() { id.clone() } else { raw_name };
        let mut entry = serde_json::Map::new();
        entry.insert("id".to_string(), Value::String(id));
        entry.insert(
            "name".to_string(),
            Value::String(credentials::display_name(&raw_name)),
        );
        // 倍率另存一份到 `credits`：管理页的倍率列读它，不读 `name` 的尾巴
        // （理由见 `insert_credits`）。`split_rate` 幂等，所以远端给的是
        // `（x3.0）` 还是已规范的 ` · x3.0`，取到的都是同一个值。
        insert_credits(&mut entry, &raw_name);
        if let Some(window) = credentials::read_number(item, "context_length") {
            if window > 0.0 {
                entry.insert("maxInputTokens".to_string(), Value::from(window as i64));
            }
        }
        // 输入模态：`capabilities.input_modalities` 含 image（与参考实现同口径）
        let supports_image = item
            .get("capabilities")
            .and_then(|value| value.get("input_modalities"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|text| text.eq_ignore_ascii_case("image"))
            })
            .unwrap_or(false);
        if supports_image {
            entry.insert("supportsImages".to_string(), Value::Bool(true));
        }
        let supports_thinking = item
            .get("capabilities")
            .and_then(|value| value.get("reasoning"))
            .and_then(Value::as_bool)
            == Some(true);
        if supports_thinking {
            entry.insert("supportsReasoning".to_string(), Value::Bool(true));
        }
        models.push(Value::Object(entry));
    }
    models
}

/// 拉取远程目录并落地。返回 `(条数, 失败原因)`；失败**不清空既有清单**。
pub async fn refresh(token: &str) -> (usize, Option<String>) {
    if token.trim().is_empty() {
        return (0, Some("缺少登录态".to_string()));
    }
    let reply = match client::get_business(product::MODELS_PATH, &client::business_headers(token), None).await {
        Ok(reply) => reply,
        Err(error) => return (0, Some(error.message)),
    };
    if !(200..300).contains(&reply.status) {
        return (0, Some(format!("上游返回 HTTP {}", reply.status)));
    }
    let Some(body) = reply.json() else {
        return (0, Some("响应不是 JSON".to_string()));
    };
    // 信封：`{code:"000000", data:[...]}`；`data` 也可能是裸数组
    let list = if let Some(array) = body.as_array() {
        array.clone()
    } else {
        match credentials::parse_envelope(&body) {
            Ok(envelope) => envelope.data.as_array().cloned().unwrap_or_default(),
            Err(failure) => {
                // 登录失效码单独报，其余按失败透出
                if failure.code == credentials::AUTH_ERROR_CODE {
                    return (0, Some("登录态已失效：请重新短信登录".to_string()));
                }
                return (0, Some(format!("模型列表查询失败：{}", failure.message)));
            }
        }
    };
    let models = normalize_entries(&list);
    if models.is_empty() {
        return (0, Some("上游没有返回可用模型".to_string()));
    }
    let count = models.len();
    let fetched_at = logging::now_ms();
    match catalog().write() {
        Ok(mut guard) => {
            guard.raw = list.clone();
            guard.fetched_at = fetched_at;
        }
        Err(poisoned) => {
            let mut guard = poisoned.into_inner();
            guard.raw = list.clone();
            guard.fetched_at = fetched_at;
        }
    }
    catalog_cache::save(catalog_cache::SCOPE_LOOMY, &list, fetched_at);
    logging::log("[Models]", &format!("Loomy 模型目录已刷新：{count} 个模型"));
    (count, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_filter_ignores_output_modalities() {
        let raw = vec![
            json!({"id": "a", "type": "chat", "name": "A", "context_length": 1000}),
            json!({"id": "b", "type": "image", "name": "B"}),
            json!({"id": "", "type": "chat"}),
        ];
        let models = normalize_entries(&raw);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], "a");
    }

    #[test]
    fn normalize_applies_display_name_and_modalities() {
        let raw = vec![json!({
            "id": "MiniMax-M3",
            "type": "chat",
            "name": "MiniMax M3 （x4.0）",
            "context_length": 1048576,
            "capabilities": {"reasoning": true, "input_modalities": ["text", "image"]},
            "reasoning_efforts": ["none", "low", "medium", "high", "xhigh"]
        })];
        let models = normalize_entries(&raw);
        assert_eq!(models[0]["name"], "MiniMax M3 · x4.0");
        assert_eq!(models[0]["maxInputTokens"], 1_048_576);
        assert_eq!(models[0]["supportsImages"], true);
        assert_eq!(models[0]["supportsReasoning"], true);
    }

    #[test]
    fn fallback_has_8_entries() {
        assert_eq!(fallback_models().len(), 8);
        assert_eq!(fallback_models()[0]["id"], "deepseek-v4-flash-0731");
    }

    /// 倍率必须**另存**到 `credits` —— 管理页的倍率列只读它。
    ///
    /// ── 这条用例钉的是什么（真实缺陷）────────────────────────────
    /// 本家的远端把倍率写在 `name` 字符串尾巴里（`DeepSeek V4 Flash 0731 · x3.0`），
    /// 没有独立字段。管理页的倍率列读的是 `credits`
    /// （`providers::catalog::view::manage_view`：`credits` → `usageMultiple`
    /// → 都没有就如实给 null），而本家从来没写过这个键 —— 于是名字里明写着
    /// `· x3.0`，旁边那列却是空 `—`，看起来像「上游没给倍率」。
    ///
    /// 因此这里断言的是**两个出口**都带 `credits`，且值与 `name` 尾巴一致：
    /// 远端路径（`normalize_entries`）与兜底路径（`fallback_models`）是两份
    /// 独立的构造代码，只测一边会让另一边继续空着。
    #[test]
    fn rate_is_exposed_as_credits_on_both_paths() {
        // 兜底路径：用户看到的那条（名字与倍率逐字取自 FALLBACK_MODELS[0]）
        let fallback = fallback_models();
        let first = fallback
            .iter()
            .find(|item| item["id"] == "deepseek-v4-flash-0731")
            .expect("兜底表第一条");
        assert_eq!(first["name"], "DeepSeek V4 Flash 0731 · x3.0");
        assert_eq!(
            first["credits"], "x3.0",
            "兜底路径必须把 name 尾巴里的倍率另存到 credits，否则管理页倍率列是空的"
        );

        // 远端路径：三种括号风格都要拆出来（远端原值不是已规范形态）
        let raw = vec![
            json!({"id": "a", "type": "chat", "name": "DeepSeek V4 Flash 0731 （x3.0）"}),
            json!({"id": "b", "type": "chat", "name": "Qwen 3.8 Max (x12.0)"}),
            json!({"id": "c", "type": "chat", "name": "GLM 5.3 Flash(x0.8)"}),
            // 已规范化形态也要认（`split_rate` 幂等）：缓存恢复的条目走这条
            json!({"id": "d", "type": "chat", "name": "MiniMax M3 · x4.0"}),
            // 无倍率：不写这个键（界面渲染 `—`，与「0 倍」区分开）
            json!({"id": "e", "type": "chat", "name": "Spark X2.5"}),
        ];
        let models = normalize_entries(&raw);
        let credits_of = |id: &str| {
            models
                .iter()
                .find(|item| item["id"] == id)
                .and_then(|item| item.get("credits"))
                .cloned()
        };
        assert_eq!(credits_of("a"), Some(json!("x3.0")), "全角括号");
        assert_eq!(credits_of("b"), Some(json!("x12.0")), "半角括号");
        assert_eq!(credits_of("c"), Some(json!("x0.8")), "括号紧贴无空格");
        assert_eq!(credits_of("d"), Some(json!("x4.0")), "已规范化形态（幂等）");
        assert_eq!(credits_of("e"), None, "无倍率时不要编一个出来");
    }
}
