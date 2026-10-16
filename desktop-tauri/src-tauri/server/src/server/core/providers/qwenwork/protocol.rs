//! OpenAI messages remain OpenAI-shaped; only the top-level QwenWork envelope changes.
use super::credentials::Credentials;
use crate::server::errors::GatewayError;
use axum::http::HeaderMap;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

pub fn build(c: &Credentials, input: &Value, headers: &HeaderMap) -> Result<Value, GatewayError> {
    let id = input.get("model").and_then(Value::as_str).unwrap_or("");
    let model = super::models::resolve(id).ok_or_else(|| {
        GatewayError::bad_request("QwenWork 模型不存在，请刷新模型清单")
            .with_code("model_not_found")
    })?;
    let request_id = super::super::qoder::cosy::random_uuid()?;
    let messages = input
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| GatewayError::bad_request("QwenWork 请求缺少 messages"))?;
    // Prefer an explicit session ID. Fallback hashes the first user message and account,
    // avoiding global sharing of the reference implementation's greeting-only session key.
    let explicit = ["x-session-id", "x-conversation-id"]
        .iter()
        .find_map(|k| headers.get(*k).and_then(|v| v.to_str().ok()))
        .or_else(|| input.get("session_id").and_then(Value::as_str));
    let seed = explicit.map(str::to_string).unwrap_or_else(|| {
        messages
            .iter()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            .map(Value::to_string)
            .unwrap_or_else(|| request_id.clone())
    });
    let digest = format!(
        "{:x}",
        Sha256::digest(format!("qwenwork:session:v1\0{}\0{id}\0{seed}", c.user_id))
    );
    let session_id = format!(
        "{}-{}-{}-{}-{}",
        &digest[..8],
        &digest[8..12],
        &digest[12..16],
        &digest[16..20],
        &digest[20..32]
    );
    let mut parameters = Map::new();
    parameters.insert("context_length".into(), json!(1_000_000));
    for key in [
        "max_tokens",
        "temperature",
        "top_p",
        "stop",
        "reasoning_effort",
        "tool_choice",
    ] {
        if let Some(v) = input.get(key) {
            parameters.insert(key.into(), v.clone());
        }
    }
    if !parameters.contains_key("max_tokens") {
        if let Some(v) = input.get("max_completion_tokens") {
            parameters.insert("max_tokens".into(), v.clone());
        }
    }
    let messages: Vec<Value> = messages
        .iter()
        .map(|m| {
            let mut out = Map::new();
            for key in [
                "role",
                "content",
                "name",
                "tool_call_id",
                "tool_calls",
                "reasoning_content",
            ] {
                if let Some(v) = m.get(key) {
                    out.insert(key.into(), v.clone());
                }
            }
            if out.get("role").and_then(Value::as_str) == Some("developer") {
                out.insert("role".into(), json!("system"));
            }
            Value::Object(out)
        })
        .collect();
    let mut body = json!({"request_id":request_id,"session_id":session_id,
        "model_config":{"key":model.get("upstreamKey").unwrap_or(&json!(id)),"source":model.get("modelSource").and_then(Value::as_str).unwrap_or("system")},
        "messages":messages,"stream":true,"stream_options":{"include_usage":true},
        "metadata":{"context":{"request_id":request_id,"session_id":session_id,"client_type":"5"}},"parameters":parameters});
    if let Some(tools) = input.get("tools") {
        body["tools"] = tools.clone();
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn envelope_preserves_tools_and_places_parameters() {
        let c = Credentials::from_value(&json!({"token":"test","userId":"demo"})).unwrap();
        let b=build(&c,&json!({"model":"qwork-auto","max_tokens":32,"tool_choice":"required","messages":[{"role":"assistant","tool_calls":[{"id":"x","function":{"name":"f","arguments":"{}"}}]},{"role":"tool","tool_call_id":"x","content":"ok"}]}),&HeaderMap::new()).unwrap();
        assert_eq!(b["parameters"]["max_tokens"], 32);
        assert!(b.get("max_tokens").is_none());
        assert_eq!(b["messages"][1]["tool_call_id"], "x");
        assert_eq!(b["model_config"]["key"], "qwork-auto");
    }
}
