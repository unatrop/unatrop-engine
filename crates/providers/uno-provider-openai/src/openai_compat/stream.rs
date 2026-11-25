use std::ops::ControlFlow;

use protocol::{FinishReason, StreamEvent, Usage};
use provider::{ProviderError, SseDecoder};
use serde_json::Value;

use crate::dto::ChatChunk;

struct EmbeddedError {
    message: String,
    retryable: bool,
}

fn retryable_code(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Number(code)) => code.as_u64().is_some_and(|code| code == 429 || code >= 500),
        Some(Value::String(code)) => {
            let normalized = code.to_ascii_lowercase();
            normalized
                .parse::<u16>()
                .is_ok_and(|code| code == 429 || code >= 500)
                || [
                    "rate_limit",
                    "overload",
                    "server_error",
                    "temporarily_unavailable",
                ]
                .iter()
                .any(|marker| normalized.contains(marker))
        }
        _ => false,
    }
}

fn embedded_error(data: &str) -> Option<EmbeddedError> {
    let value: Value = serde_json::from_str(data).ok()?;
    let error = value.get("error")?;

    match error {
        Value::Null => None,
        Value::String(message) => Some(EmbeddedError {
            message: message.clone(),
            retryable: false,
        }),
        Value::Object(object) => {
            let message = object
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| error.to_string(), str::to_owned);
            let message = match object.get("code") {
                Some(Value::String(code)) => format!("{message} (code: {code})"),
                Some(Value::Number(code)) => format!("{message} (code: {code})"),
                _ => message,
            };
            let retryable = retryable_code(object.get("code"))
                || retryable_code(object.get("status"))
                || object
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| {
                        let normalized = kind.to_ascii_lowercase();
                        normalized.contains("rate_limit")
                            || normalized.contains("server_error")
                            || normalized.contains("overload")
                    });
            Some(EmbeddedError { message, retryable })
        }
        other => Some(EmbeddedError {
            message: other.to_string(),
            retryable: false,
        }),
    }
}

/// Накапливает завершающие данные из разных чанков для единого события `Finished`
#[derive(Default)]
pub(super) struct StreamState {
    finish_reason: Option<FinishReason>,
    usage: Option<Usage>,
    failed: bool,
    finished: bool,
}

impl StreamState {
    pub(super) fn handle_data(&mut self, data: &str) -> Vec<Result<StreamEvent, ProviderError>> {
        let chunk: ChatChunk = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(err) => {
                return vec![Err(ProviderError::InvalidResponse(format!(
                    "JSON parse error: {err}; payload: {data}"
                )))];
            }
        };

        if chunk.choices.is_empty()
            && chunk.usage.is_none()
            && let Some(error) = embedded_error(data)
        {
            self.failed = true;
            return vec![Err(ProviderError::StreamError {
                message: error.message,
                retryable: error.retryable,
            })];
        }

        if let Some(usage) = chunk.usage {
            self.usage = Some(Usage {
                input_tokens: usage.total_input_tokens(),
                output_tokens: usage.completion_tokens,
                cached_tokens: usage.cached_tokens(),
            });
        }

        let mut out = Vec::new();

        for choice in chunk.choices {
            if let Some(text) = choice.delta.content
                && !text.is_empty()
            {
                out.push(Ok(StreamEvent::TextDelta { delta: text }));
            }

            // Провайдеры используют любое из этих двух имён поля
            let reasoning = choice
                .delta
                .reasoning_content
                .filter(|text| !text.is_empty())
                .or(choice.delta.reasoning);
            if let Some(reasoning) = reasoning
                && !reasoning.is_empty()
            {
                out.push(Ok(StreamEvent::ReasoningDelta { delta: reasoning }));
            }

            for tool_call in choice.delta.tool_calls {
                out.push(Ok(StreamEvent::ToolCallDelta {
                    index: tool_call.index,
                    id: tool_call.id,
                    name: tool_call
                        .function
                        .as_ref()
                        .and_then(|function| function.name.clone()),
                    arguments_delta: tool_call
                        .function
                        .and_then(|function| function.arguments)
                        .unwrap_or_default(),
                }));
            }

            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
            }
        }

        out
    }

    /// Данные `usage` могут прийти после `finish_reason`, поэтому завершение откладывается до конца потока
    pub(super) fn finish(&mut self) -> Option<Result<StreamEvent, ProviderError>> {
        if self.finished {
            return None;
        }
        self.finished = true;

        if self.failed {
            return None;
        }

        let usage = self.usage.take();
        Some(match self.finish_reason.take() {
            Some(reason) => Ok(StreamEvent::Finished { reason, usage }),
            None if usage.is_some() => Ok(StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage,
            }),
            None => Err(ProviderError::InvalidResponse(
                "Stream ended without a finish reason".into(),
            )),
        })
    }
}

impl SseDecoder for StreamState {
    fn on_event(
        &mut self,
        _event: &str,
        data: &str,
        out: &mut Vec<Result<StreamEvent, ProviderError>>,
    ) -> ControlFlow<()> {
        let mut items = self.handle_data(data);
        out.append(&mut items);
        ControlFlow::Continue(())
    }

    fn on_eof(&mut self, out: &mut Vec<Result<StreamEvent, ProviderError>>) {
        if let Some(item) = self.finish() {
            out.push(item);
        }
    }
}
