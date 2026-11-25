use protocol::{CompletionRequest, ThinkingLevel};
use serde_json::{Map, Value, json};

use provider::ProviderError;

pub(super) fn request_body(request: &CompletionRequest) -> Result<Value, ProviderError> {
    request_body_with_cache(request, None)
}

pub(super) fn request_body_with_cache(
    request: &CompletionRequest,
    prompt_cache_override: Option<bool>,
) -> Result<Value, ProviderError> {
    let mut body = serde_json::to_value(request).map_err(|err| {
        ProviderError::InvalidResponse(format!("Failed to serialize request: {err}"))
    })?;

    let Some(object) = body.as_object_mut() else {
        return Err(ProviderError::InvalidResponse(
            "Serialized request must be a JSON object".into(),
        ));
    };

    let has_tools = object
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    if has_tools {
        object.insert("tool_choice".into(), Value::String("auto".into()));
    }

    if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            let tool_object = tool.as_object().ok_or_else(|| {
                ProviderError::InvalidResponse(
                    "Serialized tool definition must be a JSON object".into(),
                )
            })?;
            let name = tool_object.get("name").cloned().ok_or_else(|| {
                ProviderError::InvalidResponse("Tool definition is missing name".into())
            })?;
            let description = tool_object.get("description").cloned().ok_or_else(|| {
                ProviderError::InvalidResponse("Tool definition is missing description".into())
            })?;
            let parameters = tool_object.get("input_schema").cloned().ok_or_else(|| {
                ProviderError::InvalidResponse("Tool definition is missing input_schema".into())
            })?;

            *tool = json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters,
                }
            });
        }
    }

    if let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            let Some(message_object) = message.as_object_mut() else {
                return Err(ProviderError::InvalidResponse(
                    "Serialized message must be a JSON object".into(),
                ));
            };
            flatten_content(message_object);
            let Some(tool_calls) = message_object
                .get_mut("tool_calls")
                .and_then(Value::as_array_mut)
            else {
                continue;
            };

            for tool_call in tool_calls {
                let tool_call_object = tool_call.as_object().ok_or_else(|| {
                    ProviderError::InvalidResponse(
                        "Serialized tool call must be a JSON object".into(),
                    )
                })?;
                let id = tool_call_object.get("id").cloned().ok_or_else(|| {
                    ProviderError::InvalidResponse("Tool call is missing id".into())
                })?;
                let name = tool_call_object.get("name").cloned().ok_or_else(|| {
                    ProviderError::InvalidResponse("Tool call is missing name".into())
                })?;
                let arguments = tool_call_object.get("arguments").ok_or_else(|| {
                    ProviderError::InvalidResponse("Tool call is missing arguments".into())
                })?;
                let arguments = serde_json::to_string(arguments).map_err(|err| {
                    ProviderError::InvalidResponse(format!(
                        "Failed to serialize tool arguments: {err}"
                    ))
                })?;

                *tool_call = json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments,
                    }
                });
            }
        }
    }

    let use_cache_control =
        prompt_cache_override.unwrap_or_else(|| is_anthropic_model(&request.model));
    if use_cache_control
        && let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut)
    {
        apply_anthropic_cache_control(messages);
    }

    object.remove("thinking_level");
    if let Some(level) = request.thinking_level {
        object.insert(
            "reasoning_effort".into(),
            Value::String(reasoning_effort(level).into()),
        );
    }

    object.remove("session_id");
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
            object.insert("metadata".into(), Value::Object(meta));
        } else {
            object.remove("metadata");
        }
    }

    object.insert("stream".into(), Value::Bool(true));
    object.insert("stream_options".into(), json!({ "include_usage": true }));
    Ok(body)
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

fn is_anthropic_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.starts_with("anthropic/") || lower.contains("claude")
}

/// Расставляет `cache_control` для моделей Anthropic Claude (через Polza.ai / OpenRouter):
/// 1) на `system`-промпт, 2) на последнее текстовое сообщение префикса (до 2 точек из 4).
fn apply_anthropic_cache_control(messages: &mut [Value]) {
    let mut marked_system = false;
    let mut last_non_system_idx = None;

    for (idx, message) in messages.iter().enumerate() {
        let Some(obj) = message.as_object() else {
            continue;
        };
        let has_non_empty_content = obj
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty());
        if !has_non_empty_content {
            continue;
        }
        if obj.get("role").and_then(Value::as_str) == Some("system") {
            if !marked_system {
                last_non_system_idx = last_non_system_idx.or(None);
                marked_system = true;
            }
        } else {
            last_non_system_idx = Some(idx);
        }
    }

    if marked_system
        && let Some(system_msg) = messages
            .iter_mut()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("system"))
    {
        mark_message_ephemeral(system_msg);
    }

    if let Some(idx) = last_non_system_idx
        && let Some(msg) = messages.get_mut(idx)
    {
        mark_message_ephemeral(msg);
    }
}

fn mark_message_ephemeral(message: &mut Value) {
    let Some(obj) = message.as_object_mut() else {
        return;
    };
    let Some(text) = obj
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    if text.is_empty() {
        return;
    }
    obj.insert(
        "content".into(),
        json!([
            {
                "type": "text",
                "text": text,
                "cache_control": { "type": "ephemeral" }
            }
        ]),
    );
}

/// Сворачивает типизированные части содержимого в строку для совместимых API
/// (или оставляет OpenAI multimodal-массив, если в сообщении есть `image_url`).
fn flatten_content(message: &mut Map<String, Value>) {
    if matches!(message.get("content"), Some(Value::String(_))) {
        return;
    }

    let text: String = match message.get("content") {
        Some(Value::Array(parts)) => {
            let has_images = parts
                .iter()
                .any(|part| part.get("type").and_then(Value::as_str) == Some("image_url"));

            if has_images {
                let mut oai_parts = Vec::with_capacity(parts.len());
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                oai_parts.push(json!({ "type": "text", "text": text }));
                            }
                        }
                        Some("image_url") => {
                            if let Some(url) = part.get("url").and_then(Value::as_str) {
                                let mut img_obj = serde_json::Map::new();
                                img_obj.insert("url".into(), Value::String(url.to_owned()));
                                if let Some(detail) = part.get("detail").and_then(Value::as_str) {
                                    img_obj
                                        .insert("detail".into(), Value::String(detail.to_owned()));
                                }
                                oai_parts.push(json!({
                                    "type": "image_url",
                                    "image_url": Value::Object(img_obj)
                                }));
                            }
                        }
                        _ => {}
                    }
                }
                message.insert("content".into(), Value::Array(oai_parts));
                return;
            }

            parts
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect()
        }
        _ => String::new(),
    };

    let has_tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty());

    if text.is_empty() && has_tool_calls {
        message.remove("content");
    } else {
        message.insert("content".into(), Value::String(text));
    }
}
