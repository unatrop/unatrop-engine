use serde_json::Value;
use tempfile::tempdir;
use uno_tools_core::{Approval, ToolError, TypedTool};
use uno_tools_exec::{ExecShell, ExecShellArgs, MAX_OUTPUT_BYTES};

#[tokio::test]
async fn executes_shell_command_and_captures_output() {
    let dir = tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("sub")).expect("mkdir");
    let tool = ExecShell::new(dir.path()).expect("tool");

    let approval = tool.approval(&ExecShellArgs {
        command: "pwd".into(),
        cwd: None,
        timeout_ms: None,
    });
    assert!(matches!(approval, Approval::Ask { .. }));

    let cmd = if cfg!(windows) {
        "echo hello&& echo warn 1>&2"
    } else {
        "printf 'hello\\n' && printf 'warn\\n' >&2"
    };

    let out = tool
        .run(ExecShellArgs {
            command: cmd.into(),
            cwd: Some("sub".into()),
            timeout_ms: None,
        })
        .await
        .expect("run");

    assert!(!out.is_error, "unexpected error: {}", out.content);
    let parsed: Value = serde_json::from_str(&out.content).expect("json");
    assert_eq!(parsed["exit_code"], 0);
    assert_eq!(parsed["timed_out"], false);
    assert_eq!(parsed["stdout"].as_str().unwrap().trim(), "hello");
    assert_eq!(parsed["stderr"].as_str().unwrap().trim(), "warn");
    assert_eq!(parsed["truncated"], false);
}

#[tokio::test]
async fn nonzero_exit_code_marks_output_as_error() {
    let dir = tempdir().expect("tempdir");
    let tool = ExecShell::new(dir.path()).expect("tool");

    let out = tool
        .run(ExecShellArgs {
            command: "echo fail >&2 && exit 7".into(),
            cwd: None,
            timeout_ms: None,
        })
        .await
        .expect("run");

    assert!(out.is_error);
    let parsed: Value = serde_json::from_str(&out.content).expect("json");
    assert_eq!(parsed["exit_code"], 7);
    assert_eq!(parsed["timed_out"], false);
    assert!(parsed["stderr"].as_str().expect("stderr").contains("fail"));
}

#[tokio::test]
async fn rejects_cwd_outside_workspace() {
    let dir = tempdir().expect("tempdir");
    let tool = ExecShell::new(dir.path()).expect("tool");

    let err = tool
        .run(ExecShellArgs {
            command: "pwd".into(),
            cwd: Some("../".into()),
            timeout_ms: None,
        })
        .await
        .expect_err("should reject escape");

    assert!(matches!(err, ToolError::Execution(msg) if msg.contains("escapes")));
}

#[tokio::test]
async fn times_out_long_running_command() {
    let dir = tempdir().expect("tempdir");
    let tool = ExecShell::new(dir.path()).expect("tool");

    let cmd = if cfg!(windows) {
        "ping -n 5 127.0.0.1 >nul"
    } else {
        "sleep 2"
    };

    let out = tool
        .run(ExecShellArgs {
            command: cmd.into(),
            cwd: None,
            timeout_ms: Some(150),
        })
        .await
        .expect("run");

    assert!(out.is_error);
    let parsed: Value = serde_json::from_str(&out.content).expect("json");
    assert_eq!(parsed["timed_out"], true);
    assert!(parsed["exit_code"].is_null());
}

#[tokio::test]
async fn truncates_oversized_stdout() {
    let dir = tempdir().expect("tempdir");
    let tool = ExecShell::new(dir.path()).expect("tool");

    let cmd = if cfg!(windows) {
        "for /L %i in (1,1,3000) do @echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()
    } else {
        format!(
            "head -c {} /dev/zero | tr '\\0' 'a'",
            MAX_OUTPUT_BYTES + 1024
        )
    };

    let out = tool
        .run(ExecShellArgs {
            command: cmd,
            cwd: None,
            timeout_ms: None,
        })
        .await
        .expect("run");

    assert!(!out.is_error, "unexpected error: {}", out.content);
    let parsed: Value = serde_json::from_str(&out.content).expect("json");
    assert_eq!(parsed["truncated"], true);
    let stdout = parsed["stdout"].as_str().expect("stdout");
    assert!(stdout.ends_with("[output truncated]"));
}
