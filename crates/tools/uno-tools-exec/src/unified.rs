use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use uno_tools_core::{Approval, OutputDeltaSender, ToolError, ToolOutput, TypedTool};

use crate::session::{ExecSessionManager, ExecSessionOutcome, SandboxPermissions};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecCommandArgs {
    #[schemars(description = "Shell command to run.")]
    #[serde(alias = "command")]
    pub cmd: String,
    #[schemars(
        description = "Working directory (relative to workspace, or any path when escalated)."
    )]
    #[serde(default, alias = "cwd")]
    pub workdir: Option<String>,
    #[schemars(description = "Wait ms before returning session_id (default 10000).")]
    #[serde(default, alias = "timeout_ms")]
    pub yield_time_ms: Option<u64>,
    #[schemars(description = "Max output bytes per stream.")]
    #[serde(default)]
    pub max_output_bytes: Option<usize>,
    #[schemars(description = "'use_default' or 'require_escalated'.")]
    #[serde(default)]
    pub sandbox_permissions: SandboxPermissions,
    #[schemars(description = "Reason when require_escalated.")]
    #[serde(default)]
    pub justification: Option<String>,
    #[schemars(description = "Allocate PTY (default true).")]
    #[serde(default = "default_tty")]
    pub tty: bool,
}

fn default_tty() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteStdinArgs {
    #[schemars(description = "Active exec_command session_id.")]
    pub session_id: u64,
    #[schemars(description = "Input to write (empty to poll output).")]
    #[serde(default)]
    pub chars: String,
    #[schemars(description = "Wait ms for output (default 5000).")]
    #[serde(default)]
    pub yield_time_ms: Option<u64>,
    #[schemars(description = "Max output bytes per stream.")]
    #[serde(default)]
    pub max_output_bytes: Option<usize>,
    #[schemars(description = "Close stdin (EOF) after writing.")]
    #[serde(default)]
    pub close_stdin: bool,
    #[schemars(description = "Kill the process.")]
    #[serde(default)]
    pub kill: bool,
}

/// Инструмент запуска команд с поддержкой интерактивных сессий и эскалации прав
pub struct ExecCommand {
    manager: Arc<ExecSessionManager>,
}

impl ExecCommand {
    /// Создаёт `ExecCommand`, разделяющий сессии с `WriteStdin`
    pub fn new(manager: Arc<ExecSessionManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl TypedTool for ExecCommand {
    type Args = ExecCommandArgs;

    fn name(&self) -> &str {
        "exec_command"
    }

    fn description(&self) -> &str {
        "Run a shell command. Returns output or a running session_id for write_stdin."
    }

    fn approval(&self, args: &Self::Args) -> Approval {
        let reason = match args.sandbox_permissions {
            SandboxPermissions::RequireEscalated => {
                let why = args
                    .justification
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("escalated system access requested");
                let dir = args.workdir.as_deref().unwrap_or("<workspace>");
                format!("[ESCALATED] {why} — execute `{}` in `{dir}`", args.cmd)
            }
            SandboxPermissions::UseDefault => format!("execute command `{}`", args.cmd),
        };
        Approval::Ask { reason }
    }

    async fn run(&self, args: Self::Args) -> Result<ToolOutput, ToolError> {
        self.run_streaming(args, None).await
    }

    async fn run_streaming(
        &self,
        args: Self::Args,
        delta_tx: Option<OutputDeltaSender>,
    ) -> Result<ToolOutput, ToolError> {
        let outcome = self
            .manager
            .start_command_streaming(
                &args.cmd,
                args.workdir.as_deref(),
                args.yield_time_ms,
                args.max_output_bytes,
                args.sandbox_permissions,
                args.tty,
                delta_tx,
            )
            .await?;
        format_outcome(&outcome)
    }
}

/// Инструмент записи в `stdin` и чтения очередного фрагмента вывода активной сессии
pub struct WriteStdin {
    manager: Arc<ExecSessionManager>,
}

impl WriteStdin {
    /// Создаёт `WriteStdin`, разделяющий сессии с `ExecCommand`
    pub fn new(manager: Arc<ExecSessionManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl TypedTool for WriteStdin {
    type Args = WriteStdinArgs;

    fn name(&self) -> &str {
        "write_stdin"
    }

    fn description(&self) -> &str {
        "Write to stdin of an active exec_command session (or poll with empty chars)."
    }

    fn approval(&self, _args: &Self::Args) -> Approval {
        Approval::Never
    }

    async fn run(&self, args: Self::Args) -> Result<ToolOutput, ToolError> {
        self.run_streaming(args, None).await
    }

    async fn run_streaming(
        &self,
        args: Self::Args,
        delta_tx: Option<OutputDeltaSender>,
    ) -> Result<ToolOutput, ToolError> {
        let outcome = self
            .manager
            .write_stdin_streaming(
                args.session_id,
                &args.chars,
                args.yield_time_ms,
                args.max_output_bytes,
                args.close_stdin,
                args.kill,
                delta_tx,
            )
            .await?;
        format_outcome(&outcome)
    }
}

fn format_outcome(outcome: &ExecSessionOutcome) -> Result<ToolOutput, ToolError> {
    let rendered = serde_json::to_string_pretty(outcome)
        .map_err(|error| ToolError::Execution(format!("failed to format output: {error}")))?;
    if !outcome.running && outcome.exit_code != Some(0) {
        Ok(ToolOutput::error(rendered))
    } else {
        Ok(ToolOutput::text(rendered))
    }
}
