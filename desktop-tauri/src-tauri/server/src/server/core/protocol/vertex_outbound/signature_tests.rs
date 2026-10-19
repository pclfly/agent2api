use super::super::anthropic::{
    anthropic_from_chat, chat_from_anthropic, AnthropicCollector, AnthropicStream,
};
use super::super::responses::{
    chat_from_responses, responses_from_chat, ResponsesCollector, ResponsesStream,
};
use super::*;

const FALLBACK: &str = "skip_thought_signature_validator";
const SIG: &str = "vertex-original-signature+/==";

fn vertex_frames(parts: Value) -> Vec<bytes::Bytes> {
    let mut stream = ChatFromVertexStream::new("m");
    let event = json!({"candidates": [{"content": {"parts": parts}, "finishReason": "STOP"}]});
    let wire = format!("data: {event}\n\n");
    // Exercise the public byte interface, including a split SSE line.
    let split = wire.len() / 2;
    let mut frames = stream.push(&wire.as_bytes()[..split]);
    frames.extend(stream.push(&wire.as_bytes()[split..]));
    frames.extend(stream.finish());
    assert_eq!(stream.last_error(), None);
    frames
}

fn events(frames: &[bytes::Bytes]) -> Vec<Value> {
    frames
        .iter()
        .flat_map(|frame| {
            std::str::from_utf8(frame)
                .expect("UTF-8 SSE")
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim)
                .filter(|data| *data != "[DONE]")
                .map(|data| serde_json::from_str(data).expect("JSON SSE payload"))
                .collect::<Vec<Value>>()
        })
        .collect()
}

// Vertex emits each complete function call in one Chat delta. Preserve the
// actual emitted call object here, including its ID and provider extension.
fn assistant_from_frames(frames: &[bytes::Bytes]) -> Value {
    let mut calls = Vec::new();
    let mut text = String::new();
    let mut reasoning = String::new();
    for event in events(frames) {
        let delta = &event["choices"][0]["delta"];
        if let Some(items) = delta["tool_calls"].as_array() {
            calls.extend(items.iter().cloned());
        }
        if let Some(part) = delta["content"].as_str() {
            text.push_str(part);
        }
        if let Some(part) = delta["reasoning_content"].as_str() {
            reasoning.push_str(part);
        }
    }
    json!({"role": "assistant", "content": text, "reasoning_content": reasoning,
        "tool_calls": calls})
}

fn signed_frames() -> Vec<bytes::Bytes> {
    vertex_frames(json!([
        {"functionCall": {"name": "Bash", "args": {"command": "pwd"}}, "thoughtSignature": SIG},
        {"functionCall": {"name": "Read", "args": {"path": "Cargo.toml"}}}
    ]))
}

fn completion(assistant: &Value) -> Value {
    json!({"id": "signature-test-completion", "object": "chat.completion", "model": "m",
        "choices": [{"index": 0, "message": assistant, "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}})
}

fn strip_extensions(assistant: &mut Value) {
    for call in assistant["tool_calls"].as_array_mut().expect("tool calls") {
        let object = call.as_object_mut().expect("call object");
        object.remove("extra_content");
        object.remove("thoughtSignature");
        object.remove("thought_signature");
    }
}

fn history(assistant: &Value) -> Value {
    let mut messages = vec![
        json!({"role": "user", "content": "Run the tools"}),
        assistant.clone(),
    ];
    for call in assistant["tool_calls"].as_array().expect("tool calls") {
        // Intentionally omit name: Vertex must resolve it via tool_call_id.
        messages.push(json!({"role": "tool", "tool_call_id": call["id"], "content": "ok"}));
    }
    json!({"model": "m", "messages": messages})
}

fn function_parts<'a>(body: &'a Value, field: &str) -> Vec<&'a Value> {
    body["contents"]
        .as_array()
        .expect("Vertex contents")
        .iter()
        .flat_map(|content| content["parts"].as_array().expect("Vertex parts"))
        .filter(|part| part.get(field).is_some())
        .collect()
}

fn assert_vertex(body: &Value, names: &[&str], signatures: &[Option<&str>]) {
    assert_eq!(names.len(), signatures.len());
    let contents = body["contents"].as_array().expect("contents");
    let mut expected_results = 0;
    for content in contents {
        let parts = content["parts"].as_array().expect("parts");
        let calls = parts.iter().filter(|part| part.get("functionCall").is_some()).count();
        let results = parts.iter().filter(|part| part.get("functionResponse").is_some()).count();
        if expected_results > 0 {
            assert_eq!(content["role"], "user");
            assert_eq!(results, expected_results, "all parallel results must share the next content");
            expected_results = 0;
        } else {
            assert_eq!(results, 0, "results must follow their call turn");
        }
        if calls > 0 {
            assert_eq!(content["role"], "model");
            expected_results = calls;
        }
    }
    assert_eq!(expected_results, 0, "last call turn must have results");
    let calls = function_parts(body, "functionCall");
    let results = function_parts(body, "functionResponse");
    assert_eq!(calls.len(), names.len(), "all calls must survive");
    assert_eq!(results.len(), names.len(), "all results must survive");
    for (index, (name, signature)) in names.iter().zip(signatures).enumerate() {
        assert_eq!(calls[index]["functionCall"]["name"].as_str(), Some(*name));
        match signature {
            Some(expected) => {
                assert_eq!(calls[index]["thoughtSignature"].as_str(), Some(*expected))
            }
            None => assert!(
                calls[index].get("thoughtSignature").is_none(),
                "unsigned means absent"
            ),
        }
        assert_eq!(
            results[index]["functionResponse"]["name"].as_str(),
            Some(*name)
        );
        assert_eq!(
            results[index]["functionResponse"]["response"]["result"],
            "ok"
        );
        assert!(results[index].get("thoughtSignature").is_none());
    }
}

fn assert_parallel_roundtrip(chat: &Value) {
    let vertex = vertex_request_from_chat(chat, "m").expect("Vertex request");
    assert_vertex(&vertex, &["Bash", "Read"], &[Some(SIG), None]);
    let calls = function_parts(&vertex, "functionCall");
    assert_eq!(calls[0]["functionCall"]["args"], json!({"command": "pwd"}));
    assert_eq!(
        calls[1]["functionCall"]["args"],
        json!({"path": "Cargo.toml"})
    );
}

fn anthropic_history(response: &Value) -> Value {
    let results: Vec<Value> = response["content"]
        .as_array()
        .expect("Anthropic content")
        .iter()
        .filter(|block| block["type"] == "tool_use")
        .map(|block| json!({"type": "tool_result", "tool_use_id": block["id"], "content": "ok"}))
        .collect();
    assert_eq!(results.len(), 2);
    chat_from_anthropic(&json!({"model": "m", "max_tokens": 100, "messages": [
        {"role": "user", "content": "Run the tools"},
        {"role": "assistant", "content": response["content"]},
        {"role": "user", "content": results}
    ]}))
    .expect("Anthropic history to Chat")
}

fn responses_history(response: &Value) -> Value {
    let output = response["output"].as_array().expect("Responses output");
    let mut input = vec![json!({"role": "user", "content": "Run the tools"})];
    input.extend(output.iter().cloned());
    let mut result_count = 0;
    for item in output.iter().filter(|item| item["type"] == "function_call") {
        input.push(
            json!({"type": "function_call_output", "call_id": item["call_id"], "output": "ok"}),
        );
        result_count += 1;
    }
    assert_eq!(result_count, 2);
    chat_from_responses(&json!({"model": "m", "input": input})).expect("Responses history to Chat")
}

#[test]
fn sequential_bash_rounds_preserve_distinct_signatures_and_result_names() {
    let first = assistant_from_frames(&vertex_frames(json!([
        {"functionCall": {"name": "Bash", "args": {"command": "pwd"}}, "thoughtSignature": "round-one"}
    ])));
    let mut chat = history(&first);
    assert_vertex(
        &vertex_request_from_chat(&chat, "m").expect("first round"),
        &["Bash"],
        &[Some("round-one")],
    );
    let second = assistant_from_frames(&vertex_frames(json!([
        {"functionCall": {"name": "Bash", "args": {"command": "ls"}}, "thoughtSignature": "round-two"}
    ])));
    assert_ne!(first["tool_calls"][0]["id"], second["tool_calls"][0]["id"]);
    chat["messages"].as_array_mut().expect("history").extend([
        second.clone(),
        json!({"role": "tool", "tool_call_id": second["tool_calls"][0]["id"], "content": "ok"}),
    ]);
    assert_vertex(
        &vertex_request_from_chat(&chat, "m").expect("second round"),
        &["Bash", "Bash"],
        &[Some("round-one"), Some("round-two")],
    );
}

#[test]
fn parallel_results_flush_at_end_as_one_content() {
    let chat = history(&assistant_from_frames(&signed_frames()));
    let vertex = vertex_request_from_chat(&chat, "m").expect("request");
    let contents = vertex["contents"].as_array().expect("contents");
    assert_eq!(contents.len(), 3);
    assert_eq!(contents[1]["parts"].as_array().unwrap().len(), 2);
    assert_eq!(contents[2]["parts"].as_array().unwrap().len(), 2);
    assert_parallel_roundtrip(&chat);
}

#[test]
fn parallel_batches_remain_separate_across_model_turns() {
    let first = assistant_from_frames(&signed_frames());
    let second = assistant_from_frames(&signed_frames());
    let mut chat = history(&first);
    chat["messages"].as_array_mut().unwrap().extend(
        history(&second)["messages"].as_array().unwrap().iter().skip(1).cloned(),
    );
    let vertex = vertex_request_from_chat(&chat, "m").expect("request");
    assert_vertex(&vertex, &["Bash", "Read", "Bash", "Read"], &[Some(SIG), None, Some(SIG), None]);
    let contents = vertex["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 5);
    assert_eq!(contents.iter().map(|c| c["role"].as_str().unwrap()).collect::<Vec<_>>(),
        vec!["user", "model", "user", "model", "user"]);
    for content in &contents[1..] {
        assert_eq!(content["parts"].as_array().unwrap().len(), 2);
    }
}

#[test]
fn parallel_results_do_not_merge_with_following_text_or_image() {
    let mut chat = history(&assistant_from_frames(&signed_frames()));
    chat["messages"].as_array_mut().unwrap().extend([
        json!({"role":"user","content":[
            {"type":"text","text":"tool image"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}
        ]}),
        json!({"role":"user","content":"continue"}),
    ]);
    let vertex = vertex_request_from_chat(&chat, "m").expect("request");
    assert_vertex(&vertex, &["Bash", "Read"], &[Some(SIG), None]);
    let contents = vertex["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 5);
    assert_eq!(contents[2]["parts"].as_array().unwrap().len(), 2);
    assert_eq!(contents[3]["parts"][0]["text"], "tool image");
    assert_eq!(contents[3]["parts"][1]["inlineData"]["data"], "AAAA");
    assert_eq!(contents[4]["parts"][0]["text"], "continue");
}

#[test]
fn sanitized_parallel_history_keeps_both_results_in_one_turn() {
    let mut chat = history(&assistant_from_frames(&signed_frames()));
    chat["messages"].as_array_mut().unwrap().insert(3, json!({"role":"user","content":"interleaved note"}));
    super::super::history::sanitize_history(&mut chat);
    let vertex = vertex_request_from_chat(&chat, "m").expect("request");
    assert_vertex(&vertex, &["Bash", "Read"], &[Some(SIG), None]);
    assert_eq!(vertex["contents"].as_array().unwrap().len(), 4);
    assert_eq!(vertex["contents"][2]["parts"].as_array().unwrap().len(), 2);
    assert_eq!(vertex["contents"][3]["parts"][0]["text"], "interleaved note");
}

#[test]
fn parallel_call_only_first_is_signed_and_second_remains_unsigned() {
    let assistant = assistant_from_frames(&signed_frames());
    let calls = assistant["tool_calls"].as_array().expect("calls");
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0]["extra_content"]["google"]["thought_signature"],
        SIG
    );
    assert!(calls[1].get("extra_content").is_none());
    assert_ne!(calls[0]["id"], calls[1]["id"]);
    assert_parallel_roundtrip(&history(&assistant));
}

#[test]
fn stripped_chat_extensions_recover_signed_and_unsigned_cache_entries() {
    let mut assistant = assistant_from_frames(&signed_frames());
    strip_extensions(&mut assistant);
    assert!(assistant["tool_calls"]
        .as_array()
        .expect("calls")
        .iter()
        .all(|call| call.get("extra_content").is_none()));
    assert_parallel_roundtrip(&history(&assistant));
}

#[test]
fn anthropic_stream_actual_tool_blocks_roundtrip_through_chat() {
    let mut stream = AnthropicStream::new("m");
    let mut frames = Vec::new();
    for frame in signed_frames() {
        frames.extend(stream.push(&frame));
    }
    frames.extend(stream.finish());
    // Reassemble the public Anthropic wire events as a client would. The
    // production AnthropicCollector consumes Chat SSE, not Anthropic SSE.
    let mut blocks = std::collections::BTreeMap::<i64, Value>::new();
    let mut arguments = std::collections::BTreeMap::<i64, String>::new();
    let mut stopped = false;
    for event in events(&frames) {
        match event["type"].as_str() {
            Some("content_block_start") => {
                let index = event["index"].as_i64().expect("block index");
                assert_eq!(event["content_block"]["type"], "tool_use");
                blocks.insert(index, event["content_block"].clone());
            }
            Some("content_block_delta") => {
                assert_eq!(event["delta"]["type"], "input_json_delta");
                arguments
                    .entry(event["index"].as_i64().expect("delta index"))
                    .or_default()
                    .push_str(event["delta"]["partial_json"].as_str().expect("arguments"));
            }
            Some("content_block_stop") => {
                let index = event["index"].as_i64().expect("stop index");
                let raw = arguments.remove(&index).expect("argument deltas");
                blocks.get_mut(&index).expect("started block")["input"] =
                    serde_json::from_str(&raw).expect("tool input");
            }
            Some("message_stop") => stopped = true,
            _ => {}
        }
    }
    assert!(stopped);
    assert!(arguments.is_empty());
    let content: Vec<Value> = blocks.into_values().collect();
    assert_parallel_roundtrip(&anthropic_history(&json!({"content": content})));
}

#[test]
fn anthropic_collector_actual_response_roundtrip_through_chat() {
    let mut collector = AnthropicCollector::new();
    for frame in signed_frames() {
        collector.push(&frame);
    }
    collector.finish();
    let response = collector.into_response("m");
    assert_eq!(response["stop_reason"], "tool_use");
    assert_parallel_roundtrip(&anthropic_history(&response));
}

#[test]
fn anthropic_nonstream_actual_response_roundtrip_through_chat() {
    let assistant = assistant_from_frames(&signed_frames());
    let response = anthropic_from_chat(&completion(&assistant), "m");
    assert_eq!(response["stop_reason"], "tool_use");
    assert_parallel_roundtrip(&anthropic_history(&response));
}

#[test]
fn responses_stream_completed_response_roundtrip_through_chat() {
    let mut stream = ResponsesStream::new("m", &json!({}));
    let mut frames = Vec::new();
    for frame in signed_frames() {
        frames.extend(stream.push(&frame));
    }
    frames.extend(stream.finish());
    let completed: Vec<Value> = events(&frames)
        .into_iter()
        .filter(|event| event["type"] == "response.completed")
        .collect();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0]["response"]["status"], "completed");
    assert_parallel_roundtrip(&responses_history(&completed[0]["response"]));
}

#[test]
fn responses_collector_actual_response_roundtrip_through_chat() {
    let mut collector = ResponsesCollector::new();
    for frame in signed_frames() {
        collector.push(&frame);
    }
    collector.finish();
    let response = collector.into_response("m", &json!({}));
    assert_eq!(response["status"], "completed");
    assert_parallel_roundtrip(&responses_history(&response));
}

#[test]
fn responses_nonstream_actual_response_roundtrip_through_chat() {
    let assistant = assistant_from_frames(&signed_frames());
    let response = responses_from_chat(&completion(&assistant), "m", &json!({}));
    assert_eq!(response["status"], "completed");
    assert_parallel_roundtrip(&responses_history(&response));
}

fn old_assistant() -> Value {
    json!({"role": "assistant", "content": null, "tool_calls": [{
        "id": "signature-tests-legacy-history-not-cached", "type": "function",
        "function": {"name": "Bash", "arguments": "{\"command\":\"pwd\"}"}
    }]})
}

#[test]
fn legacy_history_cache_miss_uses_exact_validator_fallback() {
    assert_vertex(
        &vertex_request_from_chat(&history(&old_assistant()), "m").expect("legacy history"),
        &["Bash"],
        &[Some(FALLBACK)],
    );
}

#[test]
fn empty_signature_fields_fall_back_to_cache_or_legacy_validator() {
    for empty in [json!(""), json!("   "), Value::Null] {
        let mut legacy = old_assistant();
        legacy["tool_calls"][0]["extra_content"] = json!({"google": {"thought_signature": empty}});
        legacy["tool_calls"][0]["thoughtSignature"] = empty.clone();
        legacy["tool_calls"][0]["thought_signature"] = empty.clone();
        assert_vertex(
            &vertex_request_from_chat(&history(&legacy), "m").expect("empty legacy fields"),
            &["Bash"],
            &[Some(FALLBACK)],
        );

        let mut cached = assistant_from_frames(&signed_frames());
        for call in cached["tool_calls"].as_array_mut().expect("calls") {
            call["extra_content"] = json!({"google": {"thought_signature": empty}});
            call["thoughtSignature"] = empty.clone();
            call["thought_signature"] = empty.clone();
        }
        assert_parallel_roundtrip(&history(&cached));
    }
}

#[test]
fn explicit_extension_and_both_aliases_override_cached_signature() {
    let original = assistant_from_frames(&signed_frames());
    for (extension, camel, snake, expected) in [
        (
            "explicit-extension",
            "explicit-camel",
            "explicit-snake",
            "explicit-extension",
        ),
        ("", "explicit-camel", "explicit-snake", "explicit-camel"),
        ("", "", "explicit-snake", "explicit-snake"),
    ] {
        let mut assistant = original.clone();
        // Explicit signatures override both cached signed and cached unsigned.
        for call in assistant["tool_calls"].as_array_mut().expect("calls") {
            call["extra_content"] = json!({"google": {"thought_signature": extension}});
            call["thoughtSignature"] = json!(camel);
            call["thought_signature"] = json!(snake);
        }
        assert_vertex(
            &vertex_request_from_chat(&history(&assistant), "m").expect("explicit signature"),
            &["Bash", "Read"],
            &[Some(expected), Some(expected)],
        );
    }
}

#[test]
fn upstream_camel_and_snake_signatures_are_preserved_exactly() {
    for field in ["thoughtSignature", "thought_signature"] {
        let mut part = json!({"functionCall": {"name": "Bash", "args": {"command": "pwd"}}});
        part[field] = json!(SIG);
        let assistant = assistant_from_frames(&vertex_frames(json!([part])));
        assert_eq!(
            assistant["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
            SIG
        );
        assert_vertex(
            &vertex_request_from_chat(&history(&assistant), "m").expect("signature alias"),
            &["Bash"],
            &[Some(SIG)],
        );
    }
}

#[test]
fn identical_calls_in_different_streams_have_unique_ids_and_own_signatures() {
    let mut ids = std::collections::HashSet::new();
    let mut histories = Vec::new();
    for index in 0..32 {
        let signature = format!("stream-signature-{index}");
        let mut assistant = assistant_from_frames(&vertex_frames(json!([
            {"functionCall": {"name": "Bash", "args": {"command": "pwd"}}, "thoughtSignature": signature}
        ])));
        let id = assistant["tool_calls"][0]["id"].as_str().expect("call id");
        assert!(id.starts_with("call"));
        assert!(
            ids.insert(id.to_owned()),
            "IDs must not reset for each stream"
        );
        strip_extensions(&mut assistant);
        histories.push((history(&assistant), signature));
    }
    // Check after all insertions so ID collisions cannot hide behind immediate reads.
    for (chat, signature) in histories {
        assert_vertex(
            &vertex_request_from_chat(&chat, "m").expect("cached stream"),
            &["Bash"],
            &[Some(signature.as_str())],
        );
    }
}

#[test]
fn cache_does_not_cross_model_arguments_name_or_call_id() {
    let mut assistant = assistant_from_frames(&vertex_frames(json!([
        {"functionCall": {"name": "Bash", "args": {"command": "pwd"}}, "thoughtSignature": SIG}
    ])));
    strip_extensions(&mut assistant);
    let original = history(&assistant);
    assert_vertex(
        &vertex_request_from_chat(&original, "another-model").expect("changed model"),
        &["Bash"],
        &[Some(FALLBACK)],
    );
    let mut changed_args = assistant.clone();
    changed_args["tool_calls"][0]["function"]["arguments"] = json!("{\"command\":\"ls\"}");
    assert_vertex(
        &vertex_request_from_chat(&history(&changed_args), "m").expect("changed args"),
        &["Bash"],
        &[Some(FALLBACK)],
    );
    let mut changed_name = assistant.clone();
    changed_name["tool_calls"][0]["function"]["name"] = json!("Read");
    assert_vertex(
        &vertex_request_from_chat(&history(&changed_name), "m").expect("changed name"),
        &["Read"],
        &[Some(FALLBACK)],
    );
    let mut changed_id = assistant.clone();
    changed_id["tool_calls"][0]["id"] = json!("signature-tests-uncached-replacement-id");
    assert_vertex(
        &vertex_request_from_chat(&history(&changed_id), "m").expect("changed id"),
        &["Bash"],
        &[Some(FALLBACK)],
    );
    assert_vertex(
        &vertex_request_from_chat(&original, "m").expect("original cache unchanged"),
        &["Bash"],
        &[Some(SIG)],
    );
}

#[test]
fn text_and_thought_signatures_never_migrate_to_function_calls() {
    let assistant = assistant_from_frames(&vertex_frames(json!([
        {"text": "thinking", "thought": true, "thoughtSignature": "thought-only-signature"},
        {"text": "running tools", "thought_signature": "text-only-signature"},
        {"functionCall": {"name": "Bash", "args": {"command": "pwd"}}},
        {"functionCall": {"name": "Read", "args": {"path": "Cargo.toml"}}, "thoughtSignature": SIG}
    ])));
    assert_eq!(assistant["content"], "running tools");
    assert_eq!(assistant["reasoning_content"], "thinking");
    assert!(assistant["tool_calls"][0].get("extra_content").is_none());
    assert_eq!(
        assistant["tool_calls"][1]["extra_content"]["google"]["thought_signature"],
        SIG
    );
    let mut stripped = assistant.clone();
    strip_extensions(&mut stripped);
    for message in [&assistant, &stripped] {
        let vertex =
            vertex_request_from_chat(&history(message), "m").expect("text signature isolation");
        assert_vertex(&vertex, &["Bash", "Read"], &[None, Some(SIG)]);
        for content in vertex["contents"].as_array().expect("contents") {
            for part in content["parts"].as_array().expect("parts") {
                if part.get("text").is_some() {
                    assert!(part.get("thoughtSignature").is_none());
                }
            }
        }
    }
}
