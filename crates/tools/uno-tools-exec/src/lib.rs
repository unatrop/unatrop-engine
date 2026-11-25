//! Инструменты выполнения команд агента в рабочей области и интерактивных сессиях

#[cfg(unix)]
mod pty;
mod sandbox;
mod session;
mod shell;
mod stream_buffer;
mod unified;

pub use sandbox::{
    SandboxBackend, SandboxFailureKind, SandboxMode, active_os_sandbox_backend,
    build_seatbelt_profile, classify_sandbox_failure, is_bubblewrap_available,
    is_seatbelt_available,
};
pub use session::{
    DEFAULT_MAX_ACTIVE_SESSIONS, DEFAULT_STDIN_YIELD_TIME_MS, DEFAULT_YIELD_TIME_MS,
    ExecSessionManager, ExecSessionOutcome, MAX_SESSION_CHUNK_BYTES, MAX_YIELD_TIME_MS,
    SandboxPermissions,
};
pub use shell::{DEFAULT_TIMEOUT_MS, ExecShell, ExecShellArgs, MAX_OUTPUT_BYTES, MAX_TIMEOUT_MS};
pub use unified::{ExecCommand, ExecCommandArgs, WriteStdin, WriteStdinArgs};
