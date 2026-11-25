use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use http::{Request, Response, StatusCode};
use protocol::{
    CompletionRequest, FinishReason, Message, StreamEvent, ThinkingLevel, ToolCall, ToolDefinition,
};
use provider::Provider;
use serde_json::{Value, json};
use transport::{ByteStream, HttpTransport, TransportError};
use uno_provider_anthropic::AnthropicProvider;

struct MockTransport {
    sse_body: &'static str,
    captured_url: Mutex<Option<String>>,
    captured_body: Mutex<Option<Value>>,
}

#[async_trait]
impl HttpTransport for MockTransport {
    async fn send(&self, request: Request<Bytes>) -> Result<Response<ByteStream>, TransportError> {
        *self.captured_url.lock().unwrap() = Some(request.uri().to_string());
        *self.captured_body.lock().unwrap() = serde_json::from_slice(request.body()).ok();
        let stream =
            futures_util::stream::iter(vec![Ok(Bytes::from_static(self.sse_body.as_bytes()))]);
        Ok(Response::builder()
            .status(StatusCode::OK)
            .body(Box::pin(stream) as ByteStream)
            .unwrap())
    }
}

#[tokio::test]
async fn anthropic_provider_formats_request_and_parses_sse_stream() {
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":120,\"cache_read_input_tokens\":80}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"read_file\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":25}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );

    let transport = Arc::new(MockTransport {
        sse_body: sse,
        captured_url: Mutex::new(None),
        captured_body: Mutex::new(None),
    });

    let provider = AnthropicProvider::new(
        "deepseek-anthropic",
        "https://api.deepseek.com/anthropic",
        "test-key",
        transport.clone(),
    );

    let mut assistant_msg = Message::assistant("");
    assistant_msg.tool_calls.push(ToolCall {
        id: "prev_1".into(),
        name: "list_files".into(),
        arguments: json!({}),
        signature: None,
    });

    let req = CompletionRequest {
        model: "deepseek-v4-pro".into(),
        messages: vec![
            Message::system("Be concise."),
            Message::user("hi"),
            assistant_msg,
            Message::tool_result("prev_1", "main.rs"),
        ],
        tools: vec![ToolDefinition {
            name: "read_file".into(),
            description: "Read file".into(),
            input_schema: json!({"type": "object"}),
        }],
        max_tokens: Some(1024),
        temperature: None,
        thinking_level: None,
        ..CompletionRequest::default()
    };

    let mut stream = provider.stream(req).await.unwrap();
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.unwrap());
    }

    assert_eq!(
        transport.captured_url.lock().unwrap().as_deref(),
        Some("https://api.deepseek.com/anthropic/v1/messages")
    );
    let sent = transport.captured_body.lock().unwrap().clone().unwrap();
    assert_eq!(sent["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(sent["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(
        sent["messages"][2]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );

    assert!(matches!(events[0], StreamEvent::ReasoningDelta { ref delta } if delta == "hmm"));
    assert!(matches!(events[1], StreamEvent::TextDelta { ref delta } if delta == "Hello"));
    assert!(matches!(
        events[2],
        StreamEvent::ToolCallDelta {
            index: 1,
            id: Some(_),
            name: Some(_),
            ..
        }
    ));
    assert!(matches!(
        events[3],
        StreamEvent::ToolCallDelta { index: 1, ref arguments_delta, .. } if arguments_delta == "{\"path\":\"a.rs\"}"
    ));
    match &events[4] {
        StreamEvent::Finished { reason, usage } => {
            assert_eq!(*reason, FinishReason::ToolCall);
            let u = usage.as_ref().unwrap();
            assert_eq!(u.input_tokens, 120);
            assert_eq!(u.output_tokens, 25);
            assert_eq!(u.cached_tokens, Some(80));
        }
        other => panic!("unexpected final event: {other:?}"),
    }
}

#[tokio::test]
async fn maps_thinking_level_to_anthropic_thinking_budget() {
    let table = [
        (ThinkingLevel::Minimal, 1024_u64, 2048_u64),
        (ThinkingLevel::Low, 2048, 3072),
        (ThinkingLevel::Medium, 8192, 9216),
        (ThinkingLevel::High, 16384, 17408),
        (ThinkingLevel::XHigh, 32768, 33792),
    ];

    for (level, expected_budget, expected_max) in table {
        let transport = Arc::new(MockTransport {
            sse_body: "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            captured_url: Mutex::new(None),
            captured_body: Mutex::new(None),
        });
        let provider = AnthropicProvider::new(
            "anthropic",
            "https://api.anthropic.com",
            "test-key",
            transport.clone(),
        );
        let mut req = CompletionRequest {
            model: "claude-sonnet-4-6".into(),
            messages: vec![Message::user("hi")],
            tools: Vec::new(),
            max_tokens: Some(1024),
            temperature: Some(0.5),
            thinking_level: Some(level),
            session_id: Some("sess-abc".into()),
            ..CompletionRequest::default()
        };
        let _ = provider.stream(req.clone()).await.unwrap();
        let body = transport.captured_body.lock().unwrap().clone().unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], expected_budget);
        assert_eq!(body["max_tokens"], expected_max);
        assert!(body.get("temperature").is_none());
        assert_eq!(body["metadata"]["user_id"], "sess-abc");

        req.metadata
            .insert("user_id".into(), "explicit-user".into());
        let _ = provider.stream(req).await.unwrap();
        let body_explicit = transport.captured_body.lock().unwrap().clone().unwrap();
        assert_eq!(body_explicit["metadata"]["user_id"], "explicit-user");
    }

    let transport = Arc::new(MockTransport {
        sse_body: "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        captured_url: Mutex::new(None),
        captured_body: Mutex::new(None),
    });
    let provider = AnthropicProvider::new(
        "anthropic",
        "https://api.anthropic.com",
        "test-key",
        transport.clone(),
    );
    let off_req = CompletionRequest {
        model: "claude-sonnet-4-6".into(),
        messages: vec![Message::user("hi")],
        max_tokens: Some(1024),
        temperature: Some(0.5),
        thinking_level: Some(ThinkingLevel::Off),
        ..CompletionRequest::default()
    };
    let _ = provider.stream(off_req).await.unwrap();
    let off_body = transport.captured_body.lock().unwrap().clone().unwrap();
    assert!(off_body.get("thinking").is_none());
    assert_eq!(off_body["temperature"], 0.5);

    let adaptive_provider = AnthropicProvider::new(
        "anthropic",
        "https://api.anthropic.com",
        "test-key",
        transport.clone(),
    )
    .adaptive_thinking(true);
    let adaptive_table = [
        (ThinkingLevel::Minimal, "low"),
        (ThinkingLevel::Low, "low"),
        (ThinkingLevel::Medium, "medium"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::XHigh, "xhigh"),
    ];
    for (level, expected_effort) in adaptive_table {
        let adaptive_req = CompletionRequest {
            model: "claude-opus-4-6".into(),
            messages: vec![Message::user("hi")],
            max_tokens: Some(4096),
            temperature: Some(0.5),
            thinking_level: Some(level),
            ..CompletionRequest::default()
        };
        let _ = adaptive_provider.stream(adaptive_req).await.unwrap();
        let adaptive_body = transport.captured_body.lock().unwrap().clone().unwrap();
        assert_eq!(adaptive_body["thinking"]["type"], "adaptive");
        assert_eq!(adaptive_body["thinking"]["display"], "summarized");
        assert_eq!(adaptive_body["output_config"]["effort"], expected_effort);
        assert!(adaptive_body.get("temperature").is_none());
    }
}
