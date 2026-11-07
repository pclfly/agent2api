//! AWS Bedrock 出站协议（Converse API）—— chat 体 ↔ Bedrock 请求/事件流 的双向翻译。
//!
//! ── 为什么 Bedrock 需要独立的翻译链 ──────────────────────────
//! Bedrock 与 OpenAI 兼容接口是两套完全不同的调用模型：
//!   - **认证**：AWS SigV4（每个请求用 SecretAccessKey 现场算 HMAC-SHA256 签名），
//!     不是 `Authorization: Bearer`。签名覆盖 method / path / query / 头 / body 哈希，
//!     所以**必须**在请求体最终定稿之后才能算（见 [`sign_request`]）；
//!   - **凭据形态**：二元组 `(AccessKeyId, SecretAccessKey)`，不是单个 key。
//!     账号模型只有一个 `apiKey` 字段，因此约定用 `AK:SK` 形态承载
//!     （见 [`split_credentials`]）；
//!   - **端点**：`https://bedrock-runtime.<region>.amazonaws.com/model/<modelId>/converse`，
//!     模型名在**路径**里而不是 body 里；
//!   - **响应**：`converse-stream` 返回 **AWS Event Stream**（二进制分帧，
//!     `[4B 总长][4B 头长][4B 前序CRC][headers][payload][4B 消息CRC]`），
//!     不是 SSE 的 `data:` 文本行。
//!
//! ── 数据流 ──────────────────────────────────────────────────
//! ```text
//!   客户端 chat 体
//!     → bedrock_request_from_chat()      翻译成 Converse 体
//!     → sign_request()                   SigV4 签名（headers）
//!     → POST {base}/model/{id}/converse-stream
//!     ← AWS Event Stream 二进制
//!     → ChatFromBedrockStream::push()    逐事件折成 chat SSE 帧
//!     → 既有 ForwardStream / 聚合器       （reasoning 合并 / usage / model 回写）
//! ```
//!
//! ── 事件流帧格式（实测，2026-10-12，us.anthropic.claude-sonnet-4-6）────
//! ```text
//!   messageStart      {"role":"assistant"}
//!   contentBlockDelta {"contentBlockIndex":0,"delta":{"text":"Hi"}}
//!   contentBlockStop  {"contentBlockIndex":0}
//!   messageStop       {"stopReason":"end_turn"}
//!   metadata          {"usage":{"inputTokens":14,"outputTokens":12,"totalTokens":26}}
//! ```
//! 工具调用走 `contentBlockStart`（`start.toolUse`）+ `contentBlockDelta`
//! （`delta.toolUse.input` 的**增量 JSON 片段**）+ `contentBlockStop`。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。分帧遇到畸形数据
//! 一律**停止解析并保留已产出的帧**（不 panic、不吞掉整个流）。

use hmac::{Hmac, Mac};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use super::{
    chat_frame, content_parts, random_id, FIELD_CACHE_CONTROL, FIELD_IS_ERROR,
};
use super::anthropic::parse_json_object;
use super::responses::ConvertError;

/// SigV4 签名用的算法标识（AWS 固定值）
const ALGORITHM: &str = "AWS4-HMAC-SHA256";
/// 服务名（Bedrock 的 SigV4 scope 里用 `bedrock`，不是 `bedrock-runtime`）
const SERVICE: &str = "bedrock";
/// 默认 region（账号里没配 region 时用；us-west-2 是 eazo 的实测可用区）
pub const DEFAULT_REGION: &str = "us-west-2";
/// 未提供有效输出预算时的兼容默认值（Converse 的 maxTokens 本身可省略）。
const DEFAULT_MAX_TOKENS: i64 = 4096;
/// 本次 Kimi K3 上游错误要求 maxTokens 严格低于 128000；这是保守出站保护，
/// 不是所有 Bedrock 模型的输出上限，也不是输入 + 输出的总上下文窗口。
const KIMI_K3_SAFE_MAX_TOKENS: i64 = 127_999;

/// 只规范有明确证据的模型预算；未知模型不猜上限。
fn output_budget(chat: &Value, model: &str) -> i64 {
    // 新字段优先，旧字段作为兼容回落；与 Responses 转换器保持一致。
    let requested = ["max_completion_tokens", "max_tokens"]
        .iter()
        .find_map(|key| chat.get(*key).and_then(Value::as_i64).filter(|v| *v > 0))
        .unwrap_or(DEFAULT_MAX_TOKENS);
    let normalized = model.trim().to_ascii_lowercase();
    // 基础模型与系统 inference profile 的 ARN 可读出模型 ID；application
    // profile 是不透明 ID，不能从中猜模型（原样交给上游校验）。
    let id = if normalized.starts_with("arn:") {
        let mut parts = normalized.splitn(6, ':');
        if parts.nth(2) != Some("bedrock") {
            return requested;
        }
        let Some(resource) = parts.nth(2) else {
            return requested;
        };
        let Some(id) = resource
            .strip_prefix("foundation-model/")
            .or_else(|| resource.strip_prefix("inference-profile/"))
        else {
            return requested;
        };
        id
    } else {
        &normalized
    };
    let id = ["us.", "eu.", "ap.", "global."]
        .iter()
        .find_map(|prefix| id.strip_prefix(prefix))
        .unwrap_or(id);
    let allowed = if matches!(id, "moonshotai.kimi-k3" | "moonshotai.kimi-k3:0") {
        requested.min(KIMI_K3_SAFE_MAX_TOKENS)
    } else {
        requested
    };
    if allowed != requested {
        crate::server::logging::verbose(
            "[Bedrock]",
            &format!("model={model} 输出预算 maxTokens={requested} → {allowed}（已知模型限制保护）"),
        );
    }
    allowed
}

// ─── 凭据解析 ───────────────────────────────────────────────

/// 把账号的 `apiKey` 拆成 `(AccessKeyId, SecretAccessKey, Region)`。
///
/// ── 为什么用 `AK:SK` 这个约定 ────────────────────────────────
/// 账号模型（`CustomCredential::api_key`）只有一个字符串字段，而 Bedrock 需要
/// 两个值。可选方案里：
///   - 拆成两个字段：要动账号存储 schema 与全部 UI 表单，改动面远大于收益；
///   - 用 `AK:SK`：零 schema 改动，用户在「API Key」栏按 AWS 惯例粘贴即可
///     （`aws configure` 的输出、环境变量、密钥文件都是这两个值成对出现）。
/// 取冒号分隔而非其他符号：AK 恒为 20 位 `[A-Z0-9]`、SK 恒为 40 位 base64 字符集，
/// 两者都不含 `:`，因此第一个冒号是唯一可靠的分隔点。
///
/// region 可选，用 `AK:SK:region` 三段的形态带上；缺省回落 [`DEFAULT_REGION`]。
/// （region 影响端点与签名 scope，配错会得到 `Credential should be scoped to a
/// valid region` 或 404，所以允许显式指定。）
pub fn split_credentials(api_key: &str) -> Result<(String, String, String), ConvertError> {
    let raw = api_key.trim();
    if raw.is_empty() {
        return Err(
            "Bedrock 需要 AWS 凭据：请在账号的「API Key」栏按 `AccessKeyId:SecretAccessKey` 填写\
             （可选第三段 region，如 `AKIA...:secret...:us-west-2`）"
                .to_string(),
        );
    }
    let mut parts = raw.splitn(3, ':');
    let access_key = parts.next().unwrap_or("").trim().to_string();
    let secret_key = parts.next().unwrap_or("").trim().to_string();
    let region = parts
        .next()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(DEFAULT_REGION)
        .to_string();
    if access_key.is_empty() || secret_key.is_empty() {
        return Err(
            "Bedrock 凭据格式不正确：应为 `AccessKeyId:SecretAccessKey`（可选 `:region`）".to_string(),
        );
    }
    Ok((access_key, secret_key, region))
}

// ─── 请求翻译：chat 体 → Converse 体 ────────────────────────

/// chat 体 → Bedrock Converse 请求体（不含签名；签名见 [`sign_request`]）。
///
/// ── 映射规则（对齐 Converse API 规范）──────────────────────
/// ```text
///   messages[].role              → messages[].role  (system 单独提到顶层)
///   messages[].content(string)   → [{"text": "..."}]
///   messages[].content(array)    → 逐块翻译（text / image / tool_use / tool_result）
///   tools[]                      → toolConfig.tools[].toolSpec
///   tool_choice                  → toolConfig.toolChoice
///   max_tokens / max_completion  → inferenceConfig.maxTokens
///   temperature / top_p          → inferenceConfig.temperature / topP
///   stop                         → inferenceConfig.stopSequences
/// ```
///
/// `model` **不进 body** —— Converse 从 URL 路径取模型，body 里带 model 会被拒。
pub fn bedrock_request_from_chat(chat: &Value, model: &str) -> Result<Value, ConvertError> {
    let Some(messages_in) = chat.get("messages").and_then(Value::as_array) else {
        return Err("缺少 messages 数组".to_string());
    };
    if model.trim().is_empty() {
        return Err("Bedrock 请求缺少模型名（model 字段为空）".to_string());
    }

    let mut system: Vec<Value> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    let cache = prompt_cache_support(model);

    for message in messages_in {
        // 客户端断点（入口暂存在 `_wb_cache_control`）→ 该段末尾的 cachePoint
        let client_breakpoint = cache.is_some()
            && message.get(FIELD_CACHE_CONTROL).is_some_and(Value::is_object);
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user")
            .to_lowercase();
        // developer 在 Converse 里没有对应角色，语义上等同 system
        if role == "system" || role == "developer" {
            let text = content_text(message.get("content").unwrap_or(&Value::Null));
            if !text.trim().is_empty() {
                system.push(json!({ "text": text }));
                if client_breakpoint {
                    push_cache_point(&mut system);
                }
            }
            continue;
        }
        let target = if role == "assistant" { "assistant" } else { "user" };
        // 传**原始角色**而不是归一后的 target：`role:"tool"` 的消息要按工具结果
        // 翻（挂成 user 消息里的 toolResult 块），归一只用于决定挂在哪个角色上。
        let blocks = converse_blocks_of(message, &role);
        // 一个可翻的块都没有 → **整条跳过**，绝不留一条空 content 的消息。
        //
        // 这里曾经补一个 `{"text":""}` 占位「保住消息顺序」，方向是反的：
        // Converse 明确要求 text 块非空，补空块等于主动造一个必被拒的请求
        // —— 上游原样回 `messages: text content blocks must be non-empty`，
        // 而那句话既指不出是哪条消息、也指不出是谁造的。真正需要保顺序的
        // 情形（assistant 的 toolUse 与下一条 toolResult 配对）由 toolResult
        // 自己携带的 toolUseId 维系，不依赖一条空消息占位。
        if blocks.is_empty() {
            continue;
        }
        let mut blocks = blocks;
        if client_breakpoint {
            push_cache_point(&mut blocks);
        }
        append_merged(&mut messages, target, blocks);
    }
    // Converse 要求 user / assistant 交替，且 user 消息里的 toolResult 必须
    // 排在文本块之前 —— 同角色合并之后统一归一一次。
    for message in messages.iter_mut() {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
            tool_results_first(blocks);
        }
    }

    if messages.is_empty() {
        return Err("messages 为空：Bedrock 至少需要一条 user 消息".to_string());
    }

    let mut out = Map::new();
    out.insert("messages".to_string(), Value::Array(messages));
    if !system.is_empty() {
        out.insert("system".to_string(), Value::Array(system));
    }

    // ── inferenceConfig（输出预算归一，其余字段按需带上）──────────
    let mut inference = Map::new();
    let max_tokens = output_budget(chat, model);
    inference.insert("maxTokens".to_string(), Value::Number(max_tokens.into()));

    if let Some(temperature) = chat.get("temperature").and_then(Value::as_f64) {
        if let Some(number) = serde_json::Number::from_f64(temperature) {
            inference.insert("temperature".to_string(), Value::Number(number));
        }
    }
    if let Some(top_p) = chat.get("top_p").and_then(Value::as_f64) {
        if let Some(number) = serde_json::Number::from_f64(top_p) {
            inference.insert("topP".to_string(), Value::Number(number));
        }
    }
    // stop：字符串或数组都认
    match chat.get("stop") {
        Some(Value::String(text)) if !text.is_empty() => {
            inference.insert("stopSequences".to_string(), json!([text]));
        }
        Some(Value::Array(items)) => {
            let sequences: Vec<Value> = items
                .iter()
                .filter_map(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(|text| Value::String(text.to_string()))
                .collect();
            if !sequences.is_empty() {
                inference.insert("stopSequences".to_string(), Value::Array(sequences));
            }
        }
        _ => {}
    }
    out.insert("inferenceConfig".to_string(), Value::Object(inference));

    // ── toolConfig ─────────────────────────────────────────────
    if let Some(tools) = chat.get("tools").and_then(Value::as_array) {
        let specs: Vec<Value> = tools.iter().filter_map(converse_tool_spec).collect();
        if !specs.is_empty() {
            let mut tool_config = Map::new();
            tool_config.insert("tools".to_string(), Value::Array(specs));
            if let Some(choice) = converse_tool_choice(chat.get("tool_choice")) {
                tool_config.insert("toolChoice".to_string(), choice);
            }
            out.insert("toolConfig".to_string(), Value::Object(tool_config));
        }
    }

    if let Some(support) = cache {
        apply_cache_points(&mut out, support);
    }

    Ok(Value::Object(out))
}

// ─── 提示缓存（cachePoint）────────────────────────────────────
//
// ── 为什么必须有这一步 ───────────────────────────────────────
// Bedrock Converse 的提示缓存是**显式断点制**：请求里没有
// `{"cachePoint":{"type":"default"}}` 块就等于关闭缓存，上游既不写缓存也不
// 命中，`cacheReadInputTokens` 恒为 0。此前本翻译器既不翻客户端断点
// （`_wb_cache_control`），也不自动补点，所以经 Bedrock 的请求缓存读取恒为 0。
//
// ── 断点放哪儿（与 `anthropic_outbound::apply_prompt_cache_breakpoints` 同口径）
//   1. system 末尾；2. 工具清单末尾（仅 Claude，Nova 不支持工具缓存）；
//   3. 最后一条 user 消息末尾。客户端自带断点（Claude Code 打在 system 与
//   最近几条消息上）一并保留；总数超过上限时从**最早的消息断点**开始删，
//   因为后面的断点覆盖的前缀已经包含前面的。
//
// ── 只对支持缓存的模型生效 ───────────────────────────────────
// 不支持 cachePoint 的模型（Llama / Mistral / Kimi 等）收到会 400，所以按
// 模型 ID 白名单启用；application inference profile 这类不透明 ID 读不出
// 模型，保守不加。

/// 单个请求的 cachePoint 上限（超限上游报错，宁少勿多）。
const MAX_CACHE_POINTS: usize = 4;

/// 模型对 cachePoint 的支持范围。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CacheSupport {
    /// Anthropic Claude：system / messages / tools 均可缓存
    Full,
    /// Amazon Nova：只支持 system / messages
    NoTools,
}

fn prompt_cache_support(model: &str) -> Option<CacheSupport> {
    let id = model.trim().to_ascii_lowercase();
    if id.contains("anthropic.claude") {
        Some(CacheSupport::Full)
    } else if id.contains("amazon.nova") {
        Some(CacheSupport::NoTools)
    } else {
        None
    }
}

fn is_cache_point(block: &Value) -> bool {
    block.get("cachePoint").is_some()
}

/// 在块数组末尾追加一个 cachePoint；空数组或末块已是 cachePoint 时不加。
/// 返回是否新增。
fn push_cache_point(blocks: &mut Vec<Value>) -> bool {
    match blocks.last() {
        None => false,
        Some(last) if is_cache_point(last) => false,
        Some(_) => {
            blocks.push(json!({ "cachePoint": { "type": "default" } }));
            true
        }
    }
}

fn apply_cache_points(out: &mut Map<String, Value>, support: CacheSupport) {
    // ① system 末尾
    if let Some(system) = out.get_mut("system").and_then(Value::as_array_mut) {
        push_cache_point(system);
    }
    // ② 工具清单末尾
    if support == CacheSupport::Full {
        if let Some(tools) = out
            .get_mut("toolConfig")
            .and_then(|config| config.get_mut("tools"))
            .and_then(Value::as_array_mut)
        {
            push_cache_point(tools);
        }
    }
    // ③ 最后一条 user 消息末尾
    if let Some(content) = out
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .and_then(|messages| {
            messages
                .iter_mut()
                .rev()
                .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        })
        .and_then(|message| message.get_mut("content"))
        .and_then(Value::as_array_mut)
    {
        push_cache_point(content);
    }

    // ④ 超限裁剪：从最早的消息断点开始删（system / tools 的断点最稳定，保留）
    let count_in = |value: Option<&Value>| -> usize {
        value
            .and_then(Value::as_array)
            .map(|items| items.iter().filter(|item| is_cache_point(item)).count())
            .unwrap_or(0)
    };
    let mut total = count_in(out.get("system"))
        + count_in(out.get("toolConfig").and_then(|config| config.get("tools")));
    if let Some(messages) = out.get("messages").and_then(Value::as_array) {
        total += messages.iter().map(|message| count_in(message.get("content"))).sum::<usize>();
    }
    let mut excess = total.saturating_sub(MAX_CACHE_POINTS);
    if excess == 0 {
        return;
    }
    if let Some(messages) = out.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages.iter_mut() {
            if excess == 0 {
                break;
            }
            if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
                content.retain(|block| {
                    if excess > 0 && is_cache_point(block) {
                        excess -= 1;
                        false
                    } else {
                        true
                    }
                });
            }
        }
    }
}

/// chat 的一条消息 → Converse content 块数组。
///
/// 文本 / 图片 / tool_use / tool_result 四类块都翻；无法识别的块**跳过而不是
/// 报错**（上游对未知块类型的容忍度低，整轮 400 比丢一个块更糟）。
///
/// ── 工具历史的两个入口（2026-10-13 补）─────────────────────
/// 工具历史在 chat 体里以**两种形态**出现，本函数两种都要认：
/// ```text
///   assistant.tool_calls[]      → assistant 的 toolUse 块（OpenAI 形态，客户端历史）
///   content[].type == tool_use  → assistant 的 toolUse 块（Anthropic 形态）
///   role == "tool"              → user 的 toolResult 块（OpenAI 形态，工具结果）
///   content[].type == tool_result → user 的 toolResult 块（Anthropic 形态）
/// ```
/// 只认后两种曾经导致一个致命缺口：客户端（如 Proma）重放的历史是 OpenAI
/// 形态，于是每个「content=null + tool_calls」的 assistant 都翻不出任何块，
/// 再加上调用方当时会补一个空 text 块占位 —— 整轮请求被上游以
/// `text content blocks must be non-empty` 拒绝，而工具调用本身全部丢失。
fn converse_blocks_of(message: &Value, role: &str) -> Vec<Value> {
    // tool 结果 → user 消息里的 toolResult 块，**提前返回**：`role:"tool"` 的
    // content 本身就是工具输出，已整体折进 toolResult；再走下面的通用分支会把
    // 同一段文本当成普通用户正文再发一遍。缺 id 时不伪造配对钥匙（理由见
    // [`converse_tool_result_block`]），返回空让调用方跳过整条消息。
    if role == "tool" {
        return converse_tool_result_block(message).into_iter().collect();
    }
    let mut blocks: Vec<Value> = Vec::new();
    match message.get("content") {
        Some(Value::String(text)) => {
            if !text.is_empty() {
                blocks.push(json!({ "text": text }));
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                let kind = item.get("type").and_then(Value::as_str).unwrap_or("text");
                match kind {
                    "text" => {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                blocks.push(json!({ "text": text }));
                            }
                        }
                    }
                    "image_url" => {
                        if let Some(block) = converse_image_block(item) {
                            blocks.push(block);
                        }
                    }
                    "tool_use" => {
                        // chat 的 tool_use → Converse 的 toolUse（id / name / input 同名）
                        let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                        if !name.is_empty() {
                            blocks.push(converse_tool_use_block(
                                id,
                                name,
                                item.get("input").cloned().unwrap_or(json!({})),
                            ));
                        }
                    }
                    "tool_result" => {
                        let id = item
                            .get("tool_use_id")
                            .or_else(|| item.get("toolUseId"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let text = content_text(item.get("content").unwrap_or(&Value::Null));
                        if !id.is_empty() {
                            blocks.push(converse_tool_result_from_parts(
                                id,
                                &text,
                                item.get(FIELD_IS_ERROR).or_else(|| item.get("is_error")),
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    // assistant 的 tool_calls → toolUse 块（OpenAI 形态，客户端重放的历史走这条）。
    // 放在文本之后：Converse 允许两种顺序，但「先说再调」与 chat 的直觉一致。
    if role == "assistant" {
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let id = call.get("id").and_then(Value::as_str).unwrap_or("");
                let name = call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if name.is_empty() {
                    // 空名调用无从翻译（上游的历史 sanitize 会补占位名，走到这里
                    // 说明调用形态本身就不规范）—— 跳过，让上游自己报缺配对。
                    continue;
                }
                let arguments = call
                    .pointer("/function/arguments")
                    .cloned()
                    .unwrap_or(Value::Null);
                blocks.push(converse_tool_use_block(id, name, parse_json_object(&arguments)));
            }
        }
    }
    // **不再补空 text 块**：一个块都翻不出来时返回空，由调用方跳过整条消息
    // （理由见 `bedrock_request_from_chat` 的装配循环）。
    blocks
}

/// 一个 Converse 的 `toolUse` 块。
///
/// `input` 必须是对象（上游对非对象直接 400），由调用方保证；缺 id 时现造一个
/// —— Converse 要求 toolUseId 与后面的 toolResult 配对，没有 id 就无从配对。
fn converse_tool_use_block(id: &str, name: &str, input: Value) -> Value {
    json!({
        "toolUse": {
            "toolUseId": if id.is_empty() { random_id("toolu") } else { id.to_string() },
            "name": name,
            "input": input,
        }
    })
}

/// 一条 chat 的 `role:"tool"` 消息 → Converse 的 `toolResult` 块。
///
/// `tool_call_id` 是配对的钥匙（对应 assistant 的 `tool_calls[].id`）；缺 id
/// 说明客户端数据本身缺配对，此时**不伪造** —— 返回 None 让调用方跳过，上游
/// 若真需要这条结果会以缺配对报错，比指向一个编造的 id 好排查。
fn converse_tool_result_block(message: &Value) -> Option<Value> {
    let id = message
        .get("tool_call_id")
        .or_else(|| message.get("tool_use_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if id.is_empty() {
        return None;
    }
    let content = message.get("content").unwrap_or(&Value::Null);
    // 结果里可能带图片：Converse 的 toolResult.content 支持 image 块，能翻的
    // 一并带上（与文本并列）；翻不出的图片静默丢弃，不拖垮整条请求。
    let images: Vec<Value> = content_parts(content)
        .iter()
        .filter_map(converse_image_block)
        .collect();
    let text = content_text(content);
    let mut blocks: Vec<Value> = Vec::new();
    if !text.is_empty() {
        blocks.push(json!({ "text": text }));
    }
    blocks.extend(images);
    // 文本与图片都空（工具没有输出）：toolResult 的 content 不允许为空数组，
    // 补一句中性占位而不是空串（空串 text 块同样会被上游拒）。
    if blocks.is_empty() {
        blocks.push(json!({ "text": "(the tool returned no output)" }));
    }
    let is_error = message
        .get(FIELD_IS_ERROR)
        .or_else(|| message.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut result = Map::new();
    result.insert("toolUseId".to_string(), json!(id));
    result.insert("content".to_string(), Value::Array(blocks));
    if is_error {
        result.insert("status".to_string(), json!("error"));
    }
    Some(json!({ "toolResult": Value::Object(result) }))
}

/// 已经拍平成文本（与可选图片）的 tool_result 块（`content[].type == tool_result` 形态）
fn converse_tool_result_from_parts(id: &str, text: &str, is_error: Option<&Value>) -> Value {
    let content = if text.is_empty() {
        json!([{ "text": "(the tool returned no output)" }])
    } else {
        json!([{ "text": text }])
    };
    let mut result = Map::new();
    result.insert("toolUseId".to_string(), json!(id));
    result.insert("content".to_string(), content);
    if is_error.and_then(Value::as_bool).unwrap_or(false) {
        result.insert("status".to_string(), json!("error"));
    }
    json!({ "toolResult": Value::Object(result) })
}

/// 连续同角色的消息合并成一条（Converse 要求 user / assistant 交替）。
///
/// 与 `anthropic_outbound::append_merged` 同口径：合并而不是丢弃，否则
/// 「user 文本 + user 图片」这类拆开发的消息会在第二条处被上游拒。
fn append_merged(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(existing) = last.get_mut("content").and_then(Value::as_array_mut) {
                existing.extend(blocks);
                return;
            }
        }
    }
    messages.push(json!({ "role": role, "content": blocks }));
}

/// 把 user 消息里的 toolResult 块挪到最前（原地稳定重排）。
///
/// Converse 的硬要求：user 消息里带 toolResult 时它必须排在其他内容之前。
/// 同角色合并之后，一条 user 消息里可能先是文本块、后才是 toolResult ——
/// 直接用会换一个 400，所以在装配收尾时统一归一。
fn tool_results_first(blocks: &mut Vec<Value>) {
    if !blocks.iter().any(|block| block.get("toolResult").is_some()) {
        return;
    }
    blocks.sort_by_key(|block| if block.get("toolResult").is_some() { 0 } else { 1 });
}

/// chat 的 `image_url` 块 → Converse 的 `image` 块。
///
/// 只支持 **base64 data URI**（`data:image/png;base64,...`）。远程 URL 需要
/// Bedrock 侧有 S3 访问权或先下载再转 base64，两种都超出「透传」的范围 ——
/// 跳过并留给上游报错，比发一个必被拒的 URL 块更省一次往返。
fn converse_image_block(item: &Value) -> Option<Value> {
    let url = item
        .get("image_url")
        .and_then(|value| value.get("url"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let payload = url.strip_prefix("data:")?;
    let (meta, data) = payload.split_once(',')?;
    let (media_type, encoding) = meta.split_once(';')?;
    if !encoding.eq_ignore_ascii_case("base64") {
        return None;
    }
    let format = match media_type {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => return None,
    };
    Some(json!({
        "image": {
            "format": format,
            "source": { "bytes": data },
        }
    }))
}

/// chat 的 `tools[]` 一项 → Converse 的 `toolSpec`。
///
/// 两种入参形态都认：OpenAI 的 `{"type":"function","function":{...}}` 与
/// 已经扁平化的 `{"name":...,"input_schema":...}`（Anthropic 形态）。
fn converse_tool_spec(tool: &Value) -> Option<Value> {
    let function = tool.get("function").unwrap_or(tool);
    let name = function.get("name").and_then(Value::as_str)?.trim();
    if name.is_empty() {
        return None;
    }
    let description = function
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    let schema = function
        .get("parameters")
        .or_else(|| function.get("input_schema"))
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
    let mut spec = Map::new();
    spec.insert("name".to_string(), json!(name));
    if !description.is_empty() {
        spec.insert("description".to_string(), json!(description));
    }
    spec.insert("inputSchema".to_string(), json!({ "json": schema }));
    Some(json!({ "toolSpec": Value::Object(spec) }))
}

/// chat 的 `tool_choice` → Converse 的 `toolChoice`。
fn converse_tool_choice(choice: Option<&Value>) -> Option<Value> {
    match choice {
        Some(Value::String(text)) => match text.as_str() {
            "auto" => Some(json!({ "auto": {} })),
            "required" | "any" => Some(json!({ "any": {} })),
            _ => None,
        },
        Some(Value::Object(object)) => {
            // {"type":"function","function":{"name":"x"}} / {"type":"tool","name":"x"}
            let name = object
                .get("function")
                .and_then(|value| value.get("name"))
                .or_else(|| object.get("name"))
                .and_then(Value::as_str)?;
            Some(json!({ "tool": { "name": name } }))
        }
        _ => None,
    }
}

/// 把一个 content 字段拍平成纯文本（字符串直取；数组拼接各 text 块）。
fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| item.as_str())
                    .map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

// ─── SigV4 签名 ─────────────────────────────────────────────

/// SigV4 签名结果：要附到请求上的头。
pub struct SignedHeaders {
    /// `Authorization` 头（含 Credential / SignedHeaders / Signature 三段）
    pub authorization: String,
    /// `X-Amz-Date` 头（签名的时间戳，必须与签名时用的一致）
    pub amz_date: String,
}

/// 对一次 Bedrock 请求做 SigV4 签名。
///
/// ── 签名覆盖什么 ────────────────────────────────────────────
/// ```text
///   CanonicalRequest =
///     METHOD \n
///     CanonicalURI \n          （URI 编码后的路径；ARN 形态要**双重编码**）
///     CanonicalQueryString \n
///     CanonicalHeaders \n      （小写名:值，按名排序，每个一行）
///     SignedHeaders \n         （分号连接的头名，已排序）
///     HexPayloadHash           （body 字节的 SHA256）
/// ```
/// 再对 `AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{canonical_request 的 SHA256}`
/// 做四层 HMAC 派生密钥签名。
///
/// ── 为什么 body 必须先定稿 ──────────────────────────────────
/// `HexPayloadHash` 是 body 的哈希，body 变一个字节签名就失效。所以调用序是
/// 「翻译 → 序列化 → 签名 → 发送」，本函数只接受**已序列化**的 body 字符串。
///
/// ── content_type 为什么是参数（2026-10-12 修）─────────────────
/// 原本这里硬编码 `content-type:application/json`，因为唯一的调用方是
/// Converse（POST + JSON）。但 `fetch_upstream_models` 的 bedrock 分支要发
/// **GET** `/foundation-models` —— GET 无 body、不带 Content-Type，若签名串里
/// 仍列着 `content-type` 而实际请求没发这个头，上游会以
/// `SignatureDoesNotMatch` 拒绝（签名头必须在实际请求里存在且一致）。
/// 传空串即「不签 content-type」，只签 host + x-amz-date。
#[allow(clippy::too_many_arguments)]
pub fn sign_request(
    method: &str,
    host: &str,
    canonical_uri: &str,
    query: &str,
    body: &str,
    content_type: Option<&str>,
    access_key: &str,
    secret_key: &str,
    region: &str,
    amz_date: &str,
    date_stamp: &str,
) -> SignedHeaders {
    let payload_hash = hex_sha256(body.as_bytes());

    // canonical headers：只签实际会发出的头。带上更少头 = 更少出错面；
    // 但**签了的头必须在请求里原样出现**，所以这里要与调用方的发送逻辑一致
    let (canonical_headers, signed_headers) = match content_type {
        Some(value) if !value.trim().is_empty() => (
            format!("content-type:{value}\nhost:{host}\nx-amz-date:{amz_date}\n"),
            "content-type;host;x-amz-date",
        ),
        // GET（无 body）不签 content-type
        _ => (
            format!("host:{host}\nx-amz-date:{amz_date}\n"),
            "host;x-amz-date",
        ),
    };

    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );

    let scope = format!("{date_stamp}/{region}/{SERVICE}/aws4_request");
    let string_to_sign = format!(
        "{ALGORITHM}\n{amz_date}\n{scope}\n{}",
        hex_sha256(canonical_request.as_bytes())
    );

    let signing_key = derive_signing_key(secret_key, date_stamp, region);
    let signature = match Hmac::<Sha256>::new_from_slice(&signing_key) {
        Ok(mut mac) => {
            mac.update(string_to_sign.as_bytes());
            hex(&mac.finalize().into_bytes())
        }
        // new_from_slice 只对空密钥失败；密钥已在 split_credentials 校验非空
        Err(_) => String::new(),
    };

    SignedHeaders {
        authorization: format!(
            "{ALGORITHM} Credential={access_key}/{scope}, \
             SignedHeaders={signed_headers}, Signature={signature}"
        ),
        amz_date: amz_date.to_string(),
    }
}

/// SigV4 的四层密钥派生：`AWS4+SK → date → region → service → aws4_request`
fn derive_signing_key(secret_key: &str, date_stamp: &str, region: &str) -> Vec<u8> {
    let initial = format!("AWS4{secret_key}");
    let k_date = hmac_sha256(initial.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, SERVICE.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// 单层 HMAC-SHA256。密钥非法（空）时返回空串的哈希 —— 上游会以签名不匹配
/// 报错，比在这里 panic 更符合「零 panic」约束。
fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    match Hmac::<Sha256>::new_from_slice(key) {
        Ok(mut mac) => {
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        }
        Err(_) => Vec::new(),
    }
}

/// SHA256 → 小写 hex
fn hex_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex(&hasher.finalize())
}

/// 字节 → 小写 hex
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// URL 路径编码（RFC3986）：保留 `A-Za-z0-9-_.~`，其余 `%XX` 大写。
///
/// Bedrock 的模型名进路径，形态各异：
///   - 普通模型 ID `us.anthropic.claude-sonnet-4-6` —— 只含安全字符，编码后不变；
///   - **带冒号的模型 ID** `us.amazon.nova-premier-v1:0` —— 含 `:`，要编码；
///   - inference profile ARN `arn:aws:bedrock:...:application-inference-profile/xxx`
///     —— 含 `:` 与 `/`，都要编码。
///
/// ── `:` 为什么必须编码（2026-10-12 实测修正）──────────────────
/// 最初只对 ARN 做编码，普通模型 ID 原样发出。但 `us.amazon.nova-premier-v1:0`
/// 这类带冒号的 ID 会因此签名失败 —— 实测（用 curl 内置 `--aws-sigv4` 做基准
/// 对照）确认：AWS 要求路径里的 `:` 编码成 `%3A`，且 CanonicalURI 用 `%253A`。
/// 换句话说**所有非 unreserved 字符一律编码**，不区分模型 ID 与 ARN —— 差异
/// 只在「canonical 要不要再编一层」，见 [`model_path_segments`]。
pub fn encode_uri_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~') {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 把模型名编成「请求路径用的 model 段」与「签名用的 canonical 段」。
///
/// ── 两段的规则（AWS SigV4 规范）────────────────────────────
/// ```text
///   请求路径  = encodeURIComponent(model)          单次编码
///   canonical = encodeURIComponent(请求路径)       再编码一次（双重）
/// ```
/// 第二层不是「ARN 特有」—— 只要模型名含任何被编码的字符（`:` 也算），
/// canonical 就与请求路径不同。实测依据：故意用单次编码的 canonical 发请求，
/// AWS 的 `SignatureDoesNotMatch` 回显里明确给出它期望的
/// `/model/arn%253Aaws%253A.../converse`（`%253A` = `%3A` 的再编码）。
///
/// 对只含 unreserved 字符的模型名（如 `moonshotai.kimi-k2.5`），两段相同 ——
/// 再编码一次是幂等的。
pub fn model_path_segments(model: &str) -> (String, String) {
    let request_segment = encode_uri_component(model);
    let canonical_segment = encode_uri_component(&request_segment);
    (request_segment, canonical_segment)
}

/// 当前 UTC 时间的 `(amz_date, date_stamp)` 两段形态
/// （`20261012T044836Z` 与 `20261012`）。
pub fn amz_timestamps() -> (String, String) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = civil_from_unix(now);
    (
        format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z"),
        format!("{year:04}{month:02}{day:02}"),
    )
}

/// Unix 秒 → UTC 年月日时分秒（Howard Hinnant 的 civil_from_days 算法）。
///
/// 不引 chrono：本仓依赖树里没有它，而这里只需要一个 UTC 时间戳格式化 ——
/// 为一行格式化多背一个时间库不划算。
fn civil_from_unix(seconds: i64) -> (i64, i64, i64, i64, i64, i64) {
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // days since 1970-01-01 → civil date
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, hour, minute, second)
}

// ─── 响应翻译：AWS Event Stream → chat SSE ──────────────────

/// AWS Event Stream 二进制帧的分帧器。
///
/// ── 帧格式 ──────────────────────────────────────────────────
/// ```text
///   [0..4]   total_length   (big-endian u32，含自身与尾部 CRC)
///   [4..8]   headers_length (big-endian u32)
///   [8..12]  prelude_crc    (前 8 字节的 CRC32)
///   [12..12+headers_length]  headers
///   [...]    payload
///   [..-4]   message_crc    (整个消息的 CRC32)
/// ```
/// header 项：`[1B 名长][名][1B 值类型][值]`。7 = 字符串、6 = 字节数组
/// （均为 `[2B 长][内容]`），8 = timestamp（固定 8B）。非字符串头也必须
/// 按真实长度跳过：AWS 不保证头顺序，不能让它遮住后面的异常头。
/// 普通事件用 `:event-type`；异常用 `:exception-type`；未建模错误用
/// `:error-code` / `:error-message`，不能把这三种都当普通事件解析。
///
/// **不校验 CRC**：分帧只依赖长度字段，而 CRC 错误会由 JSON 解析失败兜住。
/// 校验 CRC 要多引一个 crc32 依赖，收益只是把「解析失败」提前成「校验失败」。
#[derive(Default)]
struct EventStreamDecoder {
    /// 未凑满一帧的残留字节
    tail: Vec<u8>,
}

enum DecodedEvent {
    Message(String, Vec<u8>),
    Error { code: String, message: String, payload: Vec<u8> },
}

impl EventStreamDecoder {
    fn new() -> Self {
        Self { tail: Vec::new() }
    }

    /// 吃一段字节，吐出其中**完整**的事件（0..n 个）。
    fn push(&mut self, chunk: &[u8]) -> Vec<DecodedEvent> {
        self.tail.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            if self.tail.len() < 16 {
                break;
            }
            let total = u32::from_be_bytes([self.tail[0], self.tail[1], self.tail[2], self.tail[3]])
                as usize;
            let headers_len =
                u32::from_be_bytes([self.tail[4], self.tail[5], self.tail[6], self.tail[7]]) as usize;
            // 长度字段自洽性检查：总长至少含前序 12B + 尾 CRC 4B；头长不能越界。
            // 畸形数据就地停手（保留 tail，等更多字节或流结束），不 panic。
            if total < 16 || 12 + headers_len + 4 > total {
                break;
            }
            if self.tail.len() < total {
                break;
            }
            let frame = self.tail[..total].to_vec();
            self.tail.drain(..total);

            let header_start = 12;
            let header_end = 12 + headers_len;
            let mut event_type = String::new();
            let mut message_type = String::new();
            let mut exception_type = String::new();
            let mut error_code = String::new();
            let mut error_message = String::new();
            let mut cursor = header_start;
            while cursor < header_end {
                let name_len = frame[cursor] as usize;
                cursor += 1;
                if cursor + name_len > header_end {
                    break;
                }
                let name = String::from_utf8_lossy(&frame[cursor..cursor + name_len]).to_string();
                cursor += name_len;
                if cursor >= header_end {
                    break;
                }
                let value_type = frame[cursor];
                cursor += 1;
                match value_type {
                    7 => {
                        if cursor + 2 > header_end {
                            break;
                        }
                        let value_len =
                            u16::from_be_bytes([frame[cursor], frame[cursor + 1]]) as usize;
                        cursor += 2;
                        if cursor + value_len > header_end {
                            break;
                        }
                        let value =
                            String::from_utf8_lossy(&frame[cursor..cursor + value_len]).to_string();
                        cursor += value_len;
                        match name.as_str() {
                            ":event-type" => event_type = value,
                            ":message-type" => message_type = value,
                            ":exception-type" => exception_type = value,
                            ":error-code" => error_code = value,
                            ":error-message" => error_message = value,
                            _ => {}
                        }
                    }
                    6 => {
                        if cursor + 2 > header_end {
                            break;
                        }
                        let value_len =
                            u16::from_be_bytes([frame[cursor], frame[cursor + 1]]) as usize;
                        cursor += 2;
                        if value_len > header_end - cursor {
                            break;
                        }
                        cursor += value_len;
                    }
                    other => {
                        let size = match other {
                            0 | 1 => 0, // bool：值已编码在类型中
                            2 => 1,
                            3 => 2,
                            4 => 4,
                            5 | 8 => 8, // long / timestamp
                            9 => 16,    // UUID
                            _ => break,
                        };
                        if size > header_end - cursor {
                            break;
                        }
                        cursor += size;
                    }
                }
            }
            let payload_start = header_end;
            let payload_end = total.saturating_sub(4);
            let payload = if payload_end > payload_start {
                frame[payload_start..payload_end].to_vec()
            } else {
                Vec::new()
            };
            if message_type == "exception" || !exception_type.is_empty() {
                out.push(DecodedEvent::Error {
                    code: if exception_type.is_empty() { "BedrockException".to_string() } else { exception_type },
                    message: error_message,
                    payload,
                });
            } else if message_type == "error" || !error_code.is_empty() {
                out.push(DecodedEvent::Error {
                    code: if error_code.is_empty() { "BedrockStreamError".to_string() } else { error_code },
                    message: error_message,
                    payload,
                });
            } else if !event_type.is_empty() {
                out.push(DecodedEvent::Message(event_type, payload));
            }
        }
        out
    }

    /// 流结束：残留半帧丢弃（没有长度字段就无从解析）
    fn finish(&mut self) -> Vec<DecodedEvent> {
        self.tail.clear();
        Vec::new()
    }
}

/// Converse 的 inputTokens 不含缓存读写；Chat prompt_tokens 含全部输入。
/// 依据 AWS prompt-caching 文档的公式，缓存读/写各加一次，cacheDetails 仅作
/// 写入分项记录，不重复累计。保留白名单原始字段，便于核对上游 totalTokens。
#[derive(Default)]
struct BedrockUsage {
    raw: Map<String, Value>,
}

impl BedrockUsage {
    fn absorb(&mut self, usage: &Value) {
        for key in ["inputTokens", "outputTokens", "totalTokens", "cacheReadInputTokens", "cacheWriteInputTokens"] {
            if let Some(value) = usage.get(key).and_then(Value::as_i64).filter(|value| *value >= 0) {
                // metadata 是快照而非增量；缺字段保持旧值，显式 0 则覆盖。
                self.raw.insert(key.to_string(), json!(value));
            }
        }
        if let Some(items) = usage.get("cacheDetails").and_then(Value::as_array) {
            let details: Vec<Value> = items.iter().filter_map(|item| {
                let input = item.get("inputTokens").and_then(Value::as_i64).filter(|value| *value >= 0)?;
                let ttl = item.get("ttl").and_then(Value::as_str).filter(|ttl| matches!(*ttl, "5m" | "1h"))?;
                Some(json!({"inputTokens":input,"ttl":ttl}))
            }).collect();
            self.raw.insert("cacheDetails".to_string(), json!(details));
        }
    }

    fn count(&self, key: &str) -> i64 {
        self.raw.get(key).and_then(Value::as_i64).unwrap_or(0)
    }

    fn to_chat(&self) -> Value {
        let input = self.count("inputTokens")
            .saturating_add(self.count("cacheReadInputTokens"))
            .saturating_add(self.count("cacheWriteInputTokens"));
        let output = self.count("outputTokens");
        let mut usage = json!({
            "prompt_tokens": input,
            "completion_tokens": output,
            "total_tokens": input.saturating_add(output),
        });
        if let Some(read) = self.raw.get("cacheReadInputTokens") {
            usage["prompt_tokens_details"] = json!({"cached_tokens":read});
        }
        if let Some(write) = self.raw.get("cacheWriteInputTokens") {
            // OpenAI 无标准缓存写入字段，沿用网关/Anthropic 的扩展命名。
            usage["cache_creation_input_tokens"] = write.clone();
        }
        if !self.raw.is_empty() {
            usage["bedrock_usage"] = Value::Object(self.raw.clone());
        }
        usage
    }
}

/// Bedrock 事件流 → 标准 chat SSE 帧（与 `ChatFromAnthropicStream` 同形接口）。
pub struct ChatFromBedrockStream {
    decoder: EventStreamDecoder,
    /// 输出帧的 model（发给上游的真名；ForwardStream 的回写层按需改写）
    model: String,
    id: String,
    created: i64,
    started: bool,
    finished: bool,
    /// toolUse 块（按 contentBlockIndex 记）→ (toolUseId, name, 是否已发过参数)
    tools: std::collections::BTreeMap<i64, (String, String, bool)>,
    /// 上游用量快照，含缓存读取/写入，流式与非流式共用转换口径。
    usage: BedrockUsage,
    /// 已映射成 chat 口径的 finish_reason（messageStop 里给）
    finish_reason: Option<String>,
    /// 上游显式报错（`internalServerException` 等）
    error: Option<String>,
}

impl ChatFromBedrockStream {
    pub fn new(model: &str) -> Self {
        Self {
            decoder: EventStreamDecoder::new(),
            model: model.to_string(),
            id: String::new(),
            created: crate::server::logging::now_ms() / 1000,
            started: false,
            finished: false,
            tools: std::collections::BTreeMap::new(),
            usage: BedrockUsage::default(),
            finish_reason: None,
            error: None,
        }
    }

    /// 吃一段上游字节，吐出翻译好的 chat SSE 帧。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<bytes::Bytes> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        for event in self.decoder.push(chunk) {
            match event {
                DecodedEvent::Message(event_type, payload) => {
                    let body = serde_json::from_slice(&payload).unwrap_or(Value::Null);
                    out.extend(self.handle(&event_type, &body));
                }
                DecodedEvent::Error { code, message, payload } => {
                    let body = serde_json::from_slice(&payload).unwrap_or(Value::Null);
                    out.extend(self.fail_upstream(&code, &message, &body));
                }
            }
            if self.finished {
                break;
            }
        }
        out
    }

    /// 流结束：收尾（补 finish 帧 + usage 帧 + [DONE]）
    pub fn finish(&mut self) -> Vec<bytes::Bytes> {
        self.decoder.finish();
        self.complete()
    }

    /// 单个事件 → chat SSE 帧
    fn handle(&mut self, event_type: &str, body: &Value) -> Vec<bytes::Bytes> {
        if self.finished {
            return Vec::new();
        }
        match event_type {
            "messageStart" => self.start(),
            "contentBlockStart" => {
                let index = body.get("contentBlockIndex").and_then(Value::as_i64).unwrap_or(0);
                let start = body.get("start").cloned().unwrap_or(Value::Null);
                if let Some(tool_use) = start.get("toolUse") {
                    let tool_use_id = tool_use
                        .get("toolUseId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = tool_use
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    self.tools.insert(index, (tool_use_id.clone(), name.clone(), false));
                    let mut frames = self.start();
                    frames.push(self.tool_call_frame(index, &tool_use_id, &name, ""));
                    return frames;
                }
                Vec::new()
            }
            "contentBlockDelta" => {
                let index = body.get("contentBlockIndex").and_then(Value::as_i64).unwrap_or(0);
                let delta = body.get("delta").cloned().unwrap_or(Value::Null);
                // 文本增量
                if let Some(text) = delta.get("text").and_then(Value::as_str) {
                    if text.is_empty() {
                        return Vec::new();
                    }
                    let mut frames = self.start();
                    // delta_frame 的参数**就是** delta 的内容（它自己包 "delta" 键），
                    // 这里不能再套一层 —— 套了会产出 {"delta":{"delta":{...}}}
                    frames.push(self.delta_frame(json!({ "content": text })));
                    return frames;
                }
                // 工具参数增量（增量 JSON 片段，原样透传给客户端拼）
                if let Some(partial) = delta
                    .get("toolUse")
                    .and_then(|value| value.get("input"))
                    .and_then(Value::as_str)
                {
                    let (tool_use_id, name, sent) = self
                        .tools
                        .get(&index)
                        .cloned()
                        .unwrap_or((String::new(), String::new(), false));
                    let mut frames = self.start();
                    frames.push(self.tool_call_frame(index, &tool_use_id, &name, partial));
                    if let Some(entry) = self.tools.get_mut(&index) {
                        entry.2 = true;
                    }
                    let _ = sent;
                    return frames;
                }
                // reasoning / 其他增量：Converse 目前不产，留空即可
                Vec::new()
            }
            "contentBlockStop" => {
                let index = body.get("contentBlockIndex").and_then(Value::as_i64).unwrap_or(0);
                // 一个参数分片都没来过的工具：补一个空对象参数帧，否则客户端拿不到
                // 可解析的 arguments（与 anthropic 分支同一处理）
                if let Some((tool_use_id, name, sent)) = self.tools.get(&index).cloned() {
                    if !sent {
                        let mut frames = self.start();
                        frames.push(self.tool_call_frame(index, &tool_use_id, &name, "{}"));
                        if let Some(entry) = self.tools.get_mut(&index) {
                            entry.2 = true;
                        }
                        return frames;
                    }
                }
                Vec::new()
            }
            "messageStop" => {
                self.finish_reason = body
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .map(|reason| match reason {
                        "end_turn" | "stop_sequence" => "stop".to_string(),
                        "max_tokens" => "length".to_string(),
                        "tool_use" => "tool_calls".to_string(),
                        "content_filtered" | "guardrail_intervened" => "content_filter".to_string(),
                        other => other.to_string(),
                    });
                Vec::new()
            }
            "metadata" => {
                if let Some(usage) = body.get("usage") {
                    self.usage.absorb(usage);
                }
                Vec::new()
            }
            // Bedrock 的异常事件（internalServerException / modelStreamErrorException …）
            other if other.ends_with("Exception") => self.fail_upstream(other, "", body),
            _ => Vec::new(),
        }
    }

    /// 首帧（role assistant），只发一次
    fn start(&mut self) -> Vec<bytes::Bytes> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        if self.id.is_empty() {
            self.id = random_id("chatcmpl");
        }
        vec![self.delta_frame(json!({ "role": "assistant", "content": "" }))]
    }

    /// 收尾：finish_reason 帧 + usage 帧 + [DONE]（只做一次）
    fn complete(&mut self) -> Vec<bytes::Bytes> {
        if self.finished {
            return Vec::new();
        }
        if !self.started && self.finish_reason.is_none() {
            // messageStop 是有效终止事件，即使没有 messageStart。Opus 的
            // 内容过滤响应可以只有 messageStop + metadata；只有两者都没收到
            // 才属于无效/空事件流。
            let message = self
                .error
                .clone()
                .unwrap_or_else(|| "上游未返回有效 Bedrock 事件流".to_string());
            return self.fail_message(&message);
        }
        self.finished = true;
        let mut out = self.start();
        let finish_reason = self.finish_reason.clone().unwrap_or_else(|| {
            if self.tools.is_empty() {
                "stop".to_string()
            } else {
                "tool_calls".to_string()
            }
        });
        out.push(self.finish_frame(&finish_reason));
        out.push(chat_frame(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [],
            "usage": self.usage.to_chat(),
        })));
        if finish_reason == "content_filter" {
            // 保留标准终止原因与 usage，同时给无法解释空内容的客户端明确错误。
            // 不重试、不改写上下文，也不将上游审核拦截归类为空流或传输错误。
            let message = "Bedrock 内容审核拦截了本次请求或输出（content_filtered / guardrail_intervened）。这是上游内容策略结果，不是代理连接或事件流解析失败；网关未修改或绕过审核策略。";
            self.error = Some(message.to_string());
            out.push(chat_frame(&json!({
                "error": { "message": message, "type": "content_filter", "code": "content_filter" }
            })));
        }
        out.push(bytes::Bytes::from_static(b"data: [DONE]\n\n"));
        out
    }

    /// 保留异常的类型、正文和嵌套模型错误，不把 HTTP 200 当成流成功。
    fn fail_upstream(&mut self, code: &str, header_message: &str, body: &Value) -> Vec<bytes::Bytes> {
        let primary = ["message", "Message"]
            .iter()
            .find_map(|key| body.get(*key).and_then(Value::as_str).filter(|text| !text.trim().is_empty()))
            .or_else(|| (!header_message.trim().is_empty()).then_some(header_message))
            .unwrap_or(code);
        let mut message = primary.to_string();
        if let Some(original) = body.get("originalMessage").and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty() && *text != primary)
        {
            message.push_str(&format!("；{original}"));
        }
        self.fail_with_code(&format!("Bedrock {code}: {message}"), code)
    }

    /// 供转发层同步请求日志，避免 SSE 错误仍被记为成功且错误列为空。
    pub fn last_error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// 上游错误 → chat 错误帧 + [DONE]（只做一次）
    fn fail_message(&mut self, message: &str) -> Vec<bytes::Bytes> {
        self.fail_with_code(message, "bedrock_stream_error")
    }

    fn fail_with_code(&mut self, message: &str, code: &str) -> Vec<bytes::Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.error = Some(message.to_string());
        self.finished = true;
        vec![
            chat_frame(&json!({ "error": { "message": message, "type": "upstream_error", "code": code } })),
            bytes::Bytes::from_static(b"data: [DONE]\n\n"),
        ]
    }

    /// 一个 `choices[].delta` 帧
    fn delta_frame(&self, delta: Value) -> bytes::Bytes {
        chat_frame(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [ { "index": 0, "delta": delta, "finish_reason": Value::Null } ],
        }))
    }

    /// 工具调用帧（chat 的 `delta.tool_calls[]` 形态）
    fn tool_call_frame(&self, index: i64, id: &str, name: &str, arguments: &str) -> bytes::Bytes {
        let mut function = Map::new();
        if !name.is_empty() {
            function.insert("name".to_string(), json!(name));
        }
        function.insert("arguments".to_string(), json!(arguments));
        let mut call = Map::new();
        // chat 的 tool_calls 数组下标：用上游的 contentBlockIndex 直接映射
        call.insert("index".to_string(), json!(index));
        if !id.is_empty() {
            call.insert("id".to_string(), json!(id));
        }
        call.insert("type".to_string(), json!("function"));
        call.insert("function".to_string(), Value::Object(function));
        self.delta_frame(json!({ "tool_calls": [ Value::Object(call) ] }))
    }

    /// 收尾帧（带 finish_reason）
    fn finish_frame(&self, reason: &str) -> bytes::Bytes {
        chat_frame(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [ { "index": 0, "delta": {}, "finish_reason": reason } ],
        }))
    }
}

/// 非流式响应：把完整 Converse 响应体（`/converse` 的返回）折成 chat 完成体。
///
/// 流式路径不用它 —— 那条路走事件流翻译 + 既有聚合器。本函数服务
/// 「上游非流式」的场景（当前 forward 恒请求 `converse-stream`，保留它是为了
/// 未来支持非流式上游时不改调用面）。
pub fn chat_from_converse_response(body: &Value, model: &str) -> Value {
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    if let Some(blocks) = body
        .get("output")
        .and_then(|value| value.get("message"))
        .and_then(|value| value.get("content"))
        .and_then(Value::as_array)
    {
        for block in blocks {
            if let Some(chunk) = block.get("text").and_then(Value::as_str) {
                text.push_str(chunk);
            }
            if let Some(tool_use) = block.get("toolUse") {
                tool_calls.push(json!({
                    "id": tool_use.get("toolUseId").cloned().unwrap_or(json!("")),
                    "type": "function",
                    "function": {
                        "name": tool_use.get("name").cloned().unwrap_or(json!("")),
                        "arguments": tool_use
                            .get("input")
                            .map(|input| input.to_string())
                            .unwrap_or_else(|| "{}".to_string()),
                    }
                }));
            }
        }
    }
    let stop_reason = body
        .get("stopReason")
        .and_then(Value::as_str)
        .unwrap_or("end_turn");
    let finish_reason = match stop_reason {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        "content_filtered" | "guardrail_intervened" => "content_filter",
        _ => "stop",
    };
    let mut usage = BedrockUsage::default();
    usage.absorb(body.get("usage").unwrap_or(&Value::Null));

    let mut message = Map::new();
    message.insert("role".to_string(), json!("assistant"));
    message.insert("content".to_string(), json!(text));
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }

    json!({
        "id": random_id("chatcmpl"),
        "object": "chat.completion",
        "created": crate::server::logging::now_ms() / 1000,
        "model": model,
        "choices": [ {
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason,
        } ],
        "usage": usage.to_chat(),
    })
}

// ─── 单元测试 ───────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // 签名用例的固定凭据。刻意写成不可能撞上真凭据的形状（连 `AKIA` 前缀都
    // 不出现、无熵），免得密钥扫描把测试夹具误判成泄漏的 AWS key。
    const TEST_ACCESS_KEY: &str = "example-access-key-id";
    const TEST_SECRET_KEY: &str = "example-secret-access-key";

    #[test]
    fn split_credentials_accepts_two_and_three_parts() {
        let (ak, sk, region) = split_credentials("AKIA123:secret456").expect("两段应可解析");
        assert_eq!(ak, "AKIA123");
        assert_eq!(sk, "secret456");
        assert_eq!(region, DEFAULT_REGION);

        let (_, _, region) =
            split_credentials("AKIA123:secret456:eu-west-1").expect("三段应可解析");
        assert_eq!(region, "eu-west-1");

        assert!(split_credentials("").is_err());
        assert!(split_credentials("onlykey").is_err());
        assert!(split_credentials("AKIA123:").is_err());
    }

    #[test]
    fn request_translation_maps_roles_and_inference() {
        let chat = json!({
            "model": "us.anthropic.claude-sonnet-4-6",
            "messages": [
                { "role": "system", "content": "be brief" },
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": "hello" },
                { "role": "user", "content": "again" },
            ],
            "max_tokens": 128,
            "temperature": 0.3,
            "stop": ["END"],
        });
        let out = bedrock_request_from_chat(&chat, "us.anthropic.claude-sonnet-4-6")
            .expect("翻译应成功");
        // model 不进 body
        assert!(out.get("model").is_none());
        assert_eq!(out["system"][0]["text"], "be brief");
        assert_eq!(out["messages"].as_array().map(Vec::len), Some(3));
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"][0]["text"], "hi");
        assert_eq!(out["inferenceConfig"]["maxTokens"], 128);
        assert_eq!(out["inferenceConfig"]["stopSequences"][0], "END");
    }

    fn cache_point_positions(blocks: &Value) -> Vec<usize> {
        blocks
            .as_array()
            .map(|items| {
                items.iter().enumerate().filter(|(_, b)| is_cache_point(b)).map(|(i, _)| i).collect()
            })
            .unwrap_or_default()
    }

    fn total_cache_points(out: &Value) -> usize {
        let mut total = cache_point_positions(&out["system"]).len()
            + cache_point_positions(&out["toolConfig"]["tools"]).len();
        for message in out["messages"].as_array().into_iter().flatten() {
            total += cache_point_positions(&message["content"]).len();
        }
        total
    }

    /// chat 客户端不带断点：Claude 模型在 system / tools / 最后一条 user 末尾自动补 cachePoint
    #[test]
    fn bedrock_claude_gets_automatic_cache_points() {
        let chat = json!({
            "messages": [
                { "role": "system", "content": "sys" },
                { "role": "user", "content": "q1" },
                { "role": "assistant", "content": "a1" },
                { "role": "user", "content": "q2" },
            ],
            "tools": [{ "type": "function", "function": { "name": "t", "parameters": { "type": "object" } } }],
        });
        let out = bedrock_request_from_chat(&chat, "us.anthropic.claude-opus-5-5").expect("翻译应成功");
        assert_eq!(out["system"], json!([{ "text": "sys" }, { "cachePoint": { "type": "default" } }]));
        assert_eq!(cache_point_positions(&out["toolConfig"]["tools"]), vec![1]);
        assert_eq!(cache_point_positions(&out["messages"][2]["content"]), vec![1]);
        assert!(cache_point_positions(&out["messages"][0]["content"]).is_empty());
        assert_eq!(total_cache_points(&out), 3);
    }

    /// 客户端断点（_wb_cache_control）翻成 cachePoint，总数不超 4，裁剪优先删最早的消息断点
    #[test]
    fn bedrock_client_breakpoints_translate_and_respect_limit() {
        let cc = json!({ "type": "ephemeral" });
        let chat = json!({
            "messages": [
                { "role": "system", "content": "sys", FIELD_CACHE_CONTROL: cc },
                { "role": "user", "content": "q1", FIELD_CACHE_CONTROL: cc },
                { "role": "assistant", "content": "a1", FIELD_CACHE_CONTROL: cc },
                { "role": "user", "content": "q2", FIELD_CACHE_CONTROL: cc },
                { "role": "assistant", "content": "a2" },
                { "role": "user", "content": "q3" },
            ],
            "tools": [{ "name": "t", "input_schema": { "type": "object" } }],
        });
        let out = bedrock_request_from_chat(&chat, "anthropic.claude-sonnet-4-6").expect("翻译应成功");
        assert_eq!(total_cache_points(&out), MAX_CACHE_POINTS);
        // system 与 tools 的断点保留
        assert_eq!(cache_point_positions(&out["system"]), vec![1]);
        assert_eq!(cache_point_positions(&out["toolConfig"]["tools"]), vec![1]);
        // 最后一条 user 必有断点；最早的两个消息断点被裁掉
        assert_eq!(cache_point_positions(&out["messages"][4]["content"]), vec![1]);
        assert!(cache_point_positions(&out["messages"][0]["content"]).is_empty());
        assert!(cache_point_positions(&out["messages"][1]["content"]).is_empty());
        // 内部字段不外泄
        assert!(!serde_json::to_string(&out).unwrap_or_default().contains("_wb_"));
    }

    /// 不支持缓存的模型不加 cachePoint（否则上游 400）；Nova 不给工具加
    #[test]
    fn bedrock_cache_points_only_for_supported_models() {
        let chat = json!({
            "messages": [
                { "role": "system", "content": "sys", FIELD_CACHE_CONTROL: { "type": "ephemeral" } },
                { "role": "user", "content": "q" },
            ],
            "tools": [{ "name": "t", "input_schema": { "type": "object" } }],
        });
        for model in ["us.moonshotai.kimi-k3", "meta.llama3-70b-instruct-v1:0",
            "arn:aws:bedrock:us-west-2:123:application-inference-profile/abc"] {
            let out = bedrock_request_from_chat(&chat, model).expect("翻译应成功");
            assert_eq!(total_cache_points(&out), 0, "{model}");
        }
        let out = bedrock_request_from_chat(&chat, "us.amazon.nova-pro-v1:0").expect("翻译应成功");
        assert_eq!(cache_point_positions(&out["system"]), vec![1]);
        assert!(cache_point_positions(&out["toolConfig"]["tools"]).is_empty());
        assert_eq!(cache_point_positions(&out["messages"][0]["content"]), vec![1]);
    }

    #[test]
    fn bedrock_kimi_k3_caps_output_budget_below_reported_limit() {
        for model in [
            "moonshotai.kimi-k3",
            "us.moonshotai.kimi-k3",
            "eu.moonshotai.kimi-k3",
            "ap.moonshotai.kimi-k3",
            "global.moonshotai.kimi-k3",
            "us.moonshotai.kimi-k3:0",
            "arn:aws:bedrock:us-west-2:123456789012:inference-profile/us.moonshotai.kimi-k3",
            "arn:aws:bedrock:us-west-2::foundation-model/moonshotai.kimi-k3",
        ] {
            for key in ["max_tokens", "max_completion_tokens"] {
                for requested in [128_000, 131_072, 1_000_000] {
                    let mut chat = json!({ "messages": [{ "role": "user", "content": "hi" }] });
                    chat[key] = json!(requested);
                    let out = bedrock_request_from_chat(&chat, model).expect("翻译应成功");
                    assert_eq!(out["inferenceConfig"]["maxTokens"], 127_999, "{model} {key}");
                    assert_eq!(chat[key], requested, "不应修改调用方的请求体");
                }
            }
        }
    }

    #[test]
    fn bedrock_output_budget_preserves_smaller_values_and_defaults() {
        for requested in [1, 4096, 32_768, 127_999] {
            let chat = json!({
                "messages": [{ "role": "user", "content": "hi" }],
                "max_completion_tokens": requested,
            });
            let out = bedrock_request_from_chat(&chat, "us.moonshotai.kimi-k3")
                .expect("翻译应成功");
            assert_eq!(out["inferenceConfig"]["maxTokens"], requested);
        }
        for invalid in [Value::Null, json!(0), json!(-1), json!("131072")] {
            let chat = json!({
                "messages": [{ "role": "user", "content": "hi" }],
                "max_tokens": invalid,
            });
            let out = bedrock_request_from_chat(&chat, "us.moonshotai.kimi-k3")
                .expect("翻译应成功");
            assert_eq!(out["inferenceConfig"]["maxTokens"], DEFAULT_MAX_TOKENS);
        }
        let out = bedrock_request_from_chat(
            &json!({ "messages": [{ "role": "user", "content": "hi" }] }),
            "us.moonshotai.kimi-k3",
        ).expect("翻译应成功");
        assert_eq!(out["inferenceConfig"]["maxTokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn bedrock_output_budget_prefers_completion_tokens_and_falls_back() {
        for (completion, expected) in [(json!(8192), 8192), (json!(0), 256)] {
            let chat = json!({
                "messages": [{ "role": "user", "content": "hi" }],
                "max_tokens": 256,
                "max_completion_tokens": completion,
            });
            let out = bedrock_request_from_chat(&chat, "us.moonshotai.kimi-k3")
                .expect("翻译应成功");
            assert_eq!(out["inferenceConfig"]["maxTokens"], expected);
        }
    }

    #[test]
    fn bedrock_output_budget_does_not_guess_other_model_limits() {
        for model in [
            "us.zai.glm-5.3",
            "moonshotai.kimi-k2.5",
            "us.moonshotai.kimi-k3-preview",
            "us.moonshotai.kimi-k30",
            "unknown.model",
            "arn:aws:bedrock:us-west-2:123456789012:application-inference-profile/opaque-id",
        ] {
            let chat = json!({
                "messages": [{ "role": "user", "content": "hi" }],
                "max_completion_tokens": 200_000,
            });
            let out = bedrock_request_from_chat(&chat, model).expect("翻译应成功");
            assert_eq!(out["inferenceConfig"]["maxTokens"], 200_000, "{model}");
        }
    }

    #[test]
    fn request_translation_rejects_empty_messages() {
        let chat = json!({ "messages": [] });
        assert!(bedrock_request_from_chat(&chat, "m").is_err());
        let chat = json!({ "messages": [{ "role": "user", "content": "x" }] });
        assert!(bedrock_request_from_chat(&chat, "  ").is_err());
    }

    // ─── 工具历史（2026-10-13 修）───────────────────────────────
    // 这组用例是上游 400 `messages: text content blocks must be non-empty`
    // 的回归护栏：当时 chat 形态的 tool_calls / role:"tool" 一个都没被翻，
    // 再叠上「补空 text 块」的兜底，整轮请求必被拒。

    /// 上游 body 里不得出现空 text 块（Converse 明确拒绝），也不得有空 content。
    fn assert_no_empty_text_blocks(wire: &Value) {
        let messages = wire["messages"].as_array().expect("messages 数组");
        for (index, message) in messages.iter().enumerate() {
            let blocks = message["content"].as_array().expect("content 数组");
            assert!(!blocks.is_empty(), "第 {index} 条消息的 content 不得为空");
            for block in blocks {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    assert!(!text.is_empty(), "第 {index} 条消息含空 text 块：{message}");
                }
            }
        }
    }

    /// assistant 的 `tool_calls` → toolUse 块；`role:"tool"` → user 的 toolResult 块。
    #[test]
    fn chat_tool_history_translates_to_tool_use_and_tool_result() {
        let chat = json!({
            "messages": [
                { "role": "user", "content": "read the file" },
                { "role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": { "name": "read", "arguments": "{\"path\":\"a.txt\"}" }
                }]},
                { "role": "tool", "tool_call_id": "call_1", "content": "file body" },
                { "role": "user", "content": "now summarise" },
            ]
        });
        let wire = bedrock_request_from_chat(&chat, "us.anthropic.claude-opus-5-5")
            .expect("翻译应成功");
        assert_no_empty_text_blocks(&wire);
        let messages = wire["messages"].as_array().unwrap();
        // user / assistant / user / user(合并) → 合并后 3 条
        assert_eq!(messages.len(), 3, "同角色相邻消息应合并：{messages:?}");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        // assistant 只剩 toolUse（content 是 null，不补空文本）
        assert_eq!(messages[1]["content"].as_array().map(Vec::len), Some(1));
        let tool_use = &messages[1]["content"][0]["toolUse"];
        assert_eq!(tool_use["toolUseId"], "call_1");
        assert_eq!(tool_use["name"], "read");
        assert_eq!(tool_use["input"]["path"], "a.txt", "arguments 要解析成对象");
        // 工具结果挂在 user 上，且排在文本之前
        assert_eq!(messages[2]["role"], "user");
        let first = &messages[2]["content"][0];
        assert_eq!(first["toolResult"]["toolUseId"], "call_1");
        assert_eq!(first["toolResult"]["content"][0]["text"], "file body");
        assert_eq!(messages[2]["content"][1]["text"], "now summarise");
    }

    /// 并行工具调用：一次 assistant 里多个 tool_calls，各自与结果配对。
    #[test]
    fn parallel_tool_calls_keep_every_pair() {
        let chat = json!({
            "messages": [
                { "role": "user", "content": "check both" },
                { "role": "assistant", "content": "sure", "tool_calls": [
                    { "id": "call_a", "function": { "name": "ls", "arguments": "{\"path\":\"/x\"}" } },
                    { "id": "call_b", "function": { "name": "read", "arguments": "{}" } },
                ]},
                { "role": "tool", "tool_call_id": "call_a", "content": "dir listing" },
                { "role": "tool", "tool_call_id": "call_b", "content": "file text" },
            ]
        });
        let wire = bedrock_request_from_chat(&chat, "m").expect("翻译应成功");
        assert_no_empty_text_blocks(&wire);
        let messages = wire["messages"].as_array().unwrap();
        let assistant = &messages[1]["content"];
        assert_eq!(assistant.as_array().map(Vec::len), Some(3), "文本 + 两个 toolUse");
        assert_eq!(assistant[1]["toolUse"]["toolUseId"], "call_a");
        assert_eq!(assistant[2]["toolUse"]["toolUseId"], "call_b");
        let results: Vec<&str> = messages[2]["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|block| block["toolResult"]["toolUseId"].as_str())
            .collect();
        assert_eq!(results, vec!["call_a", "call_b"]);
    }

    /// content=null 且无 tool_calls 的 assistant：整条跳过，不补空 text 块。
    #[test]
    fn empty_assistant_is_skipped_instead_of_blank_text() {
        let chat = json!({
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": "" },
                { "role": "user", "content": "still there?" },
            ]
        });
        let wire = bedrock_request_from_chat(&chat, "m").expect("翻译应成功");
        assert_no_empty_text_blocks(&wire);
        let messages = wire["messages"].as_array().unwrap();
        // 空的 assistant 被跳过，两条 user 合并成一条 —— 没有任何占位块
        assert_eq!(messages.len(), 1, "空消息不应占位：{messages:?}");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"].as_array().map(Vec::len), Some(2));
    }

    /// 工具结果：空输出与只有图片两种边界都要产出非空 content。
    #[test]
    fn tool_result_handles_empty_and_image_only_outputs() {
        let chat = json!({
            "messages": [
                { "role": "user", "content": "go" },
                { "role": "assistant", "content": null, "tool_calls": [
                    { "id": "call_empty", "function": { "name": "noop", "arguments": "{}" } },
                    { "id": "call_img", "function": { "name": "shot", "arguments": "{}" } },
                ]},
                { "role": "tool", "tool_call_id": "call_empty", "content": "" },
                { "role": "tool", "tool_call_id": "call_img", "content": [
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }
                ]},
                { "role": "tool", "tool_call_id": "call_err", "content": "boom", "_wb_is_error": true },
            ]
        });
        let wire = bedrock_request_from_chat(&chat, "m").expect("翻译应成功");
        assert_no_empty_text_blocks(&wire);
        let results: Vec<&Value> = wire["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().unwrap())
            .filter_map(|block| block.get("toolResult"))
            .collect();
        // call_err 没有配对的调用，但 toolResult 本身仍应翻译（配对由上游校验）
        assert_eq!(results.len(), 3, "三条结果都要产出块：{results:?}");
        assert_eq!(results[0]["toolUseId"], "call_empty");
        assert!(results[0]["content"][0]["text"].as_str().is_some_and(|t| !t.is_empty()));
        assert_eq!(results[1]["toolUseId"], "call_img");
        assert_eq!(results[1]["content"][0]["image"]["format"], "png");
        assert_eq!(results[2]["status"], "error", "失败标记要带回去");
    }

    /// 缺 id 的 tool 结果不伪造配对钥匙（宁可少一个块，不可编一个 id）。
    #[test]
    fn tool_result_without_id_is_dropped() {
        let chat = json!({
            "messages": [
                { "role": "user", "content": "go" },
                { "role": "tool", "content": "orphan output" },
                { "role": "user", "content": "tail" },
            ]
        });
        let wire = bedrock_request_from_chat(&chat, "m").expect("翻译应成功");
        assert_no_empty_text_blocks(&wire);
        let rendered = wire["messages"].to_string();
        assert!(!rendered.contains("toolResult"), "缺 id 不该造出 toolResult：{rendered}");
    }

    /// Anthropic 形态的 tool_use / tool_result 块仍然照旧可翻（两条入口并存）。
    #[test]
    fn anthropic_shaped_tool_blocks_still_translate() {
        let chat = json!({
            "messages": [
                { "role": "user", "content": "go" },
                { "role": "assistant", "content": [
                    { "type": "tool_use", "id": "tu_1", "name": "lookup", "input": { "q": "x" } }
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "tu_1", "content": "found" }
                ]},
            ]
        });
        let wire = bedrock_request_from_chat(&chat, "m").expect("翻译应成功");
        assert_no_empty_text_blocks(&wire);
        let rendered = wire["messages"].to_string();
        assert!(rendered.contains("tu_1"), "Anthropic 形态的配对要保住：{rendered}");
        assert!(rendered.contains("lookup"));
        assert!(rendered.contains("found"));
    }

    /// 真实失败请求的形状：一长串「content=null + tool_calls」+ 逐条结果，
    /// 修好之后上游 body 里不得再有任何空 text 块，且每对调用/结果都在。
    #[test]
    fn long_agent_history_produces_no_empty_text_blocks() {
        let mut messages: Vec<Value> = vec![json!({ "role": "user", "content": "开始" })];
        for round in 0..30 {
            let id = format!("call_{round}");
            messages.push(json!({
                "role": "assistant", "content": null,
                "tool_calls": [{ "id": id, "type": "function",
                    "function": { "name": "TaskCreate", "arguments": "{\"subject\":\"x\"}" } }]
            }));
            messages.push(json!({ "role": "tool", "tool_call_id": id, "content": "{\"ok\":true}" }));
        }
        let chat = json!({ "messages": messages });
        let wire = bedrock_request_from_chat(&chat, "us.anthropic.claude-opus-5-5")
            .expect("翻译应成功");
        assert_no_empty_text_blocks(&wire);
        let rendered = wire["messages"].to_string();
        for round in 0..30 {
            assert!(rendered.contains(&format!("call_{round}")), "第 {round} 对不该丢");
        }
        // 每条 assistant 只该有 toolUse 一个块，不该混进空文本
        let assistants: Vec<&Value> = wire["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "assistant")
            .collect();
        assert_eq!(assistants.len(), 30);
        for assistant in assistants {
            assert_eq!(assistant["content"].as_array().map(Vec::len), Some(1));
            assert!(assistant["content"][0].get("toolUse").is_some());
        }
    }

    #[test]
    fn tool_spec_translation_covers_both_shapes() {
        let openai = json!({
            "type": "function",
            "function": {
                "name": "lookup",
                "description": "d",
                "parameters": { "type": "object", "properties": { "q": { "type": "string" } } }
            }
        });
        let spec = converse_tool_spec(&openai).expect("OpenAI 形态应可翻");
        assert_eq!(spec["toolSpec"]["name"], "lookup");
        assert!(spec["toolSpec"]["inputSchema"]["json"].is_object());

        let flat = json!({ "name": "flat", "input_schema": { "type": "object" } });
        let spec = converse_tool_spec(&flat).expect("扁平形态应可翻");
        assert_eq!(spec["toolSpec"]["name"], "flat");
    }

    #[test]
    fn uri_encoding_double_encodes_arns_only() {
        let (req, canon) = model_path_segments("us.anthropic.claude-sonnet-4-6");
        assert_eq!(req, "us.anthropic.claude-sonnet-4-6");
        assert_eq!(canon, req);

        let arn = "arn:aws:bedrock:us-west-2:612025356221:application-inference-profile/abc";
        let (req, canon) = model_path_segments(arn);
        assert!(req.contains("%3A"), "请求段应编码冒号: {req}");
        assert!(req.contains("%2F"), "请求段应编码斜杠: {req}");
        assert!(canon.contains("%253A"), "签名段应双重编码冒号: {canon}");
        assert!(canon.contains("%252F"), "签名段应双重编码斜杠: {canon}");
    }

    #[test]
    fn uri_encoding_handles_colon_suffixed_model_ids() {
        // 回归护栏（2026-10-12 实测修正）：`us.amazon.nova-premier-v1:0` 这类
        // 带冒号的普通模型 ID 也曾因「只对 ARN 做编码」而签名失败。
        // 规则是：**所有非 unreserved 字符一律编码**，canonical 一律再编一层。
        let model = "us.amazon.nova-premier-v1:0";
        let (req, canon) = model_path_segments(model);
        assert_eq!(
            req, "us.amazon.nova-premier-v1%3A0",
            "请求段应编码冒号: {req}"
        );
        assert_eq!(
            canon, "us.amazon.nova-premier-v1%253A0",
            "签名段应双重编码冒号: {canon}"
        );
        assert_ne!(req, canon, "含特殊字符的模型名两段必须不同");

        // 幂等性：只含 unreserved 字符时两段相同
        let (req, canon) = model_path_segments("moonshotai.kimi-k2.5");
        assert_eq!(req, canon);
        assert_eq!(req, "moonshotai.kimi-k2.5");
    }

    #[test]
    fn event_stream_decoder_parses_real_frame() {
        // 构造一个 messageStop 帧（无 CRC 校验，CRC 填 0）
        let payload = br#"{"stopReason":"end_turn"}"#;
        let mut headers = Vec::new();
        let name = b":event-type";
        headers.push(name.len() as u8);
        headers.extend_from_slice(name);
        headers.push(7u8);
        let value = b"messageStop";
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value);

        let total = 12 + headers.len() + payload.len() + 4;
        let mut frame = Vec::new();
        frame.extend_from_slice(&(total as u32).to_be_bytes());
        frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        frame.extend_from_slice(&[0u8; 4]); // prelude crc（不校验）
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&[0u8; 4]); // message crc（不校验）

        let mut decoder = EventStreamDecoder::new();
        // 分两次喂入，验证跨 chunk 缓冲
        let half = frame.len() / 2;
        assert!(decoder.push(&frame[..half]).is_empty(), "半帧不应产出事件");
        let events = decoder.push(&frame[half..]);
        assert_eq!(events.len(), 1);
        let DecodedEvent::Message(kind, payload) = &events[0] else {
            panic!("正常事件不应解析为异常");
        };
        assert_eq!(kind, "messageStop");
        let body: Value = serde_json::from_slice(payload).expect("payload 应是 JSON");
        assert_eq!(body["stopReason"], "end_turn");
    }

    // 合成 AWS 二进制帧；与现有 decoder 测试一致，CRC 暂不校验。
    fn bedrock_test_frame(headers: &[(&str, &str)], payload: &[u8], extra: &[u8]) -> Vec<u8> {
        let mut encoded = extra.to_vec();
        for (name, value) in headers {
            encoded.push(name.len() as u8);
            encoded.extend_from_slice(name.as_bytes());
            encoded.push(7);
            encoded.extend_from_slice(&(value.len() as u16).to_be_bytes());
            encoded.extend_from_slice(value.as_bytes());
        }
        let mut frame = Vec::new();
        frame.extend_from_slice(&((16 + encoded.len() + payload.len()) as u32).to_be_bytes());
        frame.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
        frame.extend_from_slice(&[0; 4]);
        frame.extend_from_slice(&encoded);
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&[0; 4]);
        frame
    }

    fn bedrock_frames_text(frames: &[bytes::Bytes]) -> String {
        frames.iter().map(|frame| String::from_utf8_lossy(frame).into_owned()).collect()
    }

    #[test]
    fn bedrock_filter_only_stream_preserves_stop_reason_and_usage() {
        // 2026-10-12 Opus 5.5 真实上游形状：没有 messageStart，只有
        // messageStop(content_filtered) + metadata，不能误判为无效事件流。
        for reason in ["content_filtered", "guardrail_intervened"] {
            let mut wire = bedrock_test_frame(
                &[(":message-type", "event"), (":event-type", "messageStop")],
                json!({"stopReason":reason}).to_string().as_bytes(), &[],
            );
            wire.extend(bedrock_test_frame(
                &[(":message-type", "event"), (":event-type", "metadata")],
                br#"{"usage":{"inputTokens":48918,"outputTokens":5,"totalTokens":48923}}"#,
                &[],
            ));
            let mut machine = ChatFromBedrockStream::new("us.anthropic.claude-opus-5-5");
            let mut frames = Vec::new();
            for chunk in wire.chunks(11) { frames.extend(machine.push(chunk)); }
            frames.extend(machine.finish());
            frames.extend(machine.finish());
            let text = bedrock_frames_text(&frames);
            assert!(text.contains("\"finish_reason\":\"content_filter\""));
            assert!(text.contains("\"code\":\"content_filter\""));
            assert!(text.contains("内容审核"));
            assert!(text.contains("\"prompt_tokens\":48918"));
            assert!(text.contains("\"completion_tokens\":5"));
            assert!(!text.contains("bedrock_stream_error"));
            assert!(!text.contains("未返回有效"));
            assert_eq!(text.matches("data: [DONE]").count(), 1);
            assert!(machine.last_error().is_some_and(|message| message.contains("内容审核")));
        }
    }

    #[test]
    fn bedrock_message_stop_without_start_is_a_valid_terminal_event() {
        let mut machine = ChatFromBedrockStream::new("m");
        let mut frames = machine.handle("messageStop", &json!({"stopReason":"end_turn"}));
        frames.extend(machine.handle("metadata", &json!({"usage":{"inputTokens":3,"outputTokens":0}})));
        frames.extend(machine.finish());
        let text = bedrock_frames_text(&frames);
        assert!(text.contains("\"role\":\"assistant\""));
        assert!(text.contains("\"finish_reason\":\"stop\""));
        assert!(text.contains("\"prompt_tokens\":3"));
        assert!(!text.contains("\"error\""));
        assert!(machine.last_error().is_none());
    }

    #[test]
    fn bedrock_exception_header_preserves_error_across_chunks() {
        let frame = bedrock_test_frame(
            &[(":message-type", "exception"), (":exception-type", "validationException")],
            br#"{"message":"The model rejected the inference configuration"}"#,
            &[],
        );
        let mut stream = ChatFromBedrockStream::new("us.anthropic.claude-opus-5-5");
        let mut frames = Vec::new();
        for chunk in frame.chunks(7) {
            frames.extend(stream.push(chunk));
        }
        assert!(!frames.is_empty(), "异常应立即输出，而不是等 EOF");
        frames.extend(stream.finish());
        frames.extend(stream.finish());
        let text = bedrock_frames_text(&frames);
        assert!(text.contains("The model rejected the inference configuration"));
        assert!(text.contains("\"code\":\"validationException\""));
        assert_eq!(text.matches("data: [DONE]").count(), 1);
        assert!(!text.contains("\"finish_reason\""));
        assert!(!text.contains("\"usage\""));
    }

    #[test]
    fn bedrock_exception_after_content_is_not_swallowed() {
        let mut stream = ChatFromBedrockStream::new("m");
        let mut frames = stream.handle("messageStart", &json!({"role":"assistant"}));
        frames.extend(stream.handle("contentBlockDelta", &json!({"delta":{"text":"partial"}})));
        frames.extend(stream.push(&bedrock_test_frame(
            &[(":message-type", "exception"), (":exception-type", "modelStreamErrorException")],
            br#"{"message":"upstream stream failed","originalMessage":"specific model failure","originalStatusCode":400}"#,
            &[],
        )));
        frames.extend(stream.handle("contentBlockDelta", &json!({"delta":{"text":"must not leak"}})));
        frames.extend(stream.finish());
        let text = bedrock_frames_text(&frames);
        assert!(text.contains("partial"));
        assert!(text.contains("upstream stream failed"));
        assert!(text.contains("specific model failure"));
        assert!(!text.contains("must not leak"));
        assert!(!text.contains("\"finish_reason\":\"stop\""));
        assert_eq!(text.matches("data: [DONE]").count(), 1);
    }

    #[test]
    fn bedrock_unmodeled_error_headers_are_not_dropped() {
        let frame = bedrock_test_frame(
            &[(":error-message", "service unavailable"), (":error-code", "ServiceUnavailable"), (":message-type", "error")],
            b"",
            &[],
        );
        let mut stream = ChatFromBedrockStream::new("m");
        let mut frames = stream.push(&frame);
        frames.extend(stream.finish());
        let text = bedrock_frames_text(&frames);
        assert!(text.contains("service unavailable"));
        assert!(text.contains("\"code\":\"ServiceUnavailable\""));
        assert_eq!(text.matches("data: [DONE]").count(), 1);
    }

    #[test]
    fn bedrock_error_headers_after_other_header_types_are_parsed() {
        let values = [
            (0, vec![]), (1, vec![]), (2, vec![0]), (3, vec![0; 2]),
            (4, vec![0; 4]), (5, vec![0; 8]), (6, vec![0, 2, 1, 2]),
            (8, vec![0; 8]), (9, vec![0; 16]),
        ];
        let mut extra = Vec::new();
        for (kind, value) in values {
            let name = format!("extra{kind}");
            extra.push(name.len() as u8);
            extra.extend_from_slice(name.as_bytes());
            extra.push(kind);
            extra.extend_from_slice(&value);
        }
        let mut stream = ChatFromBedrockStream::new("m");
        let frame = bedrock_test_frame(
            &[(":message-type", "exception"), (":exception-type", "validationException")],
            br#"{"Message":"invalid request"}"#,
            &extra,
        );
        let mut frames = stream.push(&frame);
        frames.extend(stream.finish());
        assert!(bedrock_frames_text(&frames).contains("invalid request"));
    }

    #[tokio::test]
    async fn bedrock_exception_becomes_502_when_aggregated() {
        use crate::server::core::upstream::{aggregate::aggregate_frame_stream, usage::RequestTelemetry};
        let mut machine = ChatFromBedrockStream::new("m");
        let mut frames = machine.push(&bedrock_test_frame(
            &[(":message-type", "exception"), (":exception-type", "validationException")],
            br#"{"message":"specific validation failure"}"#,
            &[],
        ));
        frames.extend(machine.finish());
        let stream = futures::stream::iter(frames.into_iter().map(Ok::<_, std::io::Error>));
        let result = aggregate_frame_stream(
            Box::pin(stream),
            std::sync::Arc::new(RequestTelemetry::new()),
            crate::server::core::upstream::sse::FramePolicy::with_rewrite(None),
        ).await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("异常不能聚合为成功"),
        };
        assert_eq!(error.status_code, 502);
        assert!(error.message.contains("specific validation failure"));
    }

    #[test]
    fn decoder_ignores_malformed_lengths() {
        let mut decoder = EventStreamDecoder::new();
        // 总长字段荒谬（小于最小值）→ 不 panic、不产出
        let mut bad = Vec::new();
        bad.extend_from_slice(&4u32.to_be_bytes());
        bad.extend_from_slice(&0u32.to_be_bytes());
        bad.extend_from_slice(&[0u8; 32]);
        assert!(decoder.push(&bad).is_empty());
    }

    #[test]
    fn stream_translates_text_events_to_chat_frames() {
        let mut stream = ChatFromBedrockStream::new("us.anthropic.claude-sonnet-4-6");
        let mut frames = stream.push(b"");
        frames.extend(stream.handle("messageStart", &json!({ "role": "assistant" })));
        frames.extend(stream.handle(
            "contentBlockDelta",
            &json!({ "contentBlockIndex": 0, "delta": { "text": "Hello" } }),
        ));
        frames.extend(stream.handle("messageStop", &json!({ "stopReason": "end_turn" })));
        frames.extend(stream.handle(
            "metadata",
            &json!({ "usage": { "inputTokens": 7, "outputTokens": 3 } }),
        ));
        frames.extend(stream.finish());

        let joined: String = frames
            .iter()
            .map(|frame| String::from_utf8_lossy(frame).to_string())
            .collect();
        assert!(joined.contains("\"role\":\"assistant\""), "应有首帧");
        assert!(joined.contains("\"content\":\"Hello\""), "应有文本增量");
        assert!(joined.contains("\"finish_reason\":\"stop\""), "应有收尾帧");
        assert!(joined.contains("\"prompt_tokens\":7"), "应有 usage 帧");
        assert!(joined.contains("data: [DONE]"), "应有 [DONE]");

        // 回归护栏：delta 只应有一层 —— 曾出现过 {"delta":{"delta":{...}}} 的
        // 双重嵌套（delta_frame 自己就包 "delta" 键，调用处又套了一层）
        assert!(
            !joined.contains("\"delta\":{\"delta\""),
            "delta 不应双重嵌套: {joined}"
        );
    }

    #[test]
    fn stream_translates_tool_use_events() {
        let mut stream = ChatFromBedrockStream::new("m");
        let mut frames = stream.handle("messageStart", &json!({ "role": "assistant" }));
        frames.extend(stream.handle(
            "contentBlockStart",
            &json!({ "contentBlockIndex": 1, "start": { "toolUse": { "toolUseId": "tu_1", "name": "lookup" } } }),
        ));
        frames.extend(stream.handle(
            "contentBlockDelta",
            &json!({ "contentBlockIndex": 1, "delta": { "toolUse": { "input": "{\"q\":" } } }),
        ));
        frames.extend(stream.handle("contentBlockStop", &json!({ "contentBlockIndex": 1 })));
        frames.extend(stream.handle("messageStop", &json!({ "stopReason": "tool_use" })));
        frames.extend(stream.finish());

        let joined: String = frames
            .iter()
            .map(|frame| String::from_utf8_lossy(frame).to_string())
            .collect();
        assert!(joined.contains("\"name\":\"lookup\""), "应有工具名");
        assert!(joined.contains("\"toolUseId\"").eq(&false), "不应泄漏上游字段名");
        assert!(joined.contains("\"tool_calls\""), "应有 tool_calls 帧");
        assert!(joined.contains("\"finish_reason\":\"tool_calls\""), "应有 tool_calls 收尾");
    }

    #[test]
    fn stream_without_events_reports_error_not_empty_success() {
        let mut stream = ChatFromBedrockStream::new("m");
        let frames = stream.finish();
        let joined: String = frames
            .iter()
            .map(|frame| String::from_utf8_lossy(frame).to_string())
            .collect();
        assert!(joined.contains("\"error\""), "空流应收尾为错误帧");
        assert!(joined.contains("data: [DONE]"));
    }

    #[test]
    fn signing_produces_expected_shape() {
        let signed = sign_request(
            "POST",
            "bedrock-runtime.us-west-2.amazonaws.com",
            "/model/us.anthropic.claude-sonnet-4-6/converse-stream",
            "",
            "{\"messages\":[]}",
            Some("application/json"),
            TEST_ACCESS_KEY,
            TEST_SECRET_KEY,
            "us-west-2",
            "20261012T044836Z",
            "20261012",
        );
        assert!(signed.authorization.starts_with(&format!(
            "AWS4-HMAC-SHA256 Credential={TEST_ACCESS_KEY}/20261012/us-west-2/bedrock/aws4_request"
        )));
        assert!(signed.authorization.contains("SignedHeaders=content-type;host;x-amz-date"));
        assert!(signed.authorization.contains("Signature="));
        assert_eq!(signed.amz_date, "20261012T044836Z");
    }

    #[test]
    fn get_signing_omits_content_type() {
        // GET（模型清单）不带 body、不发 Content-Type —— 签名串里也不能列它，
        // 否则上游以 SignatureDoesNotMatch 拒绝（签了的头必须实际存在）
        let signed = sign_request(
            "GET",
            "bedrock.us-west-2.amazonaws.com",
            "/foundation-models",
            "byOutputModality=TEXT",
            "",
            None,
            TEST_ACCESS_KEY,
            TEST_SECRET_KEY,
            "us-west-2",
            "20261012T044836Z",
            "20261012",
        );
        assert!(
            signed.authorization.contains("SignedHeaders=host;x-amz-date"),
            "GET 应只签 host 与 x-amz-date: {}",
            signed.authorization
        );
        assert!(
            !signed.authorization.contains("content-type"),
            "GET 不应签 content-type: {}",
            signed.authorization
        );
        assert!(signed.authorization.contains("Signature="));
    }

    #[test]
    fn get_and_post_signatures_differ() {
        // 同一 host/时间下，签不签 content-type 必须产出不同签名 ——
        // 否则说明 canonical headers 没真正参与（回归护栏）
        let common = |ct: Option<&str>| {
            sign_request(
                "POST", "bedrock-runtime.us-west-2.amazonaws.com", "/model/m/converse",
                "", "{}", ct, "AK", "SK", "us-west-2", "20261012T044836Z", "20261012",
            )
            .authorization
        };
        assert_ne!(common(Some("application/json")), common(None));
    }

    #[test]
    fn timestamps_have_aws_shape() {
        let (amz, date) = amz_timestamps();
        assert_eq!(amz.len(), 16, "amz_date 应是 20261012T044836Z 形态: {amz}");
        assert!(amz.ends_with('Z'));
        assert_eq!(date.len(), 8, "date_stamp 应是 20261012 形态: {date}");
        assert_eq!(&amz[..8], date);
    }

    #[test]
    fn civil_from_unix_matches_known_epoch() {
        // 1970-01-01T00:00:00Z
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 2026-10-12T03:58:36Z（本文件抓包时段的 epoch 秒）
        let (y, m, d, hh, mm, ss) = civil_from_unix(1_791_777_516);
        assert_eq!((y, m, d), (2026, 10, 12));
        assert_eq!((hh, mm, ss), (3, 58, 36));
        // 2026-10-12T04:48:36Z（网关实测抓包的 UTC 时刻）
        let (y, m, d, hh, mm, ss) = civil_from_unix(1_791_780_516);
        assert_eq!((y, m, d), (2026, 10, 12));
        assert_eq!((hh, mm, ss), (4, 48, 36));
    }

    fn stream_usage(model: &str, updates: &[Value]) -> Value {
        let mut machine = ChatFromBedrockStream::new(model);
        machine.handle("messageStart", &json!({"role":"assistant"}));
        for usage in updates {
            machine.handle("metadata", &json!({"usage":usage}));
        }
        machine.handle("messageStop", &json!({"stopReason":"end_turn"}));
        machine.finish().iter().filter_map(|frame| {
            let text = std::str::from_utf8(frame).ok()?.trim().strip_prefix("data: ")?;
            serde_json::from_str::<Value>(text).ok()?.get("usage").cloned()
        }).next().expect("收尾应包含usage")
    }

    #[test]
    fn bedrock_cache_usage_counts_cached_input_for_all_openai_models() {
        for model in ["us.openai.gpt-6-astra", "us.openai.gpt-6.1-sol", "us.openai.gpt-6-sol"] {
            let raw = json!({"inputTokens":2,"outputTokens":40,"totalTokens":42,
                "cacheReadInputTokens":40000,"cacheWriteInputTokens":1000});
            let usage = stream_usage(model, &[raw.clone()]);
            assert_eq!(usage["prompt_tokens"], 41002, "{model}");
            assert_eq!(usage["completion_tokens"], 40);
            assert_eq!(usage["total_tokens"], 41042);
            assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 40000);
            assert_eq!(usage["cache_creation_input_tokens"], 1000);
            assert_eq!(usage["bedrock_usage"], raw, "保留原始字段，避免混淆口径");
            let tokens = crate::server::core::upstream::usage::extract_usage(&usage).unwrap();
            assert_eq!(tokens.prompt, 41002);
            assert_eq!(tokens.cache_read, 40000);
            assert_eq!(tokens.total, 41042);
        }
    }

    #[test]
    fn bedrock_cache_usage_stream_and_completion_match() {
        for raw in [
            json!({"inputTokens":5,"outputTokens":2,"totalTokens":7}),
            json!({"inputTokens":2,"outputTokens":10,"totalTokens":1012,"cacheReadInputTokens":1000,"cacheWriteInputTokens":0}),
            json!({"inputTokens":0,"outputTokens":10,"cacheReadInputTokens":0,"cacheWriteInputTokens":1024,
                "cacheDetails":[{"inputTokens":1024,"ttl":"1h"}]}),
        ] {
            let streamed = stream_usage("m", &[raw.clone()]);
            let completion = chat_from_converse_response(&json!({"usage":raw,"stopReason":"end_turn"}), "m");
            assert_eq!(streamed, completion["usage"]);
            let read = raw["cacheReadInputTokens"].as_i64().unwrap_or(0);
            let write = raw["cacheWriteInputTokens"].as_i64().unwrap_or(0);
            let input = raw["inputTokens"].as_i64().unwrap_or(0);
            assert_eq!(streamed["prompt_tokens"], input + read + write);
            if raw.get("cacheDetails").is_some() {
                assert_eq!(streamed["bedrock_usage"]["cacheDetails"], raw["cacheDetails"]);
            }
        }
    }

    #[test]
    fn bedrock_cache_usage_distinguishes_absent_zero_and_write_only() {
        let missing = stream_usage("m", &[json!({"inputTokens":2,"outputTokens":3})]);
        assert!(missing.pointer("/prompt_tokens_details/cached_tokens").is_none());
        assert!(missing.get("cache_creation_input_tokens").is_none());
        let zero = stream_usage("m", &[json!({"inputTokens":2,"outputTokens":3,"cacheReadInputTokens":0,"cacheWriteInputTokens":0})]);
        assert_eq!(zero["prompt_tokens_details"]["cached_tokens"], 0);
        assert_eq!(zero["cache_creation_input_tokens"], 0);
        let write_only = stream_usage("m", &[json!({"inputTokens":2,"outputTokens":3,"cacheWriteInputTokens":1000})]);
        assert_eq!(write_only["prompt_tokens"], 1002);
        assert!(write_only.pointer("/prompt_tokens_details/cached_tokens").is_none());
    }

    #[test]
    fn bedrock_cache_usage_updates_are_snapshots_not_deltas() {
        let usage = stream_usage("m", &[
            json!({"inputTokens":2,"outputTokens":10,"cacheReadInputTokens":1000,"cacheWriteInputTokens":500}),
            json!({"outputTokens":20}),
            json!({"cacheReadInputTokens":0,"cacheWriteInputTokens":500}),
        ]);
        assert_eq!(usage["prompt_tokens"], 502);
        assert_eq!(usage["completion_tokens"], 20);
        assert_eq!(usage["total_tokens"], 522);
        assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 0);
        assert_eq!(usage["cache_creation_input_tokens"], 500);
    }

    #[test]
    fn bedrock_cache_usage_ignores_invalid_counts_and_saturates() {
        let usage = stream_usage("m", &[
            json!({"inputTokens":2,"outputTokens":3,"cacheReadInputTokens":1000}),
            json!({"inputTokens":-1,"outputTokens":"wrong","cacheReadInputTokens":null,"cacheWriteInputTokens":1.5,
                "secret":"must-not-be-exported"}),
        ]);
        assert_eq!(usage["prompt_tokens"], 1002);
        assert_eq!(usage["completion_tokens"], 3);
        assert!(usage["bedrock_usage"].get("secret").is_none());
        assert!(usage.get("cache_creation_input_tokens").is_none());
        let max = stream_usage("m", &[json!({"inputTokens":i64::MAX,"outputTokens":1,"cacheReadInputTokens":i64::MAX})]);
        assert_eq!(max["prompt_tokens"], i64::MAX);
        assert_eq!(max["total_tokens"], i64::MAX);
    }

    #[test]
    fn bedrock_cache_usage_anthropic_downstream_preserves_read_and_write() {
        let usage = stream_usage("m", &[json!({"inputTokens":2,"outputTokens":5,
            "cacheReadInputTokens":4096,"cacheWriteInputTokens":512})]);
        let anthropic = crate::server::core::protocol::anthropic::usage_to_anthropic(Some(&usage));
        assert_eq!(anthropic["input_tokens"], 2);
        assert_eq!(anthropic["output_tokens"], 5);
        assert_eq!(anthropic["cache_read_input_tokens"], 4096);
        assert_eq!(anthropic["cache_creation_input_tokens"], 512);
        let original = json!({"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":20}});
        assert_eq!(crate::server::core::protocol::anthropic::usage_to_anthropic(Some(&original)),
            json!({"input_tokens":80,"output_tokens":10,"cache_read_input_tokens":20}));
    }

    #[tokio::test]
    async fn bedrock_cache_usage_survives_binary_stream_and_aggregation() {
        use crate::server::core::upstream::{aggregate::aggregate_frame_stream, usage::RequestTelemetry};
        let mut wire = bedrock_test_frame(&[(":event-type","messageStart")], br#"{"role":"assistant"}"#, &[]);
        wire.extend(bedrock_test_frame(&[(":event-type","messageStop")], br#"{"stopReason":"end_turn"}"#, &[]));
        wire.extend(bedrock_test_frame(&[(":event-type","metadata")],
            br#"{"usage":{"inputTokens":2,"outputTokens":8,"cacheReadInputTokens":4096,"cacheWriteInputTokens":512}}"#, &[]));
        let mut machine = ChatFromBedrockStream::new("us.openai.gpt-6-astra");
        let mut frames = Vec::new();
        for chunk in wire.chunks(13) { frames.extend(machine.push(chunk)); }
        frames.extend(machine.finish());
        // 流式ForwardStream消费的合并器也必须把缓存命中记进同一统计槽。
        let streamed_telemetry = std::sync::Arc::new(RequestTelemetry::new());
        let mut coalescer = crate::server::core::upstream::sse::ReasoningCoalescer::with_telemetry(streamed_telemetry.clone());
        for frame in &frames { coalescer.push(frame); }
        coalescer.finish();
        assert_eq!(streamed_telemetry.snapshot().prompt_tokens, 4610);
        assert_eq!(streamed_telemetry.snapshot().cache_read_tokens, 4096);
        let telemetry = std::sync::Arc::new(RequestTelemetry::new());
        let input = futures::stream::iter(frames.into_iter().map(Ok::<_, std::io::Error>));
        let out = aggregate_frame_stream(
            Box::pin(input),
            telemetry.clone(),
            crate::server::core::upstream::sse::FramePolicy::with_rewrite(None),
        ).await.unwrap();
        assert_eq!(out.body["usage"]["prompt_tokens"], 4610);
        assert_eq!(out.body["usage"]["prompt_tokens_details"]["cached_tokens"], 4096);
        assert_eq!(out.body["usage"]["cache_creation_input_tokens"], 512);
        assert_eq!(telemetry.snapshot().prompt_tokens, 4610);
        assert_eq!(telemetry.snapshot().cache_read_tokens, 4096);
    }

    #[test]
    fn converse_response_folds_to_chat_completion() {
        let body = json!({
            "output": { "message": { "role": "assistant", "content": [
                { "text": "Hi" }, { "text": " there" }
            ] } },
            "stopReason": "end_turn",
            "usage": { "inputTokens": 5, "outputTokens": 2 }
        });
        let chat = chat_from_converse_response(&body, "m");
        assert_eq!(chat["choices"][0]["message"]["content"], "Hi there");
        assert_eq!(chat["choices"][0]["finish_reason"], "stop");
        assert_eq!(chat["usage"]["prompt_tokens"], 5);
        assert_eq!(chat["usage"]["total_tokens"], 7);
    }
}
