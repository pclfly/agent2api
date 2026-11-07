//! 账号批量管理端点；配置写入复用单账号 apply_patch。

use axum::body::Bytes;
use axum::response::Response;
use serde_json::{json, Map, Value};

use super::accounts::{parse_json_body, store_error};
use crate::server::errors::management_error;
use crate::server::http::ok_json;
use crate::server::ServerState;

pub async fn batch_accounts(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_json_body(&body, "请求内容不是有效 JSON") {
        Ok(value) => value,
        Err(response) => return response,
    };
    // ids 去重 + 过滤空串，逐字照抄 Node 版
    let mut ids: Vec<String> = Vec::new();
    if let Some(items) = payload.get("ids").and_then(Value::as_array) {
        for item in items {
            if let Some(text) = item.as_str() {
                if !text.is_empty() && !ids.iter().any(|existing| existing == text) {
                    ids.push(text.to_string());
                }
            }
        }
    }
    if ids.is_empty() {
        return management_error(400, "缺少要操作的账号 id");
    }
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    // 批量改代理后要不要去启用它引用的池条目，得看**写入结果**而不是请求内容：
    // 这条批量只携带 proxy，所以「有一条账号记下了变更」就等价于「代理真的变了」
    // （全部被准入校验拦下时一条变更都不会有，那种情况不该去动池）。
    let mut proxy_target: Option<Value> = None;
    let result = match action.as_str() {
        "remove" => state.store().batch_remove(&ids),
        "enable" | "disable" => {
            let enabled = action == "enable";
            state.store().batch_update(&ids, &json!({ "enabled": enabled }))
        }
        "proxy" => {
            // 允许 proxy=null（批量改为直连）；缺字段是显式报错
            let Some(target) = payload.get("proxy") else {
                return management_error(400, "批量修改代理时缺少 proxy 字段");
            };
            proxy_target = crate::server::core::proxies::normalize_account_proxy(target)
                .unwrap_or(None);
            state.store().batch_update(&ids, &json!({ "proxy": target }))
        }
        "concurrency" => {
            // 并发上限（0 = 不限）；缺字段是显式报错，值本身由 store 归一
            // （非负整数、封顶 999，与单账号设置面板同一份校验）
            let Some(target) = payload.get("maxConcurrent") else {
                return management_error(400, "批量设置并发上限时缺少 maxConcurrent 字段");
            };
            state
                .store()
                .batch_update(&ids, &json!({ "maxConcurrent": target }))
        }
        "usageQuery" => {
            let Some(target) = payload.get("usageQuery") else {
                return management_error(400, "批量查询设置缺少 usageQuery 字段");
            };
            state.store().batch_update(&ids, &json!({ "usageQuery": target }))
        }
        "limiters" => {
            let Some(mode) = payload.get("limiterMode") else {
                return management_error(400, "批量限制器操作缺少 limiterMode 字段");
            };
            let mut patch = Map::new();
            patch.insert("limiterMode".to_string(), mode.clone());
            if let Some(groups) = payload.get("limiterGroups") {
                patch.insert("limiterGroups".to_string(), groups.clone());
            }
            if let Some(rules) = payload.get("limiters") {
                patch.insert("limiters".to_string(), rules.clone());
            }
            state.store().batch_update(&ids, &Value::Object(patch))
        }
        other => {
            return management_error(
                400,
                format!(
                    "不支持的批量操作: {}",
                    if other.is_empty() { "(空)" } else { other }
                ),
            )
        }
    };

    match result {
        Ok(value) => {
            // 响应带上 action，与 Node 版 `{ action, ...result, list }` 一致
            let mut merged = Map::new();
            merged.insert("action".to_string(), Value::String(action));
            if let Some(object) = value.as_object() {
                for (key, item) in object {
                    merged.insert(key.clone(), item.clone());
                }
            }
            // 批量改代理成功后才动池条目：写失败时不该把节点悄悄打开。
            // 写进 `autoEnabledProxy`，与单账号那条的 `changes` 说明同口径。
            if let Some(target) = proxy_target {
                let touched = value
                    .get("ok")
                    .and_then(Value::as_array)
                    .map(|items| items.iter().any(|item| {
                        item.get("changes")
                            .and_then(Value::as_array)
                            .is_some_and(|changes| !changes.is_empty())
                    }))
                    .unwrap_or(false);
                if touched {
                    if let Some(note) = super::proxies::enable_referenced_proxy(Some(&target)) {
                        crate::server::logging::log(
                            "[Accounts]",
                            &format!("批量改代理：{note}"),
                        );
                        merged.insert("autoEnabledProxy".to_string(), Value::String(note));
                    }
                    // 这些账号刚引用的订阅节点可能从没分配过监听端口（内核上一轮
                    // 启动时还没人引用它）—— 那种情况启用那一步没有可做的，
                    // 但转发仍会报「未分配端口」，所以每次改代理都对一次账。
                    let mut body = Value::Object(merged);
                    super::proxies::annotate_listener_reconcile(
                        &mut body,
                        super::proxies::reconcile_listeners(state.store()),
                    );
                    return ok_json(body);
                }
            }
            ok_json(Value::Object(merged))
        }
        Err(error) => store_error(error),
    }
}
