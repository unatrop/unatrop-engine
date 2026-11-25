use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::json;

use protocol::{CompletionRequest, FinishReason, Message, StreamEvent, ToolDefinition};
use provider::{BoxStream, Provider, ProviderError};
use tool_core::{
    Approval, ApprovalHandler, ApprovalRequest, InMemorySpillStore, Tool, ToolError, ToolOutput,
    ToolRegistry,
};
use uno_harness::{
    AgentErrorCode, AgentInbox, Harness, RalphConfig, RalphOutcome, RalphRunner, RunEvent,
    SessionCheckpoint,
};

struct ScriptedProvider {
    responses: Mutex<VecDeque<Vec<StreamEvent>>>,
    seen_requests: Mutex<Vec<CompletionRequest>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            seen_requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.seen_requests.lock().unwrap().clone()
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
        self.seen_requests.lock().unwrap().push(request);
        let events = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                vec![StreamEvent::Finished {
                    reason: FinishReason::Stop,
                    usage: None,
                }]
            });
        Ok(Box::pin(futures_util::stream::iter(
            events.into_iter().map(Ok),
        )))
    }
}

struct AllowAll;

#[async_trait]
impl ApprovalHandler for AllowAll {
    async fn approve(&self, _request: ApprovalRequest) -> Result<bool, String> {
        Ok(true)
    }
}

struct EchoBigTool {
    calls: AtomicUsize,
}

#[async_trait]
impl Tool for EchoBigTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo_big".into(),
            description: "returns big or small text".into(),
            input_schema: json!({ "type": "object", "properties": { "len": { "type": "integer" } } }),
        }
    }

    fn approval(&self, _arguments: &serde_json::Value) -> Approval {
        Approval::Never
    }

    async fn execute(&self, arguments: serde_json::Value) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let len = arguments
            .get("len")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(10) as usize;
        Ok(ToolOutput::text("x".repeat(len)))
    }
}

struct FailingCheckpoint;

#[async_trait]
impl SessionCheckpoint for FailingCheckpoint {
    async fn flush(&self) -> Result<(), String> {
        Err("disk full".into())
    }
}

fn tool_delta(index: usize, id: &str, name: &str, args: &str) -> StreamEvent {
    StreamEvent::ToolCallDelta {
        index,
        id: Some(id.into()),
        name: Some(name.into()),
        arguments_delta: args.into(),
    }
}

#[tokio::test]
async fn harness_spills_large_output_and_drains_inbox_and_nudges_repeats() {
    let spill = Arc::new(InMemorySpillStore::new());
    let inbox = Arc::new(AgentInbox::new());
    inbox.push_next_step(Message::user("steer mid-run"));

    let mut registry = ToolRegistry::new();
    registry
        .register(EchoBigTool {
            calls: AtomicUsize::new(0),
        })
        .unwrap();

    // 3 одинаковых вызова подряд -> repeat detector должен добавить напоминание,
    // а первый большой вывод (> 8192) -> должен уйти в spill!
    let turn1 = vec![
        tool_delta(0, "c1", "echo_big", r#"{"len":12000}"#),
        tool_delta(1, "c2", "echo_big", r#"{"len":12000}"#),
        tool_delta(2, "c3", "echo_big", r#"{"len":12000}"#),
        StreamEvent::Finished {
            reason: FinishReason::ToolCall,
            usage: None,
        },
    ];
    let turn2 = vec![
        StreamEvent::TextDelta {
            delta: "done".into(),
        },
        StreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ];

    let provider = Arc::new(ScriptedProvider::new(vec![turn1, turn2]));
    let harness = Harness::new(provider.clone(), registry, Arc::new(AllowAll), 5)
        .unwrap()
        .with_inbox(Arc::clone(&inbox))
        .with_spill_store(spill, "sess-test");

    let req = CompletionRequest {
        model: "m".into(),
        messages: vec![Message::user("start")],
        tools: vec![],
        max_tokens: None,
        temperature: None,
        thinking_level: None,
        ..CompletionRequest::default()
    };

    let mut stream = harness.run(req);
    let mut completed_msgs = Vec::new();
    while let Some(ev) = stream.next().await {
        if let RunEvent::Completed { messages, .. } = ev.unwrap() {
            completed_msgs = messages;
        }
    }

    let texts: Vec<String> = completed_msgs
        .iter()
        .map(|m| {
            m.content
                .0
                .iter()
                .filter_map(|p| match p {
                    protocol::ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>()
        })
        .collect();

    assert!(texts.iter().any(|t| t.contains("saved to: mem://")));
    assert!(texts.iter().any(|t| t.contains("identical arguments")));
    assert!(texts.iter().any(|t| t == "steer mid-run"));
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn checkpoint_failure_is_fail_closed_before_provider_call() {
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let harness = Harness::new(provider.clone(), ToolRegistry::new(), Arc::new(AllowAll), 3)
        .unwrap()
        .with_checkpoint(Arc::new(FailingCheckpoint));

    let req = CompletionRequest {
        model: "m".into(),
        messages: vec![Message::user("hi")],
        tools: vec![],
        max_tokens: None,
        temperature: None,
        thinking_level: None,
        ..CompletionRequest::default()
    };

    let mut stream = harness.run(req);
    let mut got_err = None;
    while let Some(ev) = stream.next().await {
        if let Err(err) = ev {
            got_err = Some(err);
            break;
        }
    }

    let err = got_err.expect("expected checkpoint error");
    assert_eq!(err.code(), AgentErrorCode::CheckpointFailed);
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn ralph_runner_runs_two_fresh_rounds_to_completion() {
    let round1 = vec![
        StreamEvent::TextDelta {
            delta: r#"{"status":"continue","summary":"round 1","evidence":["a"],"nextSteps":["b"],"blocker":""}"#.into(),
        },
        StreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ];
    let round2 = vec![
        StreamEvent::TextDelta {
            delta: r#"```json
{"status":"complete","summary":"all done","evidence":["verified"],"nextSteps":[],"blocker":""}
```"#
                .into(),
        },
        StreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ];

    let provider = Arc::new(ScriptedProvider::new(vec![round1, round2]));
    let runner =
        RalphRunner::new(provider.clone(), Arc::new(AllowAll), RalphConfig::new("m")).unwrap();

    let outcome = runner.run("finish task", ToolRegistry::new).await.unwrap();

    match outcome {
        RalphOutcome::Complete { rounds, report } => {
            assert_eq!(rounds, 2);
            assert_eq!(report.summary, "all done");
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
    assert_eq!(provider.requests().len(), 2);
}

struct SlowReadTool;

#[async_trait]
impl Tool for SlowReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "slow_read".into(),
            description: "read-only tool with small delay".into(),
            input_schema: json!({ "type": "object", "properties": { "id": { "type": "string" } } }),
        }
    }

    fn approval(&self, _arguments: &serde_json::Value) -> Approval {
        Approval::Never
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, arguments: serde_json::Value) -> Result<ToolOutput, ToolError> {
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        let id = arguments.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        Ok(ToolOutput::text(format!("ok:{id}")))
    }
}

#[tokio::test]
async fn read_only_tool_batch_runs_concurrently_and_preserves_order() {
    let mut registry = ToolRegistry::new();
    registry.register(SlowReadTool).unwrap();

    let turn1 = vec![
        tool_delta(0, "r1", "slow_read", r#"{"id":"1"}"#),
        tool_delta(1, "r2", "slow_read", r#"{"id":"2"}"#),
        tool_delta(2, "r3", "slow_read", r#"{"id":"3"}"#),
        StreamEvent::Finished {
            reason: FinishReason::ToolCall,
            usage: None,
        },
    ];
    let turn2 = vec![
        StreamEvent::TextDelta {
            delta: "done".into(),
        },
        StreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ];

    let provider = Arc::new(ScriptedProvider::new(vec![turn1, turn2]));
    let harness = Harness::new(provider, registry, Arc::new(AllowAll), 4).unwrap();
    let req = CompletionRequest {
        model: "m".into(),
        messages: vec![Message::user("go")],
        tools: vec![],
        max_tokens: None,
        temperature: None,
        thinking_level: None,
        ..CompletionRequest::default()
    };

    let started = std::time::Instant::now();
    let mut stream = harness.run(req);
    let mut finished_ids = Vec::new();
    while let Some(ev) = stream.next().await {
        if let Ok(RunEvent::ToolCallFinished { id, .. }) = ev {
            finished_ids.push(id);
        }
    }
    let elapsed = started.elapsed();

    assert_eq!(finished_ids, vec!["r1", "r2", "r3"]);
    // 3 * 60ms последовательно >= 180ms; параллельно укладывается заметно быстрее 150ms
    assert!(
        elapsed < std::time::Duration::from_millis(150),
        "elapsed: {elapsed:?}"
    );
}

#[tokio::test]
async fn compaction_persists_into_completed_messages_and_triggers_on_input_tokens() {
    let mut registry = ToolRegistry::new();
    registry
        .register(EchoBigTool {
            calls: AtomicUsize::new(0),
        })
        .unwrap();

    let turn1 = vec![
        tool_delta(0, "c1", "echo_big", r#"{"len":2500}"#),
        StreamEvent::Finished {
            reason: FinishReason::ToolCall,
            // Высокий input_tokens с первого шага должен активировать compact_batch перед вторым шагом
            usage: Some(protocol::Usage {
                input_tokens: 20_000,
                output_tokens: 100,
                cached_tokens: None,
            }),
        },
    ];
    let turn2 = vec![
        StreamEvent::TextDelta {
            delta: "compacted-ok".into(),
        },
        StreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: Some(protocol::Usage {
                input_tokens: 1_000,
                output_tokens: 50,
                cached_tokens: None,
            }),
        },
    ];

    let provider = Arc::new(ScriptedProvider::new(vec![turn1, turn2]));
    let harness = Harness::new(provider.clone(), registry, Arc::new(AllowAll), 4)
        .unwrap()
        .with_context_cap(8_000);

    let big_old_output = "Z".repeat(3_000);
    let mut initial = vec![
        Message::system("sys"),
        Message::user("initial request"),
        Message::tool_result("old_0", big_old_output),
    ];
    for i in 1..=5 {
        initial.push(Message::tool_result(format!("old_{i}"), "ok"));
    }

    let req = CompletionRequest {
        model: "m".into(),
        messages: initial,
        ..CompletionRequest::default()
    };

    let mut stream = harness.run(req);
    let mut final_messages = Vec::new();
    while let Some(ev) = stream.next().await {
        if let RunEvent::Completed { messages, .. } = ev.unwrap() {
            final_messages = messages;
        }
    }

    // Старый длинный ответ `old_0` должен остаться сжатым и в итоговом `Completed.messages`
    let has_compacted_marker = final_messages.iter().any(|m| {
        m.content.0.iter().any(|p| match p {
            protocol::ContentPart::Text { text } => text.contains("earlier tool output compacted"),
            _ => false,
        })
    });
    assert!(
        has_compacted_marker,
        "expected Completed.messages to preserve compacted tool output"
    );
}

#[tokio::test]
async fn llm_compaction_summarizes_evicted_steps_before_next_turn() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            StreamEvent::TextDelta {
                delta: "KEY FACTS: user wants a relay service".into(),
            },
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            },
        ],
        vec![
            StreamEvent::TextDelta {
                delta: "done".into(),
            },
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            },
        ],
    ]));

    let harness = Harness::new(provider.clone(), ToolRegistry::new(), Arc::new(AllowAll), 4)
        .unwrap()
        .with_context_tokens(1_024)
        .with_llm_compaction(true);

    let mut messages = vec![Message::system("sys"), Message::user("build relay")];
    for i in 0..8 {
        messages.push(Message::tool_result(format!("old_{i}"), "x".repeat(4_000)));
    }
    let req = CompletionRequest {
        model: "m".into(),
        messages,
        ..CompletionRequest::default()
    };

    let mut final_messages = Vec::new();
    let mut stream = harness.run(req);
    while let Some(ev) = stream.next().await {
        if let RunEvent::Completed { messages, .. } = ev.unwrap() {
            final_messages = messages;
        }
    }

    let has_summary = final_messages.iter().any(|m| {
        m.content.0.iter().any(|p| match p {
            protocol::ContentPart::Text { text } => {
                text.contains("Summary of compacted earlier steps") && text.contains("KEY FACTS")
            }
            _ => false,
        })
    });
    assert!(has_summary, "expected LLM summary of evicted messages");

    let requests = provider.requests();
    assert!(
        requests
            .first()
            .is_some_and(|r| r.messages[0].content.0.iter().any(|p| matches!(
                p,
                protocol::ContentPart::Text { text } if text.contains("compact an AI agent's conversation")
            ))),
        "first provider call should be the compaction summarizer"
    );
}

#[tokio::test]
async fn default_max_output_tokens_is_applied_to_request() {
    let provider = Arc::new(ScriptedProvider::new(vec![vec![
        StreamEvent::TextDelta { delta: "ok".into() },
        StreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ]]));

    let harness = Harness::new(provider.clone(), ToolRegistry::new(), Arc::new(AllowAll), 4)
        .unwrap()
        .with_max_output_tokens(777);
    let req = CompletionRequest {
        model: "m".into(),
        messages: vec![Message::user("hi")],
        ..CompletionRequest::default()
    };

    let mut stream = harness.run(req);
    while stream.next().await.is_some() {}

    let requests = provider.requests();
    assert_eq!(requests[0].max_tokens, Some(777));
}

struct StreamingMockTool;

#[async_trait::async_trait]
impl Tool for StreamingMockTool {
    fn definition(&self) -> protocol::ToolDefinition {
        protocol::ToolDefinition {
            name: "stream_exec".into(),
            description: "streams chunks".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn approval(&self, _: &serde_json::Value) -> Approval {
        Approval::Never
    }

    async fn execute(&self, _: serde_json::Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("part1\npart2\n"))
    }

    async fn execute_streaming(
        &self,
        _: serde_json::Value,
        delta_tx: Option<tool_core::OutputDeltaSender>,
    ) -> Result<ToolOutput, ToolError> {
        if let Some(tx) = delta_tx {
            let _ = tx.send("part1\n".into());
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let _ = tx.send("part2\n".into());
        }
        Ok(ToolOutput::text("part1\npart2\n"))
    }
}

#[tokio::test]
async fn streaming_tool_emits_tool_output_delta_before_finished() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_stream".into()),
                name: Some("stream_exec".into()),
                arguments_delta: "{}".into(),
            },
            StreamEvent::Finished {
                reason: FinishReason::ToolCall,
                usage: None,
            },
        ],
        vec![
            StreamEvent::TextDelta { delta: "ok".into() },
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            },
        ],
    ]));

    let mut tools = ToolRegistry::new();
    tools.register(StreamingMockTool).unwrap();
    let harness = Harness::new(provider, tools, Arc::new(AllowAll), 4).unwrap();
    let req = CompletionRequest {
        model: "m".into(),
        messages: vec![Message::user("run")],
        ..CompletionRequest::default()
    };

    let mut deltas = Vec::new();
    let mut saw_finished_after_deltas = false;
    let mut stream = harness.run(req);
    while let Some(ev) = stream.next().await {
        match ev.unwrap() {
            RunEvent::ToolOutputDelta { id, name, chunk } => {
                assert_eq!(id, "call_stream");
                assert_eq!(name, "stream_exec");
                deltas.push(chunk);
            }
            RunEvent::ToolCallFinished { id, .. } if id == "call_stream" => {
                saw_finished_after_deltas = deltas == vec!["part1\n", "part2\n"];
            }
            _ => {}
        }
    }
    assert!(saw_finished_after_deltas, "deltas={deltas:?}");
}

#[tokio::test]
async fn update_plan_validates_and_streams_plan_updates() {
    use uno_harness::plan::{PlanStepStatus, UpdatePlanTool};

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tool = UpdatePlanTool::with_sender(tx);

    let invalid = tool
        .execute(serde_json::json!({
            "plan": [
                {"step": "a", "status": "in_progress"},
                {"step": "b", "status": "in_progress"}
            ]
        }))
        .await
        .unwrap();
    assert!(invalid.is_error);

    let valid = tool
        .execute(serde_json::json!({
            "explanation": "starting work",
            "plan": [
                {"step": "read code", "status": "completed"},
                {"step": "implement", "status": "in_progress"}
            ]
        }))
        .await
        .unwrap();
    assert!(!valid.is_error);

    let emitted = rx.try_recv().expect("plan update");
    assert_eq!(emitted.explanation.as_deref(), Some("starting work"));
    assert_eq!(emitted.steps.len(), 2);
    assert_eq!(emitted.steps[0].status, PlanStepStatus::Completed);
    assert_eq!(emitted.steps[1].status, PlanStepStatus::InProgress);
}

#[tokio::test]
async fn spawn_agent_runs_read_only_research_subagent() {
    use uno_harness::subagent::SpawnAgentTool;

    let provider = Arc::new(ScriptedProvider::new(vec![vec![
        StreamEvent::TextDelta {
            delta: "Found 3 relevant files.".into(),
        },
        StreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ]]));

    let registry = ToolRegistry::new();
    let tool = SpawnAgentTool::new(provider.clone(), "m", &registry, Arc::new(AllowAll));
    assert!(tool.read_only());

    let out = tool
        .execute(serde_json::json!({
            "task": "Find files",
            "max_turns": 4
        }))
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("Found 3 relevant files."));
}
