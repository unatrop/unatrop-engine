use std::fs;

use async_trait::async_trait;
use serde_json::json;
use uno_tools_core::{
    Approval, ApprovalHandler, ApprovalRequest, ToolError, ToolRegistry, Typed, TypedTool,
};
use uno_tools_fs::{MAX_FILE_BYTES, WriteFile, WriteFileArgs};

struct ApprovalResponse(bool);

#[async_trait]
impl ApprovalHandler for ApprovalResponse {
    async fn approve(&self, _request: ApprovalRequest) -> Result<bool, String> {
        Ok(self.0)
    }
}

#[tokio::test]
async fn creates_and_replaces_files_inside_the_workspace() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let tool = WriteFile::new(workspace.path()).expect("create write tool");

    tool.run(WriteFileArgs {
        path: "src/lib.rs".into(),
        content: "first".into(),
    })
    .await
    .expect_err("missing parent directory should fail");

    fs::create_dir(workspace.path().join("src")).expect("create src directory");
    tool.run(WriteFileArgs {
        path: "src/lib.rs".into(),
        content: "first".into(),
    })
    .await
    .expect("create file");
    tool.run(WriteFileArgs {
        path: "src/lib.rs".into(),
        content: "replacement".into(),
    })
    .await
    .expect("replace file");

    assert_eq!(
        fs::read_to_string(workspace.path().join("src/lib.rs")).expect("read result"),
        "replacement"
    );
}

#[tokio::test]
async fn rejects_parent_directory_components() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let tool = WriteFile::new(workspace.path()).expect("create write tool");

    let error = tool
        .run(WriteFileArgs {
            path: "../outside.txt".into(),
            content: "not written".into(),
        })
        .await
        .expect_err("parent path should be rejected");

    assert!(matches!(error, ToolError::InvalidArguments(_)));
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_symlink_targets() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().expect("create parent directory");
    let workspace_path = parent.path().join("workspace");
    fs::create_dir(&workspace_path).expect("create workspace");
    fs::write(parent.path().join("outside.txt"), "keep unchanged").expect("write outside file");
    symlink(
        parent.path().join("outside.txt"),
        workspace_path.join("linked.txt"),
    )
    .expect("create symlink");
    let tool = WriteFile::new(&workspace_path).expect("create write tool");

    let error = tool
        .run(WriteFileArgs {
            path: "linked.txt".into(),
            content: "overwrite attempt".into(),
        })
        .await
        .expect_err("symlink target should be rejected");

    assert!(matches!(error, ToolError::InvalidArguments(_)));
    assert_eq!(
        fs::read_to_string(parent.path().join("outside.txt")).expect("read outside file"),
        "keep unchanged"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_parent_directories_that_resolve_outside_the_workspace() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().expect("create parent directory");
    let workspace_path = parent.path().join("workspace");
    fs::create_dir(&workspace_path).expect("create workspace");
    symlink(parent.path(), workspace_path.join("outside-link")).expect("create symlink");
    let tool = WriteFile::new(&workspace_path).expect("create write tool");

    let error = tool
        .run(WriteFileArgs {
            path: "outside-link/new.txt".into(),
            content: "not written".into(),
        })
        .await
        .expect_err("outside parent should be rejected");

    assert!(matches!(error, ToolError::InvalidArguments(_)));
    assert!(!parent.path().join("new.txt").exists());
}

#[tokio::test]
async fn rejects_contents_larger_than_the_limit() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let tool = WriteFile::new(workspace.path()).expect("create write tool");

    let error = tool
        .run(WriteFileArgs {
            path: "large.txt".into(),
            content: "a".repeat(MAX_FILE_BYTES + 1),
        })
        .await
        .expect_err("oversize content should be rejected");

    assert!(matches!(error, ToolError::Execution(message) if message.contains("limit")));
    assert!(!workspace.path().join("large.txt").exists());
}

#[tokio::test]
async fn registry_requires_approval_before_writing() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::write(workspace.path().join("existing.txt"), "original").expect("write fixture");
    let mut registry = ToolRegistry::new();
    registry
        .register(Typed(
            WriteFile::new(workspace.path()).expect("create write tool"),
        ))
        .expect("register write tool");
    let arguments = json!({
        "path": "existing.txt",
        "content": "replacement"
    });

    assert!(matches!(
        registry.approval("write_file", &arguments),
        Ok(Approval::Ask { .. })
    ));
    let denied = registry
        .execute("write_file", arguments.clone(), &ApprovalResponse(false))
        .await;
    assert!(matches!(denied, Err(ToolError::ApprovalDenied(_))));
    assert_eq!(
        fs::read_to_string(workspace.path().join("existing.txt")).expect("read original"),
        "original"
    );

    registry
        .execute("write_file", arguments, &ApprovalResponse(true))
        .await
        .expect("approved write");
    assert_eq!(
        fs::read_to_string(workspace.path().join("existing.txt")).expect("read replacement"),
        "replacement"
    );
}
