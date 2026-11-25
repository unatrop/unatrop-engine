use std::path::Path;
use std::sync::OnceLock;

use tokio::process::Command;
use uno_tools_core::ToolError;

use crate::session::SandboxPermissions;

/// Стратегия ОС-изоляции для команд с `SandboxPermissions::UseDefault`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    /// Использовать доступную ОС-песочницу (`bwrap` на Linux, `sandbox-exec` на macOS); иначе откат на ограничение `workdir`
    #[default]
    Auto,
    /// Требовать ОС-песочницу (`bwrap` на Linux, `sandbox-exec` на macOS) и возвращать ошибку, если она недоступна
    BubblewrapRequired,
    /// Отключить ОС-песочницу (проверяется только `workdir` внутри `workspace_root`)
    Disabled,
}

/// Активный бэкенд ОС-песочницы на текущей платформе
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxBackend {
    None,
    /// Linux bubblewrap (`bwrap`)
    Bubblewrap,
    /// macOS Seatbelt (`sandbox-exec`)
    Seatbelt,
    /// Windows Job Object (`KILL_ON_JOB_CLOSE` + UI restrictions)
    WindowsJobObject,
}

/// Различение отказа политики песочницы и поломки самого раннера песочницы
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxFailureKind {
    /// Команда заблокирована политикой песочницы (модель может запросить escalation)
    Denied,
    /// Сломан или недоступен сам раннер песочницы (`bwrap` / `sandbox-exec`)
    RunnerUnavailable,
}

static BWRAP_AVAILABLE: OnceLock<bool> = OnceLock::new();
static SEATBELT_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Проверяет, доступен ли и реально ли работает `bwrap` в текущей среде (Linux)
pub fn is_bubblewrap_available() -> bool {
    *BWRAP_AVAILABLE.get_or_init(|| {
        #[cfg(target_os = "linux")]
        {
            std::process::Command::new("bwrap")
                .args([
                    "--ro-bind",
                    "/",
                    "/",
                    "--dev",
                    "/dev",
                    "--proc",
                    "/proc",
                    "/bin/true",
                ])
                .output()
                .is_ok_and(|out| out.status.success())
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    })
}

/// Проверяет, доступен ли и реально ли работает `sandbox-exec` на macOS
pub fn is_seatbelt_available() -> bool {
    *SEATBELT_AVAILABLE.get_or_init(|| {
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("sandbox-exec")
                .args(["-p", "(version 1) (allow default)", "/usr/bin/true"])
                .output()
                .is_ok_and(|out| out.status.success())
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    })
}

/// Возвращает доступный бэкенд ОС-песочницы для текущей платформы
pub fn active_os_sandbox_backend() -> SandboxBackend {
    if is_bubblewrap_available() {
        SandboxBackend::Bubblewrap
    } else if is_seatbelt_available() {
        SandboxBackend::Seatbelt
    } else if cfg!(windows) {
        SandboxBackend::WindowsJobObject
    } else {
        SandboxBackend::None
    }
}

#[cfg(windows)]
pub(crate) struct WindowsJobGuard {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
unsafe impl Send for WindowsJobGuard {}
#[cfg(windows)]
unsafe impl Sync for WindowsJobGuard {}

#[cfg(windows)]
impl Drop for WindowsJobGuard {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(windows)]
pub(crate) fn attach_windows_job_sandbox(
    raw_process_handle: std::os::windows::io::RawHandle,
) -> Option<WindowsJobGuard> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT_DESKTOP,
        JOB_OBJECT_UILIMIT_EXITWINDOWS, JOB_OBJECT_UILIMIT_GLOBALATOMS,
        JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation, SetInformationJobObject,
    };

    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }

        let mut ext: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        ext.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        let ok_ext = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const ext).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );

        let mut ui: JOBOBJECT_BASIC_UI_RESTRICTIONS = std::mem::zeroed();
        ui.UIRestrictionsClass = JOB_OBJECT_UILIMIT_DESKTOP
            | JOB_OBJECT_UILIMIT_EXITWINDOWS
            | JOB_OBJECT_UILIMIT_GLOBALATOMS;
        let _ = SetInformationJobObject(
            job,
            JobObjectBasicUIRestrictions,
            (&raw const ui).cast(),
            std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
        );

        if ok_ext == 0 || AssignProcessToJobObject(job, raw_process_handle.cast()) == 0 {
            CloseHandle(job);
            return None;
        }

        Some(WindowsJobGuard { handle: job })
    }
}

/// Формирует SBPL-профиль для macOS `sandbox-exec` (только чтение везде, запись только в `/dev/null` и разрешённые директории)
pub fn build_seatbelt_profile(writable_roots: &[&Path]) -> String {
    let mut forms = vec![
        "(version 1)".to_owned(),
        "(allow default)".to_owned(),
        "(deny file-write*)".to_owned(),
        format!("(allow file-write* (literal {}))", sbpl_string("/dev/null")),
    ];

    if !writable_roots.is_empty() {
        let subpaths = writable_roots
            .iter()
            .map(|root| format!("(subpath {})", sbpl_string(&root.to_string_lossy())))
            .collect::<Vec<_>>()
            .join(" ");
        forms.push(format!("(allow file-write* {subpaths})"));
    }

    forms.join(" ")
}

fn sbpl_string(path: &str) -> String {
    let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Классифицирует неудачный запуск в песочнице: нарушение политики (`Denied`) vs ошибка самого раннера (`RunnerUnavailable`)
pub fn classify_sandbox_failure(
    backend: SandboxBackend,
    exit_code: Option<i32>,
    stderr: &str,
) -> Option<SandboxFailureKind> {
    if exit_code == Some(0) || backend == SandboxBackend::None {
        return None;
    }
    let lower = stderr.to_ascii_lowercase();
    match backend {
        SandboxBackend::Bubblewrap => {
            if lower.contains("bwrap:") && (lower.contains("execvp") || lower.contains("namespace"))
            {
                return Some(SandboxFailureKind::RunnerUnavailable);
            }
            if lower.contains("read-only file system") || lower.contains("permission denied") {
                return Some(SandboxFailureKind::Denied);
            }
            None
        }
        SandboxBackend::Seatbelt => {
            if lower.contains("sandbox-exec:") {
                return Some(SandboxFailureKind::RunnerUnavailable);
            }
            if lower.contains("operation not permitted") || lower.contains("deny(") {
                return Some(SandboxFailureKind::Denied);
            }
            None
        }
        SandboxBackend::WindowsJobObject | SandboxBackend::None => None,
    }
}

const SENSITIVE_ENV_VARS: &[&str] = &[
    "UNATROP_DAEMON_TOKEN",
    "UNOTROP_DAEMON_TOKEN",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GITHUB_COPILOT_API_TOKEN",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "ANTIGRAVITY_API_KEY",
    "ANTIGRAVITY_TOKEN",
    "ANTIGRAVITY_REFRESH_TOKEN",
    "ANTIGRAVITY_ACCESS_TOKEN",
    "ANTIGRAVITY_TOKEN_JSON",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
];

pub(crate) fn strip_sensitive_env(cmd: &mut Command) {
    for var in SENSITIVE_ENV_VARS {
        cmd.env_remove(var);
    }
}

/// Создаёт `Command` для запуска shell-команды с учётом `SandboxMode` и `SandboxPermissions`.
/// Возвращает `(Command, sandboxed_with_os)`.
pub fn build_command(
    command: &str,
    workspace_root: &Path,
    workdir: &Path,
    permissions: SandboxPermissions,
    mode: SandboxMode,
) -> Result<(Command, bool), ToolError> {
    if permissions == SandboxPermissions::RequireEscalated || mode == SandboxMode::Disabled {
        let mut cmd = raw_shell_command(command);
        strip_sensitive_env(&mut cmd);
        cmd.current_dir(workdir);
        return Ok((cmd, false));
    }

    let backend = match mode {
        SandboxMode::Auto => active_os_sandbox_backend(),
        SandboxMode::BubblewrapRequired => {
            let detected = active_os_sandbox_backend();
            if matches!(
                detected,
                SandboxBackend::None | SandboxBackend::WindowsJobObject
            ) {
                return Err(ToolError::Execution(
                    "OS filesystem/network sandbox (`bwrap` on Linux / `sandbox-exec` on macOS) is required but unavailable in this environment"
                        .into(),
                ));
            }
            detected
        }
        SandboxMode::Disabled => SandboxBackend::None,
    };

    match backend {
        SandboxBackend::Bubblewrap => {
            let mut cmd = Command::new("bwrap");
            strip_sensitive_env(&mut cmd);
            cmd.arg("--ro-bind")
                .arg("/")
                .arg("/")
                .arg("--bind")
                .arg(workspace_root)
                .arg(workspace_root);

            if Path::new("/tmp").is_dir() {
                cmd.args(["--bind", "/tmp", "/tmp"]);
            }

            cmd.args(["--dev", "/dev", "--proc", "/proc", "--die-with-parent"])
                .arg("--chdir")
                .arg(workdir)
                .args(["/bin/sh", "-c", command]);

            Ok((cmd, true))
        }
        SandboxBackend::Seatbelt => {
            let tmp_dir = std::env::temp_dir();
            let profile = build_seatbelt_profile(&[workspace_root, tmp_dir.as_path()]);
            let mut cmd = Command::new("sandbox-exec");
            strip_sensitive_env(&mut cmd);
            cmd.arg("-p")
                .arg(profile)
                .args(["/bin/sh", "-c", command])
                .current_dir(workdir);
            Ok((cmd, true))
        }
        SandboxBackend::WindowsJobObject | SandboxBackend::None => {
            let mut cmd = raw_shell_command(command);
            strip_sensitive_env(&mut cmd);
            cmd.current_dir(workdir);
            // Windows Job Objects only provide process-tree cleanup, not FS/network isolation.
            Ok((cmd, false))
        }
    }
}

fn raw_shell_command(command: &str) -> Command {
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
