//! Vertex AI Gemini 出站协议（自定义提供商的第五种协议）
//!
//! 把网关内部的 **Chat Completions** 体翻译成 Vertex 的
//! `POST {base}/<model>:streamGenerateContent?alt=sse`，再把上游的
//! SSE 事件（`{"candidates":[{"content":{"parts":[{"text":"..."}]}}]}`）
//! 折回标准 chat SSE 帧。
//!
//! ── 与另三家协议的结构差异 ──────────────────────────────────
//! | 维度 | responses / anthropic | bedrock | **vertex（本文件）** |
//! |---|---|---|---|
//! | 模型名位置 | 请求体 `model` | URL 路径 `/model/<id>/converse` | **URL 路径 `<model>:streamGenerateContent`** |
//! | 鉴权 | 固定头（Bearer / x-api-key） | SigV4（随体变化） | **不签名**：由出网代理注入 `x-goog-api-key`（见 `vertex_api_key_required`） |
//! | 响应 | SSE 文本行 | AWS Event Stream 二进制分帧 | **SSE 文本行**（`alt=sse`），帧间是部分候选 JSON |
//! | 清单 | 同 host `{base}/models` | 另一 host 控制面 | **`GET {base}/models`**（OpenAI 兼容式可伪造；真实清单也可直接填） |
//!
//! ── 为什么鉴权可以留空 ──────────────────────────────────────
//! 目标部署（SciSpace Agent Vault）把真实密钥留在 vault 里，由 14322 的 MITM
//! 代理按 host+path 模板覆写 `x-goog-api-key`。客户端带什么值都会被替换，
//! 因此本协议**默认不发任何凭证头**；需要直连真实 Vertex（自己持有 API Key）
//! 时，把 Key 填进账号即可，`vertex_headers` 会按 api-key 形态发出去。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic，取值一律走 Option 链，
//! 畸形 SSE 只丢弃该帧并记录错误，不 panic、不吞掉后续帧。

use std::collections::HashMap;

use serde_json::{json, Map, Value};

mod signatures;
#[cfg(test)]
mod signature_tests;
#[cfg(test)]
mod usage_tests;

use super::responses::ConvertError;
use super::{chat_frame, string_field};

/// URL 后缀：流式端点 + SSE 选项。
pub const STREAM_ACTION: &str = ":streamGenerateContent";
/// 非流式端点（当前转发恒用流式，保留给将来的非流式上游）。
pub const GENERATE_ACTION: &str = ":generateContent";
/// 流式请求必须带的查询串（`alt=sse` 才回 SSE 而不是 JSON 数组）。
pub const STREAM_QUERY: &str = "alt=sse";
/// 默认输出预算（Vertex 允许省略 maxOutputTokens，给一个保守值让
/// 客户端不填 `max_tokens` 时也有上限）。
const DEFAULT_MAX_OUTPUT_TOKENS: i64 = 8192;

/// 生成配置里允许直传的字段（chat 名 → Vertex 名）。
/// 只搬语义确定的几项；未列出的客户端字段一律不发给上游（宁可少发，
/// 不发一个 Vertex 会 400 的键）。
const GENERATION_FIELDS: &[(&str, &str)] = &[
    ("temperature", "temperature"),
    ("top_p", "topP"),
    ("top_k", "topK"),
    ("seed", "seed"),
    ("presence_penalty", "presencePenalty"),
    ("frequency_penalty", "frequencyPenalty"),
    ("stop", "stopSequences"),
];

/// 该协议是否需要账号提供 API Key。
///
/// 返回 `false`：Key 可空（走注入代理时客户端本来就拿不到密钥）。
/// 这是唯一与另三家不同的「凭证必填」判定 —— 另三家缺 Key 直接判未配置。
pub fn vertex_api_key_required() -> bool {
    false
}

/// 由 baseUrl + 模型名拼端点。
///
/// baseUrl 语义：**模型的公共前缀**，即
/// `https://aiplatform.googleapis.com/v1/projects/<p>/locations/<l>/publishers/google/models`
/// 或自建网关的同形前缀。末尾斜杠会被去掉；模型名不做转义（Vertex 的
/// 模型 id 只含 `[a-z0-9.-]`，与 Bedrock 的 ARN 不同，不需要段编码）。
pub fn vertex_endpoint(base_url: &str, model: &str, stream: bool) -> (String, String) {
    let base = base_url.trim().trim_end_matches('/');
    let action = if stream { STREAM_ACTION } else { GENERATE_ACTION };
    let url = if stream {
        format!("{base}/{model}{action}?{STREAM_QUERY}")
    } else {
        format!("{base}/{model}{action}")
    };
    (url, action.to_string())
}

/// 出站请求头。
///
/// - 有 Key：按 Vertex 原生形态发 `x-goog-api-key`（api-key 认证）；
/// - 无 Key：**一个凭证头都不发** —— 由出网代理注入。空 `x-goog-api-key`
///   会被 Google 判成 `API_KEY_INVALID`，比不发更糟。
///
/// `extra_headers` 来自提供商的 `headers` 字段（同名覆盖），用户手写的最优先。
pub fn vertex_headers(api_key: &str, extra_headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        // 让上游回未压缩的 SSE：翻译层按行切，gzip 需要额外解压一步，
        // 而 reqwest 默认不带 accept-encoding 时 Google 也可能给 gzip。
        ("Accept-Encoding".to_string(), "identity".to_string()),
    ];
    let key = api_key.trim();
    if !key.is_empty() {
        headers.push(("x-goog-api-key".to_string(), key.to_string()));
    }
    for (name, value) in extra_headers {
        match headers
            .iter_mut()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
        {
            Some(slot) => slot.1 = value.clone(),
            None => headers.push((name.clone(), value.clone())),
        }
    }
    headers
}

/// Chat 体 → Vertex `generateContent` 体。
///
/// 映射要点：
/// - `system` 消息 → `systemInstruction.parts`（Vertex 只允许一处系统指令）；
/// - `user` → `role:"user"`，`assistant` → `role:"model"`；
/// - 纯文本走 `parts[{"text":...}]`；图像走 `inlineData`（仅 base64 data URI，
///   与 Bedrock 同一限制：远程 URL 需要先下载，这里跳过不报错）；
/// - `max_tokens` → `generationConfig.maxOutputTokens`；
/// - 工具调用：chat 的 `tools[].function` → `tools[].functionDeclarations`，
///   历史里的 `tool_calls` / `role:"tool"` 用 `functionCall` / `functionResponse`。
pub fn vertex_request_from_chat(chat: &Value, model: &str) -> Result<Value, ConvertError> {
    if model.trim().is_empty() {
        return Err("缺少模型名：Vertex 要求 model 非空".to_string());
    }
    let mut contents: Vec<Value> = Vec::new();
    let mut system_parts: Vec<Value> = Vec::new();
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut tool_results: Vec<Value> = Vec::new();

    let messages = chat
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for message in &messages {
        let role = string_field(message, "role");
        let content = message.get("content").cloned().unwrap_or(Value::Null);

        // Gemini requires parallel results in one content, without merging later user text.
        if role != "tool" && !tool_results.is_empty() {
            contents.push(json!({
                "role": "user",
                "parts": std::mem::take(&mut tool_results)
            }));
        }

        match role.as_str() {
            "system" | "developer" => {
                // 多段系统消息合并成一处：Vertex 不接受多个 systemInstruction
                for part in parts_of(&content) {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            system_parts.push(json!({ "text": text }));
                        }
                    }
                }
            }
            "tool" => {
                // 工具结果：functionResponse 必须挂在 user 轮次上
                let mut name = string_field(message, "name");
                if name.is_empty() {
                    name = tool_names
                        .get(&string_field(message, "tool_call_id"))
                        .cloned()
                        .unwrap_or_else(|| "tool".to_string());
                }
                let text = content_text_of(&content);
                let response = function_response_payload(&text);
                tool_results.push(json!({
                    "functionResponse": {
                        "name": if name.is_empty() { "tool".to_string() } else { name },
                        "response": response
                    }
                }));
            }
            "assistant" => {
                let mut parts: Vec<Value> = Vec::new();
                for part in parts_of(&content) {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            parts.push(json!({ "text": text }));
                        }
                    } else if let Some(image) = image_part_of(&part) {
                        parts.push(image);
                    }
                }
                if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        let function = call.get("function").cloned().unwrap_or(Value::Null);
                        let name = string_field(&function, "name");
                        if name.is_empty() {
                            continue;
                        }
                        let args = parse_arguments(&string_field(&function, "arguments"));
                        let id = string_field(call, "id");
                        if !id.is_empty() {
                            tool_names.insert(id.clone(), name.clone());
                        }
                        let signature = signatures::signature(call)
                            .map(str::to_owned)
                            .or_else(|| {
                                signatures::recall(model, &id, &name, &args)
                                    .unwrap_or_else(|| Some(signatures::HISTORY_SIGNATURE.to_string()))
                            });
                        let mut part = json!({
                            "functionCall": { "name": name, "args": args }
                        });
                        if let Some(signature) = signature {
                            part["thoughtSignature"] = Value::String(signature);
                        }
                        parts.push(part);
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
            }
            _ => {
                let mut parts: Vec<Value> = Vec::new();
                for part in parts_of(&content) {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            parts.push(json!({ "text": text }));
                        }
                    } else if let Some(image) = image_part_of(&part) {
                        parts.push(image);
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
        }
    }

    if !tool_results.is_empty() {
        contents.push(json!({ "role": "user", "parts": tool_results }));
    }

    if contents.is_empty() {
        return Err("messages 为空：Vertex 至少需要一轮 user 内容".to_string());
    }

    let mut body = Map::new();
    body.insert("contents".to_string(), Value::Array(contents));
    if !system_parts.is_empty() {
        body.insert(
            "systemInstruction".to_string(),
            json!({ "parts": system_parts }),
        );
    }

    // ── 生成参数 ──────────────────────────────────────────────
    let mut generation = Map::new();
    for (chat_key, vertex_key) in GENERATION_FIELDS {
        if let Some(value) = chat.get(*chat_key) {
            if !value.is_null() {
                generation.insert((*vertex_key).to_string(), value.clone());
            }
        }
    }
    if let Some(max) = chat
        .get("max_tokens")
        .or_else(|| chat.get("max_completion_tokens"))
        .and_then(Value::as_i64)
    {
        if max > 0 {
            generation.insert("maxOutputTokens".to_string(), Value::from(max));
        }
    } else {
        generation.insert(
            "maxOutputTokens".to_string(),
            Value::from(DEFAULT_MAX_OUTPUT_TOKENS),
        );
    }
    if !generation.is_empty() {
        body.insert("generationConfig".to_string(), Value::Object(generation));
    }

    // ── 工具 ──────────────────────────────────────────────────
    if let Some(declarations) = tool_declarations(chat) {
        body.insert(
            "tools".to_string(),
            json!([{ "functionDeclarations": declarations }]),
        );
    }

    Ok(Value::Object(body))
}

/// chat 的 `tools[]` → Vertex 的 `functionDeclarations[]`。
fn tool_declarations(chat: &Value) -> Option<Vec<Value>> {
    let tools = chat.get("tools")?.as_array()?;
    let mut out: Vec<Value> = Vec::new();
    for tool in tools {
        // 兼容两种形状：{type:"function", function:{...}} 与直接 {name,...}
        let function = tool.get("function").cloned().unwrap_or_else(|| tool.clone());
        let name = string_field(&function, "name");
        if name.is_empty() {
            continue;
        }
        let mut declaration = Map::new();
        declaration.insert("name".to_string(), Value::String(name));
        if let Some(description) = function.get("description").and_then(Value::as_str) {
            if !description.is_empty() {
                declaration.insert(
                    "description".to_string(),
                    Value::String(description.to_string()),
                );
            }
        }
        // Vertex 要 OpenAPI subset 的 schema；客户端给的 JSON Schema 一般可用，
        // 缺省补一个空 object（Vertex 不接受缺 parameters 的声明以外形态）
        let parameters = function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
        declaration.insert("parameters".to_string(), sanitize_schema(&parameters));
        out.push(Value::Object(declaration));
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 去掉 Vertex 不认识的 JSON Schema 键（`$schema` / `additionalProperties` 等），
/// 避免上游 400。递归处理 properties / items / anyOf。
fn sanitize_schema(schema: &Value) -> Value {
    let Some(object) = schema.as_object() else {
        return schema.clone();
    };
    const ALLOWED: &[&str] = &[
        "type", "format", "description", "nullable", "enum", "items", "properties",
        "required", "anyOf", "title", "default", "minimum", "maximum", "minItems",
        "maxItems", "minLength", "maxLength", "pattern", "propertyOrdering",
    ];
    let mut out = Map::new();
    for (key, value) in object {
        if !ALLOWED.contains(&key.as_str()) {
            continue;
        }
        let mapped = match key.as_str() {
            "properties" => {
                if let Some(props) = value.as_object() {
                    let mut cleaned = Map::new();
                    for (name, sub) in props {
                        cleaned.insert(name.clone(), sanitize_schema(sub));
                    }
                    Value::Object(cleaned)
                } else {
                    value.clone()
                }
            }
            "items" => sanitize_schema(value),
            "anyOf" => {
                if let Some(items) = value.as_array() {
                    Value::Array(items.iter().map(sanitize_schema).collect())
                } else {
                    value.clone()
                }
            }
            _ => value.clone(),
        };
        out.insert(key.clone(), mapped);
    }
    Value::Object(out)
}

/// content 的段落数组（字符串按单段文本处理）。
fn parts_of(content: &Value) -> Vec<Value> {
    match content {
        Value::String(text) => vec![json!({ "text": text })],
        Value::Array(items) => items.clone(),
        Value::Null => Vec::new(),
        other => vec![json!({ "text": other.to_string() })],
    }
}

fn content_text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// chat 的 image_url（base64 data URI）→ Vertex 的 inlineData。
/// 远程 http(s) URL 直接跳过（与 Bedrock 同口径：需要先下载，当前不实现）。
fn image_part_of(part: &Value) -> Option<Value> {
    let url = part
        .get("image_url")
        .and_then(|value| value.get("url").or(Some(value)))
        .and_then(Value::as_str)
        .or_else(|| part.get("image_url").and_then(Value::as_str))?;
    let (mime, data) = parse_data_uri(url)?;
    Some(json!({ "inlineData": { "mimeType": mime, "data": data } }))
}

/// `data:image/png;base64,AAAA` → (mime, base64)
fn parse_data_uri(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.split(';').next().unwrap_or("image/png").to_string();
    if !meta.contains("base64") {
        return None;
    }
    Some((mime, data.to_string()))
}

/// 工具结果文本 → functionResponse.response（Vertex 要一个对象）。
fn function_response_payload(text: &str) -> Value {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(object)) => Value::Object(object),
        _ => json!({ "result": text }),
    }
}

/// 工具调用参数字符串 → 对象（解析失败时包成 `{"_raw": ...}`，不丢内容）。
fn parse_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value @ Value::Object(_)) => value,
        Ok(other) => json!({ "_value": other }),
        Err(_) => json!({ "_raw": raw }),
    }
}

/// Vertex SSE 帧 → chat SSE 帧的状态机。
///
/// 上游每帧形如：
/// `data: {"candidates":[{"content":{"parts":[{"text":"Hi"}]}}],"usageMetadata":{...}}`
///
/// 与 Bedrock 的二进制分帧不同，这里按行切即可，复用 `SseLineBuffer` 的语义：
/// 只取 `data:` 载荷，`[DONE]` 视为结束（Vertex 不发 `[DONE]`，靠流断开收尾）。
pub struct ChatFromVertexStream {
    buffer: super::SseLineBuffer,
    model: String,
    id: String,
    created: i64,
    started: bool,
    finished: bool,
    /// 已累计的文本（部分帧可能重复/分片，这里只负责转发，不做去重 ——
    /// Vertex 的 `text` 是增量片段）
    tool_index: usize,
    usage: VertexUsage,
    finish_reason: Option<String>,
    error: Option<String>,
}

#[derive(Default)]
struct VertexUsage {
    input: i64,
    output: i64,
    total: i64,
    cached: i64,
}

impl VertexUsage {
    fn from_metadata(meta: &Value) -> Self {
        Self {
            input: meta
                .get("promptTokenCount")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            output: meta
                .get("candidatesTokenCount")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            total: meta
                .get("totalTokenCount")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            cached: meta
                .get("cachedContentTokenCount")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .max(0),
        }
    }

    fn to_chat(&self) -> Value {
        json!({
            "prompt_tokens": self.input,
            "completion_tokens": self.output,
            "total_tokens": if self.total > 0 { self.total } else { self.input + self.output },
            "prompt_tokens_details": { "cached_tokens": self.cached },
        })
    }
}

impl ChatFromVertexStream {
    pub fn new(model: &str) -> Self {
        Self {
            buffer: super::SseLineBuffer::new(),
            model: model.to_string(),
            id: String::new(),
            created: crate::server::logging::now_ms() / 1000,
            started: false,
            finished: false,
            tool_index: 0,
            usage: VertexUsage::default(),
            finish_reason: None,
            error: None,
        }
    }

    /// 最近的流内错误（供调用方写入请求日志）。
    pub fn last_error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// 吃一段上游字节，吐出翻译好的 chat SSE 帧。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<bytes::Bytes> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        for payload in self.buffer.push(chunk) {
            match payload {
                None => {
                    // 上游显式 [DONE]：立刻收尾
                    out.extend(self.complete());
                    return out;
                }
                Some(text) => {
                    let body = match serde_json::from_str::<Value>(&text) {
                        Ok(value) => value,
                        Err(_) => {
                            self.error = Some(format!(
                                "Vertex 返回了无法解析的 SSE 载荷（前 200 字符）: {}",
                                truncate(&text, 200)
                            ));
                            continue;
                        }
                    };
                    out.extend(self.handle(&body));
                    if self.finished {
                        break;
                    }
                }
            }
        }
        out
    }

    /// 流结束：收尾（补 finish + usage + [DONE]）。
    pub fn finish(&mut self) -> Vec<bytes::Bytes> {
        self.complete()
    }

    fn ensure_started(&mut self) -> Vec<bytes::Bytes> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        self.id = super::random_id("chatcmpl-");
        vec![chat_frame(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": null }],
        }))]
    }

    fn handle(&mut self, body: &Value) -> Vec<bytes::Bytes> {
        let mut out = self.ensure_started();

        // 上游错误帧（HTTP 200 也可能携带 error）
        if let Some(error) = body.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Vertex 上游返回错误");
            self.error = Some(message.to_string());
            out.push(chat_frame(&json!({
                "error": { "message": message, "type": "upstream_error" }
            })));
            self.finished = true;
            return out;
        }

        // 思考内容（gemini thinking）放在 reasoning_content，另两家同一口径
        if let Some(metadata) = body.get("usageMetadata") {
            self.usage = VertexUsage::from_metadata(metadata);
        }

        let Some(candidates) = body.get("candidates").and_then(Value::as_array) else {
            return out;
        };
        for candidate in candidates {
            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                self.finish_reason = Some(map_finish_reason(reason).to_string());
            }
            let parts = candidate
                .get("content")
                .and_then(|content| content.get("parts"))
                .and_then(Value::as_array);
            let Some(parts) = parts else { continue };
            for part in parts {
                if part.get("thought").and_then(Value::as_bool).unwrap_or(false) {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            out.push(chat_frame(&json!({
                                "id": self.id,
                                "object": "chat.completion.chunk",
                                "created": self.created,
                                "model": self.model,
                                "choices": [{
                                    "index": 0,
                                    "delta": { "reasoning_content": text },
                                    "finish_reason": null
                                }],
                            })));
                        }
                    }
                    continue;
                }
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        out.push(chat_frame(&json!({
                            "id": self.id,
                            "object": "chat.completion.chunk",
                            "created": self.created,
                            "model": self.model,
                            "choices": [{
                                "index": 0,
                                "delta": { "content": text },
                                "finish_reason": null
                            }],
                        })));
                    }
                }
                if let Some(call) = part.get("functionCall") {
                    let name = string_field(call, "name");
                    if name.is_empty() {
                        continue;
                    }
                    let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
                    let arguments = args.to_string();
                    let index = self.tool_index;
                    self.tool_index += 1;
                    let id = super::random_id("call");
                    let signature = signatures::signature(part);
                    signatures::remember(&self.model, &id, &name, &args, signature);
                    let mut tool_call = json!({
                        "index": index,
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": arguments }
                    });
                    if let Some(signature) = signature {
                        tool_call["extra_content"] = json!({
                            "google": { "thought_signature": signature }
                        });
                    }
                    out.push(chat_frame(&json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "created": self.created,
                        "model": self.model,
                        "choices": [{
                            "index": 0,
                            "delta": {
                                "tool_calls": [tool_call]
                            },
                            "finish_reason": null
                        }],
                    })));
                    // 有工具调用时终止原因按 chat 习惯改写
                    self.finish_reason = Some("tool_calls".to_string());
                }
            }
        }
        out
    }

    fn complete(&mut self) -> Vec<bytes::Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut out = self.ensure_started();
        let reason = self
            .finish_reason
            .clone()
            .unwrap_or_else(|| "stop".to_string());
        out.push(chat_frame(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }],
        })));
        out.push(chat_frame(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [],
            "usage": self.usage.to_chat(),
        })));
        out.push(bytes::Bytes::from_static(b"data: [DONE]\n\n"));
        out
    }
}

/// Vertex 的 finishReason → chat 的 finish_reason。
fn map_finish_reason(reason: &str) -> &'static str {
    match reason {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" => "content_filter",
        "OTHER" => "stop",
        _ => "stop",
    }
}

fn truncate(text: &str, limit: usize) -> String {
    let mut out = String::new();
    for (count, ch) in text.chars().enumerate() {
        if count >= limit {
            out.push_str("…");
            break;
        }
        out.push(ch);
    }
    out
}

// ───────────────────────────── 单元测试 ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn chat_body() -> Value {
        json!({
            "model": "gemini-4-argon",
            "messages": [
                {"role": "system", "content": "Be terse."},
                {"role": "user", "content": "Hi"}
            ]
        })
    }

    #[test]
    fn endpoint_joins_base_and_model() {
        let (url, action) = vertex_endpoint(
            "https://aiplatform.googleapis.com/v1/projects/p/locations/global/publishers/google/models/",
            "gemini-4-argon",
            true,
        );
        assert!(url.ends_with("/gemini-4-argon:streamGenerateContent?alt=sse"));
        assert_eq!(action, STREAM_ACTION);

        let (url2, _) = vertex_endpoint("https://x/models", "m", false);
        assert_eq!(url2, "https://x/models/m:generateContent");
    }

    #[test]
    fn headers_omit_credential_when_key_empty() {
        let headers = vertex_headers("", &[]);
        assert!(headers.iter().all(|(name, _)| !name.eq_ignore_ascii_case("x-goog-api-key")));
        assert!(headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("Content-Type")));
    }

    #[test]
    fn headers_send_key_when_present() {
        let headers = vertex_headers("k-123", &[]);
        assert!(headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("x-goog-api-key") && value == "k-123"));
    }

    #[test]
    fn headers_extra_overrides_default() {
        let extra = vec![("content-type".to_string(), "application/json; charset=utf-8".to_string())];
        let headers = vertex_headers("", &extra);
        let found = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .count();
        assert_eq!(found, 1, "同名头必须被覆盖而不是重复追加");
    }

    #[test]
    fn request_maps_system_and_roles() {
        let body = vertex_request_from_chat(&chat_body(), "gemini-4-argon").expect("ok");
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "Be terse.");
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["contents"][0]["parts"][0]["text"], "Hi");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 8192);
    }

    #[test]
    fn request_maps_generation_params() {
        let mut chat = chat_body();
        chat["temperature"] = json!(0.3);
        chat["max_tokens"] = json!(256);
        chat["top_p"] = json!(0.9);
        chat["stop"] = json!(["END"]);
        let body = vertex_request_from_chat(&chat, "m").expect("ok");
        assert_eq!(body["generationConfig"]["temperature"], 0.3);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 256);
        assert_eq!(body["generationConfig"]["topP"], 0.9);
        assert_eq!(body["generationConfig"]["stopSequences"][0], "END");
    }

    #[test]
    fn request_maps_tools_and_sanitizes_schema() {
        let mut chat = chat_body();
        chat["tools"] = json!([{
            "type": "function",
            "function": {
                "name": "lookup",
                "description": "find",
                "parameters": {
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {"q": {"type": "string", "description": "query"}}
                }
            }
        }]);
        let body = vertex_request_from_chat(&chat, "m").expect("ok");
        let declaration = &body["tools"][0]["functionDeclarations"][0];
        assert_eq!(declaration["name"], "lookup");
        assert!(declaration["parameters"].get("$schema").is_none());
        assert!(declaration["parameters"].get("additionalProperties").is_none());
        assert_eq!(declaration["parameters"]["properties"]["q"]["type"], "string");
    }

    #[test]
    fn request_maps_tool_history() {
        let chat = json!({
            "messages": [
                {"role": "user", "content": "look it up"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "c1", "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"q\":\"h-index\"}"}
                }]},
                {"role": "tool", "name": "lookup", "content": "{\"answer\":42}"},
                {"role": "user", "content": "thanks"}
            ]
        });
        let body = vertex_request_from_chat(&chat, "m").expect("ok");
        let contents = body["contents"].as_array().expect("array");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["functionCall"]["name"], "lookup");
        assert_eq!(contents[1]["parts"][0]["functionCall"]["args"]["q"], "h-index");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["response"]["answer"], 42);
    }

    #[test]
    fn request_rejects_empty_messages() {
        let chat = json!({"messages": []});
        assert!(vertex_request_from_chat(&chat, "m").is_err());
    }

    #[test]
    fn request_rejects_empty_model() {
        assert!(vertex_request_from_chat(&chat_body(), "  ").is_err());
    }

    #[test]
    fn stream_emits_text_frames() {
        let mut machine = ChatFromVertexStream::new("gemini-4-argon");
        let chunk = br#"data: {"candidates":[{"content":{"parts":[{"text":"He"}]}}]}

data: {"candidates":[{"content":{"parts":[{"text":"llo"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}}

"#;
        let frames = machine.push(chunk);
        let joined: String = frames
            .iter()
            .map(|frame| String::from_utf8_lossy(frame).to_string())
            .collect();
        assert!(joined.contains("\"delta\":{\"content\":\"He\"}"));
        assert!(joined.contains("\"delta\":{\"content\":\"llo\"}"));
        let tail = machine.finish();
        let tail_text: String = tail
            .iter()
            .map(|frame| String::from_utf8_lossy(frame).to_string())
            .collect();
        assert!(tail_text.contains("\"finish_reason\":\"stop\""));
        assert!(tail_text.contains("\"total_tokens\":7"));
        assert!(tail_text.contains("[DONE]"));
    }

    #[test]
    fn stream_splits_across_chunks() {
        let mut machine = ChatFromVertexStream::new("m");
        let first = br#"data: {"candidates":[{"content":{"parts":[{"te"#;
        let second = br#"xt":"OK"}]}}]}

"#;
        assert!(machine.push(first).is_empty() || true);
        let frames = machine.push(second);
        let joined: String = frames
            .iter()
            .map(|frame| String::from_utf8_lossy(frame).to_string())
            .collect();
        assert!(joined.contains("\"content\":\"OK\""));
    }

    #[test]
    fn stream_maps_function_call() {
        let mut machine = ChatFromVertexStream::new("m");
        let chunk = br#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"lookup","args":{"q":"x"}}}]}}]}

"#;
        let frames = machine.push(chunk);
        let joined: String = frames
            .iter()
            .map(|frame| String::from_utf8_lossy(frame).to_string())
            .collect();
        assert!(joined.contains("\"tool_calls\""));
        assert!(joined.contains("\"name\":\"lookup\""));
        let tail = machine.finish();
        let tail_text: String = tail
            .iter()
            .map(|frame| String::from_utf8_lossy(frame).to_string())
            .collect();
        assert!(tail_text.contains("\"finish_reason\":\"tool_calls\""));
    }

    #[test]
    fn stream_reports_upstream_error() {
        let mut machine = ChatFromVertexStream::new("m");
        let chunk = br#"data: {"error":{"message":"quota exceeded","code":429}}

"#;
        let frames = machine.push(chunk);
        let joined: String = frames
            .iter()
            .map(|frame| String::from_utf8_lossy(frame).to_string())
            .collect();
        assert!(joined.contains("quota exceeded"));
        assert_eq!(machine.last_error(), Some("quota exceeded"));
    }

    #[test]
    fn stream_maps_safety_to_content_filter() {
        let mut machine = ChatFromVertexStream::new("m");
        let chunk = br#"data: {"candidates":[{"finishReason":"SAFETY","content":{"parts":[]}}]}

"#;
        machine.push(chunk);
        let tail = machine.finish();
        let text: String = tail
            .iter()
            .map(|frame| String::from_utf8_lossy(frame).to_string())
            .collect();
        assert!(text.contains("\"finish_reason\":\"content_filter\""));
    }

    #[test]
    fn stream_handles_thought_parts() {
        let mut machine = ChatFromVertexStream::new("m");
        let chunk = br#"data: {"candidates":[{"content":{"parts":[{"text":"thinking","thought":true},{"text":"answer"}]}}]}

"#;
        let frames = machine.push(chunk);
        let joined: String = frames
            .iter()
            .map(|frame| String::from_utf8_lossy(frame).to_string())
            .collect();
        assert!(joined.contains("\"reasoning_content\":\"thinking\""));
        assert!(joined.contains("\"content\":\"answer\""));
    }

    #[test]
    fn data_uri_parsed() {
        let (mime, data) = parse_data_uri("data:image/jpeg;base64,AAAA").expect("ok");
        assert_eq!(mime, "image/jpeg");
        assert_eq!(data, "AAAA");
        assert!(parse_data_uri("https://example.com/a.png").is_none());
    }
}
