mod support;

use http::StatusCode;
use protocol::{CompletionRequest, ContentPart, Message, ToolCall, ToolDefinition};
use provider::Provider;
use serde_json::{Value, json};
use support::{provider_with_response, request_with};

async fn serialized_request(request: CompletionRequest) -> Value {
    let (provider, captured_request) = provider_with_response(StatusCode::OK, Vec::<u8>::new());
    let _stream = provider
        .stream(request)
        .await
        .expect("provider should build the request");

    let captured = captured_request
        .lock()
        .expect("request lock should not be poisoned");
    serde_json::from_slice(
        captured
            .as_ref()
            .expect("transport should receive request")
            .body(),
    )
    .expect("request body should be valid JSON")
}

#[tokio::test]
async fn request_uses_openai_tool_shapes() {
    let mut assistant = Message::assistant("");
    assistant.tool_calls.push(ToolCall {
        id: "call_1".into(),
        name: "lookup".into(),
        arguments: json!({"query": "rust"}),
        signature: None,
    });
    let mut request = request_with(vec![assistant]);
    request.tools.push(ToolDefinition {
        name: "lookup".into(),
        description: "Look up a value".into(),
        input_schema: json!({"type": "object"}),
    });

    let body = serialized_request(request).await;

    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    assert_eq!(body["messages"][0]["tool_calls"][0]["type"], "function");
    assert_eq!(
        serde_json::from_str::<Value>(
            body["messages"][0]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .expect("tool arguments should be a JSON string")
        )
        .expect("tool arguments should contain valid JSON"),
        json!({"query": "rust"})
    );
}

#[tokio::test]
async fn request_flattens_content_parts_and_omits_reasoning() {
    let mut assistant = Message::assistant("answer");
    assistant
        .content
        .0
        .insert(0, ContentPart::reasoning("thinking"));

    let body = serialized_request(request_with(vec![
        Message::system("be brief"),
        Message::user("hi"),
        assistant,
        Message::tool_result("call_1", "ok"),
    ]))
    .await;

    assert_eq!(body["messages"][0]["content"], "be brief");
    assert_eq!(body["messages"][1]["content"], "hi");
    assert_eq!(body["messages"][2]["content"], "answer");
    assert_eq!(body["messages"][3]["content"], "ok");
    assert_eq!(body["messages"][3]["tool_call_id"], "call_1");
}

#[tokio::test]
async fn request_omits_content_for_tool_call_only_assistant_message() {
    let mut assistant = Message::assistant("");
    assistant.tool_calls.push(ToolCall {
        id: "call_1".into(),
        name: "lookup".into(),
        arguments: json!({}),
        signature: None,
    });

    let body = serialized_request(request_with(vec![assistant])).await;

    assert!(body["messages"][0].get("content").is_none());
    assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call_1");
}

#[tokio::test]
async fn request_keeps_empty_content_for_message_without_text() {
    let mut message = Message::user("");
    message.content.0.clear();

    let body = serialized_request(request_with(vec![message])).await;

    assert_eq!(body["messages"][0]["content"], "");
}

#[tokio::test]
async fn anthropic_models_add_cache_control_to_system_and_last_message() {
    let mut request = request_with(vec![
        Message::system("system prompt"),
        Message::user("first question"),
        Message::assistant("first answer"),
        Message::user("second question"),
    ]);
    request.model = "anthropic/claude-sonnet-4.6".into();

    let body = serialized_request(request).await;

    assert_eq!(
        body["messages"][0]["content"],
        json!([{
            "type": "text",
            "text": "system prompt",
            "cache_control": { "type": "ephemeral" }
        }])
    );
    assert_eq!(body["messages"][1]["content"], "first question");
    assert_eq!(body["messages"][2]["content"], "first answer");
    assert_eq!(
        body["messages"][3]["content"],
        json!([{
            "type": "text",
            "text": "second question",
            "cache_control": { "type": "ephemeral" }
        }])
    );
}

#[test]
fn prompt_cache_override_forces_or_disables_cache_control() {
    let mut request = request_with(vec![
        Message::system("system prompt"),
        Message::user("hello"),
    ]);
    request.model = "custom/my-model".into();

    let forced = uno_provider_openai::openai_request_body_with_cache(&request, Some(true))
        .expect("forced cache body should serialize");
    assert_eq!(
        forced["messages"][0]["content"][0]["cache_control"],
        json!({ "type": "ephemeral" })
    );

    request.model = "anthropic/claude-sonnet-4.6".into();
    let disabled = uno_provider_openai::openai_request_body_with_cache(&request, Some(false))
        .expect("disabled cache body should serialize");
    assert_eq!(disabled["messages"][0]["content"], "system prompt");
    assert_eq!(disabled["messages"][1]["content"], "hello");
}

#[test]
fn maps_thinking_level_for_chat_completions_and_responses() {
    use protocol::ThinkingLevel;

    let cases = [
        (ThinkingLevel::Off, "none"),
        (ThinkingLevel::Minimal, "minimal"),
        (ThinkingLevel::Low, "low"),
        (ThinkingLevel::Medium, "medium"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::XHigh, "xhigh"),
    ];

    for (level, expected_effort) in cases {
        let mut request = request_with(vec![Message::user("hello")]);
        request.thinking_level = Some(level);
        request.session_id = Some("sess-99".into());
        request.metadata.insert("trace_id".into(), "tr-123".into());

        let chat = uno_provider_openai::openai_request_body(&request).expect("chat body");
        assert_eq!(chat["reasoning_effort"], expected_effort);
        assert!(chat.get("thinking_level").is_none());
        assert!(chat.get("session_id").is_none());
        assert_eq!(chat["metadata"]["session_id"], "sess-99");
        assert_eq!(chat["metadata"]["trace_id"], "tr-123");

        let responses =
            uno_provider_openai::responses_request_body(&request).expect("responses body");
        assert_eq!(responses["reasoning"]["effort"], expected_effort);
        assert_eq!(responses["metadata"]["session_id"], "sess-99");
        assert_eq!(responses["metadata"]["trace_id"], "tr-123");
    }
}
