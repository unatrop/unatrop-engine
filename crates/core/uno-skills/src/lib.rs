//! `SKILL.md` discovery, prompt formatting, and on-demand `read_skill` tool.

use std::{collections::BTreeMap, fs, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use protocol::ToolDefinition;
use serde_json::{Value, json};
use tool_core::{Approval, Tool, ToolError, ToolOutput};

/// Discovered `SKILL.md` entry.
#[derive(Clone, Debug)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub content: String,
}

/// Discovers `SKILL.md` files across the provided skill root directories (later dirs override earlier ones by name).
pub fn discover_skills(skill_dirs: &[PathBuf]) -> Vec<SkillEntry> {
    let mut by_name: BTreeMap<String, SkillEntry> = BTreeMap::new();
    for skills_dir in skill_dirs {
        let Ok(entries) = fs::read_dir(skills_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let skill_file = dir.join("SKILL.md");
            let Ok(content) = fs::read_to_string(&skill_file) else {
                continue;
            };
            if content.trim().is_empty() {
                continue;
            }
            let fallback_name = dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("skill")
                .to_owned();
            let (name, description) = parse_frontmatter(&content, &fallback_name);
            by_name.insert(
                name.clone(),
                SkillEntry {
                    name,
                    description,
                    path: skill_file,
                    content,
                },
            );
        }
    }

    by_name.into_values().collect()
}

fn parse_frontmatter(content: &str, fallback_name: &str) -> (String, String) {
    let trimmed = content.trim_start();
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;

    if let Some(after_start) = trimmed.strip_prefix("---")
        && let Some(end_idx) = after_start.find("\n---")
    {
        let fm = &after_start[..end_idx];
        for line in fm.lines() {
            let line = line.trim();
            if let Some(val) = line.strip_prefix("name:") {
                let v = val.trim().trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    name = Some(v.to_owned());
                }
            } else if let Some(val) = line.strip_prefix("description:") {
                let v = val.trim().trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    description = Some(v.to_owned());
                }
            }
        }
    }

    let resolved_name = name.unwrap_or_else(|| fallback_name.to_owned());
    let resolved_desc = description.unwrap_or_else(|| {
        content
            .lines()
            .find(|l| {
                let t = l.trim();
                !t.is_empty() && t != "---" && !t.starts_with('#')
            })
            .unwrap_or("No description")
            .trim()
            .to_owned()
    });
    (resolved_name, resolved_desc)
}

/// Formats the `# Available Skills` system prompt section.
pub fn format_skills_section(skills: &[SkillEntry]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut out = String::from(
        "# Available Skills\n\
         Call `read_skill` with `{\"name\": \"<name>\"}` to load full instructions before executing a matching task:\n",
    );
    for skill in skills {
        out.push_str(&format!("- `{}`: {}\n", skill.name, skill.description));
    }
    Some(out)
}

/// Read-only tool that loads a discovered `SKILL.md` on demand.
pub struct ReadSkillTool {
    skills: Arc<BTreeMap<String, SkillEntry>>,
}

impl ReadSkillTool {
    pub fn new(skills: Vec<SkillEntry>) -> Self {
        let map = skills
            .into_iter()
            .map(|s| (s.name.clone(), s))
            .collect::<BTreeMap<_, _>>();
        Self {
            skills: Arc::new(map),
        }
    }
}

#[async_trait]
impl Tool for ReadSkillTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_skill".to_owned(),
            description: "Load the full SKILL.md instructions for an available skill by name."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Exact skill name from the Available Skills list."
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    fn approval(&self, _arguments: &Value) -> Approval {
        Approval::Never
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let name = arguments
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let Some(skill) = self.skills.get(name) else {
            let available = self.skills.keys().cloned().collect::<Vec<_>>().join(", ");
            return Ok(ToolOutput::error(format!(
                "Skill '{name}' not found. Available: {available}"
            )));
        };
        Ok(ToolOutput::text(format!(
            "# Skill: {}\nPath: {}\n\n{}",
            skill.name,
            skill.path.display(),
            skill.content
        )))
    }
}
