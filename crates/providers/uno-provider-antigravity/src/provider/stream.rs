use std::ops::ControlFlow;

use protocol::{FinishReason, StreamEvent, Usage};
use provider::{ProviderError, SseDecoder};
use serde_json::{Value, json};

#[derive(Default)]
pub(super) struct AntigravityDecoder {
    tool_index: usize,
    pending_signature: Option<String>,
    finished: bool,
    saw_tool_call: bool,
    finish_reason: Option<FinishReason>,
    usage: Option<Usage>,
}

impl SseDecoder for AntigravityDecoder {
    fn on_event(
        &mut self,
        _event: &str,
        data: &str,
        out: &mut Vec<Result<StreamEvent, ProviderError>>,
    ) -> ControlFlow<()> {
        let value: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(error) => {
                out.push(Err(ProviderError::InvalidResponse(format!(
                    "Failed to parse Antigravity SSE frame: {error}"
                ))));
                return ControlFlow::Continue(());
            }
        };

        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown stream error");
            let code = error
                .get("code")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            out.push(Ok(StreamEvent::Error {
                message: message.to_string(),
                retryable: code == 429 || code >= 500,
            }));
            return ControlFlow::Break(());
        }

        let response = value.get("response").unwrap_or(&value);
        if let Some(metadata) = response.get("usageMetadata") {
            self.usage = Some(parse_usage(metadata));
        }

        let Some(candidate) = response
            .get("candidates")
            .and_then(|value| value.as_array())
            .and_then(|candidates| candidates.first())
        else {
            return ControlFlow::Continue(());
        };

        if let Some(parts) = candidate
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(|parts| parts.as_array())
        {
            for part in parts {
                let part_sig = part
                    .get("thoughtSignature")
                    .or_else(|| part.get("thought_signature"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());

                if let Some(text) = part.get("text").and_then(|value| value.as_str())
                    && !text.is_empty()
                {
                    let thought = part
                        .get("thought")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    if thought {
                        out.push(Ok(StreamEvent::ReasoningDelta {
                            delta: text.to_string(),
                        }));
                    } else {
                        out.push(Ok(StreamEvent::TextDelta {
                            delta: text.to_string(),
                        }));
                    }
                }
                if let Some(call) = part.get("functionCall") {
                    let index = self.tool_index;
                    self.tool_index += 1;
                    self.saw_tool_call = true;
                    let id = call
                        .get("id")
                        .and_then(|value| value.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("call_{index}"));
                    let name = call
                        .get("name")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let arguments = call.get("args").cloned().unwrap_or_else(|| json!({}));
                    let call_sig = call
                        .get("thoughtSignature")
                        .or_else(|| call.get("thought_signature"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty());
                    if let Some(signature) = part_sig
                        .or(call_sig)
                        .map(str::to_string)
                        .or_else(|| self.pending_signature.take())
                    {
                        out.push(Ok(StreamEvent::ThoughtSignature { index, signature }));
                    }
                    out.push(Ok(StreamEvent::ToolCallDelta {
                        index,
                        id: Some(id),
                        name: Some(name),
                        arguments_delta: arguments.to_string(),
                    }));
                } else if let Some(sig) = part_sig {
                    if self.tool_index > 0 {
                        out.push(Ok(StreamEvent::ThoughtSignature {
                            index: self.tool_index - 1,
                            signature: sig.to_string(),
                        }));
                    } else {
                        self.pending_signature = Some(sig.to_string());
                    }
                }
            }
        }

        if let Some(reason) = candidate
            .get("finishReason")
            .and_then(|value| value.as_str())
        {
            let reason = if self.saw_tool_call {
                FinishReason::ToolCall
            } else {
                map_finish_reason(reason)
            };
            out.push(Ok(StreamEvent::Finished {
                reason,
                usage: self.usage.take(),
            }));
            self.finished = true;
            return ControlFlow::Break(());
        }

        ControlFlow::Continue(())
    }

    fn on_eof(&mut self, out: &mut Vec<Result<StreamEvent, ProviderError>>) {
        if !self.finished {
            let reason = self.finish_reason.take().unwrap_or(if self.saw_tool_call {
                FinishReason::ToolCall
            } else {
                FinishReason::Stop
            });
            out.push(Ok(StreamEvent::Finished {
                reason,
                usage: self.usage.take(),
            }));
            self.finished = true;
        }
    }
}

fn parse_usage(metadata: &Value) -> Usage {
    let prompt = metadata
        .get("promptTokenCount")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;
    let candidates = metadata
        .get("candidatesTokenCount")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;
    let thoughts = metadata
        .get("thoughtsTokenCount")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;
    let cached = metadata
        .get("cachedContentTokenCount")
        .and_then(serde_json::Value::as_u64)
        .map(|value| value as u32);
    Usage {
        input_tokens: prompt,
        output_tokens: candidates.saturating_add(thoughts),
        cached_tokens: cached,
    }
}

fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "STOP" => FinishReason::Stop,
        "MAX_TOKENS" => FinishReason::Length,
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY" => {
            FinishReason::ContentFilter
        }
        _ => FinishReason::Other,
    }
}
