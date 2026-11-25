use std::fs;

use async_trait::async_trait;
use serde_json::json;
use uno_tools_core::{
    Approval, ApprovalHandler, ApprovalRequest, ToolError, ToolRegistry, Typed, TypedTool,
};
use uno_tools_fs::{ApplyPatch, ApplyPatchArgs, MAX_FILE_BYTES};

struct ApprovalResponse(bool);

#[async_trait]
impl ApprovalHandler for ApprovalResponse {
    async fn approve(&self, _request: ApprovalRequest) -> Result<bool, String> {
        Ok(self.0)
    }
}

#[tokio::test]
async fn replaces_one_exact_unique_text_block() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::write(
        workspace.path().join("src.rs"),
        "fn main() {\n    println!(\"before\");\n}\n",
    )
    .expect("write fixture");
    let tool = ApplyPatch::new(workspace.path()).expect("create patch tool");

    let output = tool
        .run(ApplyPatchArgs {
            path: "src.rs".into(),
            old_text: "println!(\"before\");".into(),
            new_text: "println!(\"after\");".into(),
        })
        .await
        .expect("apply patch");

    assert_eq!(output.content, "Patched src.rs");
    assert_eq!(
        fs::read_to_string(workspace.path().join("src.rs")).expect("read patched file"),
        "fn main() {\n    println!(\"after\");\n}\n"
    );
}

#[tokio::test]
async fn rejects_missing_or_ambiguous_text_without_modifying_file() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::write(workspace.path().join("src.rs"), "same\nsame\n").expect("write fixture");
    let tool = ApplyPatch::new(workspace.path()).expect("create patch tool");

    for old_text in ["missing", "same"] {
        let error = tool
            .run(ApplyPatchArgs {
                path: "src.rs".into(),
                old_text: old_text.into(),
                new_text: "changed".into(),
            })
            .await
            .expect_err("non-unique anchor should be rejected");
        assert!(matches!(error, ToolError::InvalidArguments(_)));
    }
    assert_eq!(
        fs::read_to_string(workspace.path().join("src.rs")).expect("read original"),
        "same\nsame\n"
    );
}

#[tokio::test]
async fn rejects_empty_anchor_and_oversized_file() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::write(
        workspace.path().join("large.rs"),
        vec![b'a'; MAX_FILE_BYTES + 1],
    )
    .expect("write large file");
    let tool = ApplyPatch::new(workspace.path()).expect("create patch tool");

    let empty_anchor = tool
        .run(ApplyPatchArgs {
            path: "large.rs".into(),
            old_text: String::new(),
            new_text: "replacement".into(),
        })
        .await
        .expect_err("empty anchor should be rejected");
    assert!(matches!(empty_anchor, ToolError::InvalidArguments(_)));

    let oversized_file = tool
        .run(ApplyPatchArgs {
            path: "large.rs".into(),
            old_text: "a".into(),
            new_text: "b".into(),
        })
        .await
        .expect_err("oversized file should be rejected");
    assert!(matches!(oversized_file, ToolError::Execution(message) if message.contains("limit")));
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_symlink_targets() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().expect("create parent directory");
    let workspace_path = parent.path().join("workspace");
    fs::create_dir(&workspace_path).expect("create workspace");
    fs::write(parent.path().join("outside.rs"), "unchanged").expect("write outside file");
    symlink(
        parent.path().join("outside.rs"),
        workspace_path.join("linked.rs"),
    )
    .expect("create symlink");
    let tool = ApplyPatch::new(&workspace_path).expect("create patch tool");

    let error = tool
        .run(ApplyPatchArgs {
            path: "linked.rs".into(),
            old_text: "unchanged".into(),
            new_text: "modified".into(),
        })
        .await
        .expect_err("symlink target should be rejected");

    assert!(matches!(error, ToolError::InvalidArguments(_)));
    assert_eq!(
        fs::read_to_string(parent.path().join("outside.rs")).expect("read outside file"),
        "unchanged"
    );
}

#[tokio::test]
async fn registry_requires_approval_before_patching() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::write(workspace.path().join("src.rs"), "before").expect("write fixture");
    let mut registry = ToolRegistry::new();
    registry
        .register(Typed(
            ApplyPatch::new(workspace.path()).expect("create patch tool"),
        ))
        .expect("register patch tool");
    let arguments = json!({
        "path": "src.rs",
        "old_text": "before",
        "new_text": "after"
    });

    assert!(matches!(
        registry.approval("apply_patch", &arguments),
        Ok(Approval::Ask { .. })
    ));
    let denied = registry
        .execute("apply_patch", arguments.clone(), &ApprovalResponse(false))
        .await;
    assert!(matches!(denied, Err(ToolError::ApprovalDenied(_))));
    assert_eq!(
        fs::read_to_string(workspace.path().join("src.rs")).expect("read unchanged file"),
        "before"
    );

    registry
        .execute("apply_patch", arguments, &ApprovalResponse(true))
        .await
        .expect("approved patch");
    assert_eq!(
        fs::read_to_string(workspace.path().join("src.rs")).expect("read patched file"),
        "after"
    );
}
