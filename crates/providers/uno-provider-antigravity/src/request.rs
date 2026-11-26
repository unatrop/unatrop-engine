use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{CompletionRequest, ContentPart, Role, ThinkingLevel, ToolDefinition};
use provider::ProviderError;
use serde_json::{Map, Value, json};

/// Base64 от `"skip_thought_signature_validator"` — обход валидатора `thoughtSignature` в Cloud Code (как в `agy`)
const SKIP_THOUGHT_SIGNATURE_VALIDATOR_B64: &str = "c2tpcF90aG91Z2h0X3NpZ25hdHVyZV92YWxpZGF0b3I=";

pub fn antigravity_request_body(
    project: &str,
    model: &str,
    request: &CompletionRequest,
) -> Result<Value, ProviderError> {
    let request_body = build_gemini_inner_request(model, request, true)?;
    Ok(json!({
        "project": project,
        "requestId": request_id(),
        "request": Value::Object(request_body),
        "model": model,
        "userAgent": "antigravity",
        "requestType": "agent",
    }))
}

/// Строит тело запроса для официального Google Gemini Developer API (`generateContent` / `streamGenerateContent`).
pub fn gemini_request_body(
    model: &str,
    request: &CompletionRequest,
) -> Result<Value, ProviderError> {
    let request_body = build_gemini_inner_request(model, request, false)?;
    Ok(Value::Object(request_body))
}

fn build_gemini_inner_request(
    model: &str,
    request: &CompletionRequest,
    cloud_code_mode: bool,
) -> Result<Map<String, Value>, ProviderError> {
    let tool_names = tool_names(request);
    let mut system_parts = Vec::new();
    let mut contents = Vec::new();

    for message in &request.messages {
        match message.role {
            Role::System | Role::Developer => {
                for part in &message.content.0 {
                    if let ContentPart::Text { text } = part
                        && !text.is_empty()
                    {
                        system_parts.push(text.clone());
                    }
                }
            }
            Role::User => {
                let parts = content_to_parts(&message.content.0);
                if !parts.is_empty() {
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
            Role::Assistant => {
                let mut parts = Vec::new();
                for part in &message.content.0 {
                    if let ContentPart::Text { text } = part
                        && !text.is_empty()
                    {
                        parts.push(json!({ "text": text }));
                    }
                }
                let has_any_signature = message
                    .tool_calls
                    .iter()
                    .any(|call| call.signature.as_deref().is_some_and(|s| !s.is_empty()));
                for (idx, call) in message.tool_calls.iter().enumerate() {
                    let mut part = json!({
                        "functionCall": {
                            "id": call.id,
                            "name": call.name,
                            "args": call.arguments,
                        }
                    });
                    if let Some(signature) = call.signature.as_deref().filter(|s| !s.is_empty()) {
                        part["thoughtSignature"] = Value::String(signature.to_string());
                    } else if idx == 0 && !has_any_signature {
                        part["thoughtSignature"] =
                            Value::String(SKIP_THOUGHT_SIGNATURE_VALIDATOR_B64.to_string());
                    }
                    parts.push(part);
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
            }
            Role::Tool => {
                let id = message.tool_call_id.clone().unwrap_or_default();
                let name = tool_names.get(id.as_str()).copied().unwrap_or("tool");
                let mut output = String::new();
                for part in &message.content.0 {
                    match part {
                        ContentPart::Text { text } => output.push_str(text),
                        ContentPart::Reasoning { text } => output.push_str(text),
                        ContentPart::ImageUrl { url, .. } => output.push_str(url),
                    }
                }
                let tool_role = if cloud_code_mode { "model" } else { "user" };
                contents.push(json!({
                    "role": tool_role,
                    "parts": [{
                        "functionResponse": {
                            "id": id,
                            "name": name,
                            "response": { "output": output },
                        }
                    }]
                }));
            }
        }
    }

    if contents.is_empty() {
        return Err(ProviderError::Configuration(
            "Gemini request requires at least one non-system message".into(),
        ));
    }

    let mut request_body = Map::new();
    request_body.insert("contents".into(), Value::Array(contents));
    if !system_parts.is_empty() {
        request_body.insert(
            "systemInstruction".into(),
            json!({ "role": "user", "parts": [{ "text": system_parts.join("\n\n") }] }),
        );
    }
    if !request.tools.is_empty() {
        request_body.insert("tools".into(), tools_to_gemini(&request.tools));
    }
    request_body.insert("generationConfig".into(), generation_config(model, request));
    if cloud_code_mode {
        let sid = request
            .session_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .map_or_else(session_id, str::to_owned);
        request_body.insert("sessionId".into(), Value::String(sid));
        if !request.metadata.is_empty() {
            request_body.insert("labels".into(), json!(request.metadata));
        }
    }

    Ok(request_body)
}

fn content_to_parts(content: &[ContentPart]) -> Vec<Value> {
    let mut parts = Vec::new();
    for part in content {
        match part {
            ContentPart::Text { text } => {
                if !text.is_empty() {
                    parts.push(json!({ "text": text }));
                }
            }
            ContentPart::Reasoning { .. } => {}
            ContentPart::ImageUrl { url, .. } => {
                if let Some(rest) = url.strip_prefix("data:")
                    && let Some((meta, data)) = rest.split_once(',')
                {
                    let mime = meta.split(';').next().unwrap_or("application/octet-stream");
                    parts.push(json!({
                        "inlineData": { "mimeType": mime, "data": data }
                    }));
                } else {
                    parts.push(json!({
                        "fileData": {
                            "mimeType": guess_mime(url),
                            "fileUri": url,
                        }
                    }));
                }
            }
        }
    }
    parts
}

fn guess_mime(url: &str) -> &'static str {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    let extension = path.rsplit('.').next().unwrap_or_default();
    match extension {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => "image/png",
    }
}

fn tools_to_gemini(tools: &[ToolDefinition]) -> Value {
    let declarations: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let mut declaration = Map::new();
            declaration.insert("name".into(), Value::String(tool.name.clone()));
            declaration.insert(
                "description".into(),
                Value::String(tool.description.clone()),
            );
            declaration.insert("parameters".into(), gemini_schema(&tool.input_schema));
            Value::Object(declaration)
        })
        .collect();
    json!([{ "functionDeclarations": declarations }])
}

fn gemini_schema(root: &Value) -> Value {
    normalize_schema(root, root, 0)
}

fn normalize_schema(root: &Value, value: &Value, depth: usize) -> Value {
    if depth > 32 {
        return Value::Object(Map::new());
    }
    match value {
        Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(Value::as_str)
                && let Some(definition) = lookup_definition(root, reference)
            {
                let mut merged = normalize_schema(root, definition, depth + 1);
                if let Value::Object(base) = &mut merged {
                    for (key, sibling) in map {
                        if key != "$ref" {
                            base.insert(key.clone(), normalize_schema(root, sibling, depth + 1));
                        }
                    }
                }
                return merged;
            }

            let mut out = Map::new();
            for (key, value) in map {
                match key.as_str() {
                    "$ref"
                    | "$schema"
                    | "$id"
                    | "definitions"
                    | "$defs"
                    | "additionalProperties" => {}
                    "type" => normalize_type(&mut out, value),
                    "const" => {
                        out.insert("enum".into(), Value::Array(vec![value.clone()]));
                    }
                    _ => {
                        out.insert(key.clone(), normalize_schema(root, value, depth + 1));
                    }
                }
            }
            flatten_enum_variants(&mut out);
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| normalize_schema(root, item, depth + 1))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn lookup_definition<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let name = reference
        .strip_prefix("#/$defs/")
        .or_else(|| reference.strip_prefix("#/definitions/"))?;
    ["$defs", "definitions"]
        .iter()
        .find_map(|key| root.get(key).and_then(|definitions| definitions.get(name)))
}

fn normalize_type(out: &mut Map<String, Value>, value: &Value) {
    match value {
        Value::String(kind) => {
            out.insert("type".into(), Value::String(kind.to_ascii_uppercase()));
        }
        Value::Array(items) => {
            let mut kinds: Vec<String> = items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_ascii_uppercase)
                .collect();
            let nullable = kinds.iter().any(|kind| kind == "NULL");
            kinds.retain(|kind| kind != "NULL");
            kinds.sort();
            kinds.dedup();
            if kinds.len() == 1 {
                out.insert("type".into(), Value::String(kinds.remove(0)));
            } else if kinds.len() > 1 {
                out.insert(
                    "anyOf".into(),
                    Value::Array(
                        kinds
                            .into_iter()
                            .map(|kind| json!({ "type": kind }))
                            .collect(),
                    ),
                );
            }
            if nullable {
                out.insert("nullable".into(), Value::Bool(true));
            }
        }
        _ => {
            out.insert("type".into(), value.clone());
        }
    }
}

fn flatten_enum_variants(out: &mut Map<String, Value>) {
    for key in ["oneOf", "anyOf"] {
        let Some(Value::Array(variants)) = out.get(key).cloned() else {
            continue;
        };
        if variants.is_empty() {
            continue;
        }
        let mut kind: Option<String> = None;
        let mut values = Vec::with_capacity(variants.len());
        let mut flattenable = true;
        for variant in &variants {
            let Some(object) = variant.as_object() else {
                flattenable = false;
                break;
            };
            let Some(variant_type) = object.get("type").and_then(Value::as_str) else {
                flattenable = false;
                break;
            };
            let Some(enum_values) = object.get("enum").and_then(Value::as_array) else {
                flattenable = false;
                break;
            };
            if enum_values.len() != 1
                || object
                    .keys()
                    .any(|name| !matches!(name.as_str(), "type" | "enum" | "description" | "title"))
            {
                flattenable = false;
                break;
            }
            match &kind {
                Some(existing) if existing != variant_type => {
                    flattenable = false;
                    break;
                }
                Some(_) => {}
                None => kind = Some(variant_type.to_string()),
            }
            values.push(enum_values[0].clone());
        }
        if flattenable && let Some(kind) = kind {
            out.remove(key);
            out.entry("type".to_string()).or_insert(Value::String(kind));
            out.entry("enum".to_string())
                .or_insert(Value::Array(values));
        }
    }
}

fn generation_config(model: &str, request: &CompletionRequest) -> Value {
    let mut config = Map::new();
    let is_claude = model.to_ascii_lowercase().contains("claude");
    let thinking_off = matches!(request.thinking_level, Some(ThinkingLevel::Off));
    if let Some(temperature) = request.temperature
        && (!is_claude || thinking_off)
    {
        config.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = request.max_tokens {
        config.insert("maxOutputTokens".into(), json!(max_tokens));
    }
    let mut thinking = Map::new();
    match request.thinking_level {
        Some(ThinkingLevel::Off) => {
            thinking.insert("includeThoughts".into(), Value::Bool(false));
            thinking.insert("thinkingBudget".into(), json!(0));
        }
        Some(level) => {
            thinking.insert("includeThoughts".into(), Value::Bool(true));
            thinking.insert(
                "thinkingLevel".into(),
                Value::String(antigravity_thinking_level(level).to_string()),
            );
            thinking.insert(
                "thinkingBudget".into(),
                json!(antigravity_thinking_budget(level)),
            );
        }
        None => {
            thinking.insert("includeThoughts".into(), Value::Bool(true));
            if let Some(level) = thinking_level(model) {
                thinking.insert("thinkingLevel".into(), Value::String(level.to_string()));
            }
            thinking.insert(
                "thinkingBudget".into(),
                json!(thinking_budget_from_model(model)),
            );
        }
    }
    config.insert("thinkingConfig".into(), Value::Object(thinking));
    Value::Object(config)
}

fn antigravity_thinking_level(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "NONE",
        ThinkingLevel::Minimal => "MINIMAL",
        ThinkingLevel::Low => "LOW",
        ThinkingLevel::Medium => "MEDIUM",
        ThinkingLevel::High => "HIGH",
        ThinkingLevel::XHigh => "XHIGH",
    }
}

fn antigravity_thinking_budget(level: ThinkingLevel) -> u32 {
    match level {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Minimal => 1024,
        ThinkingLevel::Low => 2048,
        ThinkingLevel::Medium => 8192,
        ThinkingLevel::High => 16384,
        ThinkingLevel::XHigh => 32768,
    }
}

fn thinking_budget_from_model(model: &str) -> u32 {
    if model.ends_with("-xhigh") {
        32768
    } else if model.ends_with("-high") {
        16384
    } else if model.ends_with("-medium") {
        8192
    } else if model.ends_with("-low") {
        2048
    } else {
        4096
    }
}

fn thinking_level(model: &str) -> Option<&'static str> {
    if model.ends_with("-xhigh") {
        Some("XHIGH")
    } else if model.ends_with("-high") {
        Some("HIGH")
    } else if model.ends_with("-medium") {
        Some("MEDIUM")
    } else if model.ends_with("-low") {
        Some("LOW")
    } else {
        None
    }
}

fn tool_names(request: &CompletionRequest) -> HashMap<&str, &str> {
    let mut names = HashMap::new();
    for message in &request.messages {
        for call in &message.tool_calls {
            names.insert(call.id.as_str(), call.name.as_str());
        }
    }
    names
}

fn session_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    ((now ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15)) as i64 % i64::MAX).to_string()
}

fn request_id() -> String {
    let cascade = pseudo_uuid();
    let trajectory = pseudo_uuid();
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("agent/{cascade}/{millis}/{trajectory}/1")
}

fn pseudo_uuid() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let first = now ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let second = first.rotate_left(17).wrapping_mul(0xBF58_476D_1CE4_E5B9) ^ counter;
    let raw = format!("{first:016x}{second:016x}");
    format!(
        "{}-{}-{}-{}-{}",
        &raw[0..8],
        &raw[8..12],
        &raw[12..16],
        &raw[16..20],
        &raw[20..32]
    )
}
