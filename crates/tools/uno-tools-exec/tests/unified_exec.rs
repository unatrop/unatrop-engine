use std::sync::Arc;

use serde_json::Value;
use tempfile::tempdir;
use uno_tools_core::{Approval, ToolError, TypedTool};
use uno_tools_exec::{
    ExecCommand, ExecCommandArgs, ExecSessionManager, SandboxMode, SandboxPermissions, WriteStdin,
    WriteStdinArgs, is_bubblewrap_available,
};

#[tokio::test]
async fn exec_command_finishes_fast_without_leaving_session() {
    let dir = tempdir().expect("tempdir");
    let manager = Arc::new(ExecSessionManager::new(dir.path()).expect("manager"));
    let exec = ExecCommand::new(manager);

    let cmd = if cfg!(windows) {
        "echo ok"
    } else {
        "printf 'ok\\n'"
    };

    let out = exec
        .run(ExecCommandArgs {
            cmd: cmd.into(),
            workdir: None,
            yield_time_ms: Some(2_000),
            max_output_bytes: None,
            sandbox_permissions: SandboxPermissions::UseDefault,
            justification: None,
            tty: false,
        })
        .await
        .expect("run");

    assert!(!out.is_error);
    let parsed: Value = serde_json::from_str(&out.content).expect("json");
    assert_eq!(parsed["running"], false);
    assert!(parsed["session_id"].is_null());
    assert_eq!(parsed["exit_code"], 0);
    assert_eq!(parsed["stdout"].as_str().unwrap().trim(), "ok");
}

#[tokio::test]
async fn exec_command_and_write_stdin_drive_interactive_session() {
    let dir = tempdir().expect("tempdir");
    let manager = Arc::new(ExecSessionManager::new(dir.path()).expect("manager"));
    let exec = ExecCommand::new(Arc::clone(&manager));
    let stdin = WriteStdin::new(manager);

    let cmd = if cfg!(windows) {
        "findstr .*"
    } else {
        "IFS= read -r line && printf 'got:%s\\n' \"$line\""
    };

    let started = exec
        .run(ExecCommandArgs {
            cmd: cmd.into(),
            workdir: None,
            yield_time_ms: Some(60),
            max_output_bytes: None,
            sandbox_permissions: SandboxPermissions::UseDefault,
            justification: None,
            tty: false,
        })
        .await
        .expect("start");

    assert!(!started.is_error);
    let parsed_start: Value = serde_json::from_str(&started.content).expect("json");
    assert_eq!(parsed_start["running"], true);
    let session_id = parsed_start["session_id"].as_u64().expect("session_id");

    let continued = stdin
        .run(WriteStdinArgs {
            session_id,
            chars: if cfg!(windows) {
                "codex-pty\r\n".into()
            } else {
                "codex-pty\n".into()
            },
            yield_time_ms: Some(2_000),
            max_output_bytes: None,
            close_stdin: cfg!(windows),
            kill: false,
        })
        .await
        .expect("write_stdin");

    assert!(!continued.is_error);
    let parsed_end: Value = serde_json::from_str(&continued.content).expect("json");
    assert_eq!(parsed_end["running"], false);
    assert_eq!(parsed_end["exit_code"], 0);
    let expected = if cfg!(windows) {
        "codex-pty"
    } else {
        "got:codex-pty"
    };
    assert_eq!(parsed_end["stdout"].as_str().unwrap().trim(), expected);
}

#[tokio::test]
async fn write_stdin_can_kill_long_running_session() {
    let dir = tempdir().expect("tempdir");
    let manager = Arc::new(ExecSessionManager::new(dir.path()).expect("manager"));
    let exec = ExecCommand::new(Arc::clone(&manager));
    let stdin = WriteStdin::new(manager);

    let cmd = if cfg!(windows) {
        "ping -n 30 127.0.0.1 >nul"
    } else {
        "sleep 30"
    };

    let started = exec
        .run(ExecCommandArgs {
            cmd: cmd.into(),
            workdir: None,
            yield_time_ms: Some(50),
            max_output_bytes: None,
            sandbox_permissions: SandboxPermissions::UseDefault,
            justification: None,
            tty: false,
        })
        .await
        .expect("start");

    let session_id = serde_json::from_str::<Value>(&started.content).expect("json")["session_id"]
        .as_u64()
        .expect("session_id");

    let killed = stdin
        .run(WriteStdinArgs {
            session_id,
            chars: String::new(),
            yield_time_ms: None,
            max_output_bytes: None,
            close_stdin: false,
            kill: true,
        })
        .await
        .expect("kill");

    let parsed: Value = serde_json::from_str(&killed.content).expect("json");
    assert_eq!(parsed["running"], false);
}

#[tokio::test]
async fn workdir_outside_workspace_requires_escalation() {
    let dir = tempdir().expect("tempdir");
    let outside = tempdir().expect("outside");
    let manager = Arc::new(ExecSessionManager::new(dir.path()).expect("manager"));
    let exec = ExecCommand::new(manager);
    let cmd = if cfg!(windows) { "cd" } else { "pwd" };

    let denied = exec
        .run(ExecCommandArgs {
            cmd: cmd.into(),
            workdir: Some(outside.path().display().to_string()),
            yield_time_ms: Some(1_000),
            max_output_bytes: None,
            sandbox_permissions: SandboxPermissions::UseDefault,
            justification: None,
            tty: false,
        })
        .await
        .expect_err("should reject outside workdir");
    assert!(matches!(denied, ToolError::Execution(_)));

    let args = ExecCommandArgs {
        cmd: cmd.into(),
        workdir: Some(outside.path().display().to_string()),
        yield_time_ms: Some(1_000),
        max_output_bytes: None,
        sandbox_permissions: SandboxPermissions::RequireEscalated,
        justification: Some("inspect outside directory".into()),
        tty: false,
    };
    let approval = exec.approval(&args);
    assert!(matches!(approval, Approval::Ask { ref reason } if reason.contains("[ESCALATED]")));

    let allowed = exec.run(args).await.expect("escalated run");
    assert!(!allowed.is_error);
}

#[cfg(unix)]
#[tokio::test]
async fn exec_command_allocates_real_pty_and_handles_interactive_input() {
    let dir = tempdir().expect("tempdir");
    let manager = Arc::new(ExecSessionManager::new(dir.path()).expect("manager"));
    let exec = ExecCommand::new(Arc::clone(&manager));
    let stdin = WriteStdin::new(manager);

    let check_tty = exec
        .run(ExecCommandArgs {
            cmd: "if [ -t 0 ] && [ -t 1 ]; then echo REAL_PTY; else echo NOT_PTY; fi".into(),
            workdir: None,
            yield_time_ms: Some(2_000),
            max_output_bytes: None,
            sandbox_permissions: SandboxPermissions::UseDefault,
            justification: None,
            tty: true,
        })
        .await
        .expect("pty check");

    let parsed: Value = serde_json::from_str(&check_tty.content).expect("json");
    assert_eq!(parsed["tty"], true);
    assert!(
        parsed["stdout"]
            .as_str()
            .expect("stdout")
            .contains("REAL_PTY")
    );

    let interactive = exec
        .run(ExecCommandArgs {
            cmd: "IFS= read -r answer && printf 'pty-got:%s\\n' \"$answer\"".into(),
            workdir: None,
            yield_time_ms: Some(60),
            max_output_bytes: None,
            sandbox_permissions: SandboxPermissions::UseDefault,
            justification: None,
            tty: true,
        })
        .await
        .expect("start pty interactive");

    let parsed_start: Value = serde_json::from_str(&interactive.content).expect("json");
    assert_eq!(parsed_start["running"], true);
    let session_id = parsed_start["session_id"].as_u64().expect("session_id");

    let continued = stdin
        .run(WriteStdinArgs {
            session_id,
            chars: "hello-tty\n".into(),
            yield_time_ms: Some(2_000),
            max_output_bytes: None,
            close_stdin: false,
            kill: false,
        })
        .await
        .expect("write_stdin pty");

    let parsed_end: Value = serde_json::from_str(&continued.content).expect("json");
    assert_eq!(parsed_end["running"], false);
    assert_eq!(parsed_end["exit_code"], 0);
    assert!(
        parsed_end["stdout"]
            .as_str()
            .expect("stdout")
            .contains("pty-got:hello-tty")
    );
}

#[tokio::test]
async fn sandbox_mode_required_matches_bwrap_availability() {
    let dir = tempdir().expect("tempdir");
    let manager = Arc::new(
        ExecSessionManager::with_sandbox_mode(dir.path(), SandboxMode::BubblewrapRequired)
            .expect("manager"),
    );
    let exec = ExecCommand::new(manager);

    let res = exec
        .run(ExecCommandArgs {
            cmd: "echo ok".into(),
            workdir: None,
            yield_time_ms: Some(2_000),
            max_output_bytes: None,
            sandbox_permissions: SandboxPermissions::UseDefault,
            justification: None,
            tty: false,
        })
        .await;

    if is_bubblewrap_available() {
        let out = res.expect("platform sandbox should succeed");
        let parsed: Value = serde_json::from_str(&out.content).expect("json");
        assert_eq!(parsed["sandboxed"], true);
    } else {
        let err = res.expect_err("bwrap required should fail when unavailable");
        assert!(matches!(err, ToolError::Execution(_)));
    }
}

#[tokio::test]
async fn session_manager_enforces_max_active_sessions_and_kills_all() {
    let dir = tempdir().expect("tempdir");
    let manager = Arc::new(
        ExecSessionManager::new(dir.path())
            .expect("manager")
            .with_max_active_sessions(2),
    );
    let exec = ExecCommand::new(Arc::clone(&manager));
    let sleep_cmd = if cfg!(windows) {
        "ping -n 30 127.0.0.1 >nul"
    } else {
        "sleep 30"
    };

    for _ in 0..2 {
        let out = exec
            .run(ExecCommandArgs {
                cmd: sleep_cmd.into(),
                workdir: None,
                yield_time_ms: Some(40),
                max_output_bytes: None,
                sandbox_permissions: SandboxPermissions::UseDefault,
                justification: None,
                tty: false,
            })
            .await
            .expect("background session started");
        assert!(!out.is_error);
    }

    assert_eq!(manager.active_session_count().await, 2);

    let third = exec
        .run(ExecCommandArgs {
            cmd: sleep_cmd.into(),
            workdir: None,
            yield_time_ms: Some(40),
            max_output_bytes: None,
            sandbox_permissions: SandboxPermissions::UseDefault,
            justification: None,
            tty: false,
        })
        .await
        .expect_err("should reject 3rd background session");
    assert!(
        third
            .to_string()
            .contains("maximum active background sessions")
    );

    let killed = manager.kill_all().await;
    assert_eq!(killed, 2);
    assert_eq!(manager.active_session_count().await, 0);
}
