use uno_tools_exec::{ExecSessionManager, SandboxPermissions};

#[tokio::test]
async fn streams_incremental_output_deltas_during_execution() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manager = ExecSessionManager::new(dir.path()).expect("manager");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cmd = if cfg!(windows) {
        "echo stream-ok"
    } else {
        "printf 'stream-ok\n'"
    };

    let outcome = manager
        .start_command_streaming(
            cmd,
            None,
            Some(2000),
            None,
            SandboxPermissions::UseDefault,
            false,
            Some(tx),
        )
        .await
        .expect("start_command_streaming");

    assert_eq!(outcome.exit_code, Some(0));
    if cfg!(windows) {
        assert!(
            !outcome.sandboxed,
            "Windows Job Object should not claim full OS FS/network sandbox"
        );
    }
    let mut streamed = String::new();
    while let Ok(chunk) = rx.try_recv() {
        streamed.push_str(&chunk);
    }
    assert!(
        streamed.contains("stream-ok"),
        "expected streamed chunk to contain stream-ok, got: {streamed:?}"
    );
}
