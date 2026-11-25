use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Approval, is_known_safe_command};

/// Режим политики подтверждения вызовов инструментов (в стиле Antigravity)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Спрашивает подтверждение для инструментов записи/shell/UI, если они не в белом списке
    #[default]
    Default,
    /// Разрешает только инструменты чтения и безопасные read-only команды; остальные отклоняются
    ReadOnly,
    /// Автоматически разрешает изменения файлов в рабочей области (`write_file`, `apply_patch`), спрашивает про `shell` и `ui_action`
    AutoApproveWorkspace,
    /// Автоматически разрешает все инструменты без подтверждения (режим Turbo / Always Proceed)
    FullAuto,
}

/// Явное правило для конкретного инструмента в [`ApprovalPolicy`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolRule {
    Allow,
    Ask { reason: String },
    Deny { reason: String },
}

/// Итоговое решение политики подтверждения для конкретного вызова
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    Ask { reason: String },
    Deny { reason: String },
}

/// Политика фильтрации и авто-подтверждения инструментов на основе списков/правил
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalPolicy {
    pub mode: ApprovalMode,
    /// Таблица явных правил по имени инструмента (имеет приоритет над режимом)
    pub rules: BTreeMap<String, ToolRule>,
    pub allowed_tools: BTreeSet<String>,
    pub denied_tools: BTreeSet<String>,
    /// Инструменты, к которым применяются `allowed_command_prefixes` и `allow_known_safe_commands`
    pub shell_tools: BTreeSet<String>,
    /// Инструменты, автоматически разрешаемые в режиме `ApprovalMode::AutoApproveWorkspace`
    pub workspace_write_tools: BTreeSet<String>,
    pub allowed_command_prefixes: Vec<String>,
    /// Автоматически разрешает безопасные read-only shell-команды через AST/лексический разбор
    pub allow_known_safe_commands: bool,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            mode: ApprovalMode::Default,
            rules: BTreeMap::new(),
            allowed_tools: BTreeSet::new(),
            denied_tools: BTreeSet::new(),
            shell_tools: ["exec_shell", "exec_command"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            workspace_write_tools: ["write_file", "apply_patch"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            allowed_command_prefixes: Vec::new(),
            allow_known_safe_commands: false,
        }
    }
}

impl ApprovalPolicy {
    /// Создаёт политику с указанным режимом
    pub fn new(mode: ApprovalMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    /// Создаёт политику по умолчанию с базовым набором безопасных read-only команд
    pub fn with_default_safe_commands() -> Self {
        Self {
            allow_known_safe_commands: true,
            ..Self::default()
        }
        .allow_command_prefixes(["pwd", "ls", "git status", "git diff", "git log"])
    }

    /// Задаёт режим политики подтверждения (`ApprovalMode`)
    #[must_use]
    pub fn with_mode(mut self, mode: ApprovalMode) -> Self {
        self.mode = mode;
        self
    }

    /// Задаёт явное правило (`Allow` / `Ask` / `Deny`) для инструмента
    pub fn set_rule(mut self, tool_name: impl Into<String>, rule: ToolRule) -> Self {
        let name = tool_name.into();
        match &rule {
            ToolRule::Allow => {
                self.denied_tools.remove(&name);
                self.allowed_tools.insert(name.clone());
            }
            ToolRule::Deny { .. } => {
                self.allowed_tools.remove(&name);
                self.denied_tools.insert(name.clone());
            }
            ToolRule::Ask { .. } => {
                self.allowed_tools.remove(&name);
                self.denied_tools.remove(&name);
            }
        }
        self.rules.insert(name, rule);
        self
    }

    /// Применяет список пар `(tool_name, ToolRule)` к политике
    pub fn with_rules<I, S>(mut self, rules: I) -> Self
    where
        I: IntoIterator<Item = (S, ToolRule)>,
        S: Into<String>,
    {
        for (name, rule) in rules {
            self = self.set_rule(name, rule);
        }
        self
    }

    /// Добавляет инструмент в список принудительно разрешённых
    pub fn allow_tool(self, tool_name: impl Into<String>) -> Self {
        self.set_rule(tool_name, ToolRule::Allow)
    }

    /// Добавляет инструмент в список требующих подтверждения
    pub fn ask_tool(self, tool_name: impl Into<String>, reason: impl Into<String>) -> Self {
        self.set_rule(
            tool_name,
            ToolRule::Ask {
                reason: reason.into(),
            },
        )
    }

    /// Добавляет инструмент в список принудительно запрещённых
    pub fn deny_tool(mut self, tool_name: impl Into<String>) -> Self {
        let name = tool_name.into();
        self.denied_tools.insert(name.clone());
        self.rules.insert(
            name.clone(),
            ToolRule::Deny {
                reason: format!("tool `{name}` is blocked by policy"),
            },
        );
        self
    }

    /// Переопределяет список имён shell-инструментов
    pub fn shell_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.shell_tools = tools.into_iter().map(Into::into).collect();
        self
    }

    /// Переопределяет список инструментов записи в рабочую область для `AutoApproveWorkspace`
    pub fn workspace_write_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.workspace_write_tools = tools.into_iter().map(Into::into).collect();
        self
    }

    /// Добавляет безопасные префиксы shell-команд для авто-одобрения
    pub fn allow_command_prefixes<I, S>(mut self, prefixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_command_prefixes
            .extend(prefixes.into_iter().map(Into::into));
        self
    }

    /// Вычисляет решение политики для конкретного вызова инструмента
    pub fn evaluate(
        &self,
        tool_name: &str,
        read_only: bool,
        tool_approval: Approval,
        arguments: &Value,
    ) -> PolicyDecision {
        if let Some(rule) = self.rules.get(tool_name) {
            return match rule {
                ToolRule::Allow => PolicyDecision::Allow,
                ToolRule::Ask { reason } => PolicyDecision::Ask {
                    reason: reason.clone(),
                },
                ToolRule::Deny { reason } => PolicyDecision::Deny {
                    reason: reason.clone(),
                },
            };
        }

        if self.denied_tools.contains(tool_name) {
            return PolicyDecision::Deny {
                reason: format!("tool `{tool_name}` is blocked by policy"),
            };
        }

        if self.allowed_tools.contains(tool_name) {
            return PolicyDecision::Allow;
        }

        let is_shell_tool = self.shell_tools.contains(tool_name);
        let requires_escalation = is_shell_tool && requests_escalated_permissions(arguments);

        if requires_escalation {
            return match self.mode {
                ApprovalMode::FullAuto => PolicyDecision::Allow,
                ApprovalMode::ReadOnly => PolicyDecision::Deny {
                    reason: format!(
                        "escalated permissions for `{tool_name}` are not allowed in read-only mode"
                    ),
                },
                ApprovalMode::Default | ApprovalMode::AutoApproveWorkspace => match tool_approval {
                    Approval::Never => PolicyDecision::Allow,
                    Approval::Ask { reason } => PolicyDecision::Ask { reason },
                },
            };
        }

        if is_shell_tool && self.is_allowed_shell_command(arguments) {
            return PolicyDecision::Allow;
        }

        match self.mode {
            ApprovalMode::FullAuto => PolicyDecision::Allow,
            ApprovalMode::ReadOnly => match tool_approval {
                Approval::Ask { reason } if read_only => PolicyDecision::Ask { reason },
                Approval::Never => PolicyDecision::Allow,
                Approval::Ask { .. } => PolicyDecision::Deny {
                    reason: format!("tool `{tool_name}` is not allowed in read-only mode"),
                },
            },
            ApprovalMode::AutoApproveWorkspace => {
                if self.workspace_write_tools.contains(tool_name) {
                    PolicyDecision::Allow
                } else {
                    match tool_approval {
                        Approval::Never => PolicyDecision::Allow,
                        Approval::Ask { reason } => PolicyDecision::Ask { reason },
                    }
                }
            }
            ApprovalMode::Default => match tool_approval {
                Approval::Never => PolicyDecision::Allow,
                Approval::Ask { reason } => PolicyDecision::Ask { reason },
            },
        }
    }

    fn is_allowed_shell_command(&self, arguments: &Value) -> bool {
        let Some(command) = extract_shell_command(arguments) else {
            return false;
        };
        if self
            .allowed_command_prefixes
            .iter()
            .any(|prefix| matches_safe_command_prefix(command, prefix))
        {
            return true;
        }
        self.allow_known_safe_commands && is_known_safe_command(command)
    }
}

fn extract_shell_command(arguments: &Value) -> Option<&str> {
    arguments
        .get("cmd")
        .or_else(|| arguments.get("command"))
        .and_then(Value::as_str)
}

/// Извлекает безопасный префикс команды (бинарник + подкоманда в стиле Antigravity "Always Allow")
#[must_use]
pub fn extract_command_prefix(arguments: &Value) -> Option<String> {
    let command = extract_shell_command(arguments)?.trim();
    if command.is_empty() || has_shell_chaining_or_redirection(command) {
        return None;
    }
    let mut parts = command.split_whitespace();
    let bin = parts.next()?;
    let bin_base = bin.rsplit(['/', '\\']).next().unwrap_or(bin);
    let bin_lower = bin_base.to_ascii_lowercase();
    let bin_norm = bin_lower.strip_suffix(".exe").unwrap_or(&bin_lower);

    let blocked_bins = [
        "sh",
        "bash",
        "zsh",
        "dash",
        "cmd",
        "powershell",
        "pwsh",
        "python",
        "python3",
        "py",
        "node",
        "deno",
        "bunx",
        "ruby",
        "perl",
        "php",
        "lua",
        "awk",
        "sed",
        "find",
        "xargs",
        "rm",
        "del",
        "rmdir",
        "erase",
        "chmod",
        "chown",
        "sudo",
        "su",
        "env",
        "printenv",
        "curl",
        "wget",
        "nc",
        "ncat",
        "ssh",
        "scp",
    ];
    if blocked_bins.contains(&bin_norm) {
        return None;
    }

    let multi_subcommand_bins = [
        "cargo",
        "git",
        "npm",
        "pnpm",
        "yarn",
        "bun",
        "docker",
        "kubectl",
        "rustup",
        "pip",
        "uv",
        "poetry",
        "go",
        "apt",
        "systemctl",
    ];
    if multi_subcommand_bins.contains(&bin_norm) {
        let sub = parts.next()?;
        if sub.starts_with('-') {
            return None;
        }
        return Some(format!("{bin} {sub}"));
    }
    Some(bin.to_string())
}

fn requests_escalated_permissions(arguments: &Value) -> bool {
    arguments.get("sandbox_permissions").and_then(Value::as_str) == Some("require_escalated")
}

/// Проверяет, что команда начинается с разрешённого префикса на границе слова и не содержит shell-склейки
pub fn matches_safe_command_prefix(command: &str, prefix: &str) -> bool {
    let command = command.trim();
    let prefix = prefix.trim();
    if command.is_empty() || prefix.is_empty() {
        return false;
    }

    if has_shell_chaining_or_redirection(command) {
        return false;
    }

    let Some(rest) = command.strip_prefix(prefix) else {
        return false;
    };
    rest.is_empty() || rest.starts_with(char::is_whitespace)
}

fn has_shell_chaining_or_redirection(command: &str) -> bool {
    command.contains([';', '|', '&', '`', '>', '<', '\n', '\r']) || command.contains("$(")
}
