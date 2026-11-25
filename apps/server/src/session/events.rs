//! Maps `uno-harness` `RunEvent`s into wire `pb::ServerMsg`s and updates session metrics.

use uno_harness::{AgentError, RunEvent};
use uno_protocol::StreamEvent;

use super::entry::SessionEntry;
use crate::protocol::{pb, stringify_json_args};

pub(super) fn map_run_event(
    entry: &SessionEntry,
    item: Result<RunEvent, AgentError>,
) -> pb::ServerMsg {
    let msg = match item {
        Ok(RunEvent::Started) => pb::server_msg::Msg::Started(pb::Empty {}),
        Ok(RunEvent::StateChanged(s)) => pb::server_msg::Msg::State(pb::StateEvent {
            state: pb::RunState::from(s) as i32,
        }),
        Ok(RunEvent::ProviderEvent(ev)) => {
            if matches!(
                ev,
                StreamEvent::TextDelta { .. }
                    | StreamEvent::ReasoningDelta { .. }
                    | StreamEvent::ToolCallDelta { .. }
            ) && let Some(ttft_ms) = entry.mark_first_token_ms()
            {
                tracing::info!(session_id = %entry.id, ttft_ms, "first token received");
            }
            pb::server_msg::Msg::Delta(pb::DeltaEvent {
                event: Some(pb::StreamEvent::from(&ev)),
            })
        }
        Ok(RunEvent::ToolCallStarted {
            id,
            name,
            arguments,
        }) => pb::server_msg::Msg::ToolStarted(pb::ToolStartedEvent {
            id,
            name,
            arguments_json: stringify_json_args(&arguments),
        }),
        Ok(RunEvent::ToolOutputDelta { id, name, chunk }) => {
            pb::server_msg::Msg::ToolDelta(pb::ToolDeltaEvent { id, name, chunk })
        }
        Ok(RunEvent::ToolCallFinished { id, name, output }) => {
            if let Ok(mut m) = entry.metrics.lock() {
                m.record_tool_call(output.is_error);
            }
            tracing::debug!(
                session_id = %entry.id,
                tool = %name,
                is_error = output.is_error,
                "tool call finished"
            );
            pb::server_msg::Msg::ToolFinished(pb::ToolFinishedEvent {
                id,
                name,
                content: output.content,
                is_error: output.is_error,
            })
        }
        Ok(RunEvent::MessageAppended(message)) => {
            pb::server_msg::Msg::MessageAppended(pb::MessageAppendedEvent {
                message: Some(pb::Message::from(&message)),
            })
        }
        Ok(RunEvent::TurnLimitReached { max_turns }) => {
            pb::server_msg::Msg::TurnLimitReached(pb::TurnLimitReachedEvent {
                max_turns: max_turns as u64,
            })
        }
        Ok(RunEvent::ContextCompacted {
            dropped_messages,
            estimated_tokens_after,
        }) => {
            if let Ok(mut m) = entry.metrics.lock() {
                m.record_compaction();
            }
            tracing::info!(
                session_id = %entry.id,
                dropped_messages,
                estimated_tokens_after,
                "context compacted"
            );
            pb::server_msg::Msg::Compacted(pb::CompactedEvent {
                dropped_messages: dropped_messages as u64,
                estimated_tokens_after,
            })
        }
        Ok(RunEvent::Completed {
            messages,
            finish_reason,
            usage,
        }) => {
            if let Ok(mut m) = entry.metrics.lock() {
                m.record_turn(usage.as_ref());
                let total_ms = entry.turn_elapsed_ms().unwrap_or(0);
                tracing::info!(
                    session_id = %entry.id,
                    turns = m.turns,
                    total_ms,
                    input_tokens = m.input_tokens,
                    output_tokens = m.output_tokens,
                    cached_tokens = m.cached_tokens,
                    "turn completed"
                );
            }
            entry.persist_async();
            pb::server_msg::Msg::Completed(pb::CompletedEvent {
                messages: messages.iter().map(pb::Message::from).collect(),
                finish_reason: finish_reason.map(|r| pb::FinishReason::from(r) as i32),
                usage: usage.as_ref().map(pb::Usage::from),
            })
        }
        Err(err) => pb::server_msg::Msg::Error(pb::ErrorEvent {
            message: err.to_string(),
        }),
    };
    pb::ServerMsg { msg: Some(msg) }
}
