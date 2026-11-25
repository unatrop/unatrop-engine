//! Pure helper functions for `uno-harness`: tool result deduplication, spill retention,
//! compaction summarization, provider stream retries, and tool-call validation.

use std::collections::HashSet;
use std::sync::Arc;

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use protocol::{CompletionRequest, ContentPart, Message, Role, StreamEvent, ToolCall};
use provider::{Provider, ProviderError};
use tool_core::{
    ToolOutput,
    retention::{RetentionConfig, prune_text},
    spill::SpillStore,
};

use crate::error::AgentError;
use crate::harness::{
    COMPACTION_SUMMARY_MAX_TOKENS, MAX_PROVIDER_START_ATTEMPTS, tool_result_content,
};

/// Минимальный размер результата, для которого имеет смысл схлопывать подряд идущие дубли
pub const MIN_DEDUPE_TOOL_CHARS: usize = 256;

/// Схлопывает подряд идущий идентичный tool-результат в короткую ссылку, экономя токены.
/// Сравнивается только с непосредственно предыдущим `Tool`-сообщением (в пределах одного хода).
pub fn dedupe_consecutive_tool_result(messages: &[Message], content: String) -> String {
    if content.len() < MIN_DEDUPE_TOOL_CHARS {
        return content;
    }
    let Some(previous) = messages.last() else {
        return content;
    };
    if previous.role != Role::Tool {
        return content;
    }
    let previous_text: String = previous
        .content
        .0
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if previous_text != content {
        return content;
    }
    let previous_id = previous.tool_call_id.as_deref().unwrap_or("previous");
    format!(
        "[duplicate of tool result {previous_id}; identical {content_len} bytes omitted]",
        content_len = content.len()
    )
}

pub(super) async fn format_tool_output_with_spill(
    tool_name: &str,
    output: &ToolOutput,
    spill: Option<&(Arc<dyn SpillStore>, String)>,
) -> String {
    let Some((store, session_id)) = spill else {
        return tool_result_content(output);
    };

    let raw = if output.is_error {
        format!("Tool error: {}", output.content)
    } else {
        output.content.clone()
    };

    let cfg = RetentionConfig::default();
    let pruned = prune_text(&raw, cfg);
    if !pruned.truncated {
        return pruned.text;
    }

    match store
        .save(session_id, &format!("{tool_name}.log"), &raw)
        .await
    {
        Ok(saved) => format!(
            "{}\n\n[full output ({} bytes) saved to: {}]",
            pruned.text, saved.bytes, saved.location
        ),
        Err(_) => pruned.text,
    }
}

/// Суммирует вытесняемые сообщения отдельным дешёвым запросом к модели.
/// При любой ошибке возвращает `None`, чтобы компактификация продолжилась без суммаризации.
pub(super) async fn summarize_dropped(
    provider: &dyn Provider,
    model: &str,
    session_id: Option<&str>,
    dropped: &[Message],
) -> Option<String> {
    if dropped.is_empty() {
        return None;
    }

    let mut transcript = String::new();
    for msg in dropped {
        transcript.push_str(role_label(msg.role));
        transcript.push_str(":\n");
        for part in &msg.content.0 {
            match part {
                ContentPart::Text { text } => transcript.push_str(text),
                ContentPart::Reasoning { .. } => {}
                ContentPart::ImageUrl { .. } => transcript.push_str("[image omitted]"),
            }
            transcript.push('\n');
        }
        for call in &msg.tool_calls {
            transcript.push_str("tool_call ");
            transcript.push_str(&call.name);
            transcript.push(' ');
            transcript.push_str(&call.arguments.to_string());
            transcript.push('\n');
        }
        transcript.push('\n');
    }
    let transcript = bound_transcript(&transcript, 12_000);

    let mut request = CompletionRequest::new(
        model,
        vec![
            Message::system(
                "You compact an AI agent's conversation history. Summarize the transcript below \
                 into at most 12 concise bullet points. Preserve: the user's goal, decisions made, \
                 files/commands touched, tool results that still matter, and open questions. \
                 Omit redundant intermediate steps and raw tool output. Reply with the summary only.",
            ),
            Message::user(transcript),
        ],
    );
    request.max_tokens = Some(COMPACTION_SUMMARY_MAX_TOKENS);
    request.temperature = Some(0.0);
    request.session_id = session_id.map(str::to_owned);

    let Ok(mut stream) = start_provider_stream(provider, &request).await else {
        return None;
    };
    let mut summary = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamEvent::TextDelta { delta }) => summary.push_str(&delta),
            Ok(StreamEvent::Error { .. }) | Err(_) => return None,
            _ => {}
        }
    }
    let summary = summary.trim();
    (!summary.is_empty()).then(|| summary.to_owned())
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::Developer => "developer",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn bound_transcript(text: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens.saturating_mul(4);
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str("\n[... transcript truncated ...]");
    out
}

/// Вставляет сводку вытесненных шагов сразу после защищённого префикса
pub(super) fn insert_compaction_summary(messages: &mut Vec<Message>, summary: String) {
    let mut prefix_len = 0usize;
    while prefix_len < messages.len()
        && matches!(messages[prefix_len].role, Role::System | Role::Developer)
    {
        prefix_len += 1;
    }
    if prefix_len < messages.len() && messages[prefix_len].role == Role::User {
        prefix_len += 1;
    }
    messages.insert(
        prefix_len,
        Message::system(format!("[Summary of compacted earlier steps]\n{summary}")),
    );
}

pub(super) async fn start_provider_stream(
    provider: &dyn Provider,
    request: &CompletionRequest,
) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, AgentError> {
    let mut last_retryable_error = None;
    for attempt in 0..MAX_PROVIDER_START_ATTEMPTS {
        match provider.stream(request.clone()).await {
            Ok(stream) => return Ok(stream),
            Err(error) if error.is_retryable() => {
                last_retryable_error = Some(error);
                if attempt + 1 < MAX_PROVIDER_START_ATTEMPTS {
                    continue;
                }
                break;
            }
            Err(error) => return Err(AgentError::Provider(error)),
        }
    }

    match last_retryable_error {
        Some(error) => Err(AgentError::Provider(error)),
        None => Err(AgentError::Provider(ProviderError::InvalidResponse(
            "provider stream attempts ended without a result".into(),
        ))),
    }
}

pub(super) fn validate_tool_calls(calls: &[ToolCall]) -> Result<(), AgentError> {
    let mut ids = HashSet::with_capacity(calls.len());
    for call in calls {
        if call.id.trim().is_empty() || call.name.trim().is_empty() {
            return Err(AgentError::InvalidToolCall);
        }

        if !ids.insert(call.id.trim()) {
            return Err(AgentError::DuplicateToolCallId {
                id: call.id.clone(),
            });
        }
    }

    Ok(())
}
