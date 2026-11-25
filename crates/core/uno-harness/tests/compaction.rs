use protocol::{Message, Role, ToolCall};
use tool_core::ToolOutput;
use uno_harness::compaction::{
    DEFAULT_CONTEXT_CAP_TOKENS, MAX_TOOL_OUTPUT_HISTORY_CHARS, RECENT_FULL_MESSAGES, compact_batch,
    estimate_messages_tokens, estimate_text_tokens, tool_result_content, trim_tool_call_arguments,
};

#[test]
fn compacts_old_write_file_args_and_enforces_budget() {
    let big_code = "fn main() {}\n".repeat(1_000);
    let mut messages = vec![Message::system("sys"), Message::user("build relay")];
    for i in 0..10 {
        messages.push(Message {
            role: Role::Assistant,
            content: Default::default(),
            tool_calls: vec![ToolCall {
                id: format!("w_{i}"),
                name: "write_file".into(),
                arguments: serde_json::json!({ "path": "src/lib.rs", "content": big_code }),
                signature: None,
            }],
            tool_call_id: None,
        });
        messages.push(Message::tool_result(format!("w_{i}"), "ok"));
    }
    let original_len = messages.len();

    let dropped = compact_batch(&mut messages, DEFAULT_CONTEXT_CAP_TOKENS);

    assert!(!dropped.is_empty());
    assert!(messages.len() < original_len);
    assert!(estimate_messages_tokens(&messages) as u64 <= DEFAULT_CONTEXT_CAP_TOKENS);
    assert_eq!(messages[0], Message::system("sys"));
    assert_eq!(messages[1], Message::user("build relay"));
}

#[test]
fn under_budget_history_is_left_untouched() {
    let long_out = "A".repeat(3_000);
    let mut messages = vec![
        Message::user("start"),
        Message::tool_result("c1", long_out.clone()),
    ];
    for i in 0..RECENT_FULL_MESSAGES {
        messages.push(Message::tool_result(
            format!("recent_{i}"),
            long_out.clone(),
        ));
    }
    let before = messages.clone();

    let dropped = compact_batch(&mut messages, DEFAULT_CONTEXT_CAP_TOKENS);

    assert!(dropped.is_empty());
    assert_eq!(messages, before);
}

#[test]
fn drains_old_blocks_at_user_boundary_keeping_recent() {
    let mut messages = vec![Message::system("sys"), Message::user("start")];
    for i in 0..20 {
        messages.push(Message::user(format!("turn {i}")));
        messages.push(Message::assistant("ok"));
        messages.push(Message {
            role: Role::Assistant,
            content: Default::default(),
            tool_calls: vec![ToolCall {
                id: format!("c_{i}"),
                name: "read_file".into(),
                arguments: serde_json::json!({ "path": "a.txt" }),
                signature: None,
            }],
            tool_call_id: None,
        });
        messages.push(Message::tool_result(format!("c_{i}"), "X".repeat(8_000)));
    }
    let before_len = messages.len();

    let dropped = compact_batch(&mut messages, DEFAULT_CONTEXT_CAP_TOKENS);

    assert!(!dropped.is_empty());
    assert!(messages.len() < before_len);
    assert!(estimate_messages_tokens(&messages) as u64 <= DEFAULT_CONTEXT_CAP_TOKENS);
    assert_eq!(messages[0], Message::system("sys"));
    assert_eq!(messages[1], Message::user("start"));
    assert!(matches!(
        &messages[2],
        Message {
            role: Role::System,
            ..
        }
    ));
    assert_eq!(messages.last().unwrap().role, Role::Tool);
}

#[test]
fn trims_large_tool_call_arguments_at_write_time() {
    let mut assistant = Message {
        role: Role::Assistant,
        content: Default::default(),
        tool_calls: vec![ToolCall {
            id: "w_1".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({ "path": "src/lib.rs", "content": "A".repeat(5_000) }),
            signature: None,
        }],
        tool_call_id: None,
    };

    trim_tool_call_arguments(&mut assistant);

    let content = assistant.tool_calls[0].arguments["content"]
        .as_str()
        .expect("content stays a string");
    assert!(content.contains("<omitted-content"));
    assert!(content.len() < 100);
    assert_eq!(assistant.tool_calls[0].arguments["path"], "src/lib.rs");
}

#[test]
fn truncates_oversized_tool_output_before_entering_history() {
    let huge = "X".repeat(MAX_TOOL_OUTPUT_HISTORY_CHARS + 5_000);
    let result = tool_result_content(&ToolOutput::text(huge));
    assert!(result.len() <= MAX_TOOL_OUTPUT_HISTORY_CHARS);
    assert!(result.contains("earlier tool output compacted"));
}

#[test]
fn token_estimate_accounts_for_cjk_and_ascii() {
    let ascii = "a".repeat(4_000);
    let cjk = "漢".repeat(4_000);
    let ascii_tokens = estimate_text_tokens(&ascii);
    let cjk_tokens = estimate_text_tokens(&cjk);
    assert!(ascii_tokens < 1_100, "ascii: {ascii_tokens}");
    assert!(cjk_tokens > 3_500, "cjk: {cjk_tokens}");
    assert!(cjk_tokens > ascii_tokens * 3);

    let cyrillic = "а".repeat(3_000);
    let cyr_tokens = estimate_text_tokens(&cyrillic);
    assert!(cyr_tokens >= 2_000, "cyrillic: {cyr_tokens}");
}
