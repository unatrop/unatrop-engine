//! Secret redaction helpers for persisted session logs and error messages.

use crate::{ContentPart, Message};

const PREFIXES: &[&str] = &[
    "Bearer ", "bearer ", "ghp_", "gho_", "ghu_", "ghs_", "ya29.", "GOCSPX-", "AKIA", "sk-proj-",
    "sk-ant-", "sk-",
];

fn is_secret_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+' | b'/' | b'=')
}

/// Masks common API keys, bearer tokens, and OAuth credentials inside arbitrary text.
pub fn redact_secrets(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;

    while i < bytes.len() {
        if let Some(prefix) = PREFIXES.iter().copied().find(|p| input[i..].starts_with(p)) {
            let value_start = i + prefix.len();
            let mut value_end = value_start;
            while value_end < bytes.len() && is_secret_char(bytes[value_end]) {
                value_end += 1;
            }
            if value_end.saturating_sub(value_start) >= 12 {
                if prefix.eq_ignore_ascii_case("Bearer ") {
                    out.push_str(&input[i..value_start]);
                    out.push_str("[REDACTED]");
                } else {
                    out.push_str(prefix);
                    out.push_str("[REDACTED]");
                }
                i = value_end;
                continue;
            }
        }

        if input[i..].starts_with("1//") {
            let value_start = i + 3;
            let mut value_end = value_start;
            while value_end < bytes.len() && is_secret_char(bytes[value_end]) {
                value_end += 1;
            }
            if value_end.saturating_sub(value_start) >= 16 {
                out.push_str("1//[REDACTED]");
                i = value_end;
                continue;
            }
        }

        let ch = input[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Returns a copy of `messages` with common secret tokens masked in content and tool calls.
pub fn redact_message_secrets(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|msg| {
            let mut cloned = msg.clone();
            for part in &mut cloned.content.0 {
                match part {
                    ContentPart::Text { text } | ContentPart::Reasoning { text } => {
                        *text = redact_secrets(text);
                    }
                    ContentPart::ImageUrl { .. } => {}
                }
            }
            for call in &mut cloned.tool_calls {
                if let Ok(serialized) = serde_json::to_string(&call.arguments) {
                    let redacted = redact_secrets(&serialized);
                    if redacted != serialized
                        && let Ok(val) = serde_json::from_str(&redacted)
                    {
                        call.arguments = val;
                    }
                }
            }
            cloned
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::redact_secrets;

    #[test]
    fn redacts_bearer_and_provider_tokens_without_touching_normal_text() {
        let raw = "auth failed: Bearer secret_token_123456 and key sk-proj-abcdefghijklmnop";
        let out = redact_secrets(raw);
        assert!(!out.contains("secret_token_123456"));
        assert!(!out.contains("abcdefghijklmnop"));
        assert!(out.contains("Bearer [REDACTED]"));
        assert!(out.contains("sk-proj-[REDACTED]"));
        assert_eq!(
            redact_secrets("plain status message"),
            "plain status message"
        );
    }
}
