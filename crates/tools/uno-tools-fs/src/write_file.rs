use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use uno_tools_core::{Approval, ToolError, ToolOutput, TypedTool};

use crate::MAX_FILE_BYTES;
use crate::observation::FileObservationTracker;
use crate::workspace::Workspace;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteFileArgs {
    #[schemars(description = "Workspace-relative path.")]
    pub path: String,
    #[schemars(description = "UTF-8 file content.")]
    pub content: String,
}

/// Инструмент создания и перезаписи UTF-8 файлов в рабочей области
pub struct WriteFile {
    workspace: Workspace,
    observation: Option<Arc<FileObservationTracker>>,
}

impl WriteFile {
    /// Создаёт инструмент для указанного корня рабочей области
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self, ToolError> {
        Ok(Self {
            workspace: Workspace::new(workspace)?,
            observation: None,
        })
    }

    /// Подключает трекер наблюдений для защиты от затирания внешних изменений
    pub fn with_observation_tracker(mut self, tracker: Arc<FileObservationTracker>) -> Self {
        self.observation = Some(tracker);
        self
    }
}

#[async_trait]
impl TypedTool for WriteFile {
    type Args = WriteFileArgs;

    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Create or replace a UTF-8 file inside the current workspace (maximum 1 MiB)."
    }

    fn approval(&self, args: &Self::Args) -> Approval {
        Approval::Ask {
            reason: format!("write or replace workspace file `{}`", args.path),
        }
    }

    async fn run(&self, args: Self::Args) -> Result<ToolOutput, ToolError> {
        if args.content.len() > MAX_FILE_BYTES {
            return Err(ToolError::Execution(format!(
                "content exceeds the {MAX_FILE_BYTES}-byte limit"
            )));
        }

        let path: PathBuf = self.workspace.write_target(&args.path).await?;
        if let Some(tracker) = &self.observation {
            tracker.verify_fresh_for_write(&path).await?;
        }
        tokio::fs::write(&path, args.content.as_bytes())
            .await
            .map_err(|error| ToolError::Execution(format!("cannot write file: {error}")))?;
        if let Some(tracker) = &self.observation {
            tracker.record_present(&path).await?;
        }

        Ok(ToolOutput::text(format!(
            "Wrote {} bytes to {}",
            args.content.len(),
            args.path
        )))
    }
}
