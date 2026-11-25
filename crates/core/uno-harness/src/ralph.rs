//! Оркестратор Ralph-loop: запускает серию раундов со свежим контекстом
//! и строгим структурированным JSON-handoff между раундами.

use std::sync::Arc;

use futures_util::StreamExt;
use protocol::{CompletionRequest, Message};
use provider::Provider;
use serde::{Deserialize, Serialize};
use tool_core::{ApprovalHandler, ToolRegistry};

use crate::{AgentError, Harness, RunEvent};

/// Конфигурация Ralph-цикла
#[derive(Debug, Clone)]
pub struct RalphConfig {
    pub model: String,
    pub max_rounds: usize,
    pub max_turns_per_round: usize,
    pub max_handoff_chars: usize,
    pub system_prompt: Option<String>,
}

impl RalphConfig {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            max_rounds: 32,
            max_turns_per_round: 24,
            max_handoff_chars: 16_384,
            system_prompt: None,
        }
    }
}

/// Статус завершения одного раунда Ralph-цикла
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RalphStatus {
    Continue,
    Complete,
    Blocked,
}

/// Строгий структурированный отчёт одного раунда
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RalphReport {
    pub status: RalphStatus,
    pub summary: String,
    pub evidence: Vec<String>,
    pub next_steps: Vec<String>,
    pub blocker: String,
}

impl RalphReport {
    /// Проверяет инварианты handoff-отчёта (аналог `validateReport` из DeepSeek Harness)
    pub fn validate(&self, max_handoff_chars: usize) -> Result<(), String> {
        let is_normalized = |s: &str| !s.is_empty() && s == s.trim();
        if !is_normalized(&self.summary) {
            return Err("summary must be non-empty and trimmed".into());
        }
        if !self.evidence.iter().all(|s| is_normalized(s))
            || !self.next_steps.iter().all(|s| is_normalized(s))
        {
            return Err(
                "evidence and nextSteps must contain only non-empty trimmed strings".into(),
            );
        }
        if self.blocker != self.blocker.trim() {
            return Err("blocker must be trimmed".into());
        }
        match self.status {
            RalphStatus::Continue => {
                if self.next_steps.is_empty() || !self.blocker.is_empty() {
                    return Err(
                        "continue report requires non-empty nextSteps and empty blocker".into(),
                    );
                }
            }
            RalphStatus::Complete => {
                if self.evidence.is_empty()
                    || !self.next_steps.is_empty()
                    || !self.blocker.is_empty()
                {
                    return Err(
                        "complete report requires evidence, empty nextSteps, and empty blocker"
                            .into(),
                    );
                }
            }
            RalphStatus::Blocked => {
                if !is_normalized(&self.blocker) {
                    return Err("blocked report requires a non-empty blocker".into());
                }
            }
        }
        let serialized = serde_json::to_string(self).map_err(|e| e.to_string())?;
        if serialized.len() > max_handoff_chars {
            return Err(format!(
                "report exceeds max_handoff_chars ({} > {max_handoff_chars})",
                serialized.len()
            ));
        }
        Ok(())
    }
}

/// Итог выполнения Ralph-цикла
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RalphOutcome {
    Complete {
        rounds: usize,
        report: RalphReport,
    },
    Blocked {
        rounds: usize,
        report: RalphReport,
    },
    MaxRoundsReached {
        rounds: usize,
        last_report: Option<RalphReport>,
    },
}

/// Ошибки запуска или валидации Ralph-цикла
#[derive(Debug, thiserror::Error)]
pub enum RalphError {
    #[error("invalid Ralph config: {0}")]
    InvalidConfig(&'static str),
    #[error("agent error in round {round}: {source}")]
    Agent { round: usize, source: AgentError },
    #[error("round {round} produced invalid report: {reason}")]
    InvalidReport { round: usize, reason: String },
}

/// Исполнитель Ralph-цикла поверх `Harness` (каждый раунд с чистым контекстом)
pub struct RalphRunner {
    provider: Arc<dyn Provider>,
    approval_handler: Arc<dyn ApprovalHandler>,
    config: RalphConfig,
}

impl RalphRunner {
    pub fn new(
        provider: Arc<dyn Provider>,
        approval_handler: Arc<dyn ApprovalHandler>,
        config: RalphConfig,
    ) -> Result<Self, RalphError> {
        if config.max_rounds == 0 {
            return Err(RalphError::InvalidConfig("max_rounds must be > 0"));
        }
        if config.max_turns_per_round == 0 {
            return Err(RalphError::InvalidConfig("max_turns_per_round must be > 0"));
        }
        Ok(Self {
            provider,
            approval_handler,
            config,
        })
    }

    /// Запускает цикл раундов. `build_tools` вызывается на каждый раунд, чтобы дать свежий реестр инструментов.
    pub async fn run<F>(
        &self,
        objective: &str,
        mut build_tools: F,
    ) -> Result<RalphOutcome, RalphError>
    where
        F: FnMut() -> ToolRegistry,
    {
        let mut previous: Option<RalphReport> = None;

        for round in 1..=self.config.max_rounds {
            let tools = build_tools();
            let harness = Harness::new(
                Arc::clone(&self.provider),
                tools,
                Arc::clone(&self.approval_handler),
                self.config.max_turns_per_round,
            )
            .map_err(|source| RalphError::Agent { round, source })?;

            let prior_json = match &previous {
                Some(report) => serde_json::to_string(report).unwrap_or_default(),
                None => "(none — this is the first round)".to_owned(),
            };

            let prompt = format!(
                "You are one fresh worker in a Ralph loop (round {round} of {}).\n\
                 Immutable objective:\n{objective}\n\n\
                 Previous structured handoff:\n{prior_json}\n\n\
                 Perform concrete work in the workspace, verify changes, and respond ONLY with valid JSON matching:\n\
                 {{\"status\":\"continue|complete|blocked\",\"summary\":\"...\",\"evidence\":[\"...\"],\"nextSteps\":[\"...\"],\"blocker\":\"\"}}",
                self.config.max_rounds
            );

            let mut messages = Vec::new();
            if let Some(sys) = &self.config.system_prompt {
                messages.push(Message::system(sys.clone()));
            }
            messages.push(Message::user(prompt));

            let request = CompletionRequest {
                model: self.config.model.clone(),
                messages,
                ..CompletionRequest::default()
            };

            let mut stream = harness.run(request);
            let mut final_messages = None;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(RunEvent::Completed { messages, .. }) => final_messages = Some(messages),
                    Ok(_) => {}
                    Err(source) => return Err(RalphError::Agent { round, source }),
                }
            }

            let text = final_messages
                .and_then(|msgs| {
                    msgs.into_iter()
                        .rev()
                        .find(|m| m.role == protocol::Role::Assistant)
                })
                .map(|m| {
                    m.content
                        .0
                        .into_iter()
                        .filter_map(|part| match part {
                            protocol::ContentPart::Text { text } => Some(text),
                            protocol::ContentPart::Reasoning { .. }
                            | protocol::ContentPart::ImageUrl { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();

            let report = parse_report(&text)
                .map_err(|reason| RalphError::InvalidReport { round, reason })?;
            report
                .validate(self.config.max_handoff_chars)
                .map_err(|reason| RalphError::InvalidReport { round, reason })?;

            match report.status {
                RalphStatus::Complete => {
                    return Ok(RalphOutcome::Complete {
                        rounds: round,
                        report,
                    });
                }
                RalphStatus::Blocked => {
                    return Ok(RalphOutcome::Blocked {
                        rounds: round,
                        report,
                    });
                }
                RalphStatus::Continue => {
                    previous = Some(report);
                }
            }
        }

        Ok(RalphOutcome::MaxRoundsReached {
            rounds: self.config.max_rounds,
            last_report: previous,
        })
    }
}

fn parse_report(raw: &str) -> Result<RalphReport, String> {
    let trimmed = raw.trim();
    let json_slice = if let Some(stripped) = trimmed.strip_prefix("```json") {
        stripped
            .trim()
            .strip_suffix("```")
            .unwrap_or(stripped)
            .trim()
    } else if let Some(stripped) = trimmed.strip_prefix("```") {
        stripped
            .trim()
            .strip_suffix("```")
            .unwrap_or(stripped)
            .trim()
    } else {
        trimmed
    };
    serde_json::from_str::<RalphReport>(json_slice).map_err(|e| format!("invalid JSON report: {e}"))
}
