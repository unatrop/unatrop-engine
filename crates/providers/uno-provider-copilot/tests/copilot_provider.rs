use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use http::{Request, Response, StatusCode};
use protocol::{CompletionRequest, FinishReason, Message, StreamEvent};
use provider::Provider;
use transport::{ByteStream, HttpTransport, TransportError};
use uno_provider_copilot::{CopilotAuth, CopilotProvider, DeviceFlowPoll};

struct MockTransport {
    requests: Mutex<Vec<Request<Bytes>>>,
    responses: Mutex<Vec<Response<Bytes>>>,
}

impl MockTransport {
    fn new(responses: Vec<Response<Bytes>>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl HttpTransport for MockTransport {
    async fn send(&self, request: Request<Bytes>) -> Result<Response<ByteStream>, TransportError> {
        self.requests.lock().expect("lock requests").push(request);
        let response = self.responses.lock().expect("lock responses").remove(0);
        let (parts, body) = response.into_parts();
        let stream: ByteStream = Box::pin(stream::iter(vec![Ok(body)]));
        Ok(Response::from_parts(parts, stream))
    }
}

fn chat_request() -> CompletionRequest {
    CompletionRequest {
        model: "gpt-4o".into(),
        messages: vec![Message::user("hello")],
        tools: Vec::new(),
        temperature: None,
        max_tokens: None,
        thinking_level: None,
        ..CompletionRequest::default()
    }
}

#[tokio::test]
async fn copilot_provider_sends_required_headers_with_direct_token() {
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let response = Response::builder()
        .status(StatusCode::OK)
        .body(Bytes::from_static(sse_body.as_bytes()))
        .expect("build response");
    let transport = Arc::new(MockTransport::new(vec![response]));

    let auth = CopilotAuth::from_api_token(
        "copilot-secret",
        "https://copilot.example.test",
        transport.clone(),
    );
    let provider = CopilotProvider::new(auth, transport.clone())
        .editor_version("unatrop-test/1.0")
        .integration_id("test-cli");

    let mut events = provider
        .stream(chat_request())
        .await
        .expect("stream should succeed");
    let mut text = String::new();
    let mut finished = false;
    while let Some(ev) = events.next().await {
        match ev.expect("event ok") {
            StreamEvent::TextDelta { delta } => text.push_str(&delta),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                ..
            } => finished = true,
            _ => {}
        }
    }

    assert_eq!(text, "hi");
    assert!(finished);

    let captured = transport.requests.lock().expect("lock requests");
    assert_eq!(captured.len(), 1);
    let req = &captured[0];
    assert_eq!(
        req.uri().to_string(),
        "https://copilot.example.test/chat/completions"
    );
    assert_eq!(req.headers()["authorization"], "Bearer copilot-secret");
    assert_eq!(req.headers()["user-agent"], "GitHubCopilotChat/0.32.0");
    assert_eq!(req.headers()["editor-version"], "unatrop-test/1.0");
    assert_eq!(
        req.headers()["editor-plugin-version"],
        "copilot-chat/0.32.0"
    );
    assert_eq!(req.headers()["copilot-integration-id"], "test-cli");
    assert!(!req.headers().contains_key("x-github-api-version"));
}

#[tokio::test]
async fn copilot_auth_exchanges_and_caches_token() {
    let token_json = r#"{"token":"exchanged-tok","expires_at":4102444800,"endpoints":{"api":"https://api.cop.test"}}"#;
    let token_resp = Response::builder()
        .status(StatusCode::OK)
        .body(Bytes::from_static(token_json.as_bytes()))
        .expect("build token response");
    let transport = Arc::new(MockTransport::new(vec![token_resp]));

    let auth = CopilotAuth::from_github_token("ghu_test_123", transport.clone());
    let first = auth.access().await.expect("first exchange ok");
    let second = auth.access().await.expect("cached access ok");

    assert_eq!(first.token, "exchanged-tok");
    assert_eq!(first.api_url, "https://api.cop.test");
    assert_eq!(second.token, "exchanged-tok");
    assert_eq!(transport.requests.lock().expect("lock").len(), 1);
}

#[tokio::test]
async fn copilot_provider_sets_agent_initiator_after_tool_message() {
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let response = Response::builder()
        .status(StatusCode::OK)
        .body(Bytes::from_static(sse_body.as_bytes()))
        .expect("build response");
    let transport = Arc::new(MockTransport::new(vec![response]));

    let auth = CopilotAuth::from_api_token(
        "copilot-secret",
        "https://copilot.example.test",
        transport.clone(),
    );
    let provider = CopilotProvider::new(auth, transport.clone());

    let request = CompletionRequest {
        model: "gpt-4o".into(),
        messages: vec![
            Message::user("run ls"),
            Message::tool_result("call_1", "ok"),
        ],
        tools: Vec::new(),
        temperature: None,
        max_tokens: None,
        thinking_level: None,
        ..CompletionRequest::default()
    };

    let mut events = provider.stream(request).await.expect("stream ok");
    while let Some(ev) = events.next().await {
        ev.expect("event ok");
    }

    let captured = transport.requests.lock().expect("lock requests");
    assert_eq!(captured[0].headers()["x-initiator"], "agent");
}

#[tokio::test]
async fn copilot_provider_lists_models() {
    let models_json = r#"{"data":[
        {"id":"gpt-4o","name":"GPT-4o","vendor":"openai","model_picker_enabled":true},
        {"id":"claude-3.7-sonnet","vendor":"anthropic","policy":{"state":"enabled"},"capabilities":{"type":"chat"},"supported_endpoints":["/v1/messages","/chat/completions"]},
        {"id":"claude-fable-5.1","policy":{"state":"disabled"},"capabilities":{"type":"chat"},"supported_endpoints":["/chat/completions"]},
        {"id":"text-embedding-3-small","capabilities":{"type":"embeddings"}},
        {"id":"gpt-5.6-luna","policy":{"state":"enabled"},"capabilities":{"type":"chat"},"supported_endpoints":["/responses","ws:/responses"]}
    ]}"#;
    let response = Response::builder()
        .status(StatusCode::OK)
        .body(Bytes::from_static(models_json.as_bytes()))
        .expect("build response");
    let transport = Arc::new(MockTransport::new(vec![response]));

    let auth = CopilotAuth::from_api_token(
        "copilot-secret",
        "https://copilot.example.test",
        transport.clone(),
    );
    let provider = CopilotProvider::new(auth, transport.clone());

    let models = provider.list_models().await.expect("list_models ok");
    assert_eq!(models.len(), 3);
    assert_eq!(models[0].id, "gpt-4o");
    assert!(models[0].model_picker_enabled);
    assert_eq!(models[1].id, "claude-3.7-sonnet");
    assert_eq!(models[1].vendor.as_deref(), Some("anthropic"));
    assert_eq!(models[2].id, "gpt-5.6-luna");

    let captured = transport.requests.lock().expect("lock requests");
    assert_eq!(
        captured[0].uri().to_string(),
        "https://copilot.example.test/models"
    );
    assert_eq!(
        captured[0].headers()["authorization"],
        "Bearer copilot-secret"
    );
}

#[tokio::test]
async fn copilot_auth_device_flow_start_and_poll() {
    let start_json = r#"{"device_code":"dev_123","user_code":"ABCD-EFGH","verification_uri":"https://github.com/login/device","expires_in":900,"interval":5}"#;
    let pending_json = r#"{"error":"authorization_pending"}"#;
    let ok_json = r#"{"access_token":"ghu_fresh_token"}"#;

    let transport = Arc::new(MockTransport::new(vec![
        Response::builder()
            .status(StatusCode::OK)
            .body(Bytes::from_static(start_json.as_bytes()))
            .expect("start resp"),
        Response::builder()
            .status(StatusCode::OK)
            .body(Bytes::from_static(pending_json.as_bytes()))
            .expect("pending resp"),
        Response::builder()
            .status(StatusCode::OK)
            .body(Bytes::from_static(ok_json.as_bytes()))
            .expect("ok resp"),
    ]));

    let session = CopilotAuth::start_device_flow_with_config(
        transport.as_ref(),
        "https://github.example.test",
        "client_test",
    )
    .await
    .expect("device flow start ok");

    assert_eq!(session.user_code, "ABCD-EFGH");
    assert_eq!(
        session.poll_once(transport.as_ref()).await.expect("poll 1"),
        DeviceFlowPoll::Pending
    );
    assert_eq!(
        session.poll_once(transport.as_ref()).await.expect("poll 2"),
        DeviceFlowPoll::Authorized {
            github_token: "ghu_fresh_token".to_string()
        }
    );
}

#[tokio::test]
async fn copilot_provider_streams_responses_endpoint_for_responses_only_models() {
    let models_json = r#"{"data":[{"id":"gpt-5.6-luna","policy":{"state":"enabled"},"capabilities":{"type":"chat"},"supported_endpoints":["/responses","ws:/responses"]}]}"#;
    let models_resp = Response::builder()
        .status(StatusCode::OK)
        .body(Bytes::from_static(models_json.as_bytes()))
        .expect("build models response");

    let sse = concat!(
        "event: response.reasoning_summary_text.delta\n",
        "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"think \"}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read_file\"}}\n\n",
        "event: response.function_call_arguments.delta\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"input_tokens_details\":{\"cached_tokens\":2}}}}\n\n",
    );
    let stream_resp = Response::builder()
        .status(StatusCode::OK)
        .body(Bytes::from_static(sse.as_bytes()))
        .expect("build stream response");

    let transport = Arc::new(MockTransport::new(vec![models_resp, stream_resp]));
    let auth = CopilotAuth::from_api_token(
        "copilot-secret",
        "https://copilot.example.test",
        transport.clone(),
    );
    let provider = CopilotProvider::new(auth, transport.clone());

    let listed = provider.list_models().await.expect("list_models ok");
    assert_eq!(listed.len(), 1);

    let mut req = chat_request();
    req.model = "gpt-5.6-luna".into();
    let mut stream = provider.stream(req).await.expect("responses stream ok");
    let mut events = Vec::new();
    while let Some(item) = stream.next().await {
        events.push(item.expect("stream event ok"));
    }

    assert!(matches!(
        &events[0],
        StreamEvent::ReasoningDelta { delta } if delta == "think "
    ));
    assert!(matches!(
        &events[1],
        StreamEvent::TextDelta { delta } if delta == "hello"
    ));
    assert!(matches!(
        &events[2],
        StreamEvent::ToolCallDelta { index: 0, id: Some(id), name: Some(name), arguments_delta }
        if id == "call_1" && name == "read_file" && arguments_delta.is_empty()
    ));
    assert!(matches!(
        &events[3],
        StreamEvent::ToolCallDelta { index: 0, id: None, name: None, arguments_delta }
        if arguments_delta == "{\"path\":\"a.txt\"}"
    ));
    assert!(matches!(
        &events[4],
        StreamEvent::Finished {
            reason: protocol::FinishReason::ToolCall,
            usage: Some(protocol::Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_tokens: Some(2),
            }),
        }
    ));

    let captured = transport.requests.lock().expect("lock requests");
    assert_eq!(
        captured[1].uri().to_string(),
        "https://copilot.example.test/responses"
    );
    let body: serde_json::Value =
        serde_json::from_slice(captured[1].body()).expect("parse responses body");
    assert_eq!(body["model"], "gpt-5.6-luna");
    assert_eq!(body["store"], false);
    assert!(body["input"].is_array());
}

#[tokio::test]
async fn copilot_auth_fetches_account_login_and_plan() {
    let account_json = r#"{"login":"octocat","copilot_plan":"individual","access_type_sku":"free_limited_copilot"}"#;
    let response = Response::builder()
        .status(StatusCode::OK)
        .body(Bytes::from_static(account_json.as_bytes()))
        .expect("build account response");
    let transport = Arc::new(MockTransport::new(vec![response]));

    let auth = CopilotAuth::from_github_token("ghu_test_123", transport.clone())
        .with_github_api_base("https://github.example.test");
    let account = auth
        .fetch_account()
        .await
        .expect("account ok")
        .expect("account present");

    assert_eq!(account.login.as_deref(), Some("octocat"));
    assert_eq!(account.plan.as_deref(), Some("Copilot Free"));
    let captured = transport.requests.lock().expect("lock requests");
    assert_eq!(
        captured[0].uri().to_string(),
        "https://github.example.test/copilot_internal/user"
    );
    assert_eq!(captured[0].headers()["authorization"], "token ghu_test_123");
}
