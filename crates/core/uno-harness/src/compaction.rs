use protocol::{ContentPart, Message, Role, ToolDefinition};
use serde_json::Value;
use tool_core::ToolOutput;

pub const RECENT_FULL_MESSAGES: usize = 4;
const COMPACT_TOOL_CHAR_LIMIT: usize = 1_200;
const COMPACT_TOOL_ARG_CHAR_LIMIT: usize = 400;
/// Максимальный размер одного ответа инструмента, сохраняемого в историю (~6 KiB)
pub const MAX_TOOL_OUTPUT_HISTORY_CHARS: usize = 6 * 1024;
/// Порог пакетной компактификации контекста по умолчанию в токенах (~12k)
pub const DEFAULT_CONTEXT_CAP_TOKENS: u64 = 12_000;
/// Оценка стоимости одного изображения в токенах (консервативно)
const IMAGE_TOKEN_ESTIMATE: usize = 1_024;

/// Оценивает число токенов в тексте: ASCII ≈ 4 символа/токен, CJK ≈ 1, кириллица/прочий не-ASCII ≈ 1.5
pub fn estimate_text_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut ascii = 0usize;
    let mut wide = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else if is_wide_char(ch) {
            wide += 1;
        } else {
            other += 1;
        }
    }
    ascii.div_ceil(4) + wide + (other * 2).div_ceil(3)
}

fn is_wide_char(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x3FFFD)
}

pub fn tool_result_content(output: &ToolOutput) -> String {
    let raw = if output.is_error {
        format!("Tool error: {}", output.content)
    } else {
        output.content.clone()
    };
    if raw.len() > MAX_TOOL_OUTPUT_HISTORY_CHARS {
        truncate_head_tail(&raw, 2_000, 3_500)
    } else {
        raw
    }
}

/// Маскирует старые крупные `Role::Tool` результаты при достижении 40% бюджета контекста
/// (без LLM-вызова и без удаления сообщений), чтобы экономить токены в длинных сессиях.
pub(crate) fn mask_stale_tool_results(
    messages: &mut [Message],
    tools: &[protocol::ToolDefinition],
    last_input_tokens: Option<u32>,
    context_cap_tokens: u64,
) -> bool {
    if messages.len() <= RECENT_FULL_MESSAGES {
        return false;
    }
    let mask_threshold = (context_cap_tokens * 2 / 5).max(1_024);
    let over_threshold = last_input_tokens.is_some_and(|t| u64::from(t) >= mask_threshold)
        || (estimate_messages_tokens(messages) + estimate_tools_tokens(tools)) as u64
            >= mask_threshold;
    if !over_threshold {
        return false;
    }
    let mask_until = messages.len() - RECENT_FULL_MESSAGES;
    let mut changed = false;
    for msg in messages.iter_mut().take(mask_until) {
        if msg.role != Role::Tool {
            continue;
        }
        for part in &mut msg.content.0 {
            if let ContentPart::Text { text } = part
                && text.len() > COMPACT_TOOL_CHAR_LIMIT
            {
                *text = truncate_head_tail(text, 250, 350);
                changed = true;
            }
        }
    }
    changed
}

/// Пакетная компактификация контекста в стиле `codex-rs` / `jcode`.
///
/// Кэш-дружелюбная стратегия: уже отправленная история не переписывается. При нехватке
/// бюджета вытесняется целый непрерывный блок старых ходов (по границе `User`), а на его
/// место вставляется одно короткое summary-сообщение. Поэтому KV-префикс меняется только
/// в момент компакции, а не на каждом ходе.
pub fn compact_batch(messages: &mut Vec<Message>, context_cap_tokens: u64) -> Vec<Message> {
    let target_budget = (context_cap_tokens * 60 / 100).max(1_024);
    enforce_context_budget(messages, target_budget)
}

/// Сжимает тяжёлые аргументы инструментов (`content`/`patch`) до плейсхолдера,
/// чтобы они не раздували историю при каждом последующем запросе.
pub fn trim_tool_call_arguments(msg: &mut Message) {
    for call in &mut msg.tool_calls {
        if let Value::Object(map) = &mut call.arguments {
            for key in ["content", "patch", "chars"] {
                if let Some(Value::String(val)) = map.get_mut(key)
                    && val.len() > COMPACT_TOOL_ARG_CHAR_LIMIT
                {
                    *val = format!(
                        "<omitted-{key} bytes=\"{}\" note=\"use read_file if needed; do not copy this placeholder\" />",
                        val.len()
                    );
                }
            }
        }
    }
}

fn enforce_context_budget(messages: &mut Vec<Message>, max_tokens: u64) -> Vec<Message> {
    let mut drained = Vec::new();
    if estimate_messages_tokens(messages) as u64 <= max_tokens {
        return drained;
    }

    let prefix_len = protected_prefix_len(messages);

    // Оставляем минимум 2 последних сообщения, вытесняя старые ходы целыми блоками.
    while estimate_messages_tokens(messages) as u64 > max_tokens && messages.len() > prefix_len + 2
    {
        let drain_end = stable_drain_end(messages, prefix_len).min(messages.len() - 2);
        if drain_end <= prefix_len {
            break;
        }
        drained.extend(messages.drain(prefix_len..drain_end));
    }

    if !drained.is_empty() {
        messages.insert(
            prefix_len,
            Message::system(
                "[Earlier intermediate steps were compacted to stay within the token budget.]",
            ),
        );
    }

    drained
}

/// Защищённый префикс: начальные System/Developer-сообщения + первый User-запрос.
fn protected_prefix_len(messages: &[Message]) -> usize {
    let mut prefix_len = 0usize;
    while prefix_len < messages.len()
        && matches!(messages[prefix_len].role, Role::System | Role::Developer)
    {
        prefix_len += 1;
    }
    if prefix_len < messages.len() && messages[prefix_len].role == Role::User {
        prefix_len += 1;
    }
    prefix_len
}

/// Конец непрерывного блока старых ходов, который можно вытеснить, не разрывая
/// пары `Assistant`/`Tool` и начиная сохранённую историю заново с `User`-хода.
fn stable_drain_end(messages: &[Message], prefix_len: usize) -> usize {
    if let Some(boundary) =
        (prefix_len + 1..messages.len()).find(|&i| messages[i].role == Role::User)
    {
        return boundary;
    }
    pair_safe_end(messages, prefix_len)
}

fn pair_safe_end(messages: &[Message], prefix_len: usize) -> usize {
    if prefix_len >= messages.len() {
        return prefix_len;
    }
    if messages[prefix_len].role == Role::Assistant && !messages[prefix_len].tool_calls.is_empty() {
        let mut n = 1;
        while prefix_len + n < messages.len() && messages[prefix_len + n].role == Role::Tool {
            n += 1;
        }
        prefix_len + n
    } else {
        prefix_len + 1
    }
}

/// Решает, нужна ли компактификация. Приоритет — реальные `input_tokens` от провайдера,
/// оценка по коэффициенту (ASCII/CJK) используется только как фолбэк.
pub(crate) fn should_compact(
    messages: &[Message],
    tools: &[ToolDefinition],
    last_input_tokens: Option<u32>,
    context_cap_tokens: u64,
) -> bool {
    if let Some(input_tokens) = last_input_tokens
        && u64::from(input_tokens) >= context_cap_tokens
    {
        return true;
    }
    estimate_messages_tokens(messages).saturating_add(estimate_tools_tokens(tools)) as u64
        > context_cap_tokens
}

pub(crate) fn estimate_tools_tokens(tools: &[ToolDefinition]) -> usize {
    tools
        .iter()
        .map(|t| {
            estimate_text_tokens(&t.name)
                + estimate_text_tokens(&t.description)
                + serde_json::to_string(&t.input_schema)
                    .map(|s| estimate_text_tokens(&s))
                    .unwrap_or(0)
                + 8
        })
        .sum()
}

pub fn estimate_messages_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| {
            let text_len: usize = m
                .content
                .0
                .iter()
                .map(|p| match p {
                    ContentPart::Text { text } | ContentPart::Reasoning { text } => {
                        estimate_text_tokens(text)
                    }
                    ContentPart::ImageUrl { .. } => IMAGE_TOKEN_ESTIMATE,
                })
                .sum();
            let calls_len: usize = m
                .tool_calls
                .iter()
                .map(|c| {
                    estimate_text_tokens(&c.name) + estimate_text_tokens(&c.arguments.to_string())
                })
                .sum();
            text_len + calls_len + 8
        })
        .sum()
}

fn truncate_head_tail(text: &str, head_max: usize, tail_max: usize) -> String {
    let mut head_end = head_max.min(text.len());
    while head_end > 0 && !text.is_char_boundary(head_end) {
        head_end -= 1;
    }

    let mut tail_start = text.len().saturating_sub(tail_max);
    while tail_start < text.len() && !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }

    if head_end >= tail_start {
        return text.to_string();
    }

    let omitted = tail_start - head_end;
    format!(
        "{}\n... [earlier tool output compacted: {omitted} bytes omitted] ...\n{}",
        &text[..head_end],
        &text[tail_start..]
    )
}
