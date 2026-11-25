use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use uno_tools_core::{Approval, ToolError, ToolOutput, TypedTool};

use crate::MAX_FILE_BYTES;
use crate::observation::FileObservationTracker;
use crate::workspace::{Workspace, read_text_file};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApplyPatchArgs {
    #[schemars(description = "Workspace-relative path.")]
    pub path: String,
    #[schemars(description = "Unique exact text to replace.")]
    pub old_text: String,
    #[schemars(description = "Replacement text.")]
    pub new_text: String,
}

/// Инструмент точечной замены уникального фрагмента текста в файле
pub struct ApplyPatch {
    workspace: Workspace,
    observation: Option<Arc<FileObservationTracker>>,
}

impl ApplyPatch {
    /// Создаёт инструмент для указанного корня рабочей области
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self, ToolError> {
        Ok(Self {
            workspace: Workspace::new(workspace)?,
            observation: None,
        })
    }

    /// Подключает трекер наблюдений для проверки чтения и защиты от гонок
    pub fn with_observation_tracker(mut self, tracker: Arc<FileObservationTracker>) -> Self {
        self.observation = Some(tracker);
        self
    }
}

#[async_trait]
impl TypedTool for ApplyPatch {
    type Args = ApplyPatchArgs;

    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Replace one unique exact text block in a workspace file (maximum 1 MiB)."
    }

    fn approval(&self, args: &Self::Args) -> Approval {
        Approval::Ask {
            reason: format!("modify workspace file `{}`", args.path),
        }
    }

    async fn run(&self, args: Self::Args) -> Result<ToolOutput, ToolError> {
        if args.old_text.is_empty() {
            return Err(ToolError::InvalidArguments(
                "old_text must not be empty".into(),
            ));
        }
        if args.old_text.len() > MAX_FILE_BYTES || args.new_text.len() > MAX_FILE_BYTES {
            return Err(ToolError::InvalidArguments(format!(
                "old_text and new_text must not exceed {MAX_FILE_BYTES} bytes"
            )));
        }

        let path = self.workspace.write_target(&args.path).await?;
        if let Some(tracker) = &self.observation {
            tracker.verify_fresh_for_edit(&path).await?;
        }
        let original = read_text_file(&path).await?;
        let mut matches = original.match_indices(&args.old_text);
        let first = matches.next();
        if first.is_none() {
            return Err(ToolError::InvalidArguments(
                "old_text was not found in the file".into(),
            ));
        }
        if matches.next().is_some() {
            return Err(ToolError::InvalidArguments(
                "old_text must match exactly one occurrence".into(),
            ));
        }

        let updated = original.replacen(&args.old_text, &args.new_text, 1);
        if updated.len() > MAX_FILE_BYTES {
            return Err(ToolError::Execution(format!(
                "patched file exceeds the {MAX_FILE_BYTES}-byte limit"
            )));
        }
        if updated == original {
            return Ok(ToolOutput::text(
                "No changes made; replacement is identical.",
            ));
        }

        tokio::fs::write(&path, updated.as_bytes())
            .await
            .map_err(|error| ToolError::Execution(format!("cannot write patched file: {error}")))?;
        if let Some(tracker) = &self.observation {
            tracker.record_present(&path).await?;
        }

        Ok(ToolOutput::text(format!("Patched {}", args.path)))
    }
}
