use std::collections::HashMap;
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use uno_tools_core::ToolError;

#[cfg(unix)]
use crate::pty::{configure_pty_command, open_pty};
use crate::sandbox::{SandboxMode, build_command};
#[cfg(unix)]
use crate::stream_buffer::pump_blocking_reader;
use crate::stream_buffer::{MAX_BUFFER_BYTES, StreamBuffer, pump_stream};

/// Таймаут ожидания вывода по умолчанию для `exec_command` (10 секунд)
pub const DEFAULT_YIELD_TIME_MS: u64 = 10_000;
/// Таймаут ожидания вывода по умолчанию для `write_stdin` (5 секунд)
pub const DEFAULT_STDIN_YIELD_TIME_MS: u64 = 5_000;
/// Максимальное время ожидания одного вызова (5 минут)
pub const MAX_YIELD_TIME_MS: u64 = 300_000;
/// Максимальный размер отдаваемого вывода за один вызов на каждый поток
pub const MAX_SESSION_CHUNK_BYTES: usize = 64 * 1024;
/// Лимит одновременно запущенных фоновых сессий по умолчанию
pub const DEFAULT_MAX_ACTIVE_SESSIONS: usize = 16;

// Режим песочницы для запуска команды (по модели `codex-rs`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxPermissions {
    // Стандартный режим: рабочий каталог ограничен `workspace_root` и включается ОС-песочница
    #[default]
    UseDefault,
    // Запрос повышенных прав: отключает ОС-песочницу, разрешает выход за пределы `workspace_root`
    RequireEscalated,
}

/// Результат выполнения или ожидания сессии `exec_command` / `write_stdin`
#[derive(Debug, Clone, Serialize)]
pub struct ExecSessionOutcome {
    pub session_id: Option<u64>,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub workdir: String,
    pub tty: bool,
    pub sandboxed: bool,
}

enum SessionStdin {
    Pipe(ChildStdin),
    #[cfg(unix)]
    Pty(std::fs::File),
}

struct ExecSession {
    workdir: PathBuf,
    tty: bool,
    sandboxed: bool,
    child: Mutex<Child>,
    stdin: Mutex<Option<SessionStdin>>,
    stdout: Arc<Mutex<StreamBuffer>>,
    stderr: Arc<Mutex<StreamBuffer>>,
    notify: Arc<Notify>,
    exited: Arc<AtomicBool>,
    exit_code: Arc<Mutex<Option<i32>>>,
    delta_sink: Arc<std::sync::Mutex<Option<uno_tools_core::OutputDeltaSender>>>,
    #[cfg(windows)]
    _job_guard: Option<crate::sandbox::WindowsJobGuard>,
    _readers: Vec<JoinHandle<()>>,
}

/// Менеджер интерактивных и фоновых shell-сессий для `exec_command` и `write_stdin`
pub struct ExecSessionManager {
    workspace_root: PathBuf,
    sandbox_mode: SandboxMode,
    max_active_sessions: usize,
    next_session_id: AtomicU64,
    sessions: Mutex<HashMap<u64, Arc<ExecSession>>>,
}

impl ExecSessionManager {
    /// Создаёт менеджер сессий с автоматическим выбором ОС-песочницы (`SandboxMode::Auto`)
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self, ToolError> {
        Self::with_sandbox_mode(workspace, SandboxMode::Auto)
    }

    /// Создаёт менеджер сессий с явно заданным режимом ОС-песочницы
    pub fn with_sandbox_mode(
        workspace: impl AsRef<Path>,
        sandbox_mode: SandboxMode,
    ) -> Result<Self, ToolError> {
        let workspace_root = std::fs::canonicalize(workspace.as_ref()).map_err(|error| {
            ToolError::Execution(format!("cannot resolve workspace root: {error}"))
        })?;
        Ok(Self {
            workspace_root,
            sandbox_mode,
            max_active_sessions: DEFAULT_MAX_ACTIVE_SESSIONS,
            next_session_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// Задаёт максимальное число одновременно активных фоновых сессий
    pub fn with_max_active_sessions(mut self, max_sessions: usize) -> Self {
        self.max_active_sessions = max_sessions.max(1);
        self
    }

    /// Очищает уже завершившиеся фоновые сессии и возвращает число живых сессий
    pub async fn active_session_count(&self) -> usize {
        self.reap_exited_sessions().await
    }

    /// Принудительно завершает все активные фоновые сессии и возвращает число остановленных процессов
    pub async fn kill_all(&self) -> usize {
        let drained: Vec<Arc<ExecSession>> = {
            let mut guard = self.sessions.lock().await;
            guard.drain().map(|(_, s)| s).collect()
        };
        let count = drained.len();
        for session in drained {
            let mut child = session.child.lock().await;
            let _ = child.kill().await;
            if let Ok(status) = child.wait().await {
                *session.exit_code.lock().await = status.code();
            }
            session.exited.store(true, Ordering::Release);
        }
        count
    }

    async fn reap_exited_sessions(&self) -> usize {
        let entries: Vec<(u64, Arc<ExecSession>)> = {
            let guard = self.sessions.lock().await;
            guard.iter().map(|(&id, s)| (id, Arc::clone(s))).collect()
        };
        let mut dead_ids = Vec::new();
        for (id, session) in entries {
            if check_exit(&session).await.unwrap_or(true) {
                dead_ids.push(id);
            }
        }
        let mut guard = self.sessions.lock().await;
        for id in dead_ids {
            guard.remove(&id);
        }
        guard.len()
    }

    /// Возвращает канонический корень рабочей области
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Разрешает рабочий каталог с учётом `SandboxPermissions`
    pub fn resolve_workdir(
        &self,
        workdir: Option<&str>,
        permissions: SandboxPermissions,
    ) -> Result<PathBuf, ToolError> {
        let Some(raw) = workdir.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(self.workspace_root.clone());
        };

        let path = Path::new(raw);
        let allow_outside = permissions == SandboxPermissions::RequireEscalated;

        if path.is_absolute() && !allow_outside {
            return Err(ToolError::Execution(
                "workdir must be a relative path inside the workspace unless sandbox_permissions is require_escalated"
                    .into(),
            ));
        }

        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace_root.join(path)
        };

        let resolved = std::fs::canonicalize(&candidate).map_err(|error| {
            ToolError::Execution(format!("cannot resolve workdir `{raw}`: {error}"))
        })?;

        if !allow_outside && !resolved.starts_with(&self.workspace_root) {
            return Err(ToolError::Execution(format!(
                "workdir `{raw}` escapes the workspace root (use sandbox_permissions: \"require_escalated\" if needed)"
            )));
        }

        if !resolved.is_dir() {
            return Err(ToolError::Execution(format!(
                "workdir `{raw}` is not a directory"
            )));
        }

        Ok(resolved)
    }

    /// Запускает команду и ждёт её завершения или истечения `yield_time_ms`
    pub async fn start_command(
        &self,
        cmd_line: &str,
        workdir: Option<&str>,
        yield_time_ms: Option<u64>,
        max_output_bytes: Option<usize>,
        permissions: SandboxPermissions,
        tty: bool,
    ) -> Result<ExecSessionOutcome, ToolError> {
        self.start_command_streaming(
            cmd_line,
            workdir,
            yield_time_ms,
            max_output_bytes,
            permissions,
            tty,
            None,
        )
        .await
    }

    /// Запускает команду со стримингом инкрементальных чанков вывода в `delta_tx`
    #[allow(clippy::too_many_arguments)]
    pub async fn start_command_streaming(
        &self,
        cmd_line: &str,
        workdir: Option<&str>,
        yield_time_ms: Option<u64>,
        max_output_bytes: Option<usize>,
        permissions: SandboxPermissions,
        tty: bool,
        delta_tx: Option<uno_tools_core::OutputDeltaSender>,
    ) -> Result<ExecSessionOutcome, ToolError> {
        if cmd_line.trim().is_empty() {
            return Err(ToolError::InvalidArguments("cmd must not be empty".into()));
        }

        let resolved_workdir = self.resolve_workdir(workdir, permissions)?;
        let yield_ms = yield_time_ms
            .unwrap_or(DEFAULT_YIELD_TIME_MS)
            .clamp(1, MAX_YIELD_TIME_MS);
        let output_cap = max_output_bytes
            .unwrap_or(MAX_SESSION_CHUNK_BYTES)
            .clamp(1024, MAX_BUFFER_BYTES);

        let (mut cmd, sandboxed) = build_command(
            cmd_line,
            &self.workspace_root,
            &resolved_workdir,
            permissions,
            self.sandbox_mode,
        )?;
        cmd.kill_on_drop(true);

        let stdout_buf = Arc::new(Mutex::new(StreamBuffer::default()));
        let stderr_buf = Arc::new(Mutex::new(StreamBuffer::default()));
        let notify = Arc::new(Notify::new());
        let delta_sink = Arc::new(std::sync::Mutex::new(delta_tx));

        #[cfg(unix)]
        let use_pty = tty;
        #[cfg(not(unix))]
        let use_pty = {
            let _ = tty;
            false
        };

        let (child, stdin, readers) = if use_pty {
            #[cfg(unix)]
            {
                let pty = open_pty().map_err(|error| {
                    ToolError::Execution(format!("failed to allocate PTY: {error}"))
                })?;
                let (master_reader, master_writer) =
                    configure_pty_command(&mut cmd, pty).map_err(|error| {
                        ToolError::Execution(format!("failed to configure PTY: {error}"))
                    })?;

                let child = cmd.spawn().map_err(|error| {
                    ToolError::Execution(format!("failed to spawn shell: {error}"))
                })?;

                // Закрываем slave-дескрипторы в `cmd` сразу после `spawn`, чтобы EOF на master сработал при выходе
                drop(cmd);

                let reader_task = tokio::task::spawn_blocking({
                    let stdout_buf = Arc::clone(&stdout_buf);
                    let notify = Arc::clone(&notify);
                    let delta_sink = Arc::clone(&delta_sink);
                    move || pump_blocking_reader(master_reader, stdout_buf, notify, delta_sink)
                });
                (
                    child,
                    Some(SessionStdin::Pty(master_writer)),
                    vec![reader_task],
                )
            }
            #[cfg(not(unix))]
            {
                unreachable!()
            }
        } else {
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            let mut child = cmd
                .spawn()
                .map_err(|error| ToolError::Execution(format!("failed to spawn shell: {error}")))?;

            let stdin = child.stdin.take().map(SessionStdin::Pipe);
            let stdout_pipe = child
                .stdout
                .take()
                .ok_or_else(|| ToolError::Execution("failed to capture stdout".into()))?;
            let stderr_pipe = child
                .stderr
                .take()
                .ok_or_else(|| ToolError::Execution("failed to capture stderr".into()))?;

            let stdout_task = tokio::spawn(pump_stream(
                stdout_pipe,
                Arc::clone(&stdout_buf),
                Arc::clone(&notify),
                Arc::clone(&delta_sink),
            ));
            let stderr_task = tokio::spawn(pump_stream(
                stderr_pipe,
                Arc::clone(&stderr_buf),
                Arc::clone(&notify),
                Arc::clone(&delta_sink),
            ));
            (child, stdin, vec![stdout_task, stderr_task])
        };

        #[cfg(windows)]
        let (job_guard, sandboxed) = match (permissions, child.raw_handle()) {
            (SandboxPermissions::UseDefault, Some(raw)) => {
                let guard = crate::sandbox::attach_windows_job_sandbox(raw);
                (guard, sandboxed)
            }
            _ => (None, sandboxed),
        };

        let session = Arc::new(ExecSession {
            workdir: resolved_workdir,
            tty: use_pty,
            sandboxed,
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            stdout: stdout_buf,
            stderr: stderr_buf,
            notify,
            exited: Arc::new(AtomicBool::new(false)),
            exit_code: Arc::new(Mutex::new(None)),
            delta_sink,
            #[cfg(windows)]
            _job_guard: job_guard,
            _readers: readers,
        });

        let outcome = wait_or_yield(&session, yield_ms, output_cap).await?;
        if let Ok(mut guard) = session.delta_sink.lock() {
            *guard = None;
        }
        if outcome.running {
            let active = self.reap_exited_sessions().await;
            if active >= self.max_active_sessions {
                let mut child = session.child.lock().await;
                let _ = child.kill().await;
                let _ = child.wait().await;
                session.exited.store(true, Ordering::Release);
                return Err(ToolError::Execution(format!(
                    "maximum active background sessions ({}) reached; close or kill an existing session via write_stdin first",
                    self.max_active_sessions
                )));
            }
            let id = self.next_session_id.fetch_add(1, Ordering::Relaxed);
            self.sessions.lock().await.insert(id, session);
            Ok(ExecSessionOutcome {
                session_id: Some(id),
                ..outcome
            })
        } else {
            Ok(outcome)
        }
    }

    /// Отправляет ввод в активную сессию `session_id` и ждёт новый вывод
    pub async fn write_stdin(
        &self,
        session_id: u64,
        chars: &str,
        yield_time_ms: Option<u64>,
        max_output_bytes: Option<usize>,
        close_stdin: bool,
        kill: bool,
    ) -> Result<ExecSessionOutcome, ToolError> {
        self.write_stdin_streaming(
            session_id,
            chars,
            yield_time_ms,
            max_output_bytes,
            close_stdin,
            kill,
            None,
        )
        .await
    }

    /// Отправляет ввод в активную сессию `session_id` со стримингом вывода в `delta_tx`
    #[allow(clippy::too_many_arguments)]
    pub async fn write_stdin_streaming(
        &self,
        session_id: u64,
        chars: &str,
        yield_time_ms: Option<u64>,
        max_output_bytes: Option<usize>,
        close_stdin: bool,
        kill: bool,
        delta_tx: Option<uno_tools_core::OutputDeltaSender>,
    ) -> Result<ExecSessionOutcome, ToolError> {
        let session = self
            .sessions
            .lock()
            .await
            .get(&session_id)
            .cloned()
            .ok_or_else(|| {
                ToolError::Execution(format!("unknown or already exited session_id {session_id}"))
            })?;

        if let Ok(mut guard) = session.delta_sink.lock() {
            *guard = delta_tx;
        }

        if kill {
            {
                let mut child = session.child.lock().await;
                let _ = child.kill().await;
                if let Ok(status) = child.wait().await {
                    *session.exit_code.lock().await = status.code();
                }
                session.exited.store(true, Ordering::Release);
            }
            self.sessions.lock().await.remove(&session_id);
            if let Ok(mut guard) = session.delta_sink.lock() {
                *guard = None;
            }
            let output_cap = max_output_bytes
                .unwrap_or(MAX_SESSION_CHUNK_BYTES)
                .clamp(1024, MAX_BUFFER_BYTES);
            return collect_outcome(&session, None, output_cap).await;
        }

        if !chars.is_empty() {
            let mut stdin_guard = session.stdin.lock().await;
            let stdin = stdin_guard.as_mut().ok_or_else(|| {
                ToolError::Execution(format!("stdin is closed for session {session_id}"))
            })?;
            write_session_stdin(stdin, chars.as_bytes(), session_id).await?;
        }

        if close_stdin {
            session.stdin.lock().await.take();
        }

        let yield_ms = yield_time_ms
            .unwrap_or(DEFAULT_STDIN_YIELD_TIME_MS)
            .clamp(1, MAX_YIELD_TIME_MS);
        let output_cap = max_output_bytes
            .unwrap_or(MAX_SESSION_CHUNK_BYTES)
            .clamp(1024, MAX_BUFFER_BYTES);

        let outcome = wait_or_yield(&session, yield_ms, output_cap).await?;
        if let Ok(mut guard) = session.delta_sink.lock() {
            *guard = None;
        }
        if outcome.running {
            Ok(ExecSessionOutcome {
                session_id: Some(session_id),
                ..outcome
            })
        } else {
            self.sessions.lock().await.remove(&session_id);
            Ok(outcome)
        }
    }
}

async fn write_session_stdin(
    stdin: &mut SessionStdin,
    bytes: &[u8],
    session_id: u64,
) -> Result<(), ToolError> {
    match stdin {
        SessionStdin::Pipe(pipe) => {
            pipe.write_all(bytes).await.map_err(|error| {
                ToolError::Execution(format!(
                    "failed writing to stdin of session {session_id}: {error}"
                ))
            })?;
            pipe.flush().await.map_err(|error| {
                ToolError::Execution(format!(
                    "failed flushing stdin of session {session_id}: {error}"
                ))
            })?;
            Ok(())
        }
        #[cfg(unix)]
        SessionStdin::Pty(file) => {
            let mut cloned = file.try_clone().map_err(|error| {
                ToolError::Execution(format!(
                    "failed cloning PTY writer for session {session_id}: {error}"
                ))
            })?;
            let owned = bytes.to_vec();
            tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                cloned.write_all(&owned)?;
                cloned.flush()
            })
            .await
            .map_err(|error| {
                ToolError::Execution(format!(
                    "PTY write task failed for session {session_id}: {error}"
                ))
            })?
            .map_err(|error| {
                ToolError::Execution(format!(
                    "failed writing to PTY of session {session_id}: {error}"
                ))
            })
        }
    }
}

async fn wait_or_yield(
    session: &Arc<ExecSession>,
    yield_ms: u64,
    output_cap: usize,
) -> Result<ExecSessionOutcome, ToolError> {
    let deadline = Instant::now() + Duration::from_millis(yield_ms);

    loop {
        if check_exit(session).await? {
            tokio::time::sleep(Duration::from_millis(15)).await;
            return collect_outcome(session, None, output_cap).await;
        }

        let now = Instant::now();
        if now >= deadline {
            return collect_outcome(session, None, output_cap).await;
        }

        let remaining = (deadline - now).min(Duration::from_millis(50));
        tokio::select! {
            _ = session.notify.notified() => {}
            _ = tokio::time::sleep(remaining) => {}
        }
    }
}

async fn check_exit(session: &Arc<ExecSession>) -> Result<bool, ToolError> {
    if session.exited.load(Ordering::Acquire) {
        return Ok(true);
    }

    let mut child = session.child.lock().await;
    match child.try_wait() {
        Ok(Some(status)) => {
            *session.exit_code.lock().await = status.code();
            session.exited.store(true, Ordering::Release);
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(error) => Err(ToolError::Execution(format!(
            "failed polling process status: {error}"
        ))),
    }
}

async fn collect_outcome(
    session: &Arc<ExecSession>,
    session_id: Option<u64>,
    output_cap: usize,
) -> Result<ExecSessionOutcome, ToolError> {
    let running = !session.exited.load(Ordering::Acquire);
    let exit_code = *session.exit_code.lock().await;
    let (stdout, stdout_trunc) = session.stdout.lock().await.take_capped(output_cap);
    let (stderr, stderr_trunc) = session.stderr.lock().await.take_capped(output_cap);

    Ok(ExecSessionOutcome {
        session_id,
        running,
        exit_code,
        stdout,
        stderr,
        truncated: stdout_trunc || stderr_trunc,
        workdir: session.workdir.display().to_string(),
        tty: session.tty,
        sandboxed: session.sandboxed,
    })
}
