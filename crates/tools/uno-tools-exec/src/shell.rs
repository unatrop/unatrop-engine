use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::timeout;
use uno_tools_core::{Approval, ToolError, ToolOutput, TypedTool};

/// Таймаут выполнения команды по умолчанию (30 секунд)
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Максимальный допустимый таймаут команды (5 минут)
pub const MAX_TIMEOUT_MS: u64 = 300_000;
/// Максимальный размер захваченного вывода на каждый поток (`stdout` / `stderr`)
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Аргументы инструмента `exec_shell`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecShellArgs {
    /// Команда оболочки для выполнения
    pub command: String,
    /// Необязательный относительный путь рабочего каталога внутри рабочей области
    #[serde(default)]
    pub cwd: Option<String>,
    /// Необязательный таймаут в миллисекундах (от 1 до 300000)
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Инструмент выполнения shell-команд внутри рабочей области с таймаутом и лимитом вывода
pub struct ExecShell {
    workspace_root: PathBuf,
}

impl ExecShell {
    /// Создаёт инструмент, привязанный к каноническому корню рабочей области
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self, ToolError> {
        let workspace_root = std::fs::canonicalize(workspace.as_ref()).map_err(|error| {
            ToolError::Execution(format!("cannot resolve workspace root: {error}"))
        })?;
        Ok(Self { workspace_root })
    }

    fn resolve_cwd(&self, cwd: Option<&str>) -> Result<PathBuf, ToolError> {
        let Some(raw) = cwd.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(self.workspace_root.clone());
        };
        let path = Path::new(raw);
        if path.is_absolute() {
            return Err(ToolError::Execution(
                "cwd must be a relative path inside the workspace".into(),
            ));
        }
        let resolved = std::fs::canonicalize(self.workspace_root.join(path)).map_err(|error| {
            ToolError::Execution(format!("cannot resolve cwd `{raw}`: {error}"))
        })?;
        if !resolved.starts_with(&self.workspace_root) {
            return Err(ToolError::Execution(format!(
                "cwd `{raw}` escapes the workspace root"
            )));
        }
        if !resolved.is_dir() {
            return Err(ToolError::Execution(format!(
                "cwd `{raw}` is not a directory"
            )));
        }
        Ok(resolved)
    }
}

#[async_trait]
impl TypedTool for ExecShell {
    type Args = ExecShellArgs;

    fn name(&self) -> &str {
        "exec_shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command inside the workspace with a timeout and capped stdout/stderr."
    }

    fn approval(&self, args: &Self::Args) -> Approval {
        Approval::Ask {
            reason: format!("execute shell command `{}`", args.command),
        }
    }

    async fn run(&self, args: Self::Args) -> Result<ToolOutput, ToolError> {
        if args.command.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "command must not be empty".into(),
            ));
        }

        let cwd = self.resolve_cwd(args.cwd.as_deref())?;
        let timeout_ms = args
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS);

        let mut cmd = shell_command(&args.command);
        crate::sandbox::strip_sensitive_env(&mut cmd);
        cmd.current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|error| ToolError::Execution(format!("failed to spawn shell: {error}")))?;

        let stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Execution("failed to capture stdout".into()))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Execution("failed to capture stderr".into()))?;

        let stdout_task = tokio::spawn(read_capped(stdout_pipe, MAX_OUTPUT_BYTES));
        let stderr_task = tokio::spawn(read_capped(stderr_pipe, MAX_OUTPUT_BYTES));

        let wait_result = timeout(Duration::from_millis(timeout_ms), child.wait()).await;
        let (timed_out, exit_code) = match wait_result {
            Ok(Ok(status)) => (false, status.code()),
            Ok(Err(error)) => {
                return Err(ToolError::Execution(format!(
                    "failed waiting for command: {error}"
                )));
            }
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                (true, None)
            }
        };

        let (stdout, stdout_truncated) = join_output(stdout_task).await;
        let (stderr, stderr_truncated) = join_output(stderr_task).await;

        let payload = json!({
            "exit_code": exit_code,
            "timed_out": timed_out,
            "stdout": stdout,
            "stderr": stderr,
            "truncated": stdout_truncated || stderr_truncated,
        });
        let rendered = serde_json::to_string_pretty(&payload)
            .map_err(|error| ToolError::Execution(format!("failed to format output: {error}")))?;

        if timed_out || exit_code != Some(0) {
            Ok(ToolOutput::error(rendered))
        } else {
            Ok(ToolOutput::text(rendered))
        }
    }
}

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    }
}

async fn read_capped<R: AsyncRead + Unpin>(mut reader: R, limit: usize) -> (String, bool) {
    let mut captured = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 4096];

    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if captured.len() < limit {
                    let take = n.min(limit - captured.len());
                    captured.extend_from_slice(&buf[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
        }
    }

    let mut text = String::from_utf8_lossy(&captured).into_owned();
    if truncated {
        text.push_str("\n[output truncated]");
    }
    (text, truncated)
}

async fn join_output(task: tokio::task::JoinHandle<(String, bool)>) -> (String, bool) {
    task.await.unwrap_or_default()
}
