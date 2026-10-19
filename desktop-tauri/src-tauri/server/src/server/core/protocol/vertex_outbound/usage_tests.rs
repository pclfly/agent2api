use super::*;
use crate::server::core::upstream::{
    aggregate::aggregate_frame_stream,
    sse::FramePolicy,
    usage::{extract_usage, RequestTelemetry},
};
use std::sync::Arc;

fn frames(metadata: &[Value]) -> Vec<bytes::Bytes> {
    let mut machine = ChatFromVertexStream::new("gemini-test");
    let mut frames =
        machine.push(b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]}}]}\n\n");
    for meta in metadata {
        let wire = format!("data: {}\n\n", json!({"usageMetadata": meta}));
        frames.extend(machine.push(wire.as_bytes()));
    }
    frames.extend(machine.finish());
    frames
}

fn metadata(cached: Value) -> Value {
    json!({"promptTokenCount":1000,"candidatesTokenCount":40,"totalTokenCount":1040,
        "cachedContentTokenCount":cached})
}

#[test]
fn cached_input_is_a_subset_of_prompt_not_added_to_totals() {
    let usage = VertexUsage::from_metadata(&metadata(json!(750))).to_chat();
    assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 750);
    let tokens = extract_usage(&usage).expect("standard usage");
    assert_eq!(tokens.prompt, 1000);
    assert_eq!(tokens.completion, 40);
    assert_eq!(tokens.total, 1040);
    assert_eq!(tokens.cache_read, 750);
    assert_eq!(tokens.cache_read as f64 / tokens.prompt as f64, 0.75);
}

#[test]
fn missing_zero_or_invalid_cache_count_does_not_fabricate_hits() {
    for cached in [
        Value::Null,
        json!(0),
        json!(-1),
        json!("invalid"),
        json!({}),
    ] {
        let usage = VertexUsage::from_metadata(&metadata(cached)).to_chat();
        assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 0);
        assert_eq!(extract_usage(&usage).unwrap().cache_read, 0);
    }
    let usage =
        VertexUsage::from_metadata(&json!({"promptTokenCount":1000,"candidatesTokenCount":40}))
            .to_chat();
    assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 0);
    assert_eq!(usage["total_tokens"], 1040);
}

#[test]
fn streaming_usage_reports_latest_cache_count_to_telemetry() {
    let telemetry = RequestTelemetry::new();
    let frames = frames(&[metadata(json!(100)), metadata(json!(750))]);
    let mut usage_frames = 0;
    for frame in frames {
        for line in std::str::from_utf8(&frame).unwrap().lines() {
            if let Some(data) = line.strip_prefix("data: ").filter(|data| *data != "[DONE]") {
                let event: Value = serde_json::from_str(data).unwrap();
                if let Some(usage) = event.get("usage") {
                    usage_frames += 1;
                    assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 750);
                    telemetry.report_usage(usage);
                }
            }
        }
    }
    assert_eq!(usage_frames, 1);
    let snapshot = telemetry.snapshot();
    assert_eq!(snapshot.cache_read_tokens, 750);
    assert_eq!(snapshot.prompt_tokens, 1000);
}

#[tokio::test]
async fn nonstream_aggregation_preserves_cache_count_and_records_it() {
    let telemetry = Arc::new(RequestTelemetry::new());
    let stream = futures::stream::iter(frames(&[metadata(json!(750))]).into_iter().map(Ok));
    let completion =
        aggregate_frame_stream(Box::pin(stream), telemetry.clone(), FramePolicy::default())
            .await
            .expect("completion");
    assert_eq!(completion.body["choices"][0]["message"]["content"], "ok");
    assert_eq!(
        completion.body["usage"]["prompt_tokens_details"]["cached_tokens"],
        750
    );
    assert_eq!(completion.body["usage"]["prompt_tokens"], 1000);
    assert_eq!(telemetry.snapshot().cache_read_tokens, 750);
}
