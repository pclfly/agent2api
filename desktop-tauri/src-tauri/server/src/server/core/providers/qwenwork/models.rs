//! QwenWork model catalog: persistent cache and two reference fallback entries.
use super::{auth, refresh};
use crate::server::core::{
    account_store::AccountStore,
    providers::{adapter::ModelRefreshOutcome, catalog_cache},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    sync::{OnceLock, RwLock},
};
#[derive(Default)]
struct Cache {
    models: Vec<Value>,
    at: i64,
}
fn cache() -> &'static RwLock<Cache> {
    static C: OnceLock<RwLock<Cache>> = OnceLock::new();
    C.get_or_init(|| {
        RwLock::new(
            catalog_cache::load("qwenwork")
                .map(|c| Cache {
                    models: c.models,
                    at: c.fetched_at,
                })
                .unwrap_or_default(),
        )
    })
}
pub fn last_refreshed_at() -> i64 {
    cache().read().map(|c| c.at).unwrap_or(0)
}
pub fn remote_refreshed() -> bool {
    cache()
        .read()
        .map(|c| !c.models.is_empty())
        .unwrap_or(false)
}
pub fn list() -> Vec<Value> {
    if let Ok(c) = cache().read() {
        if !c.models.is_empty() {
            return c.models.clone();
        }
    }
    ["qwork-auto","qwork-advanced"].iter().map(|id|json!({"id":id,"name":id,"upstreamKey":id,"modelSource":"system","maxInputTokens":1_000_000,"supportsToolCall":true,"isDefault":*id=="qwork-auto"})).collect()
}
pub fn resolve(id: &str) -> Option<Value> {
    list()
        .into_iter()
        .find(|v| v.get("id").and_then(Value::as_str) == Some(id))
}
pub fn parse(value: &Value) -> Vec<Value> {
    let root = value.get("data").filter(|v| v.is_object()).unwrap_or(value);
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for scene in ["qwork", "chat", "assistant", "developer", "experts"] {
        for m in root
            .get(scene)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = super::credentials::text(m, &["key"]);
            if id.is_empty()
                || m.get("enable") == Some(&Value::Bool(false))
                || !seen.insert(id.clone())
            {
                continue;
            }
            let mut item = json!({"id":id,"name":super::credentials::text(m,&["display_name","key"]),
                "upstreamKey":id,"modelSource":m.get("source").and_then(Value::as_str).filter(|s|!s.is_empty()).unwrap_or("system"),
                "description":m.get("description"),"isDefault":m.get("is_default").and_then(Value::as_bool).unwrap_or(false),
                "supportsToolCall":true,"contextConfig":m.get("context_config"),"thinkingConfig":m.get("thinking_config"),
                "priceFactor":m.get("price_factor")});
            for (src, dst) in [
                ("max_input_tokens", "maxInputTokens"),
                ("max_output_tokens", "maxOutputTokens"),
                ("is_vl", "supportsImages"),
                ("is_reasoning", "supportsReasoning"),
            ] {
                if let Some(v) = m.get(src) {
                    item[dst] = v.clone();
                }
            }
            // price_factor is upstream metadata, not proof of a free request or a debit.
            if let Some(n) = m
                .get("price_factor")
                .and_then(Value::as_f64)
                .filter(|n| n.is_finite() && *n > 0.0)
            {
                item["credits"] = json!(format!("x{n} credits"));
            }
            out.push(item);
        }
    }
    out
}
pub async fn fetch(store: &AccountStore, id: &str, force: bool) -> ModelRefreshOutcome {
    if !force && last_refreshed_at() > crate::server::logging::now_ms() - 3_600_000 {
        return ModelRefreshOutcome::unchanged();
    }
    let result = async {
        let r = store.qwenwork_account_record(id).ok_or_else(|| {
            crate::server::errors::GatewayError::bad_request("请先添加 QwenWork 账号")
        })?;
        let c = refresh::ensure(store, &super::credentials::text(&r, &["id"]), false).await?;
        let p = auth::proxy(&r)?;
        let body = auth::get(&c, auth::MODELS, true, p.as_ref()).await?;
        let models = parse(&body);
        if models.is_empty() {
            return Err(crate::server::errors::GatewayError::with_status(
                502,
                "QwenWork 未返回可用模型，保留原清单",
            ));
        }
        let at = crate::server::logging::now_ms();
        let count = models.len();
        catalog_cache::save("qwenwork", &models, at);
        if let Ok(mut c) = cache().write() {
            *c = Cache { models, at };
        }
        Ok(count)
    }
    .await;
    match result {
        Ok(n) => ModelRefreshOutcome::refreshed(n),
        Err(e) => ModelRefreshOutcome::failed(e.message),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_scenes_dedupe_and_disable() {
        let v = parse(
            &json!({"qwork":[{"key":"a","enable":true,"source":"system","price_factor":0.0},{"key":"off","enable":false}],"chat":[{"key":"a"},{"key":"b"}]}),
        );
        assert_eq!(v.len(), 2);
        assert_eq!(v[0]["id"], "a");
        assert_eq!(v[1]["id"], "b");
    }
}
