//! QwenWork SSE envelope decoder. Only a verified terminal signal completes a response.
use bytes::Bytes;
use serde_json::{json, Value};

pub struct Decoder {
    tail: Vec<u8>,
    data: Vec<String>,
    model: String,
    finished: bool,
    terminal: bool,
    content: bool,
    reported_credit: Option<f64>,
    error: Option<String>,
}
fn frame(v: &Value) -> Bytes {
    Bytes::from(format!("data: {v}\n\n"))
}
fn done(s: &str) -> bool {
    s.trim()
        .trim_matches(['[', ']', '"', '\''])
        .trim()
        .eq_ignore_ascii_case("done")
}

/// Reference empirical rates. This is not an upstream debit or a model price quote.
pub fn estimate(usage: &mut Value) {
    if usage
        .get("credit")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
        .is_some()
    {
        return;
    }
    let (Some(prompt), Some(completion)) = (
        usage.get("prompt_tokens").and_then(Value::as_u64),
        usage.get("completion_tokens").and_then(Value::as_u64),
    ) else {
        return;
    };
    let cached = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(prompt);
    let credit = (prompt - cached) as f64 * 0.00000744
        + cached as f64 * 0.000000795
        + completion as f64 * 0.00002381;
    usage["credit"] = json!((credit * 1_000_000.0).round() / 1_000_000.0);
    usage["credit_source"] = json!("estimate");
}
impl Decoder {
    pub fn new(model: &str) -> Self {
        Self {
            tail: Vec::new(),
            data: Vec::new(),
            model: model.into(),
            finished: false,
            terminal: false,
            content: false,
            reported_credit: None,
            error: None,
        }
    }
    pub fn is_finished(&self) -> bool {
        self.finished
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    fn fail(&mut self, status: u16, message: &str) -> Vec<Bytes> {
        self.finished = true;
        self.error = Some(message.to_string());
        vec![
            frame(
                &json!({"error":{"message":message,"status":status,"type":match status {401=>"authentication_error",429=>"rate_limit_error",503=>"temporarily_unavailable",_=>"upstream_error"}}}),
            ),
            Bytes::from_static(b"data: [DONE]\n\n"),
        ]
    }
    fn end(&mut self) -> Vec<Bytes> {
        if !self.content {
            return self.fail(502, "QwenWork 返回空响应");
        }
        self.finished = true;
        let mut frames = Vec::new();
        if !self.terminal {
            frames.push(frame(&json!({"object":"chat.completion.chunk","model":self.model,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})));
        }
        frames.push(Bytes::from_static(b"data: [DONE]\n\n"));
        frames
    }
    fn payload(&mut self, raw: &str, depth: u8) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        if done(raw) {
            return self.end();
        }
        if depth > 2 {
            return self.fail(502, "QwenWork 响应信封层数异常");
        }
        let Ok(mut v) = serde_json::from_str::<Value>(raw) else {
            return self.fail(502, "QwenWork SSE 数据格式无效");
        };
        let status = v
            .get("statusCodeValue")
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())
            .unwrap_or(200);
        if status >= 400
            || !super::auth::business_ok(&v)
            || v.get("error").is_some_and(|e| !e.is_null())
        {
            let classified = super::auth::error_status(status, &v.to_string());
            return self.fail(
                classified,
                &format!("QwenWork 上游返回业务错误（{classified}）"),
            );
        }
        if let Some(inner) = v.get("body").and_then(Value::as_str) {
            return self.payload(inner, depth + 1);
        }
        if let Some(inner) = v.get("body").filter(|v| v.is_object()) {
            return self.payload(&inner.to_string(), depth + 1);
        }
        let has_usage = v.get("usage").is_some_and(Value::is_object);
        let has_choices = v.get("choices").is_some_and(Value::is_array);
        if !has_usage && !has_choices {
            return Vec::new();
        }
        // A finish signal closes generation, not accounting: accept trailing usage only.
        if self.terminal {
            v["choices"] = json!([]);
            if !has_usage {
                return Vec::new();
            }
        }
        for choice in v
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let delta = choice.get("delta").or_else(|| choice.get("message"));
            if delta.is_some_and(|d| {
                d.get("content").is_some_and(|v| {
                    v.as_str().is_some_and(|s| !s.is_empty())
                        || v.as_array().is_some_and(|a| !a.is_empty())
                }) || d
                    .get("reasoning_content")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
                    || d.get("tool_calls")
                        .and_then(Value::as_array)
                        .is_some_and(|a| !a.is_empty())
            }) {
                self.content = true;
            }
            if let Some(reason) = choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && *s != "null")
            {
                if reason == "error" || reason == "upstream_error" {
                    return self.fail(502, "QwenWork 上游生成失败");
                }
                self.terminal = true;
            }
        }
        if let Some(usage) = v.get_mut("usage").filter(|v| v.is_object()) {
            if usage.get("credit_source").and_then(Value::as_str) != Some("estimate") {
                if let Some(credit) = usage
                    .get("credit")
                    .and_then(Value::as_f64)
                    .filter(|n| n.is_finite())
                {
                    self.reported_credit = Some(credit);
                }
            }
            if let Some(credit) = self.reported_credit {
                // A later token-only tail must not overwrite an earlier upstream debit.
                usage["credit"] = json!(credit);
                if let Some(object) = usage.as_object_mut() {
                    object.remove("credit_source");
                }
            } else {
                estimate(usage);
            }
        }
        v["model"] = json!(self.model);
        v["object"] = json!("chat.completion.chunk");
        if !has_choices {
            v["choices"] = json!([]);
        }
        vec![frame(&v)]
    }
    fn line(&mut self, line: &str) -> Vec<Bytes> {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            if self.data.is_empty() {
                return Vec::new();
            }
            let payload = std::mem::take(&mut self.data).join("\n");
            return self.payload(&payload, 0);
        }
        if let Some(data) = line.strip_prefix("data:") {
            self.data
                .push(data.strip_prefix(' ').unwrap_or(data).into());
        }
        Vec::new()
    }
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.tail.extend_from_slice(bytes);
        if self.tail.len() + self.data.iter().map(String::len).sum::<usize>() > 4 * 1024 * 1024 {
            return self.fail(502, "QwenWork SSE 单帧过大");
        }
        let mut out = Vec::new();
        let mut consumed = 0;
        while let Some(i) = self.tail[consumed..].iter().position(|b| *b == b'\n') {
            let end = consumed + i;
            let Ok(line) = String::from_utf8(self.tail[consumed..end].to_vec()) else {
                return self.fail(502, "QwenWork SSE 文本编码无效");
            };
            consumed = end + 1;
            out.extend(self.line(&line));
            if self.finished {
                self.tail.clear();
                return out;
            }
        }
        self.tail.drain(..consumed);
        out
    }
    pub fn finish(&mut self) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !self.tail.is_empty() {
            let Ok(line) = String::from_utf8(std::mem::take(&mut self.tail)) else {
                return self.fail(502, "QwenWork SSE 文本编码无效");
            };
            out.extend(self.line(&line));
        }
        out.extend(self.line(""));
        if !self.finished {
            if self.terminal {
                out.extend(self.end());
            } else {
                out.extend(self.fail(502, "QwenWork 响应被截断，未收到完成标记"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn collect(frames: Vec<Bytes>) -> String {
        frames.iter().map(|b| String::from_utf8_lossy(b)).collect()
    }
    #[test]
    fn envelope_fragmented_tools_usage_tail() {
        let content = json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_x","function":{"name":"f","arguments":"{\"x\":"}}]},"finish_reason":null}]});
        let finish = json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":"tool_calls"}]});
        let usage = json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":50}}});
        let source = format!(
            "data: {}\r\n\r\ndata: {finish}\n\ndata: {usage}\n\ndata: {{\"body\":\"DONE]\"}}\n\n",
            json!({"statusCodeValue":200,"body":content.to_string()})
        );
        let mut d = Decoder::new("test");
        let mut out = String::new();
        for b in source.as_bytes().chunks(3) {
            out.push_str(&collect(d.push(b)));
        }
        out.push_str(&collect(d.finish()));
        assert!(out.contains("call_x"));
        assert!(out.contains("tool_calls"));
        assert!(out.contains("credit_source\":\"estimate"));
        assert_eq!(out.matches("[DONE]").count(), 1);
        assert!(!out.contains("\"error\""));
    }
    #[test]
    fn truncated_and_business_errors_fail() {
        let mut d = Decoder::new("test");
        d.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        assert!(collect(d.finish()).contains("截断"));
        let mut d = Decoder::new("test");
        assert!(collect(
            d.push(b"data: {\"statusCodeValue\":429,\"body\":\"All backends failed\"}\n\n")
        )
        .contains("503"));
    }
    #[test]
    fn late_delta_is_dropped_but_usage_is_preserved() {
        let mut d = Decoder::new("test");
        d.push(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}

",
        );
        let frames=collect(d.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"late\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}

"));
        assert!(!frames.contains("late"));
        assert!(frames.contains("credit_source"));
    }
    #[test]
    fn estimate_missing_zero_and_reported() {
        let mut u = json!({"prompt_tokens":100});
        estimate(&mut u);
        assert!(u.get("credit").is_none());
        let mut u = json!({"prompt_tokens":0,"completion_tokens":0});
        estimate(&mut u);
        assert_eq!(u["credit"], 0.0);
        assert_eq!(u["credit_source"], "estimate");
        let mut u = json!({"prompt_tokens":100,"completion_tokens":200,"credit":0});
        estimate(&mut u);
        assert_eq!(u["credit"], 0);
        assert!(u.get("credit_source").is_none());
    }
    #[test]
    fn reported_credit_wins_across_usage_frames_including_zero() {
        for credit in [0.0, 1.25] {
            let mut decoder = Decoder::new("test");
            decoder.push(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}

",
            );
            let mut latest = Value::Null;
            for usage in [
                json!({"prompt_tokens":100,"completion_tokens":20}),
                json!({"prompt_tokens":100,"completion_tokens":20,"credit":credit}),
                json!({"prompt_tokens":100,"completion_tokens":20}),
            ] {
                let frames = decoder.push(
                    format!(
                        "data: {}

",
                        json!({"usage":usage})
                    )
                    .as_bytes(),
                );
                latest = serde_json::from_str::<Value>(
                    std::str::from_utf8(&frames[0])
                        .unwrap()
                        .trim()
                        .strip_prefix("data: ")
                        .unwrap(),
                )
                .unwrap();
            }
            assert_eq!(latest["usage"]["credit"], credit);
            assert!(latest["usage"].get("credit_source").is_none());
        }
        let mut usage = json!({"prompt_tokens":10,"completion_tokens":0,"prompt_tokens_details":{"cached_tokens":20}});
        estimate(&mut usage);
        assert_eq!(usage["credit"], 0.000008);
    }
    #[test]
    fn multi_data_lines_and_empty_stream() {
        let mut d = Decoder::new("test");
        let out=collect(d.push(b"data: {\"choices\":\ndata: [{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n"));
        assert!(out.contains("ok"));
        assert!(!collect(d.finish()).contains("error"));
        assert!(collect(Decoder::new("test").push(b"data: [DONE]\n\n")).contains("error"));
    }
}
