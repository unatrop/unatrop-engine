use protocol::{CompletionRequest, ContentPart, Role, ThinkingLevel};
use serde_json::{Value, json};

const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Строит JSON-тело запроса в формате Anthropic Messages API с выбором режима thinking
pub(crate) fn build_messages_request_with_options(
    request: &CompletionRequest,
    prompt_cache: bool,
    adaptive_thinking: bool,
) -> Value {
    let mut system_blocks = Vec::new();
    let mut messages_json: Vec<Value> = Vec::new();

    for msg in &request.messages {
        match msg.role {
            Role::System | Role::Developer => {
                for part in &msg.content.0 {
                    if let ContentPart::Text { text } = part
                        && !text.is_empty()
                    {
                        system_blocks.push(json!({ "type": "text", "text": text }));
                    }
                }
            }
            Role::User => {
                let blocks = content_parts_to_anthropic(&msg.content.0);
                if !blocks.is_empty() {
                    push_or_merge_user_blocks(&mut messages_json, blocks);
                }
            }
            Role::Assistant => {
                let mut blocks = Vec::new();
                for part in &msg.content.0 {
                    match part {
                        ContentPart::Text { text } if !text.is_empty() => {
                            blocks.push(json!({ "type": "text", "text": text }));
                        }
                        ContentPart::Reasoning { text } if !text.is_empty() => {
                            blocks.push(json!({ "type": "thinking", "thinking": text }));
                        }
                        _ => {}
                    }
                }
                for call in &msg.tool_calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": call.arguments,
                    }));
                }
                if !blocks.is_empty() {
                    messages_json.push(json!({
                        "role": "assistant",
                        "content": blocks,
                    }));
                }
            }
            Role::Tool => {
                let tool_use_id = msg.tool_call_id.as_deref().unwrap_or("");
                let content_text: String = msg
                    .content
                    .0
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content_text,
                });
                push_or_merge_user_blocks(&mut messages_json, vec![block]);
            }
        }
    }

    if prompt_cache && let Some(last_sys) = system_blocks.last_mut().and_then(Value::as_object_mut)
    {
        last_sys.insert("cache_control".into(), json!({ "type": "ephemeral" }));
    }
    if prompt_cache && let Some(last_message) = messages_json.last_mut() {
        mark_last_block_ephemeral(last_message);
    }

    let mut body = serde_json::Map::new();
    body.insert("model".into(), Value::String(request.model.clone()));
    body.insert(
        "max_tokens".into(),
        Value::Number(serde_json::Number::from(
            request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        )),
    );
    body.insert("stream".into(), Value::Bool(true));

    if let Some(temp) = request.temperature
        && let Some(num) = serde_json::Number::from_f64(f64::from(temp))
    {
        body.insert("temperature".into(), Value::Number(num));
    }

    if !system_blocks.is_empty() {
        body.insert("system".into(), Value::Array(system_blocks));
    }

    body.insert("messages".into(), Value::Array(messages_json));

    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }

    let use_adaptive = adaptive_thinking
        || request
            .metadata
            .get("anthropic_thinking_mode")
            .is_some_and(|mode| mode.eq_ignore_ascii_case("adaptive"));

    if use_adaptive {
        if let Some(effort) = request.thinking_level.and_then(thinking_level_to_effort) {
            body.remove("temperature");
            body.insert(
                "thinking".into(),
                json!({ "type": "adaptive", "display": "summarized" }),
            );
            body.insert("output_config".into(), json!({ "effort": effort }));
        }
    } else if let Some(budget) = request.thinking_level.and_then(thinking_budget_tokens) {
        let current_max = body
            .get("max_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(u64::from(DEFAULT_MAX_TOKENS));
        if current_max <= budget {
            body.insert("max_tokens".into(), Value::from(budget + 1024));
        }
        // Anthropic требует, чтобы при включённом extended thinking temperature не задавалась (или была 1)
        body.remove("temperature");
        body.insert(
            "thinking".into(),
            json!({ "type": "enabled", "budget_tokens": budget }),
        );
    }

    if let Some(user_id) = request
        .metadata
        .get("user_id")
        .or(request.session_id.as_ref())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        body.insert("metadata".into(), json!({ "user_id": user_id }));
    }

    Value::Object(body)
}

/// Ставит `cache_control` на последний блок сообщения, чтобы кэшировать растущий
/// префикс диалога между ходами (system-блок покрывает только tools+system).
fn mark_last_block_ephemeral(message: &mut Value) {
    let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    let block = blocks.iter_mut().rev().find(|block| {
        matches!(
            block.get("type").and_then(Value::as_str),
            Some("text" | "image" | "tool_use" | "tool_result" | "document")
        )
    });
    if let Some(block) = block.and_then(Value::as_object_mut) {
        block.insert("cache_control".into(), json!({ "type": "ephemeral" }));
    }
}

fn thinking_budget_tokens(level: ThinkingLevel) -> Option<u64> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => Some(1024),
        ThinkingLevel::Low => Some(2048),
        ThinkingLevel::Medium => Some(8192),
        ThinkingLevel::High => Some(16384),
        ThinkingLevel::XHigh => Some(32768),
    }
}

fn thinking_level_to_effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal | ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High => Some("high"),
        ThinkingLevel::XHigh => Some("xhigh"),
    }
}

fn content_parts_to_anthropic(parts: &[ContentPart]) -> Vec<Value> {
    let mut blocks = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text { text } if !text.is_empty() => {
                blocks.push(json!({ "type": "text", "text": text }));
            }
            ContentPart::ImageUrl { url, .. } => {
                if let Some((media_type, data)) = parse_data_uri(url) {
                    blocks.push(json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": media_type,
                            "data": data,
                        }
                    }));
                } else {
                    blocks.push(json!({
                        "type": "image",
                        "source": {
                            "type": "url",
                            "url": url,
                        }
                    }));
                }
            }
            _ => {}
        }
    }
    blocks
}

fn parse_data_uri(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media_type = meta.strip_suffix(";base64")?;
    Some((media_type, data))
}

fn push_or_merge_user_blocks(messages: &mut Vec<Value>, mut blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut().and_then(Value::as_object_mut)
        && last.get("role").and_then(Value::as_str) == Some("user")
        && let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        arr.append(&mut blocks);
        return;
    }
    messages.push(json!({
        "role": "user",
        "content": blocks,
    }));
}
