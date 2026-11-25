use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use protocol::{
    CompletionRequest, ContentPart, FinishReason, Message, StreamEvent, ToolDefinition,
};
use provider::{BoxStream, Provider, ProviderError};
use serde_json::{Value, json};
use tool_core::{
    Approval, ApprovalHandler, ApprovalRequest, Tool, ToolError, ToolOutput, ToolRegistry,
};
use uno_harness::{AgentError, AgentErrorCode, Harness, RunEvent, RunState};

struct ScriptedProvider {
    turns: Mutex<VecDeque<Vec<Result<StreamEvent, ProviderError>>>>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl ScriptedProvider {
    fn new(
        turns: Vec<Vec<Result<StreamEvent, ProviderError>>>,
    ) -> (Self, Arc<Mutex<Vec<CompletionRequest>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                turns: Mutex::new(turns.into()),
                requests: requests.clone(),
            },
            requests,
        )
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        self.requests
            .lock()
            .expect("request lock should not be poisoned")
            .push(request);
        let events = self
            .turns
            .lock()
            .expect("turn lock should not be poisoned")
            .pop_front()
            .ok_or_else(|| ProviderError::InvalidResponse("no scripted turn".into()))?;
        Ok(Box::pin(stream::iter(events)))
    }
}

struct RetryOnceProvider {
    attempts: AtomicUsize,
}

#[async_trait]
impl Provider for RetryOnceProvider {
    fn name(&self) -> &'static str {
        "retry-once"
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(ProviderError::HttpStatus {
                status: 503,
                body: "temporarily unavailable".into(),
            });
        }

        Ok(Box::pin(stream::iter(vec![
            Ok(StreamEvent::TextDelta {
                delta: "ready".into(),
            }),
            finish(FinishReason::Stop),
        ])))
    }
}

struct ApprovalReply(bool);

#[async_trait]
impl ApprovalHandler for ApprovalReply {
    async fn approve(&self, _request: ApprovalRequest) -> Result<bool, String> {
        Ok(self.0)
    }
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            description: "Returns the supplied text".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn approval(&self, _arguments: &Value) -> Approval {
        Approval::Never
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(
            arguments["text"].as_str().unwrap_or_default(),
        ))
    }
}

struct ConfirmedTool {
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for ConfirmedTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "confirmed".into(),
            description: "Requires confirmation".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn approval(&self, _arguments: &Value) -> Approval {
        Approval::Ask {
            reason: "test confirmation".into(),
        }
    }

    async fn execute(&self, _arguments: Value) -> Result<ToolOutput, ToolError> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("ran"))
    }
}

fn request() -> CompletionRequest {
    CompletionRequest {
        model: "test-model".into(),
        messages: vec![Message::user("run a tool")],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        thinking_level: None,
        ..CompletionRequest::default()
    }
}

fn tool_call(name: &str) -> StreamEvent {
    StreamEvent::ToolCallDelta {
        index: 0,
        id: Some("call_1".into()),
        name: Some(name.into()),
        arguments_delta: if name == "echo" {
            r#"{"text":"hello"}"#.into()
        } else {
            "{}".into()
        },
    }
}

fn tool_call_at(index: usize, id: &str, name: &str) -> StreamEvent {
    StreamEvent::ToolCallDelta {
        index,
        id: Some(id.into()),
        name: Some(name.into()),
        arguments_delta: "{}".into(),
    }
}

fn finish(reason: FinishReason) -> Result<StreamEvent, ProviderError> {
    Ok(StreamEvent::Finished {
        reason,
        usage: None,
    })
}

fn finish_with_usage(
    reason: FinishReason,
    usage: protocol::Usage,
) -> Result<StreamEvent, ProviderError> {
    Ok(StreamEvent::Finished {
        reason,
        usage: Some(usage),
    })
}

fn scripted_provider(
    turns: Vec<Vec<Result<StreamEvent, ProviderError>>>,
) -> (Arc<ScriptedProvider>, Arc<Mutex<Vec<CompletionRequest>>>) {
    let (provider, requests) = ScriptedProvider::new(turns);
    (Arc::new(provider), requests)
}

#[tokio::test]
async fn runs_provider_tool_provider_round_trip() {
    let (provider, requests) = scripted_provider(vec![
        vec![Ok(tool_call("echo")), finish(FinishReason::ToolCall)],
        vec![
            Ok(StreamEvent::TextDelta {
                delta: "done".into(),
            }),
            finish(FinishReason::Stop),
        ],
    ]);
    let mut tools = ToolRegistry::new();
    tools.register(EchoTool).expect("register echo");
    let harness =
        Harness::new(provider, tools, Arc::new(ApprovalReply(true)), 3).expect("valid turn limit");

    let events = harness.run(request()).collect::<Vec<_>>().await;

    assert!(matches!(
        events.last(),
        Some(Ok(RunEvent::Completed {
            messages,
            finish_reason: Some(FinishReason::Stop),
            usage: None,
        }))
            if messages.len() == 4 && messages.last().is_some_and(|message| {
                message.content.0 == vec![protocol::ContentPart::text("done")]
            })
    ));
    let appended = events
        .iter()
        .filter_map(|event| match event {
            Ok(RunEvent::MessageAppended(message)) => Some(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(appended.len(), 3);
    assert_eq!(appended[1], &Message::tool_result("call_1", "hello"));
    assert!(
        appended[0]
            .tool_calls
            .iter()
            .any(|call| call.id == "call_1")
    );
    assert_eq!(appended[2].content.0, vec![ContentPart::text("done")]);
    let states = events
        .iter()
        .filter_map(|event| match event {
            Ok(RunEvent::StateChanged(state)) => Some(*state),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        [
            RunState::WaitingForModel,
            RunState::StreamingResponse,
            RunState::HandlingToolCalls,
            RunState::WaitingForModel,
            RunState::StreamingResponse,
            RunState::Completed,
        ]
    );
    assert!(events.iter().any(|event| matches!(
        event,
        Ok(RunEvent::ToolCallFinished { id, name, output })
            if id == "call_1" && name == "echo" && output == &ToolOutput::text("hello")
    )));

    let requests = requests
        .lock()
        .expect("request lock should not be poisoned");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].tools.len(), 1);
    assert_eq!(requests[0].tools[0].name, "echo");
    assert_eq!(
        requests[1].messages[2],
        Message::tool_result("call_1", "hello")
    );
}

#[tokio::test]
async fn appended_history_is_available_when_a_later_provider_turn_fails() {
    let (provider, _) = scripted_provider(vec![vec![
        Ok(tool_call("echo")),
        finish(FinishReason::ToolCall),
    ]]);
    let mut tools = ToolRegistry::new();
    tools.register(EchoTool).expect("register echo");
    let harness =
        Harness::new(provider, tools, Arc::new(ApprovalReply(true)), 3).expect("valid turn limit");

    let events = harness.run(request()).collect::<Vec<_>>().await;
    let appended = events
        .iter()
        .filter_map(|event| match event {
            Ok(RunEvent::MessageAppended(message)) => Some(message),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(appended.len(), 2);
    assert_eq!(appended[1], &Message::tool_result("call_1", "hello"));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Err(AgentError::Provider(_))))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(RunEvent::Completed { .. })))
    );
}

#[tokio::test]
async fn duplicate_tool_call_ids_are_rejected_before_any_tool_executes() {
    let executions = Arc::new(AtomicUsize::new(0));
    let (provider, _) = scripted_provider(vec![vec![
        Ok(tool_call_at(0, "call_1", "confirmed")),
        Ok(tool_call_at(1, "call_1", "confirmed")),
        finish(FinishReason::ToolCall),
    ]]);
    let mut tools = ToolRegistry::new();
    tools
        .register(ConfirmedTool {
            executions: executions.clone(),
        })
        .expect("register confirmed tool");
    let harness =
        Harness::new(provider, tools, Arc::new(ApprovalReply(true)), 2).expect("valid turn limit");

    let events = harness.run(request()).collect::<Vec<_>>().await;

    assert_eq!(executions.load(Ordering::SeqCst), 0);
    assert!(matches!(
        events.last(),
        Some(Err(AgentError::DuplicateToolCallId { id })) if id == "call_1"
    ));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(RunEvent::MessageAppended(_))))
    );
}

#[tokio::test]
async fn retries_retryable_provider_start_failures_once() {
    let provider = Arc::new(RetryOnceProvider {
        attempts: AtomicUsize::new(0),
    });
    let harness = Harness::new(
        provider.clone(),
        ToolRegistry::new(),
        Arc::new(ApprovalReply(true)),
        1,
    )
    .expect("valid turn limit");

    let events = harness.run(request()).collect::<Vec<_>>().await;

    assert_eq!(provider.attempts.load(Ordering::SeqCst), 2);
    assert!(matches!(
        events.last(),
        Some(Ok(RunEvent::Completed { .. }))
    ));
}

#[tokio::test]
async fn does_not_retry_provider_errors_after_streamed_content() {
    let (provider, requests) = scripted_provider(vec![vec![
        Ok(StreamEvent::TextDelta {
            delta: "partial".into(),
        }),
        Err(ProviderError::HttpStatus {
            status: 503,
            body: "connection lost".into(),
        }),
    ]]);
    let harness = Harness::new(
        provider,
        ToolRegistry::new(),
        Arc::new(ApprovalReply(true)),
        1,
    )
    .expect("valid turn limit");

    let events = harness.run(request()).collect::<Vec<_>>().await;
    let error = events
        .iter()
        .find_map(|event| event.as_ref().err())
        .expect("stream error should terminate the run");

    assert_eq!(error.code(), AgentErrorCode::ProviderStream);
    assert!(!error.is_retryable());
    assert_eq!(
        requests
            .lock()
            .expect("request lock should not be poisoned")
            .len(),
        1
    );
}

#[tokio::test]
async fn run_owned_can_be_moved_into_a_background_task() {
    let (provider, _) = scripted_provider(vec![vec![
        Ok(StreamEvent::TextDelta {
            delta: "ready".into(),
        }),
        finish(FinishReason::Stop),
    ]]);
    let harness = Arc::new(
        Harness::new(
            provider,
            ToolRegistry::new(),
            Arc::new(ApprovalReply(true)),
            1,
        )
        .expect("valid turn limit"),
    );

    let events = tokio::spawn(harness.run_owned(request()).collect::<Vec<_>>())
        .await
        .expect("background task should complete");

    assert!(matches!(
        events.last(),
        Some(Ok(RunEvent::Completed { .. }))
    ));
}

#[tokio::test]
async fn completed_usage_is_summed_across_provider_turns() {
    let (provider, _) = scripted_provider(vec![
        vec![
            Ok(tool_call("echo")),
            finish_with_usage(
                FinishReason::ToolCall,
                protocol::Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                    cached_tokens: Some(3),
                },
            ),
        ],
        vec![
            Ok(StreamEvent::TextDelta {
                delta: "done".into(),
            }),
            finish_with_usage(
                FinishReason::Stop,
                protocol::Usage {
                    input_tokens: 5,
                    output_tokens: 4,
                    cached_tokens: None,
                },
            ),
        ],
    ]);
    let mut tools = ToolRegistry::new();
    tools.register(EchoTool).expect("register echo");
    let harness =
        Harness::new(provider, tools, Arc::new(ApprovalReply(true)), 3).expect("valid turn limit");

    let events = harness.run(request()).collect::<Vec<_>>().await;

    assert!(matches!(
        events.last(),
        Some(Ok(RunEvent::Completed {
            usage: Some(protocol::Usage {
                input_tokens: 15,
                output_tokens: 6,
                cached_tokens: Some(3),
            }),
            ..
        }))
    ));
}

#[tokio::test]
async fn denied_approval_becomes_a_model_visible_tool_error() {
    let executions = Arc::new(AtomicUsize::new(0));
    let (provider, requests) = scripted_provider(vec![
        vec![Ok(tool_call("confirmed")), finish(FinishReason::ToolCall)],
        vec![
            Ok(StreamEvent::TextDelta {
                delta: "stopped".into(),
            }),
            finish(FinishReason::Stop),
        ],
    ]);
    let mut tools = ToolRegistry::new();
    tools
        .register(ConfirmedTool {
            executions: executions.clone(),
        })
        .expect("register confirmed tool");
    let harness =
        Harness::new(provider, tools, Arc::new(ApprovalReply(false)), 3).expect("valid turn limit");

    let events = harness.run(request()).collect::<Vec<_>>().await;

    assert_eq!(executions.load(Ordering::SeqCst), 0);
    assert!(events.iter().any(|event| matches!(
        event,
        Ok(RunEvent::ToolCallFinished { output, .. }) if output.is_error
    )));
    let requests = requests
        .lock()
        .expect("request lock should not be poisoned");
    assert!(matches!(
        &requests[1].messages[2].content.0[..],
        [ContentPart::Text { text }] if text.contains("approval denied")
    ));
}

#[tokio::test]
async fn final_turn_tool_call_is_not_executed_without_a_follow_up_turn() {
    let executions = Arc::new(AtomicUsize::new(0));
    let (provider, _) = scripted_provider(vec![vec![
        Ok(StreamEvent::TextDelta {
            delta: "tool calls cannot run without a follow-up turn".into(),
        }),
        Ok(tool_call("confirmed")),
        finish(FinishReason::ToolCall),
    ]]);
    let mut tools = ToolRegistry::new();
    tools
        .register(ConfirmedTool {
            executions: executions.clone(),
        })
        .expect("register confirmed tool");
    let harness =
        Harness::new(provider, tools, Arc::new(ApprovalReply(true)), 1).expect("valid turn limit");

    let events = harness.run(request()).collect::<Vec<_>>().await;

    assert_eq!(executions.load(Ordering::SeqCst), 0);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Ok(RunEvent::TurnLimitReached { max_turns: 1 })))
    );
    assert!(matches!(
        events.last(),
        Some(Err(AgentError::MaxTurnsExceeded { max_turns: 1 }))
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        Ok(RunEvent::MessageAppended(message))
            if message.tool_calls.is_empty()
                && message.content.0 == vec![ContentPart::text(
                    "tool calls cannot run without a follow-up turn"
                )]
    )));
    let error = events
        .iter()
        .find_map(|event| event.as_ref().err())
        .expect("run should report a fatal error");
    assert_eq!(error.code().as_str(), "max_turns_exceeded");
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn provider_stream_must_finish() {
    let (provider, _) = scripted_provider(vec![vec![Ok(StreamEvent::TextDelta {
        delta: "partial".into(),
    })]]);
    let harness = Harness::new(
        provider,
        ToolRegistry::new(),
        Arc::new(ApprovalReply(true)),
        1,
    )
    .expect("valid turn limit");

    let events = harness.run(request()).collect::<Vec<_>>().await;

    assert!(matches!(
        events.last(),
        Some(Err(AgentError::IncompleteProviderStream {
            retryable: false
        }))
    ));
    let error = events
        .iter()
        .find_map(|event| event.as_ref().err())
        .expect("run should report a fatal error");
    assert_eq!(error.code(), AgentErrorCode::IncompleteProviderStream);
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn empty_incomplete_provider_stream_is_retryable() {
    let (provider, _) = scripted_provider(vec![vec![]]);
    let harness = Harness::new(
        provider,
        ToolRegistry::new(),
        Arc::new(ApprovalReply(true)),
        1,
    )
    .expect("valid turn limit");

    let events = harness.run(request()).collect::<Vec<_>>().await;
    let error = events
        .iter()
        .find_map(|event| event.as_ref().err())
        .expect("incomplete stream should report an error");

    assert_eq!(error.code(), AgentErrorCode::IncompleteProviderStream);
    assert!(error.is_retryable());
}

#[tokio::test]
async fn provider_stream_error_preserves_retryability() {
    let (provider, _) = scripted_provider(vec![vec![Ok(StreamEvent::Error {
        message: "provider unavailable".into(),
        retryable: true,
    })]]);
    let harness = Harness::new(
        provider,
        ToolRegistry::new(),
        Arc::new(ApprovalReply(true)),
        1,
    )
    .expect("valid turn limit");

    let events = harness.run(request()).collect::<Vec<_>>().await;
    let error = events
        .iter()
        .find_map(|event| event.as_ref().err())
        .expect("run should report a provider stream error");

    assert_eq!(error.code(), AgentErrorCode::ProviderStream);
    assert!(error.is_retryable());
}

#[tokio::test]
async fn zero_turn_limit_is_rejected() {
    let (provider, _) = scripted_provider(Vec::new());

    assert!(matches!(
        Harness::new(
            provider,
            ToolRegistry::new(),
            Arc::new(ApprovalReply(true)),
            0
        ),
        Err(AgentError::InvalidMaxProviderTurns)
    ));
}
