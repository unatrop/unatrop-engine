//! Structured task plan tool (`update_plan`) for tracking step progress.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use protocol::ToolDefinition;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tool_core::{Approval, Tool, ToolError, ToolOutput};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub step: String,
    pub status: PlanStepStatus,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanState {
    #[serde(default)]
    pub explanation: Option<String>,
    pub steps: Vec<PlanStep>,
}

#[derive(Debug, Deserialize)]
struct UpdatePlanArgs {
    #[serde(default)]
    explanation: Option<String>,
    plan: Vec<PlanStep>,
}

pub struct UpdatePlanTool {
    state: Arc<Mutex<PlanState>>,
    updates_tx: Option<mpsc::UnboundedSender<PlanState>>,
}

impl Default for UpdatePlanTool {
    fn default() -> Self {
        Self::new()
    }
}

impl UpdatePlanTool {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(PlanState::default())),
            updates_tx: None,
        }
    }

    pub fn with_sender(updates_tx: mpsc::UnboundedSender<PlanState>) -> Self {
        Self {
            state: Arc::new(Mutex::new(PlanState::default())),
            updates_tx: Some(updates_tx),
        }
    }

    pub fn current_plan(&self) -> PlanState {
        self.state.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl Tool for UpdatePlanTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "update_plan".to_owned(),
            description: "Update the structured task plan. Keep at most one step `in_progress`."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "explanation": {
                        "type": "string",
                        "description": "Optional note explaining why the plan changed."
                    },
                    "plan": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": { "type": "string" },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                }
                            },
                            "required": ["step", "status"]
                        }
                    }
                },
                "required": ["plan"]
            }),
        }
    }

    fn approval(&self, _arguments: &Value) -> Approval {
        Approval::Never
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args: UpdatePlanArgs = serde_json::from_value(arguments)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        if args.plan.is_empty() {
            return Ok(ToolOutput::error("plan must contain at least one step"));
        }

        let in_progress = args
            .plan
            .iter()
            .filter(|s| s.status == PlanStepStatus::InProgress)
            .count();
        if in_progress > 1 {
            return Ok(ToolOutput::error(
                "at most one plan step can be `in_progress` at a time",
            ));
        }

        if args.plan.iter().any(|s| s.step.trim().is_empty()) {
            return Ok(ToolOutput::error(
                "plan step descriptions must not be empty",
            ));
        }

        let next = PlanState {
            explanation: args.explanation,
            steps: args.plan,
        };
        if let Ok(mut guard) = self.state.lock() {
            *guard = next.clone();
        }
        if let Some(tx) = &self.updates_tx {
            let _ = tx.send(next);
        }

        Ok(ToolOutput::text("Plan updated."))
    }
}
