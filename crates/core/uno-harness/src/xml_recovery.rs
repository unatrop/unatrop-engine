use protocol::{ContentPart, Message, ToolCall};
use serde_json::{Map, Value};

pub fn has_truncated_tool_call(assistant: &Message) -> bool {
    assistant.content.0.iter().any(|part| {
        let ContentPart::Text { text } = part else {
            return false;
        };
        text.contains("<tool_call>")
            || text.contains("</tool_call>")
            || text.contains("</function>")
            || text.contains("</parameter>")
            || text.contains("<function=")
    })
}

/// Fallback-парсер для open-weight моделей (например Qwen), если на длинном контексте
/// модель выдала `<tool_call><function=...><parameter=...>...</parameter></function></tool_call>`
/// в текстовом ответе вместо нативного JSON `tool_calls`.
pub fn recover_xml_tool_calls(assistant: &mut Message, turn: usize) {
    let full_text: String = assistant
        .content
        .0
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();

    if !full_text.contains("<tool_call>") {
        return;
    }

    let mut recovered = Vec::new();
    let mut cleaned = String::new();
    let mut rest = full_text.as_str();

    while let Some(start) = rest.find("<tool_call>") {
        cleaned.push_str(&rest[..start]);
        let after_start = &rest[start + "<tool_call>".len()..];
        let Some(end) = after_start.find("</tool_call>") else {
            cleaned.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let block = &after_start[..end];
        if let Some(call) = parse_xml_tool_block(block, turn, recovered.len()) {
            recovered.push(call);
        } else {
            cleaned
                .push_str(&rest[start..start + "<tool_call>".len() + end + "</tool_call>".len()]);
        }
        rest = &after_start[end + "</tool_call>".len()..];
    }
    cleaned.push_str(rest);

    if recovered.is_empty() {
        return;
    }

    let trimmed = cleaned.trim();
    assistant.content.0 = if trimmed.is_empty() {
        Vec::new()
    } else {
        vec![ContentPart::text(trimmed)]
    };
    assistant.tool_calls = recovered;
}

fn parse_xml_tool_block(block: &str, turn: usize, idx: usize) -> Option<ToolCall> {
    let fn_start = block.find("<function=")?;
    let after_fn = &block[fn_start + "<function=".len()..];
    let fn_name_end = after_fn.find('>')?;
    let name = after_fn[..fn_name_end].trim();
    if name.is_empty() {
        return None;
    }

    let body = &after_fn[fn_name_end + 1..];
    let body = body.rfind("</function>").map_or(body, |end| &body[..end]);

    let mut params = Map::new();
    let mut cursor = body;
    while let Some(p_start) = cursor.find("<parameter=") {
        let after_p = &cursor[p_start + "<parameter=".len()..];
        let Some(p_name_end) = after_p.find('>') else {
            break;
        };
        let key = after_p[..p_name_end].trim();
        let after_key = &after_p[p_name_end + 1..];
        let Some(p_end) = after_key.find("</parameter>") else {
            break;
        };
        let raw_val = after_key[..p_end].trim();
        if !key.is_empty() {
            let parsed_val = parse_xml_param_value(raw_val);
            params.insert(key.to_string(), parsed_val);
        }
        cursor = &after_key[p_end + "</parameter>".len()..];
    }

    Some(ToolCall {
        id: format!("xml_call_{turn}_{idx}"),
        name: name.to_string(),
        arguments: Value::Object(params),
        signature: None,
    })
}

fn parse_xml_param_value(raw: &str) -> Value {
    if raw == "true" {
        return Value::Bool(true);
    }
    if raw == "false" {
        return Value::Bool(false);
    }
    if let Ok(num) = raw.parse::<i64>() {
        return Value::Number(num.into());
    }
    if ((raw.starts_with('{') && raw.ends_with('}'))
        || (raw.starts_with('[') && raw.ends_with(']')))
        && let Ok(json) = serde_json::from_str::<Value>(raw)
    {
        return json;
    }
    Value::String(raw.to_string())
}
