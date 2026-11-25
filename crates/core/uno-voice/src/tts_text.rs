//! Pure-Rust TTS text chunking, Markdown sanitization, voice-mode prompts, tool announcements,
//! and echo normalization shared across daemon and clients.

/// Localized replacement announcements used when TTS skips tables or long Markdown lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TtsHints<'a> {
    pub table: &'a str,
    pub list: &'a str,
}

impl Default for TtsHints<'static> {
    fn default() -> Self {
        Self::for_locale(None)
    }
}

impl TtsHints<'static> {
    /// Returns localized table/list hints for the given BCP-47/ISO locale (defaults to Russian).
    pub fn for_locale(locale: Option<&str>) -> Self {
        if is_english_locale(locale) {
            Self {
                table: "The table is in the chat, take a look.",
                list: "The full list is in the chat.",
            }
        } else {
            Self {
                table: "Таблица в чате, посмотри.",
                list: "Подробный список в чате.",
            }
        }
    }
}

fn is_english_locale(locale: Option<&str>) -> bool {
    locale
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some_and(|s| !s.to_ascii_lowercase().starts_with("ru"))
}

const MAX_SPOKEN_LIST_ITEMS: usize = 3;

/// Returns the ephemeral voice-mode instruction prepended for the LLM without polluting chat history.
pub fn voice_mode_instruction(locale: Option<&str>) -> &'static str {
    if is_english_locale(locale) {
        concat!(
            "The user is in voice call mode (your reply is spoken aloud via TTS). ",
            "Before calling any tool, first write one brief conversational sentence (3-7 words) ",
            "saying what you are about to do, then invoke the tool. ",
            "In your final reply, lead with a concise 2-3 sentence natural spoken summary ",
            "without reading raw technical names or PIDs aloud, and place detailed lists, tables, ",
            "logs, or code below a `---` horizontal rule (everything after `---` is shown in chat but not spoken)."
        )
    } else {
        concat!(
            "Пользователь общается в голосовом режиме (ответ озвучивается TTS). ",
            "Перед вызовом любого инструмента сначала напиши одну короткую разговорную фразу ",
            "(3–7 слов), что именно ты сейчас сделаешь, и только затем вызывай инструмент. ",
            "В финальном ответе дай короткую естественную устную выжимку (2–3 предложения) ",
            "без перечисления технических имён и PID, а все подробные списки, таблицы, логи и код ",
            "выноси ниже после разделителя `---` (всё после `---` видно в чате, но не зачитывается вслух)."
        )
    }
}

/// Maps an agent tool name into a brief spoken fallback status phrase when a tool runs silently.
pub fn tts_tool_announcement(tool_name: &str, locale: Option<&str>) -> &'static str {
    let lower = tool_name.to_ascii_lowercase();
    let is_en = is_english_locale(locale);
    if matches!(tool_name, "exec_command" | "write_stdin")
        || lower.contains("exec")
        || lower.contains("shell")
        || lower.contains("bash")
        || lower.contains("terminal")
    {
        if is_en {
            "Running a command."
        } else {
            "Выполняю команду."
        }
    } else if matches!(
        tool_name,
        "read_file" | "write_file" | "apply_patch" | "list_files" | "search_text"
    ) || lower.contains("file")
        || lower.contains("grep")
        || lower.starts_with("fs_")
    {
        if is_en {
            "Checking files."
        } else {
            "Проверяю файлы."
        }
    } else if matches!(tool_name, "web_search" | "web_fetch") || lower.starts_with("browser_") {
        if is_en {
            "Looking that up."
        } else {
            "Ищу информацию."
        }
    } else if is_en {
        "Working on it."
    } else {
        "Выполняю действие."
    }
}

/// Normalizes text to lowercase alphanumeric words for loudspeaker-to-mic echo matching.
pub fn normalize_echo_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = true;
    for ch in text.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            out.push(ch);
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Returns `true` if `phrase` looks like acoustic feedback of any recently spoken candidate.
pub fn is_echo_of_spoken(phrase: &str, spoken_candidates: &[&str]) -> bool {
    let norm_phrase = normalize_echo_text(phrase);
    if norm_phrase.chars().count() < 4 {
        return false;
    }
    for &candidate in spoken_candidates {
        if !candidate.is_empty() && normalize_echo_text(candidate).contains(&norm_phrase) {
            return true;
        }
    }
    false
}

/// Extracts complete, TTS-ready sentences from `full_text[offset..]` while skipping code blocks, tables, and long/technical lists.
pub fn extract_ready_sentences(full_text: &str, offset: &mut usize) -> Vec<String> {
    extract_ready_sentences_with_hints(full_text, offset, TtsHints::default())
}

/// Extracts complete, TTS-ready sentences from `full_text[offset..]` with localized table/list hints.
pub fn extract_ready_sentences_with_hints(
    full_text: &str,
    offset: &mut usize,
    hints: TtsHints<'_>,
) -> Vec<String> {
    extract_tts_chunks(full_text, offset, hints, false)
}

/// Flushes the remaining `full_text[offset..]` after streaming completes with localized table/list hints.
pub fn finalize_tts_tail_with_hints(
    full_text: &str,
    offset: &mut usize,
    hints: TtsHints<'_>,
) -> Vec<String> {
    extract_tts_chunks(full_text, offset, hints, true)
}

fn is_table_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed[1..].contains('|')
}

fn is_heading_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ')
}

fn is_list_item_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with("- ")
        || trimmed.starts_with("* ")
        || trimmed.starts_with("+ ")
        || trimmed.starts_with("• ")
    {
        return true;
    }
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && trimmed[digits..].starts_with(". ")
}

fn is_indented_list_item(line: &str) -> bool {
    if !is_list_item_line(line) {
        return false;
    }
    line.starts_with("  ") || line.starts_with('\t')
}

fn is_technical_list_line(line: &str) -> bool {
    if !is_list_item_line(line) {
        return false;
    }
    if is_indented_list_item(line) || line.contains('`') {
        return true;
    }
    let lower = line.to_ascii_lowercase();
    [
        ".exe", ".dll", ".sys", ".rs", ".ts", ".js", ".py", ".json", ".log", "pid ", "(pid", "0x",
        ":\\", "://",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn next_non_empty_line(text: &str) -> Option<&str> {
    text.split_inclusive('\n')
        .find(|line| !line.trim().is_empty())
}

fn has_spoken_prose_before(full_text: &str, offset: usize) -> bool {
    let Some(prefix) = full_text.get(..offset) else {
        return false;
    };
    let mut in_code = false;
    for line in prefix.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code
            || trimmed.is_empty()
            || is_table_line(line)
            || is_horizontal_rule(trimmed)
            || is_heading_line(line)
            || is_list_item_line(line)
        {
            continue;
        }
        if !sanitize_for_tts(line).is_empty() {
            return true;
        }
    }
    false
}

/// Inspects the contiguous list block directly preceding `offset`.
/// Returns `(item_count_in_block, any_prior_item_skipped)`.
fn inspect_prior_list_block(full_text: &str, offset: usize) -> (usize, bool) {
    let Some(prefix) = full_text.get(..offset) else {
        return (0, false);
    };
    let mut lines = Vec::new();
    for line in prefix.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_list_item_line(line) {
            lines.push(line);
        } else {
            lines.clear();
        }
    }

    let mut count = 0usize;
    let mut any_skipped = false;
    for (idx, &line) in lines.iter().enumerate() {
        let next_line = lines.get(idx + 1).copied();
        let category_before_tech = sanitize_for_tts(line).ends_with(':')
            && next_line.is_some_and(|n| is_indented_list_item(n) || is_technical_list_line(n));
        if count >= MAX_SPOKEN_LIST_ITEMS || is_technical_list_line(line) || category_before_tech {
            any_skipped = true;
        }
        count += 1;
    }
    (count, any_skipped)
}

/// Finds a `---` / `***` / `___` horizontal rule after spoken summary prose, cutting off chat-only details.
fn find_details_cutoff_offset(full_text: &str, flush_tail: bool) -> Option<usize> {
    let mut pos = 0usize;
    let mut in_code = false;
    let mut saw_spoken_text = false;

    for line in full_text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            pos += line.len();
            continue;
        }
        if !in_code && !trimmed.is_empty() {
            if is_horizontal_rule(trimmed) {
                if saw_spoken_text && (has_newline || flush_tail) {
                    return Some(pos);
                }
            } else if !is_table_line(line) && !sanitize_for_tts(line).is_empty() {
                saw_spoken_text = true;
            }
        }
        pos += line.len();
    }
    None
}

fn already_announced_table(full_text: &str, offset: usize) -> bool {
    let prefix = full_text.get(..offset).unwrap_or("");
    for line in prefix.lines().rev() {
        let t = line.trim();
        if !t.is_empty() {
            return is_table_line(t);
        }
    }
    false
}

fn find_table_line_start(text: &str) -> Option<usize> {
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        if is_table_line(line) {
            return Some(pos);
        }
        pos += line.len();
    }
    if pos < text.len() && is_table_line(&text[pos..]) {
        return Some(pos);
    }
    None
}

fn extract_tts_chunks(
    full_text: &str,
    offset: &mut usize,
    hints: TtsHints<'_>,
    flush_tail: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    let cutoff = find_details_cutoff_offset(full_text, flush_tail);
    if let Some(limit) = cutoff
        && *offset >= limit
    {
        *offset = full_text.len();
        return out;
    }

    let (active_text, active_flush) = match cutoff {
        Some(limit) => (&full_text[..limit], true),
        None => (full_text, flush_tail),
    };

    let in_code_block = active_text
        .get(..*offset)
        .is_some_and(|prefix| prefix.matches("```").count() % 2 == 1);
    if in_code_block {
        if let Some(rel_end) = active_text.get(*offset..).and_then(|s| s.find("```")) {
            *offset += rel_end + 3;
        } else {
            if active_flush {
                *offset = full_text.len();
            }
            return out;
        }
    }

    while let Some(tail) = active_text.get(*offset..) {
        if tail.is_empty() {
            break;
        }

        let line_end = tail.find('\n').map(|i| i + 1);
        let current_line = line_end.map_or(tail, |end| &tail[..end]);

        if current_line.trim().is_empty() {
            if line_end.is_some() || active_flush {
                *offset += current_line.len();
                continue;
            }
            break;
        }

        if is_table_line(current_line) {
            if line_end.is_none() && !active_flush {
                break;
            }
            if !already_announced_table(active_text, *offset) {
                out.push(hints.table.to_owned());
            }
            let mut consumed = current_line.len();
            while let Some(rest) = tail.get(consumed..) {
                if rest.is_empty() {
                    break;
                }
                let next_end = rest.find('\n').map(|i| i + 1);
                let next_line = next_end.map_or(rest, |end| &rest[..end]);
                if is_table_line(next_line) {
                    if next_end.is_none() && !active_flush {
                        break;
                    }
                    consumed += next_line.len();
                } else {
                    break;
                }
            }
            *offset += consumed;
            continue;
        }

        if is_heading_line(current_line) && has_spoken_prose_before(active_text, *offset) {
            if line_end.is_none() && !active_flush {
                break;
            }
            if current_line.contains('`') {
                *offset += current_line.len();
                continue;
            }
            let after_heading = &tail[current_line.len()..];
            let next_line = next_non_empty_line(after_heading);
            if next_line.is_none() && !active_flush {
                break;
            }
            if let Some(next) = next_line
                && (is_table_line(next)
                    || next.trim_start().starts_with("```")
                    || is_list_item_line(next))
            {
                *offset += current_line.len();
                continue;
            }
        }

        if is_list_item_line(current_line) {
            if line_end.is_none() && !active_flush {
                break;
            }
            let (prior_items, prior_skipped) = inspect_prior_list_block(active_text, *offset);
            let is_category_header = sanitize_for_tts(current_line).ends_with(':');
            let mut skip_list_item = prior_skipped
                || prior_items >= MAX_SPOKEN_LIST_ITEMS
                || is_technical_list_line(current_line);
            if !skip_list_item && is_category_header {
                let after_line = &tail[current_line.len()..];
                let next_line = next_non_empty_line(after_line);
                if next_line.is_none() && !active_flush {
                    break;
                }
                if next_line.is_some_and(|n| is_indented_list_item(n) || is_technical_list_line(n))
                {
                    skip_list_item = true;
                }
            }
            if skip_list_item {
                if !prior_skipped && !has_spoken_prose_before(active_text, *offset) {
                    out.push(hints.list.to_owned());
                }
                *offset += current_line.len();
                continue;
            }
        }

        let code_pos = tail.find("```");
        let table_pos = find_table_line_start(tail);

        let next_special = match (code_pos, table_pos) {
            (Some(c), Some(t)) => Some((c.min(t), c <= t)),
            (Some(c), None) => Some((c, true)),
            (None, Some(t)) => Some((t, false)),
            (None, None) => None,
        };

        if let Some((block_start, is_code)) = next_special {
            let before = &tail[..block_start];
            if let Some(split_idx) = find_sentence_boundary(before) {
                let cleaned = sanitize_for_tts(&before[..split_idx]);
                *offset += split_idx;
                if !cleaned.is_empty() {
                    out.push(cleaned);
                }
                continue;
            }
            let cleaned = sanitize_for_tts(before);
            if !cleaned.is_empty() {
                out.push(cleaned);
            }
            if is_code {
                if let Some(code_end) = tail[block_start + 3..].find("```") {
                    *offset += block_start + 3 + code_end + 3;
                    continue;
                }
                *offset += if active_flush {
                    tail.len()
                } else {
                    block_start
                };
                break;
            } else {
                *offset += block_start;
                continue;
            }
        }

        if let Some(split_idx) = find_sentence_boundary(tail) {
            let cleaned = sanitize_for_tts(&tail[..split_idx]);
            *offset += split_idx;
            if !cleaned.is_empty() {
                out.push(cleaned);
            }
            continue;
        }

        if active_flush {
            let cleaned = sanitize_for_tts(tail);
            *offset = active_text.len();
            if !cleaned.is_empty() {
                out.push(cleaned);
            }
        }
        break;
    }
    if let Some(limit) = cutoff
        && *offset >= limit
    {
        *offset = full_text.len();
    }
    out
}

/// Strips Markdown syntax, links, and decorative symbols so TTS reads clean natural prose.
pub fn sanitize_for_tts(raw: &str) -> String {
    let mut normalized = String::with_capacity(raw.len());
    for line in raw.lines() {
        let mut trimmed = line.trim();
        if trimmed.is_empty() || is_horizontal_rule(trimmed) {
            continue;
        }
        while let Some(rest) = trimmed.strip_prefix('>') {
            trimmed = rest.trim_start();
        }
        let hash_count = trimmed.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hash_count) && trimmed[hash_count..].starts_with(' ') {
            trimmed = trimmed[hash_count + 1..].trim_start();
        }
        for prefix in ["- ", "* ", "+ ", "• "] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                trimmed = rest.trim_start();
                break;
            }
        }
        for task in ["[ ] ", "[x] ", "[X] "] {
            if let Some(rest) = trimmed.strip_prefix(task) {
                trimmed = rest.trim_start();
                break;
            }
        }
        if !trimmed.is_empty() {
            if !normalized.is_empty() {
                normalized.push(' ');
            }
            normalized.push_str(trimmed);
        }
    }

    let cleaned = strip_inline_markdown(&normalized);
    let mut collapsed = String::with_capacity(cleaned.len());
    for word in cleaned.split_whitespace() {
        if !collapsed.is_empty() {
            collapsed.push(' ');
        }
        collapsed.push_str(word);
    }
    if !collapsed.chars().any(char::is_alphanumeric) {
        return String::new();
    }
    collapsed
}

fn is_horizontal_rule(trimmed: &str) -> bool {
    let mut rule_char: Option<char> = None;
    let mut count = 0usize;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            continue;
        }
        if matches!(ch, '-' | '*' | '_') {
            match rule_char {
                Some(prev) if prev != ch => return false,
                None => rule_char = Some(ch),
                _ => {}
            }
            count += 1;
        } else {
            return false;
        }
    }
    count >= 3
}

fn strip_inline_markdown(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'!'
            && let Some((label, end_idx)) = parse_markdown_link(&input[i + 1..])
        {
            out.push_str(label);
            i += 1 + end_idx;
            continue;
        }
        if bytes[i] == b'['
            && let Some((label, end_idx)) = parse_markdown_link(&input[i..])
        {
            out.push_str(label);
            i += end_idx;
            continue;
        }
        if input[i..].starts_with("http://") || input[i..].starts_with("https://") {
            let url_len = input[i..]
                .find(|c: char| c.is_whitespace() || matches!(c, ')' | ']' | '>' | '"'))
                .unwrap_or(input.len() - i);
            i += url_len;
            continue;
        }
        if input[i..].starts_with("**")
            || input[i..].starts_with("__")
            || input[i..].starts_with("~~")
        {
            i += 2;
            continue;
        }
        if matches!(bytes[i], b'`' | b'*') {
            i += 1;
            continue;
        }
        if bytes[i] == b'_' {
            let prev_is_alnum = out.chars().next_back().is_some_and(char::is_alphanumeric);
            let next_is_alnum = input[i + 1..]
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric);
            if prev_is_alnum && next_is_alnum {
                out.push('_');
            }
            i += 1;
            continue;
        }
        if let Some(ch) = input[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn parse_markdown_link(s: &str) -> Option<(&str, usize)> {
    let rest = s.strip_prefix('[')?;
    let label_end = rest.find(']')?;
    let after_label = &rest[label_end + 1..];
    let url_part = after_label.strip_prefix('(')?;
    let url_end = url_part.find(')')?;
    let consumed = 1 + label_end + 1 + 1 + url_end + 1;
    Some((&rest[..label_end], consumed))
}

fn is_markdown_line_start(trimmed_line: &str) -> bool {
    if trimmed_line.starts_with('#')
        || trimmed_line.starts_with("- ")
        || trimmed_line.starts_with("* ")
        || trimmed_line.starts_with("+ ")
        || trimmed_line.starts_with("• ")
        || trimmed_line.starts_with("> ")
    {
        return true;
    }
    let digits = trimmed_line.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && trimmed_line[digits..].starts_with(". ")
}

fn find_sentence_boundary(text: &str) -> Option<usize> {
    const MIN_SENTENCE_BYTES: usize = 8;
    const MIN_CLAUSE_BYTES: usize = 48;
    const MIN_LINE_BYTES: usize = 20;
    const MIN_SOFT_LIMIT_BYTES: usize = 100;
    const MAX_CHUNK_BYTES: usize = 220;

    let mut line_start = 0usize;
    for (idx, ch) in text.char_indices() {
        let end = idx + ch.len_utf8();
        if ch == '\n' {
            if text[end..].starts_with('\n') {
                return Some(end + 1);
            }
            let current_line = &text[line_start..idx];
            if !text[..idx].trim().is_empty()
                && (idx >= MIN_LINE_BYTES || is_markdown_line_start(current_line.trim_start()))
            {
                return Some(end);
            }
            line_start = end;
            continue;
        }
        if end < MIN_SENTENCE_BYTES {
            continue;
        }
        if matches!(ch, '.' | '!' | '?') && text[end..].starts_with(|c: char| c.is_whitespace()) {
            return Some(end);
        }
        if end >= MIN_CLAUSE_BYTES
            && matches!(ch, ';' | ':')
            && text[end..].starts_with(|c: char| c.is_whitespace())
        {
            return Some(end);
        }
    }

    if text.len() >= MAX_CHUNK_BYTES {
        let mut limit = MAX_CHUNK_BYTES;
        while limit > 0 && !text.is_char_boundary(limit) {
            limit -= 1;
        }
        let window = &text[..limit];
        let mut best_punct: Option<usize> = None;
        let mut best_space: Option<usize> = None;
        for (idx, ch) in window.char_indices() {
            let end = idx + ch.len_utf8();
            if end < MIN_SOFT_LIMIT_BYTES {
                continue;
            }
            if matches!(ch, ',' | '—' | ';' | ':')
                && window[end..].starts_with(|c: char| c.is_whitespace())
            {
                best_punct = Some(end);
            } else if ch.is_whitespace() {
                best_space = Some(end);
            }
        }
        if let Some(split) = best_punct.or(best_space) {
            return Some(split);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_markdown_tables_and_announces_once() {
        let full =
            "Вот сравнение.\n| Имя | Значение |\n| --- | --- |\n| A | 1 |\n| B | 2 |\nГотово.";
        let hints = TtsHints::for_locale(Some("ru-RU"));
        let mut offset = 0;

        let part1 = extract_ready_sentences_with_hints(&full[..35], &mut offset, hints);
        assert_eq!(part1, vec!["Вот сравнение."]);

        let part2 = extract_ready_sentences_with_hints(&full[..60], &mut offset, hints);
        assert_eq!(part2, vec![hints.table]);

        let part3 = finalize_tts_tail_with_hints(full, &mut offset, hints);
        assert_eq!(part3, vec!["Готово."]);
    }

    #[test]
    fn splits_markdown_headings_and_bullet_lists_while_streaming() {
        let streamed =
            "### Краткий план\n- Открыть проект\n- Проверить тесты\n- Перезапустить сервер\nГотово";
        let hints = TtsHints::for_locale(Some("ru"));
        let mut offset = 0;

        let chunks = extract_ready_sentences_with_hints(streamed, &mut offset, hints);
        assert_eq!(
            chunks,
            vec![
                "Краткий план",
                "Открыть проект",
                "Проверить тесты",
                "Перезапустить сервер",
            ]
        );
        let tail = finalize_tts_tail_with_hints(streamed, &mut offset, hints);
        assert_eq!(tail, vec!["Готово"]);
    }

    #[test]
    fn stops_tts_at_horizontal_rule_after_summary() {
        let full = "Больше всего памяти занимает rust-analyzer.\n---\n- `rust-analyzer.exe` (PID 3500) — 4,45 ГБ\n- `msedge.exe` — 533 МБ";
        let hints = TtsHints::for_locale(Some("ru"));
        let mut offset = 0;

        let part1 = extract_ready_sentences_with_hints(full, &mut offset, hints);
        let part2 = finalize_tts_tail_with_hints(full, &mut offset, hints);
        let all: Vec<String> = part1.into_iter().chain(part2).collect();
        assert_eq!(all, vec!["Больше всего памяти занимает rust-analyzer."]);
        assert_eq!(offset, full.len());
    }

    #[test]
    fn skips_technical_process_list_after_summary_without_hr() {
        let full = concat!(
            "Сейчас запущены Zed, Edge и Unatrop. ",
            "Больше всего памяти занимает rust-analyzer — около 4,5 ГБ.\n\n",
            "### Основные запущенные процессы (`tasklist`)\n\n",
            "- **Разработка и приложения:**\n",
            "  - `rust-analyzer.exe` (PID 3500) — ~4,45 ГБ\n",
            "  - `msedge.exe` (до ~533 МБ)\n",
            "  - `unatropd.exe` (PID 13356) — ~292 МБ\n",
            "- **Системные и фоновые процессы:**\n",
            "  - `audiodg.exe` — ~281 МБ\n",
            "  - `dwm.exe` — ~151 МБ"
        );
        let hints = TtsHints::for_locale(Some("ru"));
        let mut offset = 0;

        let split = full
            .char_indices()
            .find(|&(i, _)| i >= 200)
            .map_or(full.len(), |(i, _)| i);
        let part1 = extract_ready_sentences_with_hints(&full[..split], &mut offset, hints);
        let part2 = finalize_tts_tail_with_hints(full, &mut offset, hints);
        let all: Vec<String> = part1.into_iter().chain(part2).collect();
        assert_eq!(
            all,
            vec![
                "Сейчас запущены Zed, Edge и Unatrop.",
                "Больше всего памяти занимает rust-analyzer — около 4,5 ГБ.",
            ]
        );
    }

    #[test]
    fn skips_long_list_and_announces_hint_when_no_summary() {
        let full = concat!(
            "- `rust-analyzer.exe` (PID 3500)\n",
            "- `msedge.exe` (PID 100)\n",
            "- `unatropd.exe` (PID 200)\n",
            "- `audiodg.exe` (PID 300)"
        );
        let hints = TtsHints::for_locale(Some("ru"));
        let mut offset = 0;

        let split = full
            .char_indices()
            .find(|&(i, _)| i >= 60)
            .map_or(full.len(), |(i, _)| i);
        let part1 = extract_ready_sentences_with_hints(&full[..split], &mut offset, hints);
        let part2 = finalize_tts_tail_with_hints(full, &mut offset, hints);
        let all: Vec<String> = part1.into_iter().chain(part2).collect();
        assert_eq!(all, vec![hints.list]);
    }

    #[test]
    fn sanitizes_markdown_formatting_for_tts() {
        let raw = "## **Важно**: проверь [документацию](https://example.com) и `foo_bar`\n---";
        assert_eq!(
            sanitize_for_tts(raw),
            "Важно: проверь документацию и foo_bar"
        );
    }

    #[test]
    fn splits_long_unpunctuated_text_at_soft_limit() {
        let long = "Это очень длинное объяснение результата работы утилиты без единой точки в конце предложения которое продолжает идти дальше, чтобы проверить мягкое разбиение по запятой или пробелу до завершения генерации всего ответа целиком и ещё немного текста сверху";
        let hints = TtsHints::for_locale(Some("ru"));
        let mut offset = 0;
        let chunks = extract_ready_sentences_with_hints(long, &mut offset, hints);
        assert!(!chunks.is_empty());
        assert!(offset < long.len());
    }

    #[test]
    fn tool_announcements_and_echo_matching_work_across_locales() {
        assert_eq!(
            tts_tool_announcement("exec_command", Some("ru-RU")),
            "Выполняю команду."
        );
        assert_eq!(
            tts_tool_announcement("read_file", Some("en-US")),
            "Checking files."
        );
        assert_eq!(
            tts_tool_announcement("browser_navigate", None),
            "Ищу информацию."
        );
        assert!(is_echo_of_spoken(
            "Запущен rust analyzer",
            &["Сейчас запущен rust-analyzer и браузер Edge."]
        ));
        assert!(!is_echo_of_spoken("да", &["Да, конечно."]));
    }
}
