//! Read-only research subagent tool (`spawn_agent`) for parallel exploration.

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use protocol::{CompletionRequest, Message, ToolDefinition};
use provider::Provider;
use serde::Deserialize;
use serde_json::{Value, json};
use tool_core::{Approval, ApprovalHandler, Tool, ToolError, ToolOutput, ToolRegistry};

use crate::{Harness, RunEvent};

const DEFAULT_SUBAGENT_MAX_TURNS: usize = 12;
const HARD_SUBAGENT_MAX_TURNS: usize = 24;
const SUBAGENT_SYSTEM_PROMPT: &str = "You are a focused read-only research subagent. Inspect the workspace using the available read-only tools and return concise, factual findings.";

#[derive(Debug, Deserialize)]
struct SpawnAgentArgs {
    task: String,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    max_turns: Option<usize>,
}

pub struct SpawnAgentTool {
    provider: Arc<dyn Provider>,
    model: String,
    read_only_tools: Arc<ToolRegistry>,
    approval_handler: Arc<dyn ApprovalHandler>,
}

impl SpawnAgentTool {
    pub fn new(
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
        tools: &ToolRegistry,
        approval_handler: Arc<dyn ApprovalHandler>,
    ) -> Self {
        Self {
            provider,
            model: model.into(),
            read_only_tools: Arc::new(tools.read_only_subset(&["spawn_agent"])),
            approval_handler,
        }
    }
}

#[async_trait]
impl Tool for SpawnAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "spawn_agent".to_owned(),
            description: "Spawn a read-only research subagent to inspect files/code in parallel without polluting the main context."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "Clear, self-contained research task for the subagent."
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional background context or target paths."
                    },
                    "max_turns": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 24,
                        "description": "Optional turn budget (default 12, max 24)."
                    }
                },
                "required": ["task"]
            }),
        }
    }

    fn approval(&self, _arguments: &Value) -> Approval {
        Approval::Never
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args: SpawnAgentArgs = serde_json::from_value(arguments)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        if args.task.trim().is_empty() {
            return Ok(ToolOutput::error("task must not be empty"));
        }

        let max_turns = args
            .max_turns
            .unwrap_or(DEFAULT_SUBAGENT_MAX_TURNS)
            .clamp(1, HARD_SUBAGENT_MAX_TURNS);

        let mut prompt = args.task;
        if let Some(ctx) = args.context.filter(|c| !c.trim().is_empty()) {
            prompt.push_str("\n\nContext:\n");
            prompt.push_str(&ctx);
        }

        // Rebuild a lightweight registry cloning the read-only tool implementations.
        let tools = self.read_only_tools.read_only_subset(&["spawn_agent"]);
        let harness = Harness::new(
            Arc::clone(&self.provider),
            tools,
            Arc::clone(&self.approval_handler),
            max_turns,
        )
        .map_err(|e| ToolError::Execution(e.to_string()))?;

        let req = CompletionRequest {
            model: self.model.clone(),
            messages: vec![
                Message::system(SUBAGENT_SYSTEM_PROMPT),
                Message::user(prompt),
            ],
            ..CompletionRequest::default()
        };

        let mut stream = Arc::new(harness).run_owned(req);
        let mut final_text = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(RunEvent::Completed { messages, .. }) => {
                    if let Some(last) = messages
                        .into_iter()
                        .rev()
                        .find(|m| m.role == protocol::Role::Assistant)
                    {
                        for part in last.content.0 {
                            if let protocol::ContentPart::Text { text } = part {
                                final_text.push_str(&text);
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    return Ok(ToolOutput::error(format!("subagent failed: {err}")));
                }
            }
        }

        if final_text.trim().is_empty() {
            Ok(ToolOutput::text("(subagent returned no text output)"))
        } else {
            Ok(ToolOutput::text(final_text))
        }
    }
}
