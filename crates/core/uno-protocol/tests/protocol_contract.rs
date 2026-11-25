use uno_protocol::{
    AccumulatorError, ContentPart, FinishReason, Message, MessageAccumulator, Role, StreamEvent,
    Usage,
};

fn finish_event() -> StreamEvent {
    StreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }
}

#[test]
fn deserialize_plain_string_content() {
    let json = r#"{"role":"assistant","content":"Plain response text"}"#;
    let message: Message = serde_json::from_str(json).expect("should parse string content");
    assert_eq!(
        message.content.0,
        vec![ContentPart::text("Plain response text")]
    );
}

#[test]
fn deserialize_array_content() {
    let json = r#"{"role":"assistant","content":[{"type":"text","text":"Array response"}]}"#;
    let message: Message = serde_json::from_str(json).expect("should parse array content");
    assert_eq!(message.content.0, vec![ContentPart::text("Array response")]);
}

#[test]
fn deserialize_null_content() {
    let json = r#"{"role":"assistant","content":null}"#;
    let message: Message = serde_json::from_str(json).expect("should parse null content");
    assert!(message.content.is_empty());
}

#[test]
fn accumulator_assembles_stream() {
    let mut accumulator = MessageAccumulator::new();

    accumulator
        .process_event(&StreamEvent::ReasoningDelta {
            delta: "Step 1: Check git status. ".into(),
        })
        .expect("reasoning fits limits");
    accumulator
        .process_event(&StreamEvent::TextDelta {
            delta: "I will check ".into(),
        })
        .expect("text fits limits");
    accumulator
        .process_event(&StreamEvent::TextDelta {
            delta: "the repository.".into(),
        })
        .expect("text fits limits");
    accumulator
        .process_event(&StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("call_123".into()),
            name: Some("git_status".into()),
            arguments_delta: "{}".into(),
        })
        .expect("tool call fits limits");
    accumulator
        .process_event(&StreamEvent::Finished {
            reason: FinishReason::ToolCall,
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_tokens: Some(2),
            }),
        })
        .expect("finished event should be recorded");

    let accumulated = accumulator.finish().expect("should build message");
    let message = accumulated.message;
    assert_eq!(message.role, Role::Assistant);
    assert_eq!(message.tool_calls.len(), 1);
    assert_eq!(message.tool_calls[0].id, "call_123");
    assert_eq!(message.tool_calls[0].name, "git_status");
    assert_eq!(message.content.0.len(), 2);
    assert_eq!(accumulated.finish_reason, Some(FinishReason::ToolCall));
    assert_eq!(
        accumulated.usage,
        Some(Usage {
            input_tokens: 10,
            output_tokens: 5,
            cached_tokens: Some(2),
        })
    );
}

#[test]
fn accumulator_rejects_out_of_range_tool_call_index() {
    let mut accumulator = MessageAccumulator::new();

    assert!(matches!(
        accumulator.process_event(&StreamEvent::ToolCallDelta {
            index: usize::MAX,
            id: None,
            name: None,
            arguments_delta: String::new(),
        }),
        Err(AccumulatorError::TooManyToolCalls { .. })
    ));
}

#[test]
fn accumulator_rejects_oversized_text_delta() {
    let mut accumulator = MessageAccumulator::new();

    assert!(matches!(
        accumulator.process_event(&StreamEvent::TextDelta {
            delta: "x".repeat(8 * 1024 * 1024 + 1),
        }),
        Err(AccumulatorError::TextTooLarge { .. })
    ));
}

#[test]
fn accumulator_does_not_create_calls_for_sparse_indices() {
    let mut accumulator = MessageAccumulator::new();
    accumulator
        .process_event(&StreamEvent::ToolCallDelta {
            index: 5,
            id: Some("call_5".into()),
            name: Some("lookup".into()),
            arguments_delta: "{}".into(),
        })
        .expect("sparse index should be stored without placeholder calls");
    accumulator
        .process_event(&finish_event())
        .expect("stream should finish");

    let accumulated = accumulator.finish().expect("call metadata is valid");
    assert_eq!(accumulated.message.tool_calls.len(), 1);
    assert_eq!(accumulated.message.tool_calls[0].id, "call_5");
}

#[test]
fn accumulator_deduplicates_repeated_tool_metadata() {
    let mut accumulator = MessageAccumulator::new();
    for arguments_delta in ["{", "}"] {
        accumulator
            .process_event(&StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("lookup".into()),
                arguments_delta: arguments_delta.into(),
            })
            .expect("tool deltas should fit");
    }
    accumulator
        .process_event(&finish_event())
        .expect("stream should finish");

    let accumulated = accumulator.finish().expect("arguments should parse");
    let call = &accumulated.message.tool_calls[0];
    assert_eq!(call.id, "call_1");
    assert_eq!(call.name, "lookup");
    assert_eq!(call.arguments, serde_json::json!({}));
}

#[test]
fn accumulator_limits_tool_metadata_size() {
    let mut accumulator = MessageAccumulator::new();
    assert!(matches!(
        accumulator.process_event(&StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("i".repeat(257)),
            name: Some("lookup".into()),
            arguments_delta: String::new(),
        }),
        Err(AccumulatorError::ToolMetadataTooLarge {
            index: 0,
            field: "id",
            ..
        })
    ));
}

#[test]
fn accumulator_reports_stream_errors() {
    let mut accumulator = MessageAccumulator::new();
    let error = accumulator.process_event(&StreamEvent::Error {
        message: "provider failed".into(),
        retryable: true,
    });

    assert!(matches!(
        error,
        Err(AccumulatorError::Stream {
            message,
            retryable: true
        }) if message == "provider failed"
    ));
    assert!(matches!(
        accumulator.finish(),
        Err(AccumulatorError::Stream {
            message,
            retryable: true
        }) if message == "provider failed"
    ));
}

#[test]
fn accumulator_rejects_streams_without_a_finish_event() {
    let accumulator = MessageAccumulator::new();
    assert!(matches!(
        accumulator.finish(),
        Err(AccumulatorError::MissingFinish)
    ));
}

#[test]
fn accumulator_rejects_non_object_tool_arguments() {
    let mut accumulator = MessageAccumulator::new();
    accumulator
        .process_event(&StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            name: Some("lookup".into()),
            arguments_delta: "[]".into(),
        })
        .expect("delta should fit");
    accumulator
        .process_event(&finish_event())
        .expect("stream should finish");

    assert!(matches!(
        accumulator.finish(),
        Err(AccumulatorError::ToolArgumentsNotObject { index: 0, name })
            if name == "lookup"
    ));
}

#[test]
fn accumulator_includes_tool_context_in_argument_parse_errors() {
    let mut accumulator = MessageAccumulator::new();
    accumulator
        .process_event(&StreamEvent::ToolCallDelta {
            index: 3,
            id: Some("call_3".into()),
            name: Some("lookup".into()),
            arguments_delta: "{".into(),
        })
        .expect("delta should fit");
    accumulator
        .process_event(&finish_event())
        .expect("stream should finish");

    assert!(matches!(
        accumulator.finish(),
        Err(AccumulatorError::InvalidToolArguments {
            index: 3,
            name,
            ..
        }) if name == "lookup"
    ));
}

#[test]
fn accumulator_enforces_total_buffer_limit() {
    let mut accumulator = MessageAccumulator::new();
    accumulator
        .process_event(&StreamEvent::TextDelta {
            delta: "x".repeat(8 * 1024 * 1024),
        })
        .expect("text is within its individual limit");
    accumulator
        .process_event(&StreamEvent::ReasoningDelta {
            delta: "x".repeat(8 * 1024 * 1024),
        })
        .expect("reasoning is within its individual limit");

    assert!(matches!(
        accumulator.process_event(&StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            name: Some("lookup".into()),
            arguments_delta: "{}".into(),
        }),
        Err(AccumulatorError::TotalBufferTooLarge { .. })
    ));
}

#[test]
fn accumulator_preserves_thought_signature_on_tool_call() {
    let mut accumulator = MessageAccumulator::new();
    accumulator
        .process_event(&StreamEvent::ThoughtSignature {
            index: 0,
            signature: "sig-123".into(),
        })
        .expect("signature fits limits");
    accumulator
        .process_event(&StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            name: Some("read_file".into()),
            arguments_delta: "{\"path\":\"a.txt\"}".into(),
        })
        .expect("tool call fits limits");
    accumulator
        .process_event(&finish_event())
        .expect("finished");

    let accumulated = accumulator.finish().expect("accumulated");
    assert_eq!(accumulated.message.tool_calls.len(), 1);
    assert_eq!(
        accumulated.message.tool_calls[0].signature.as_deref(),
        Some("sig-123")
    );
    assert_eq!(accumulated.message.tool_calls[0].id, "call_1");
}
