use std::collections::HashSet;

use protocol::{Message, Role};

/// Добавляет ответы с ошибкой для незавершённых вызовов инструментов после отмены или сбоя;
/// вызывать перед повторной отправкой истории провайдеру
pub fn repair_history(messages: &mut Vec<Message>) {
    let Some(position) = messages
        .iter()
        .rposition(|message| !message.tool_calls.is_empty())
    else {
        return;
    };

    let answered = messages[position + 1..]
        .iter()
        .filter(|message| message.role == Role::Tool)
        .filter_map(|message| message.tool_call_id.as_deref())
        .collect::<HashSet<_>>();
    let missing = messages[position]
        .tool_calls
        .iter()
        .filter(|call| !answered.contains(call.id.as_str()))
        .map(|call| call.id.clone())
        .collect::<Vec<_>>();

    messages.extend(
        missing
            .into_iter()
            .map(|id| Message::tool_result(id, "Tool error: run was cancelled")),
    );
}
