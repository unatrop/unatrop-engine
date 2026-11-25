use protocol::{Message, ToolCall};
use serde_json::json;
use uno_session::repair_history;

#[test]
fn repairs_only_tool_calls_without_results_and_is_idempotent() {
    let mut assistant = Message::assistant("running tools");
    assistant.tool_calls = vec![
        ToolCall {
            id: "call_1".into(),
            name: "first".into(),
            arguments: json!({}),
            signature: None,
        },
        ToolCall {
            id: "call_2".into(),
            name: "second".into(),
            arguments: json!({}),
            signature: None,
        },
    ];
    let mut history = vec![assistant, Message::tool_result("call_1", "done")];

    repair_history(&mut history);

    assert_eq!(history.len(), 3);
    assert_eq!(
        history[2],
        Message::tool_result("call_2", "Tool error: run was cancelled")
    );

    repair_history(&mut history);
    assert_eq!(history.len(), 3);
}

#[test]
fn history_without_unanswered_tool_calls_is_unchanged() {
    let mut history = vec![Message::user("hello"), Message::assistant("hi")];

    repair_history(&mut history);

    assert_eq!(history, [Message::user("hello"), Message::assistant("hi")]);
}
