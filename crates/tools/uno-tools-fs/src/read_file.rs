use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use uno_tools_core::{Approval, ToolError, ToolOutput, TypedTool};

use crate::observation::FileObservationTracker;
use crate::workspace::{Workspace, read_text_file};

/// Максимальный размер файла для чтения и записи в байтах (1 МиБ)
pub const MAX_FILE_BYTES: usize = 1024 * 1024;

/// Аргументы инструмента `read_file`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// Путь к файлу относительно корня рабочей области
    #[schemars(description = "Workspace-relative file path.")]
    pub path: String,
    /// Начальная строка (нумерация с 1)
    #[schemars(description = "1-based first line to return; defaults to 1.")]
    pub offset: Option<usize>,
    /// Максимальное число строк в ответе
    #[schemars(
        description = "Maximum number of lines to return; defaults to the end of the file."
    )]
    pub limit: Option<usize>,
}

/// Инструмент чтения UTF-8 файлов в рабочей области
pub struct ReadFile {
    workspace: Workspace,
    observation: Option<Arc<FileObservationTracker>>,
}

impl ReadFile {
    /// Создаёт инструмент для указанного корня рабочей области
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self, ToolError> {
        Ok(Self {
            workspace: Workspace::new(workspace)?,
            observation: None,
        })
    }

    /// Подключает трекер наблюдений для CAS-защиты последующих правок
    pub fn with_observation_tracker(mut self, tracker: Arc<FileObservationTracker>) -> Self {
        self.observation = Some(tracker);
        self
    }
}

#[async_trait]
impl TypedTool for ReadFile {
    type Args = ReadFileArgs;

    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a UTF-8 workspace file (maximum 1 MiB), optionally selecting a 1-based line range with offset and limit."
    }

    fn approval(&self, _args: &Self::Args) -> Approval {
        Approval::Never
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn run(&self, args: Self::Args) -> Result<ToolOutput, ToolError> {
        if args.offset == Some(0) {
            return Err(ToolError::InvalidArguments(
                "offset must be greater than zero".into(),
            ));
        }
        if args.limit == Some(0) {
            return Err(ToolError::InvalidArguments(
                "limit must be greater than zero".into(),
            ));
        }

        let path = self.workspace.existing_file(&args.path).await?;
        let content = read_text_file(&path).await?;
        if let Some(tracker) = &self.observation {
            tracker.record_present(&path).await?;
        }
        let content = if args.offset.is_some() || args.limit.is_some() {
            content
                .split_inclusive('\n')
                .skip(args.offset.unwrap_or(1) - 1)
                .take(args.limit.unwrap_or(usize::MAX))
                .collect()
        } else {
            content
        };

        Ok(ToolOutput::text(content))
    }
}
