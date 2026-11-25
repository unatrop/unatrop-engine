/// Извлечённый из HTML заголовок и очищенный Markdown/текст
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedHtml {
    pub title: Option<String>,
    pub text: String,
}

/// Однопроходно преобразует HTML в компактный Markdown/текст, удаляя `<script>`, `<style>`, `<svg>`, `<noscript>`, `<nav>`, `<footer>`, `<aside>`
pub fn html_to_markdown(html: &str) -> ExtractedHtml {
    let mut out = String::with_capacity(html.len() / 3);
    let mut title = None;
    let mut skip_tag: Option<(&'static str, usize)> = None;
    let mut in_title = false;
    let mut title_buf = String::new();
    let mut in_pre = false;
    let mut last_was_space = true;

    let bytes = html.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'<' {
            let Some(rel_end) = html[i..].find('>') else {
                break;
            };
            let tag_raw = &html[i + 1..i + rel_end];
            i += rel_end + 1;

            let tag_trimmed = tag_raw.trim();
            if tag_trimmed.starts_with("!--") {
                continue;
            }
            let is_closing = tag_trimmed.starts_with('/');
            let is_self_closing = tag_trimmed.ends_with('/');
            let name_part = tag_trimmed
                .trim_start_matches('/')
                .split(|c: char| c.is_ascii_whitespace() || c == '/')
                .next()
                .unwrap_or("");
            let tag = name_part.to_ascii_lowercase();

            if let Some((skip, depth)) = skip_tag {
                if is_closing && tag == skip {
                    if depth <= 1 {
                        skip_tag = None;
                    } else {
                        skip_tag = Some((skip, depth - 1));
                    }
                } else if !is_closing && !is_self_closing && tag == skip {
                    skip_tag = Some((skip, depth + 1));
                }
                continue;
            }

            if !is_closing {
                match tag.as_str() {
                    "script" if !is_self_closing => skip_tag = Some(("script", 1)),
                    "style" if !is_self_closing => skip_tag = Some(("style", 1)),
                    "noscript" if !is_self_closing => skip_tag = Some(("noscript", 1)),
                    "svg" if !is_self_closing => skip_tag = Some(("svg", 1)),
                    "nav" if !is_self_closing => skip_tag = Some(("nav", 1)),
                    "footer" if !is_self_closing => skip_tag = Some(("footer", 1)),
                    "aside" if !is_self_closing => skip_tag = Some(("aside", 1)),
                    "title" => {
                        in_title = true;
                        title_buf.clear();
                    }
                    "h1" => push_newline_prefix(&mut out, "# ", &mut last_was_space),
                    "h2" => push_newline_prefix(&mut out, "## ", &mut last_was_space),
                    "h3" => push_newline_prefix(&mut out, "### ", &mut last_was_space),
                    "h4" | "h5" | "h6" => {
                        push_newline_prefix(&mut out, "#### ", &mut last_was_space)
                    }
                    "li" => push_newline_prefix(&mut out, "- ", &mut last_was_space),
                    "br" | "p" | "div" | "section" | "article" | "tr" | "blockquote" => {
                        push_newline(&mut out, &mut last_was_space);
                    }
                    "pre" => {
                        push_newline(&mut out, &mut last_was_space);
                        out.push_str("```\n");
                        in_pre = true;
                        last_was_space = true;
                    }
                    _ => {}
                }
            } else {
                match tag.as_str() {
                    "title" => {
                        in_title = false;
                        let cleaned = decode_entities(title_buf.trim());
                        if !cleaned.is_empty() && title.is_none() {
                            title = Some(cleaned);
                        }
                    }
                    "pre" => {
                        if in_pre {
                            if !out.ends_with('\n') {
                                out.push('\n');
                            }
                            out.push_str("```\n");
                            in_pre = false;
                            last_was_space = true;
                        }
                    }
                    "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "li" | "tr" => {
                        push_newline(&mut out, &mut last_was_space);
                    }
                    _ => {}
                }
            }
            continue;
        }

        let next_tag = html[i..].find('<').map_or(html.len(), |p| i + p);
        let chunk = &html[i..next_tag];
        i = next_tag;

        if skip_tag.is_some() {
            continue;
        }
        if in_title {
            title_buf.push_str(chunk);
            continue;
        }

        let decoded = decode_entities(chunk);
        if in_pre {
            out.push_str(&decoded);
            last_was_space = decoded.ends_with(char::is_whitespace);
        } else {
            for ch in decoded.chars() {
                if ch.is_whitespace() {
                    if !last_was_space {
                        out.push(' ');
                        last_was_space = true;
                    }
                } else {
                    out.push(ch);
                    last_was_space = false;
                }
            }
        }
    }

    let cleaned = collapse_blank_lines(&out);
    ExtractedHtml {
        title,
        text: cleaned,
    }
}

fn push_newline(out: &mut String, last_was_space: &mut bool) {
    while out.ends_with(' ') {
        out.pop();
    }
    if !out.is_empty() && !out.ends_with("\n\n") {
        out.push('\n');
    }
    *last_was_space = true;
}

fn push_newline_prefix(out: &mut String, prefix: &str, last_was_space: &mut bool) {
    push_newline(out, last_was_space);
    out.push_str(prefix);
    *last_was_space = true;
}

fn collapse_blank_lines(s: &str) -> String {
    let mut res = String::with_capacity(s.len());
    let mut blank_run = 0usize;
    for line in s.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !res.is_empty() {
                res.push('\n');
            }
        } else {
            blank_run = 0;
            res.push_str(trimmed);
            res.push('\n');
        }
    }
    res.trim().to_owned()
}

fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input.as_bytes()[i] == b'&'
            && let Some(semi) = input[i..].find(';')
            && semi <= 12
        {
            let entity = &input[i + 1..i + semi];
            if let Some(ch) = decode_one_entity(entity) {
                out.push(ch);
                i += semi + 1;
                continue;
            }
        }
        let ch = input[i..].chars().next().unwrap_or_default();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn decode_one_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" | "#39" => Some('\''),
        "nbsp" => Some(' '),
        _ if entity.starts_with("#x") || entity.starts_with("#X") => {
            u32::from_str_radix(&entity[2..], 16)
                .ok()
                .and_then(char::from_u32)
        }
        _ if entity.starts_with('#') => entity[1..].parse::<u32>().ok().and_then(char::from_u32),
        _ => None,
    }
}
