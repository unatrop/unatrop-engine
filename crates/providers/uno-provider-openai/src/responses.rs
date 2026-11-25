//! Преобразование запросов и SSE-потока для OpenAI Responses API (`/responses`)

use std::collections::HashMap;
use std::ops::ControlFlow;

use protocol::{
    CompletionRequest, ContentPart, FinishReason, Role, StreamEvent, ThinkingLevel, Usage,
};
use provider::{ProviderError, SseDecoder};
use serde_json::{Map, Value, json};

/// Формирует JSON-тело запроса в формате OpenAI `/responses`
pub fn responses_request_body(request: &CompletionRequest) -> Result<Value, ProviderError> {
    let mut instructions_parts = Vec::new();
    let mut input = Vec::new();

    for message in &request.messages {
        match message.role {
            Role::System | Role::Developer => {
                let text = collect_text(&message.content.0);
                if !text.is_empty() {
                    instructions_parts.push(text);
                }
            }
            Role::User => {
                let content = map_user_content(&message.content.0);
                input.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": content,
                }));
            }
            Role::Assistant => {
                let text = collect_text(&message.content.0);
                if !text.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": text,
                        }],
                    }));
                }
                for tool_call in &message.tool_calls {
                    let arguments = serde_json::to_string(&tool_call.arguments).map_err(|err| {
                        ProviderError::InvalidResponse(format!(
                            "Failed to serialize tool call arguments: {err}"
                        ))
                    })?;
                    input.push(json!({
                        "type": "function_call",
                        "call_id": tool_call.id,
                        "name": tool_call.name,
                        "arguments": arguments,
                    }));
                }
            }
            Role::Tool => {
                let Some(call_id) = message.tool_call_id.as_deref() else {
                    return Err(ProviderError::InvalidResponse(
                        "Tool message is missing tool_call_id".into(),
                    ));
                };
                let output = collect_text(&message.content.0);
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                }));
            }
        }
    }

    let mut body = Map::new();
    body.insert("model".into(), Value::String(request.model.clone()));
    body.insert("input".into(), Value::Array(input));
    body.insert("stream".into(), Value::Bool(true));
    body.insert("store".into(), Value::Bool(false));

    if !instructions_parts.is_empty() {
        body.insert(
            "instructions".into(),
            Value::String(instructions_parts.join("\n\n")),
        );
    }
    if let Some(max_tokens) = request.max_tokens {
        body.insert("max_output_tokens".into(), Value::from(max_tokens));
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), Value::from(temperature));
    }
    if let Some(level) = request.thinking_level {
        body.insert(
            "reasoning".into(),
            json!({ "effort": reasoning_effort(level) }),
        );
    }
    if !request.metadata.is_empty() || request.session_id.is_some() {
        let mut meta = Map::new();
        for (k, v) in &request.metadata {
            meta.insert(k.clone(), Value::String(v.clone()));
        }
        if let Some(sid) = request.session_id.as_deref().filter(|s| !s.is_empty()) {
            meta.entry("session_id".to_string())
                .or_insert_with(|| Value::String(sid.to_string()));
        }
        if !meta.is_empty() {
            body.insert("metadata".into(), Value::Object(meta));
        }
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
        body.insert("tool_choice".into(), Value::String("auto".into()));
    }

    Ok(Value::Object(body))
}

fn reasoning_effort(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "none",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
    }
}

fn collect_text(parts: &[ContentPart]) -> String {
    let mut out = String::new();
    for part in parts {
        if let ContentPart::Text { text } = part {
            if !out.is_empty() && !text.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

fn map_user_content(parts: &[ContentPart]) -> Vec<Value> {
    let mut out = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            ContentPart::Text { text } => {
                out.push(json!({
                    "type": "input_text",
                    "text": text,
                }));
            }
            ContentPart::ImageUrl { url, detail } => {
                let mut item = Map::new();
                item.insert("type".into(), Value::String("input_image".into()));
                item.insert("image_url".into(), Value::String(url.clone()));
                if let Some(detail) = detail {
                    item.insert("detail".into(), Value::String(detail.clone()));
                }
                out.push(Value::Object(item));
            }
            ContentPart::Reasoning { .. } => {}
        }
    }
    out
}

/// Декодер SSE-событий OpenAI `/responses`
#[derive(Default)]
pub(crate) struct ResponsesStreamState {
    tool_indices: HashMap<String, usize>,
    next_tool_index: usize,
    has_tool_calls: bool,
    usage: Option<Usage>,
    failed: bool,
    finished: bool,
}

impl ResponsesStreamState {
    fn tool_index_for(&mut self, item_id: &str) -> usize {
        if let Some(&idx) = self.tool_indices.get(item_id) {
            return idx;
        }
        let idx = self.next_tool_index;
        self.next_tool_index += 1;
        self.tool_indices.insert(item_id.to_string(), idx);
        idx
    }

    fn parse_usage(response: &Value) -> Option<Usage> {
        let usage = response.get("usage")?;
        let input_tokens = usage.get("input_tokens").and_then(Value::as_u64)? as u32;
        let output_tokens = usage.get("output_tokens").and_then(Value::as_u64)? as u32;
        let cached_tokens = usage
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
            .map(|count| count as u32);
        Some(Usage {
            input_tokens,
            output_tokens,
            cached_tokens,
        })
    }

    fn finish_reason(&self, event_type: &str, response: &Value) -> FinishReason {
        if event_type == "response.incomplete" {
            let reason = response
                .get("incomplete_details")
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str);
            return match reason {
                Some("max_output_tokens") => FinishReason::Length,
                Some("content_filter") => FinishReason::ContentFilter,
                _ if self.has_tool_calls => FinishReason::ToolCall,
                _ => FinishReason::Other,
            };
        }
        if self.has_tool_calls {
            FinishReason::ToolCall
        } else {
            FinishReason::Stop
        }
    }
}

impl SseDecoder for ResponsesStreamState {
    fn on_event(
        &mut self,
        event: &str,
        data: &str,
        out: &mut Vec<Result<StreamEvent, ProviderError>>,
    ) -> ControlFlow<()> {
        if self.finished || self.failed {
            return ControlFlow::Break(());
        }

        let value: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(err) => {
                out.push(Err(ProviderError::InvalidResponse(format!(
                    "JSON parse error: {err}; payload: {data}"
                ))));
                return ControlFlow::Continue(());
            }
        };

        let event_type = value.get("type").and_then(Value::as_str).unwrap_or(event);

        match event_type {
            "response.output_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    out.push(Ok(StreamEvent::TextDelta {
                        delta: delta.to_string(),
                    }));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    out.push(Ok(StreamEvent::ReasoningDelta {
                        delta: delta.to_string(),
                    }));
                }
            }
            "response.output_item.added" => {
                if let Some(item) = value.get("item")
                    && item.get("type").and_then(Value::as_str) == Some("function_call")
                {
                    let item_id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or(item_id.as_str())
                        .to_string();
                    let name = item.get("name").and_then(Value::as_str).map(str::to_string);
                    let index = self.tool_index_for(&item_id);
                    self.has_tool_calls = true;
                    out.push(Ok(StreamEvent::ToolCallDelta {
                        index,
                        id: Some(call_id),
                        name,
                        arguments_delta: String::new(),
                    }));
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    let index = value
                        .get("item_id")
                        .and_then(Value::as_str)
                        .map(|id| self.tool_index_for(id))
                        .unwrap_or_else(|| self.next_tool_index.saturating_sub(1));
                    self.has_tool_calls = true;
                    out.push(Ok(StreamEvent::ToolCallDelta {
                        index,
                        id: None,
                        name: None,
                        arguments_delta: delta.to_string(),
                    }));
                }
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                if let Some(response) = value.get("response") {
                    if let Some(usage) = Self::parse_usage(response) {
                        self.usage = Some(usage);
                    }
                    let reason = self.finish_reason(event_type, response);
                    self.finished = true;
                    out.push(Ok(StreamEvent::Finished {
                        reason,
                        usage: self.usage.take(),
                    }));
                    return ControlFlow::Break(());
                }
            }
            "response.failed" | "error" => {
                let error_obj = value
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .or_else(|| value.get("error"))
                    .unwrap_or(&value);
                let message = error_obj
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| error_obj.to_string());
                let code = error_obj.get("code").and_then(Value::as_str);
                let retryable = code.is_some_and(|c| {
                    let lc = c.to_ascii_lowercase();
                    lc.contains("rate_limit")
                        || lc.contains("server_error")
                        || lc.contains("overload")
                });
                self.failed = true;
                out.push(Err(ProviderError::StreamError { message, retryable }));
                return ControlFlow::Break(());
            }
            _ => {}
        }

        ControlFlow::Continue(())
    }

    fn on_eof(&mut self, out: &mut Vec<Result<StreamEvent, ProviderError>>) {
        if self.finished || self.failed {
            return;
        }
        self.finished = true;
        let reason = if self.has_tool_calls {
            FinishReason::ToolCall
        } else {
            FinishReason::Stop
        };
        out.push(Ok(StreamEvent::Finished {
            reason,
            usage: self.usage.take(),
        }));
    }
}
