use std::collections::BTreeMap;

use serde_json::Value;

use crate::{
    AccumulatorError, Content, ContentPart, FinishReason, Message, Role, StreamEvent, ToolCall,
    Usage,
};

const MAX_TOOL_CALLS: usize = 128;
const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;
const MAX_REASONING_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOOL_ARGUMENT_BYTES: usize = 1024 * 1024;
const MAX_TOOL_ID_BYTES: usize = 256;
const MAX_TOOL_NAME_BYTES: usize = 256;
const MAX_TOOL_SIGNATURE_BYTES: usize = 64 * 1024;
const MAX_TOTAL_BUFFER_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Default)]
struct ToolCallDraft {
    id: String,
    name: String,
    arguments_buf: String,
    signature: Option<String>,
}

/// Итоговое сообщение ассистента, собранное из потока событий
#[derive(Debug)]
pub struct AccumulatedMessage {
    /// Собранное сообщение ассистента
    pub message: Message,
    /// Причина завершения генерации
    pub finish_reason: Option<FinishReason>,
    /// Итоговая статистика токенов
    pub usage: Option<Usage>,
}

/// Собирает потоковые дельты в цельное сообщение с проверкой лимитов буфера
#[derive(Debug, Default)]
pub struct MessageAccumulator {
    text_buf: String,
    reasoning_buf: String,
    tool_calls: BTreeMap<usize, ToolCallDraft>,
    buffered_bytes: usize,
    finish_reason: Option<FinishReason>,
    usage: Option<Usage>,
    finished: bool,
    stream_error: Option<(String, bool)>,
}

impl MessageAccumulator {
    /// Создаёт пустой аккумулятор
    pub fn new() -> Self {
        Self::default()
    }

    /// Обрабатывает очередное событие потока
    pub fn process_event(&mut self, event: &StreamEvent) -> Result<(), AccumulatorError> {
        if let Some((message, retryable)) = &self.stream_error {
            return Err(AccumulatorError::Stream {
                message: message.clone(),
                retryable: *retryable,
            });
        }
        if self.finished {
            return Err(AccumulatorError::EventAfterFinish);
        }

        match event {
            StreamEvent::TextDelta { delta } => {
                if self.text_buf.len().saturating_add(delta.len()) > MAX_TEXT_BYTES {
                    return Err(AccumulatorError::TextTooLarge {
                        limit: MAX_TEXT_BYTES,
                    });
                }
                self.check_total_size(delta.len())?;
                self.text_buf.push_str(delta);
                self.buffered_bytes += delta.len();
            }
            StreamEvent::ReasoningDelta { delta } => {
                if self.reasoning_buf.len().saturating_add(delta.len()) > MAX_REASONING_BYTES {
                    return Err(AccumulatorError::ReasoningTooLarge {
                        limit: MAX_REASONING_BYTES,
                    });
                }
                self.check_total_size(delta.len())?;
                self.reasoning_buf.push_str(delta);
                self.buffered_bytes += delta.len();
            }
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                if *index >= MAX_TOOL_CALLS {
                    return Err(AccumulatorError::TooManyToolCalls {
                        index: *index,
                        max: MAX_TOOL_CALLS,
                    });
                }

                let draft = self.tool_calls.get(index);
                if draft.is_none() && self.tool_calls.len() == MAX_TOOL_CALLS {
                    return Err(AccumulatorError::TooManyToolCalls {
                        index: *index,
                        max: MAX_TOOL_CALLS,
                    });
                }

                let old_id = draft.map_or("", |draft| draft.id.as_str());
                let old_name = draft.map_or("", |draft| draft.name.as_str());
                let new_id_len = merged_metadata_len(old_id, id.as_deref().unwrap_or_default());
                let new_name_len =
                    merged_metadata_len(old_name, name.as_deref().unwrap_or_default());
                if new_id_len > MAX_TOOL_ID_BYTES {
                    return Err(AccumulatorError::ToolMetadataTooLarge {
                        index: *index,
                        field: "id",
                        limit: MAX_TOOL_ID_BYTES,
                    });
                }
                if new_name_len > MAX_TOOL_NAME_BYTES {
                    return Err(AccumulatorError::ToolMetadataTooLarge {
                        index: *index,
                        field: "name",
                        limit: MAX_TOOL_NAME_BYTES,
                    });
                }

                let argument_size = draft
                    .map_or(0, |draft| draft.arguments_buf.len())
                    .saturating_add(arguments_delta.len());
                if argument_size > MAX_TOOL_ARGUMENT_BYTES {
                    return Err(AccumulatorError::ToolArgumentsTooLarge {
                        index: *index,
                        limit: MAX_TOOL_ARGUMENT_BYTES,
                    });
                }

                let added_bytes = new_id_len
                    .saturating_sub(old_id.len())
                    .saturating_add(new_name_len.saturating_sub(old_name.len()))
                    .saturating_add(arguments_delta.len());
                self.check_total_size(added_bytes)?;

                let id = merge_metadata(old_id, id.as_deref().unwrap_or_default());
                let name = merge_metadata(old_name, name.as_deref().unwrap_or_default());
                let draft = self.tool_calls.entry(*index).or_default();
                draft.id = id;
                draft.name = name;
                draft.arguments_buf.push_str(arguments_delta);
                self.buffered_bytes += added_bytes;
            }
            StreamEvent::ThoughtSignature { index, signature } => {
                if *index >= MAX_TOOL_CALLS {
                    return Err(AccumulatorError::TooManyToolCalls {
                        index: *index,
                        max: MAX_TOOL_CALLS,
                    });
                }
                if signature.len() > MAX_TOOL_SIGNATURE_BYTES {
                    return Err(AccumulatorError::ToolMetadataTooLarge {
                        index: *index,
                        field: "signature",
                        limit: MAX_TOOL_SIGNATURE_BYTES,
                    });
                }
                let previous = self
                    .tool_calls
                    .get(index)
                    .and_then(|draft| draft.signature.as_ref())
                    .map_or(0, String::len);
                self.check_total_size(signature.len().saturating_sub(previous))?;
                self.tool_calls.entry(*index).or_default().signature = Some(signature.clone());
            }
            StreamEvent::Finished { reason, usage } => {
                self.finish_reason = Some(*reason);
                self.usage = usage.clone();
                self.finished = true;
            }
            StreamEvent::Error { message, retryable } => {
                self.stream_error = Some((message.clone(), *retryable));
                return Err(AccumulatorError::Stream {
                    message: message.clone(),
                    retryable: *retryable,
                });
            }
        }
        Ok(())
    }

    /// Завершает сборку и валидирует вызовы инструментов
    pub fn finish(self) -> Result<AccumulatedMessage, AccumulatorError> {
        if let Some((message, retryable)) = self.stream_error {
            return Err(AccumulatorError::Stream { message, retryable });
        }
        if !self.finished {
            return Err(AccumulatorError::MissingFinish);
        }

        let mut content_parts = Vec::new();

        if !self.reasoning_buf.is_empty() {
            content_parts.push(ContentPart::reasoning(self.reasoning_buf));
        }
        if !self.text_buf.is_empty() {
            content_parts.push(ContentPart::text(self.text_buf));
        }

        let mut ready_tool_calls = Vec::with_capacity(self.tool_calls.len());
        for (index, draft) in self.tool_calls {
            if draft.id.trim().is_empty() {
                return Err(AccumulatorError::MissingToolCallMetadata { index, field: "id" });
            }
            if draft.name.trim().is_empty() {
                return Err(AccumulatorError::MissingToolCallMetadata {
                    index,
                    field: "name",
                });
            }

            let arguments = if draft.arguments_buf.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                serde_json::from_str(&draft.arguments_buf).map_err(|source| {
                    AccumulatorError::InvalidToolArguments {
                        index,
                        name: draft.name.clone(),
                        source,
                    }
                })?
            };
            if !arguments.is_object() {
                return Err(AccumulatorError::ToolArgumentsNotObject {
                    index,
                    name: draft.name,
                });
            };

            ready_tool_calls.push(ToolCall {
                id: draft.id,
                name: draft.name,
                arguments,
                signature: draft.signature,
            });
        }

        Ok(AccumulatedMessage {
            message: Message {
                role: Role::Assistant,
                content: Content(content_parts),
                tool_calls: ready_tool_calls,
                tool_call_id: None,
            },
            finish_reason: self.finish_reason,
            usage: self.usage,
        })
    }

    fn check_total_size(&self, additional_bytes: usize) -> Result<(), AccumulatorError> {
        if self.buffered_bytes.saturating_add(additional_bytes) > MAX_TOTAL_BUFFER_BYTES {
            return Err(AccumulatorError::TotalBufferTooLarge {
                limit: MAX_TOTAL_BUFFER_BYTES,
            });
        }
        Ok(())
    }
}

fn merge_metadata(current: &str, incoming: &str) -> String {
    if incoming.is_empty() || current == incoming || current.ends_with(incoming) {
        current.to_owned()
    } else if incoming.starts_with(current) {
        incoming.to_owned()
    } else {
        format!("{current}{incoming}")
    }
}

fn merged_metadata_len(current: &str, incoming: &str) -> usize {
    if incoming.is_empty() || current == incoming || current.ends_with(incoming) {
        current.len()
    } else if incoming.starts_with(current) {
        incoming.len()
    } else {
        current.len().saturating_add(incoming.len())
    }
}
