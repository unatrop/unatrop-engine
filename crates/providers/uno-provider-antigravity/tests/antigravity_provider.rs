use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use http::{Request, Response, StatusCode};
use protocol::{
    CompletionRequest, Content, ContentPart, FinishReason, Message, StreamEvent, ThinkingLevel,
    ToolCall, ToolDefinition,
};
use provider::Provider;
use serde_json::{Value, json};
use transport::{ByteStream, HttpTransport, TransportError};
use uno_provider_antigravity::{AntigravityAuth, AntigravityProvider};

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

    fn requests(&self) -> std::sync::MutexGuard<'_, Vec<Request<Bytes>>> {
        self.requests.lock().expect("lock requests")
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

fn sse_response(body: &'static str) -> Response<Bytes> {
    Response::builder()
        .status(StatusCode::OK)
        .body(Bytes::from_static(body.as_bytes()))
        .expect("build response")
}

fn body_json(request: &Request<Bytes>) -> Value {
    serde_json::from_slice(request.body()).expect("request body json")
}

fn chat_request(model: &str) -> CompletionRequest {
    CompletionRequest {
        model: model.into(),
        messages: vec![Message::user("hello")],
        temperature: Some(0.4),
        max_tokens: Some(512),
        ..CompletionRequest::default()
    }
}

#[tokio::test]
async fn sends_request_and_parses_text_reasoning_usage() {
    let sse = concat!(
        "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"thi\",\"thought\":true},{\"text\":\"hi\"}]}}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":3,\"thoughtsTokenCount\":2,\"cachedContentTokenCount\":4}}}\n\n",
        "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" there\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":5,\"thoughtsTokenCount\":2,\"cachedContentTokenCount\":4}}}\n\n"
    );
    let transport = Arc::new(MockTransport::new(vec![sse_response(sse)]));
    let auth = AntigravityAuth::from_access_token("access-1", transport.clone())
        .with_api_url("https://cloud.example.test/")
        .with_project("proj-1");
    let provider =
        AntigravityProvider::new(auth, transport.clone()).with_user_agent("antigravity/cli/test");

    let mut events = provider
        .stream(chat_request("gemini-4-hydrogen-medium"))
        .await
        .expect("stream ok");

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut finished = None;
    while let Some(event) = events.next().await {
        match event.expect("event ok") {
            StreamEvent::TextDelta { delta } => text.push_str(&delta),
            StreamEvent::ReasoningDelta { delta } => reasoning.push_str(&delta),
            StreamEvent::Finished { reason, usage } => finished = Some((reason, usage)),
            other => panic!("unexpected event {other:?}"),
        }
    }

    assert_eq!(reasoning, "thi");
    assert_eq!(text, "hi there");
    let (reason, usage) = finished.expect("finished");
    assert_eq!(reason, FinishReason::Stop);
    let usage = usage.expect("usage");
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.output_tokens, 7);
    assert_eq!(usage.cached_tokens, Some(4));

    let captured = transport.requests();
    assert_eq!(captured.len(), 1);
    let request = &captured[0];
    assert_eq!(
        request.uri().to_string(),
        "https://cloud.example.test/v1internal:streamGenerateContent?alt=sse"
    );
    assert_eq!(request.headers()["authorization"], "Bearer access-1");
    assert_eq!(request.headers()["content-type"], "application/json");
    assert_eq!(request.headers()["accept"], "text/event-stream");
    assert_eq!(request.headers()["user-agent"], "antigravity/cli/test");

    let body = body_json(request);
    assert_eq!(body["project"], "proj-1");
    assert_eq!(body["model"], "gemini-4-hydrogen-medium");
    assert_eq!(body["userAgent"], "antigravity");
    assert_eq!(body["requestType"], "agent");
    assert!(body["requestId"].as_str().unwrap().starts_with("agent/"));
    assert_eq!(body["request"]["contents"][0]["role"], "user");
    assert_eq!(body["request"]["contents"][0]["parts"][0]["text"], "hello");
    let temperature = body["request"]["generationConfig"]["temperature"]
        .as_f64()
        .expect("temperature");
    assert!((temperature - 0.4).abs() < 1e-6);
    assert_eq!(
        body["request"]["generationConfig"]["maxOutputTokens"],
        json!(512)
    );
    assert_eq!(
        body["request"]["generationConfig"]["thinkingConfig"]["includeThoughts"],
        json!(true)
    );
    assert_eq!(
        body["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "MEDIUM"
    );
}

#[tokio::test]
async fn maps_system_tools_tool_calls_and_tool_results() {
    let sse = "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call_abc\",\"name\":\"read_file\",\"args\":{\"path\":\"a.txt\"}}}]},\"finishReason\":\"STOP\"}]}}\n\n";
    let transport = Arc::new(MockTransport::new(vec![sse_response(sse)]));
    let auth = AntigravityAuth::from_access_token("access-1", transport.clone());
    let provider = AntigravityProvider::new(auth, transport.clone());

    let mut request = chat_request("gemini-4-hydrogen-low");
    request.messages = vec![
        Message::system("be nice"),
        Message {
            role: protocol::Role::Developer,
            content: Content(vec![ContentPart::text("developer note")]),
            tool_calls: Vec::new(),
            tool_call_id: None,
        },
        Message::user("read a.txt"),
        Message {
            role: protocol::Role::Assistant,
            content: Content::default(),
            tool_calls: vec![ToolCall {
                id: "call_abc".into(),
                name: "read_file".into(),
                arguments: json!({"path": "a.txt"}),
                signature: None,
            }],
            tool_call_id: None,
        },
        Message::tool_result("call_abc", "file contents"),
    ];
    request.tools = vec![ToolDefinition {
        name: "read_file".into(),
        description: "Read a file".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "integer"}
            },
            "required": ["path"],
            "additionalProperties": false
        }),
    }];

    let mut events = provider.stream(request).await.expect("stream ok");
    let mut tool_call = None;
    while let Some(event) = events.next().await {
        match event.expect("event ok") {
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                tool_call = Some((
                    index,
                    id,
                    name,
                    serde_json::from_str::<Value>(&arguments_delta).expect("args json"),
                ));
            }
            StreamEvent::Finished { reason, .. } => {
                assert_eq!(reason, FinishReason::ToolCall);
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
    let (index, id, name, args) = tool_call.expect("tool call");
    assert_eq!(index, 0);
    assert_eq!(id.as_deref(), Some("call_abc"));
    assert_eq!(name.as_deref(), Some("read_file"));
    assert_eq!(args, json!({"path": "a.txt"}));

    let captured = transport.requests();
    let body = body_json(&captured[0]);
    let request_body = &body["request"];
    assert_eq!(
        request_body["systemInstruction"]["parts"][0]["text"],
        "be nice\n\ndeveloper note"
    );

    let declarations = request_body["tools"][0]["functionDeclarations"]
        .as_array()
        .unwrap();
    assert_eq!(declarations[0]["name"], "read_file");
    assert_eq!(declarations[0]["parameters"]["type"], "OBJECT");
    assert_eq!(
        declarations[0]["parameters"]["properties"]["path"]["type"],
        "STRING"
    );
    assert!(
        declarations[0]["parameters"]
            .get("additionalProperties")
            .is_none()
    );
    assert_eq!(declarations[0]["parameters"]["required"], json!(["path"]));

    let contents = request_body["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 3);
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[1]["role"], "model");
    assert_eq!(contents[1]["parts"][0]["functionCall"]["id"], "call_abc");
    assert_eq!(contents[2]["role"], "model");
    assert_eq!(
        contents[2]["parts"][0]["functionResponse"]["name"],
        "read_file"
    );
    assert_eq!(
        contents[2]["parts"][0]["functionResponse"]["response"]["output"],
        "file contents"
    );
    assert_eq!(
        request_body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "LOW"
    );
}

#[tokio::test]
async fn refreshes_token_and_caches_it() {
    let token_json = r#"{"access_token":"fresh-token","expires_in":3600,"token_type":"Bearer"}"#;
    let sse = "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}}\n\n";
    let transport = Arc::new(MockTransport::new(vec![
        Response::builder()
            .status(StatusCode::OK)
            .body(Bytes::from_static(token_json.as_bytes()))
            .unwrap(),
        sse_response(sse),
        sse_response(sse),
    ]));

    let auth = AntigravityAuth::from_refresh_token_with_config(
        "refresh-1",
        "client-1",
        "secret-1",
        transport.clone(),
    )
    .with_token_url("https://oauth.example.test/token")
    .with_api_url("https://cloud.example.test");
    let provider = AntigravityProvider::new(auth, transport.clone());

    for _ in 0..2 {
        let mut events = provider
            .stream(chat_request("gemini-4-hydrogen-medium"))
            .await
            .expect("stream ok");
        while let Some(event) = events.next().await {
            event.expect("event ok");
        }
    }

    let captured = transport.requests();
    assert_eq!(captured.len(), 3);
    assert_eq!(
        captured[0].uri().to_string(),
        "https://oauth.example.test/token"
    );
    assert_eq!(
        captured[0].headers()["content-type"],
        "application/x-www-form-urlencoded"
    );
    let form = std::str::from_utf8(captured[0].body()).unwrap();
    assert!(form.contains("grant_type=refresh_token"));
    assert!(form.contains("refresh_token=refresh-1"));
    assert!(form.contains("client_id=client-1"));
    assert!(form.contains("client_secret=secret-1"));
    assert_eq!(captured[1].headers()["authorization"], "Bearer fresh-token");
    assert_eq!(captured[2].headers()["authorization"], "Bearer fresh-token");
}

#[tokio::test]
async fn lists_models_from_map_and_skips_non_declarative_fields() {
    let models_json = json!({
        "models": {
            "gemini-4-hydrogen-medium": {
                "displayName": "Gemini 4 Hydrogen (Medium)",
                "supportsThinking": true,
                "thinkingLevel": 2,
                "maxOutputTokens": 65536,
                "recommended": true,
                "model": "MODEL_PLACEHOLDER_M16",
                "apiProvider": "google"
            },
            "claude-sonnet-4-6": {
                "displayName": "Claude Sonnet 4.6",
                "apiProvider": "anthropic"
            }
        }
    });
    let transport = Arc::new(MockTransport::new(vec![
        Response::builder()
            .status(StatusCode::OK)
            .body(Bytes::from(models_json.to_string()))
            .unwrap(),
    ]));
    let auth = AntigravityAuth::from_access_token("access-1", transport.clone())
        .with_api_url("https://cloud.example.test");
    let provider = AntigravityProvider::new(auth, transport.clone());

    let mut models = provider.list_models().await.expect("models ok");
    models.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "claude-sonnet-4-6");
    assert_eq!(models[1].id, "gemini-4-hydrogen-medium");
    assert_eq!(
        models[1].display_name.as_deref(),
        Some("Gemini 4 Hydrogen (Medium)")
    );
    assert_eq!(models[1].max_output_tokens, Some(65536));
    assert_eq!(models[1].thinking_level, Some(2));
    assert_eq!(models[1].recommended, Some(true));

    let captured = transport.requests();
    assert_eq!(
        captured[0].uri().to_string(),
        "https://cloud.example.test/v1internal:fetchAvailableModels"
    );
    let body = body_json(&captured[0]);
    assert_eq!(body["project"], "aicode-consumers");
}

#[tokio::test]
async fn loads_stored_keyring_token_shape() {
    let stored = json!({
        "token": {
            "access_token": "stored-access",
            "token_type": "Bearer",
            "refresh_token": "stored-refresh",
            "expiry": "2026-01-01T00:00:00Z"
        },
        "auth_method": "consumer"
    });
    let transport = Arc::new(MockTransport::new(Vec::new()));
    let auth = AntigravityAuth::from_stored_token(&stored.to_string(), transport).expect("auth");
    assert_eq!(auth.project(), "aicode-consumers");
    assert_eq!(auth.api_url(), "https://daily-cloudcode-pa.googleapis.com");
}

#[tokio::test]
async fn maps_thinking_level_override_and_off() {
    let sse = "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}}\n\n";
    let transport = Arc::new(MockTransport::new(vec![
        sse_response(sse),
        sse_response(sse),
    ]));
    let auth = AntigravityAuth::from_access_token("access-1", transport.clone());
    let provider = AntigravityProvider::new(auth, transport.clone());

    let mut high = chat_request("gemini-4-hydrogen-low");
    high.thinking_level = Some(ThinkingLevel::High);
    let mut events = provider.stream(high).await.expect("stream ok");
    while let Some(event) = events.next().await {
        event.expect("event ok");
    }

    let mut off = chat_request("gemini-4-hydrogen-medium");
    off.thinking_level = Some(ThinkingLevel::Off);
    let mut events = provider.stream(off).await.expect("stream ok");
    while let Some(event) = events.next().await {
        event.expect("event ok");
    }

    let captured = transport.requests();
    let high_body = body_json(&captured[0]);
    assert_eq!(
        high_body["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "HIGH"
    );
    assert_eq!(
        high_body["request"]["generationConfig"]["thinkingConfig"]["includeThoughts"],
        json!(true)
    );

    let off_body = body_json(&captured[1]);
    let thinking = &off_body["request"]["generationConfig"]["thinkingConfig"];
    assert_eq!(thinking["includeThoughts"], json!(false));
    assert_eq!(thinking["thinkingBudget"], json!(0));
    assert!(thinking.get("thinkingLevel").is_none());
}

#[tokio::test]
async fn normalizes_schemars_schemas_for_gemini() {
    let sse = "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}}\n\n";
    let transport = Arc::new(MockTransport::new(vec![sse_response(sse)]));
    let auth = AntigravityAuth::from_access_token("access-1", transport.clone());
    let provider = AntigravityProvider::new(auth, transport.clone());

    let mut request = chat_request("gemini-4-hydrogen-high");
    request.tools = vec![ToolDefinition {
        name: "exec_command".into(),
        description: "Run a command".into(),
        input_schema: json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "ExecCommandArgs",
            "type": "object",
            "properties": {
                "path": {"type": ["string", "null"], "description": "workspace path"},
                "limit": {"type": ["integer", "null"], "format": "uint", "minimum": 0},
                "sandbox": {
                    "$ref": "#/$defs/SandboxPermissions",
                    "default": "use_default",
                    "description": "sandbox mode"
                },
                "value": {"type": ["string", "integer"]}
            },
            "required": ["path"],
            "additionalProperties": false,
            "$defs": {
                "SandboxPermissions": {
                    "type": "string",
                    "oneOf": [
                        {"type": "string", "const": "use_default"},
                        {"type": "string", "const": "require_escalated"}
                    ]
                }
            }
        }),
    }];

    let mut events = provider.stream(request).await.expect("stream ok");
    while let Some(event) = events.next().await {
        event.expect("event ok");
    }

    let captured = transport.requests();
    let body = body_json(&captured[0]);
    let parameters = &body["request"]["tools"][0]["functionDeclarations"][0]["parameters"];

    assert_eq!(parameters["type"], "OBJECT");
    assert!(parameters.get("$schema").is_none());
    assert!(parameters.get("$defs").is_none());
    assert!(parameters.get("additionalProperties").is_none());

    assert_eq!(parameters["properties"]["path"]["type"], "STRING");
    assert_eq!(parameters["properties"]["path"]["nullable"], json!(true));

    assert_eq!(parameters["properties"]["limit"]["type"], "INTEGER");
    assert_eq!(parameters["properties"]["limit"]["nullable"], json!(true));
    assert_eq!(parameters["properties"]["limit"]["format"], "uint");

    let sandbox = &parameters["properties"]["sandbox"];
    assert!(sandbox.get("$ref").is_none());
    assert_eq!(sandbox["type"], "STRING");
    assert_eq!(sandbox["enum"], json!(["use_default", "require_escalated"]));
    assert_eq!(sandbox["default"], "use_default");
    assert_eq!(sandbox["description"], "sandbox mode");

    assert_eq!(
        parameters["properties"]["value"]["anyOf"],
        json!([{"type": "INTEGER"}, {"type": "STRING"}])
    );
    assert_eq!(parameters["required"], json!(["path"]));

    let text = serde_json::to_string(parameters).expect("schema json");
    for forbidden in ["$ref", "const", "\"oneOf\"", "[\"integer\"", "[\"string\""] {
        assert!(
            !text.contains(forbidden),
            "normalized schema still contains {forbidden}: {text}"
        );
    }
}

#[tokio::test]
async fn caches_list_models_within_ttl_and_maps_session_and_labels() {
    let models_json = json!({
        "models": {
            "gemini-4-hydrogen-medium": {
                "displayName": "Gemini 4 Hydrogen (Medium)",
                "supportsThinking": true,
                "maxTokens": 1048576,
                "maxOutputTokens": 65536,
                "recommended": true
            }
        }
    });
    let transport = Arc::new(MockTransport::new(vec![
        Response::builder()
            .status(StatusCode::OK)
            .body(Bytes::from(models_json.to_string()))
            .unwrap(),
    ]));
    let auth = AntigravityAuth::from_access_token("access-1", transport.clone());
    let provider = AntigravityProvider::new(auth, transport.clone());

    let first = provider.list_models().await.expect("first list_models");
    let second = provider.list_models().await.expect("cached list_models");
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(transport.requests().len(), 1);

    let catalog = provider.model_catalog().await.expect("catalog from cache");
    let info = catalog.get("gemini-4-hydrogen-medium").expect("model info");
    assert!(info.supports_thinking);
    assert!(info.recommended);
    assert_eq!(info.max_output_tokens, Some(65536));

    let mut req = chat_request("gemini-4-hydrogen-medium");
    req.session_id = Some("sess-custom-42".into());
    req.metadata.insert("env".into(), "test".into());
    let body = uno_provider_antigravity::antigravity_request_body(
        "proj-1",
        "gemini-4-hydrogen-medium",
        &req,
    )
    .expect("request body");
    assert_eq!(body["request"]["sessionId"], "sess-custom-42");
    assert_eq!(body["request"]["labels"]["env"], "test");

    let table = [
        (ThinkingLevel::Off, None, false),
        (ThinkingLevel::Minimal, Some("MINIMAL"), true),
        (ThinkingLevel::Low, Some("LOW"), true),
        (ThinkingLevel::Medium, Some("MEDIUM"), true),
        (ThinkingLevel::High, Some("HIGH"), true),
        (ThinkingLevel::XHigh, Some("XHIGH"), true),
    ];
    for (level, expected_level, include_thoughts) in table {
        let mut r = chat_request("gemini-4-hydrogen");
        r.thinking_level = Some(level);
        let b =
            uno_provider_antigravity::antigravity_request_body("proj-1", "gemini-4-hydrogen", &r)
                .unwrap();
        let tc = &b["request"]["generationConfig"]["thinkingConfig"];
        assert_eq!(tc["includeThoughts"], json!(include_thoughts));
        assert_eq!(
            tc.get("thinkingLevel").and_then(Value::as_str),
            expected_level
        );
    }
}

#[tokio::test]
async fn skips_disabled_internal_and_exhausted_models() {
    let models_json = json!({
        "models": {
            "gemini-healthy": {
                "displayName": "Healthy",
                "quotaInfo": { "remainingFraction": 0.5, "resetTime": "2026-11-20T00:00:00Z" }
            },
            "gemini-disabled": {
                "displayName": "Disabled",
                "disabled": true
            },
            "chat_12345": {
                "displayName": "Internal",
                "isInternal": true,
                "quotaInfo": { "remainingFraction": 1 }
            },
            "claude-exhausted": {
                "displayName": "Exhausted",
                "quotaInfo": { "resetTime": "2026-11-19T01:24:44Z" }
            },
            "gemini-legacy-disabled": {
                "displayName": "Legacy disabled"
            }
        }
    });
    let transport = Arc::new(MockTransport::new(vec![
        Response::builder()
            .status(StatusCode::OK)
            .body(Bytes::from(models_json.to_string()))
            .unwrap(),
    ]));
    let auth = AntigravityAuth::from_access_token("access-1", transport.clone())
        .with_api_url("https://cloud.example.test");
    let provider = AntigravityProvider::new(auth, transport);

    let models = provider.list_models().await.expect("models ok");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "gemini-healthy");
}

#[tokio::test]
async fn fetches_account_email_and_paid_tier() {
    let userinfo = json!({
        "id": "123",
        "email": "user@example.com",
        "verified_email": true
    });
    let load_code_assist = json!({
        "currentTier": { "id": "free-tier", "name": "Antigravity" },
        "paidTier": { "id": "g1-pro-tier", "name": "Google AI Pro" },
        "cloudaicompanionProject": "aicode-consumers"
    });
    let transport = Arc::new(MockTransport::new(vec![
        Response::builder()
            .status(StatusCode::OK)
            .body(Bytes::from(userinfo.to_string()))
            .unwrap(),
        Response::builder()
            .status(StatusCode::OK)
            .body(Bytes::from(load_code_assist.to_string()))
            .unwrap(),
    ]));
    let auth = AntigravityAuth::from_access_token("access-1", transport.clone())
        .with_api_url("https://cloud.example.test");
    let provider = AntigravityProvider::new(auth, transport.clone());

    let account = provider.fetch_account().await.expect("account ok");
    assert_eq!(account.email.as_deref(), Some("user@example.com"));
    assert_eq!(account.plan.as_deref(), Some("Google AI Pro"));
    assert_eq!(account.tier_id.as_deref(), Some("g1-pro-tier"));

    let captured = transport.requests();
    assert_eq!(
        captured[0].uri().to_string(),
        "https://www.googleapis.com/oauth2/v2/userinfo"
    );
    assert_eq!(captured[0].headers()["authorization"], "Bearer access-1");
    assert_eq!(
        captured[1].uri().to_string(),
        "https://cloud.example.test/v1internal:loadCodeAssist"
    );
    assert_eq!(captured[1].headers()["authorization"], "Bearer access-1");
    assert_eq!(
        body_json(&captured[1])["metadata"]["ideType"],
        "ANTIGRAVITY"
    );
}

#[tokio::test]
async fn preserves_and_replays_thought_signature_for_tool_calls() {
    let sse = "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"thoughtSignature\":\"sig-123\",\"functionCall\":{\"id\":\"call_abc\",\"name\":\"read_file\",\"args\":{\"path\":\"a.txt\"}}}]},\"finishReason\":\"STOP\"}]}}\n\n";
    let transport = Arc::new(MockTransport::new(vec![sse_response(sse)]));
    let auth = AntigravityAuth::from_access_token("access-1", transport.clone());
    let provider = AntigravityProvider::new(auth, transport);

    let mut events = provider
        .stream(chat_request("gemini-4-hydrogen-low"))
        .await
        .expect("stream ok");
    let mut signature = None;
    while let Some(event) = events.next().await {
        match event.expect("event ok") {
            StreamEvent::ThoughtSignature {
                index,
                signature: value,
            } => {
                assert_eq!(index, 0);
                signature = Some(value);
            }
            StreamEvent::ToolCallDelta { index, .. } => assert_eq!(index, 0),
            StreamEvent::Finished { .. } => {}
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert_eq!(signature.as_deref(), Some("sig-123"));

    let request = CompletionRequest {
        model: "gemini-4-hydrogen-low".into(),
        messages: vec![
            Message::user("read a.txt"),
            Message {
                role: protocol::Role::Assistant,
                content: Content::default(),
                tool_calls: vec![ToolCall {
                    id: "call_abc".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "a.txt"}),
                    signature: Some("sig-123".into()),
                }],
                tool_call_id: None,
            },
            Message::tool_result("call_abc", "file contents"),
        ],
        ..CompletionRequest::default()
    };
    let body = uno_provider_antigravity::antigravity_request_body(
        "aicode-consumers",
        "gemini-4-hydrogen-low",
        &request,
    )
    .expect("request body");
    assert_eq!(
        body["request"]["contents"][1]["parts"][0]["thoughtSignature"],
        "sig-123"
    );
    assert_eq!(
        body["request"]["contents"][1]["parts"][0]["functionCall"]["name"],
        "read_file"
    );

    let mut claude_req = chat_request("claude-sonnet-4-6-thinking");
    claude_req.thinking_level = Some(ThinkingLevel::High);
    let claude_body = uno_provider_antigravity::antigravity_request_body(
        "aicode-consumers",
        "claude-sonnet-4-6-thinking",
        &claude_req,
    )
    .expect("claude request body");
    assert!(
        claude_body["request"]["generationConfig"]
            .get("temperature")
            .is_none()
    );
    assert_eq!(
        claude_body["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "HIGH"
    );

    // Если в истории вызова инструмента нет сохранённой thoughtSignature, на первый functionCall
    // подставляется base64("skip_thought_signature_validator"), как в agy, чтобы избежать HTTP 400.
    let un_signed_req = CompletionRequest {
        model: "gemini-4-argon-low".into(),
        messages: vec![
            Message::user("run command"),
            Message {
                role: protocol::Role::Assistant,
                content: Content::default(),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "exec_command".into(),
                    arguments: json!({"cmd": "ps aux"}),
                    signature: None,
                }],
                tool_call_id: None,
            },
            Message::tool_result("call_1", "ok"),
        ],
        ..CompletionRequest::default()
    };
    let unsigned_body = uno_provider_antigravity::antigravity_request_body(
        "aicode-consumers",
        "gemini-4-argon-low",
        &un_signed_req,
    )
    .expect("unsigned request body");
    assert_eq!(
        unsigned_body["request"]["contents"][1]["parts"][0]["thoughtSignature"],
        "c2tpcF90aG91Z2h0X3NpZ25hdHVyZV92YWxpZGF0b3I="
    );
}
