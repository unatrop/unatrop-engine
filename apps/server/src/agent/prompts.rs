//! Layered system prompt assembly for `uno-daemon` sessions.

use std::{fs, io::ErrorKind, path::Path};

use uno_harness::{SystemPromptBuilder, section_order};
use uno_skills as skills;

use crate::server::DaemonError;

const PROJECT_INSTRUCTION_FILE: &str = "UNATROP.md";
const SYSTEM_PROMPT_FILE: &str = "system.md";
const PERSONA_PROMPT_FILE: &str = "persona.md";
const MAX_USER_INSTRUCTION_CHARS: usize = 8_000;
const DEFAULT_SYSTEM_PROMPT_MD: &str = include_str!("../../prompts/system.md");

fn load_system_prompt(prompts_dir: Option<&Path>) -> Result<String, DaemonError> {
    let Some(dir) = prompts_dir else {
        return Ok(DEFAULT_SYSTEM_PROMPT_MD.to_owned());
    };

    let path = dir.join(SYSTEM_PROMPT_FILE);
    match fs::read_to_string(&path) {
        Ok(content) if !content.trim().is_empty() => Ok(content),
        Ok(_) => Ok(DEFAULT_SYSTEM_PROMPT_MD.to_owned()),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let _ = fs::create_dir_all(dir);
            let _ = fs::write(&path, DEFAULT_SYSTEM_PROMPT_MD);
            Ok(DEFAULT_SYSTEM_PROMPT_MD.to_owned())
        }
        Err(error) => Err(DaemonError::internal(error.to_string())),
    }
}

fn truncate_instruction(content: String) -> String {
    if content.len() <= MAX_USER_INSTRUCTION_CHARS {
        return content;
    }
    let mut end = MAX_USER_INSTRUCTION_CHARS;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n...[truncated to save prompt budget]", &content[..end])
}

fn load_optional_file(path: &Path) -> Result<Option<String>, DaemonError> {
    match fs::read_to_string(path) {
        Ok(content) if !content.trim().is_empty() => Ok(Some(truncate_instruction(content))),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(DaemonError::internal(error.to_string())),
    }
}

fn family_addendum(model: &str) -> &'static str {
    let lower = model.to_ascii_lowercase();
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    if base.starts_with("gpt")
        || (base.starts_with('o') && base.as_bytes().get(1).is_some_and(u8::is_ascii_digit))
    {
        "Prefer `apply_patch` over rewriting full files. Keep preambles to one sentence."
    } else if base.starts_with("claude") || lower.starts_with("anthropic/") {
        "Batch independent read-only tool calls in parallel."
    } else if base.starts_with("gemini") {
        "Send tool arguments as plain JSON values; do not wrap numbers or booleans in quotes."
    } else {
        "Call tools only through the native function-calling interface; never emit XML or JSON tool calls in text."
    }
}

/// Builds the layered system prompt (`system.md`, `persona.md`, `UNATROP.md`, skills, environment).
pub(crate) fn build_system_prompt(
    prompts_dir: Option<&Path>,
    workspace: &Path,
    model: &str,
    discovered_skills: &[skills::SkillEntry],
) -> Result<String, DaemonError> {
    let mut builder = SystemPromptBuilder::new();
    builder.set_section(
        "system",
        section_order::HARNESS_IDENTITY,
        load_system_prompt(prompts_dir)?,
    );

    if let Some(dir) = prompts_dir
        && let Some(persona) = load_optional_file(&dir.join(PERSONA_PROMPT_FILE))?
    {
        builder.set_section("persona", section_order::PERSONA_PREFIX, persona);
    }

    if let Some(project) = load_optional_file(&workspace.join(PROJECT_INSTRUCTION_FILE))? {
        builder.set_section(
            "project_instructions",
            section_order::WORKSPACE_CONTEXT,
            project,
        );
    }

    if let Some(skills_section) = skills::format_skills_section(discovered_skills) {
        builder.set_section(
            "skills",
            section_order::WORKSPACE_CONTEXT + 2,
            skills_section,
        );
    }

    builder.set_section(
        "environment",
        section_order::PERSONA_SUFFIX,
        format!(
            "# Environment\n- OS: {}\n- Workspace: {}\n- Model note: {}",
            std::env::consts::OS,
            workspace.display(),
            family_addendum(model),
        ),
    );

    Ok(builder.build())
}
