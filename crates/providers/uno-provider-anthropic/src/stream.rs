use std::ops::ControlFlow;

use protocol::{FinishReason, StreamEvent, Usage};
use provider::{BoxStream, ProviderError, SseDecoder, sse_stream};
use serde_json::Value;
use transport::ByteStream;

#[derive(Default)]
struct AnthropicStreamState {
    usage: Usage,
    has_usage: bool,
    finish_reason: Option<FinishReason>,
    finished_emitted: bool,
}

impl SseDecoder for AnthropicStreamState {
    fn on_event(
        &mut self,
        event: &str,
        data: &str,
        out: &mut Vec<Result<StreamEvent, ProviderError>>,
    ) -> ControlFlow<()> {
        match handle_sse_event(event, data, self) {
            Ok(Some(ev)) => {
                let is_finished = matches!(ev, StreamEvent::Finished { .. });
                out.push(Ok(ev));
                if is_finished {
                    return ControlFlow::Break(());
                }
            }
            Ok(None) => {}
            Err(err) => {
                out.push(Err(err));
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }

    fn on_eof(&mut self, out: &mut Vec<Result<StreamEvent, ProviderError>>) {
        if !self.finished_emitted && self.finish_reason.is_some() {
            self.finished_emitted = true;
            out.push(Ok(StreamEvent::Finished {
                reason: self.finish_reason.unwrap_or(FinishReason::Stop),
                usage: self.has_usage.then_some(self.usage.clone()),
            }));
        }
    }
}

/// Преобразует поток байтов Anthropic SSE (`/v1/messages`) в поток [`StreamEvent`]
pub(crate) fn parse_anthropic_sse_stream(
    byte_stream: ByteStream,
) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
    sse_stream(byte_stream, AnthropicStreamState::default())
}

fn handle_sse_event(
    event_name: &str,
    data: &str,
    state: &mut AnthropicStreamState,
) -> Result<Option<StreamEvent>, ProviderError> {
    let payload: Value = serde_json::from_str(data).map_err(|err| {
        ProviderError::InvalidResponse(format!("invalid anthropic SSE JSON: {err}"))
    })?;

    let event_type = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or(event_name);

    match event_type {
        "message_start" => {
            if let Some(usage) = payload.pointer("/message/usage") {
                update_usage_from_value(&mut state.usage, usage);
                state.has_usage = true;
            }
            Ok(None)
        }
        "content_block_start" => {
            let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let block = payload.get("content_block");
            if block.and_then(|b| b.get("type")).and_then(Value::as_str) == Some("tool_use") {
                let id = block
                    .and_then(|b| b.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let name = block
                    .and_then(|b| b.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                return Ok(Some(StreamEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments_delta: String::new(),
                }));
            }
            Ok(None)
        }
        "content_block_delta" => {
            let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let Some(delta) = payload.get("delta") else {
                return Ok(None);
            };
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => {
                    let text = delta
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if text.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(StreamEvent::TextDelta {
                            delta: text.to_owned(),
                        }))
                    }
                }
                Some("thinking_delta") => {
                    let thinking = delta
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if thinking.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(StreamEvent::ReasoningDelta {
                            delta: thinking.to_owned(),
                        }))
                    }
                }
                Some("input_json_delta") => {
                    let partial = delta
                        .get("partial_json")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if partial.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(StreamEvent::ToolCallDelta {
                            index,
                            id: None,
                            name: None,
                            arguments_delta: partial.to_owned(),
                        }))
                    }
                }
                _ => Ok(None),
            }
        }
        "message_delta" => {
            if let Some(reason_str) = payload
                .pointer("/delta/stop_reason")
                .and_then(Value::as_str)
            {
                state.finish_reason = Some(map_stop_reason(reason_str));
            }
            if let Some(usage) = payload.get("usage") {
                update_usage_from_value(&mut state.usage, usage);
                state.has_usage = true;
            }
            Ok(None)
        }
        "message_stop" => {
            state.finished_emitted = true;
            Ok(Some(StreamEvent::Finished {
                reason: state.finish_reason.unwrap_or(FinishReason::Stop),
                usage: state.has_usage.then_some(state.usage.clone()),
            }))
        }
        "error" => {
            let err_type = payload
                .pointer("/error/type")
                .and_then(Value::as_str)
                .unwrap_or("error");
            let msg = payload
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("unknown anthropic stream error");
            let formatted = format!("{err_type}: {msg}");
            let fallback_retryable = matches!(
                err_type,
                "overloaded_error" | "rate_limit_error" | "api_error"
            );
            Err(ProviderError::from_stream_error(
                formatted,
                fallback_retryable,
            ))
        }
        _ => Ok(None),
    }
}

fn update_usage_from_value(usage: &mut Usage, val: &Value) {
    if let Some(input) = val.get("input_tokens").and_then(Value::as_u64) {
        usage.input_tokens = input as u32;
    }
    if let Some(output) = val.get("output_tokens").and_then(Value::as_u64) {
        usage.output_tokens = output as u32;
    }
    if let Some(cached) = val.get("cache_read_input_tokens").and_then(Value::as_u64) {
        usage.cached_tokens = Some(cached as u32);
    }
}

fn map_stop_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "tool_use" => FinishReason::ToolCall,
        "max_tokens" => FinishReason::Length,
        _ => FinishReason::Other,
    }
}
