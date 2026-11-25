mod support;

use bytes::Bytes;
use futures_util::StreamExt;
use http::StatusCode;
use protocol::{FinishReason, Message, StreamEvent, Usage};
use provider::{Provider, ProviderError};
use serde_json::{Value, json};
use support::{provider_with_response, request_with};

fn chunk(delta: Value, finish_reason: Option<&str>) -> String {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }],
    })
    .to_string()
}

async fn events_from(chunks: &[String], done: bool) -> Vec<Result<StreamEvent, ProviderError>> {
    let mut body = String::new();
    for data in chunks {
        body.push_str("data: ");
        body.push_str(data);
        body.push_str("\n\n");
    }
    if done {
        body.push_str("data: [DONE]\n\n");
    }

    let (provider, _) = provider_with_response(StatusCode::OK, Bytes::from(body));
    provider
        .stream(request_with(vec![Message::user("hi")]))
        .await
        .expect("provider should return a stream")
        .collect()
        .await
}

fn finished(events: &[Result<StreamEvent, ProviderError>]) -> Vec<(FinishReason, Option<Usage>)> {
    events
        .iter()
        .filter_map(|event| match event {
            Ok(StreamEvent::Finished { reason, usage }) => Some((*reason, usage.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn finish_reason_accepts_openai_tool_calls_spelling() {
    let reason: FinishReason =
        serde_json::from_str(r#""tool_calls""#).expect("finish reason should deserialize");
    assert_eq!(reason, FinishReason::ToolCall);
}

#[tokio::test]
async fn text_delta_is_forwarded() {
    let events = events_from(&[chunk(json!({"content": "Hi"}), Some("stop"))], true).await;

    assert!(matches!(
        events.as_slice(),
        [
            Ok(StreamEvent::TextDelta { delta }),
            Ok(StreamEvent::Finished {
                reason: FinishReason::Stop,
                ..
            })
        ] if delta == "Hi"
    ));
}

#[tokio::test]
async fn finish_reason_and_trailing_usage_are_combined_once() {
    let usage = json!({
        "prompt_tokens": 10,
        "completion_tokens": 5,
        "total_tokens": 15,
    });
    let chunks = [
        chunk(
            json!({"tool_calls": [{
                "index": 0,
                "id": "call_1",
                "type": "function",
                "function": {"name": "lookup", "arguments": "{}"},
            }]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
        json!({"choices": [], "usage": usage}).to_string(),
    ];

    let events = events_from(&chunks, true).await;
    let finished = finished(&events);

    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].0, FinishReason::ToolCall);
    assert_eq!(
        finished[0].1.as_ref().map(|usage| usage.input_tokens),
        Some(10)
    );
}

#[tokio::test]
async fn usage_in_finish_chunk_is_reported_once() {
    let finish = json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "total_tokens": 15,
        },
    })
    .to_string();

    let events = events_from(&[finish], true).await;
    let finished = finished(&events);

    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].0, FinishReason::Stop);
    assert!(finished[0].1.is_some());
}

#[tokio::test]
async fn stream_without_finish_reason_ends_with_an_error() {
    let events = events_from(&[chunk(json!({"content": "partial"}), None)], false).await;

    assert!(matches!(
        events.last(),
        Some(Err(ProviderError::InvalidResponse(message)))
            if message.contains("without a finish reason")
    ));
}

#[tokio::test]
async fn malformed_json_is_reported_and_later_frames_are_processed() {
    let events = events_from(&["{not json".into(), chunk(json!({}), Some("stop"))], true).await;

    assert!(matches!(
        events.first(),
        Some(Err(ProviderError::InvalidResponse(message))) if message.contains("JSON parse error")
    ));
    assert_eq!(finished(&events).len(), 1);
}

async fn usage_from(chunk_usage: Value) -> Usage {
    let chunks = [
        chunk(json!({}), Some("stop")),
        json!({"choices": [], "usage": chunk_usage}).to_string(),
    ];
    let events = events_from(&chunks, true).await;
    finished(&events)
        .into_iter()
        .find_map(|(_, usage)| usage)
        .expect("expected usage on Finished")
}

#[tokio::test]
async fn cached_tokens_are_read_from_openai_usage_details() {
    let usage = usage_from(json!({
        "prompt_tokens": 10,
        "completion_tokens": 5,
        "prompt_tokens_details": {"cached_tokens": 7},
    }))
    .await;

    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.output_tokens, 5);
    assert_eq!(usage.cached_tokens, Some(7));
}

#[tokio::test]
async fn deepseek_cache_field_takes_priority() {
    let usage = usage_from(json!({
        "prompt_tokens": 10,
        "completion_tokens": 5,
        "prompt_cache_hit_tokens": 3,
        "prompt_tokens_details": {"cached_tokens": 7},
    }))
    .await;

    assert_eq!(usage.cached_tokens, Some(3));
}

#[tokio::test]
async fn usage_without_cache_info_has_no_cached_tokens() {
    let usage = usage_from(json!({"prompt_tokens": 10, "completion_tokens": 5})).await;

    assert_eq!(usage.cached_tokens, None);
}

#[tokio::test]
async fn reasoning_field_is_forwarded() {
    let events = events_from(
        &[chunk(json!({"reasoning": "thinking"}), Some("stop"))],
        true,
    )
    .await;

    assert!(matches!(
        events.first(),
        Some(Ok(StreamEvent::ReasoningDelta { delta })) if delta == "thinking"
    ));
}

#[tokio::test]
async fn reasoning_aliases_do_not_duplicate_a_delta() {
    let events = events_from(
        &[chunk(
            json!({"reasoning_content": "thinking", "reasoning": "thinking"}),
            Some("stop"),
        )],
        true,
    )
    .await;

    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Ok(StreamEvent::ReasoningDelta { .. })))
            .count(),
        1
    );
}

#[tokio::test]
async fn embedded_rate_limit_error_is_retryable() {
    let events = events_from(
        &[r#"{"error":{"message":"rate limited","code":429,"type":"rate_limit"}}"#.into()],
        true,
    )
    .await;

    assert!(matches!(
        events.as_slice(),
        [Err(ProviderError::StreamError { message, retryable: true })]
            if message.contains("rate limited") && message.contains("429")
    ));
}

#[tokio::test]
async fn plain_embedded_error_is_not_retryable() {
    let events = events_from(&[r#"{"error":"model overloaded"}"#.into()], true).await;

    assert!(matches!(
        events.as_slice(),
        [Err(ProviderError::StreamError { message, retryable: false })]
            if message.contains("model overloaded")
    ));
}

#[tokio::test]
async fn null_error_field_is_not_reported_as_an_embedded_error() {
    let events = events_from(&[r#"{"choices":[],"error":null}"#.into()], true).await;

    assert!(
        events
            .iter()
            .all(|event| !matches!(event, Err(ProviderError::StreamError { .. })))
    );
}

#[tokio::test]
async fn stream_emits_only_one_finished_event() {
    let events = events_from(
        &[
            chunk(json!({}), Some("stop")),
            chunk(json!({}), Some("stop")),
        ],
        true,
    )
    .await;

    assert_eq!(finished(&events).len(), 1);
}

#[tokio::test]
async fn polza_anthropic_cache_usage_fields_are_parsed_and_summed() {
    let events = events_from(
        &[
            chunk(json!({"content": "ok"}), Some("stop")),
            json!({
                "choices": [],
                "usage": {
                    "prompt_tokens": 120,
                    "completion_tokens": 45,
                    "cache_read_input_tokens": 1800,
                    "cache_creation_input_tokens": 80
                }
            })
            .to_string(),
        ],
        true,
    )
    .await;

    assert_eq!(
        finished(&events),
        vec![(
            FinishReason::Stop,
            Some(Usage {
                input_tokens: 2000,
                output_tokens: 45,
                cached_tokens: Some(1800),
            })
        )]
    );
}
