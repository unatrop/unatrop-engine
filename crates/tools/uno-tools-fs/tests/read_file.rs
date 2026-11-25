use std::fs;

#[cfg(unix)]
use std::os::unix::fs::symlink;
use uno_tools_core::{ToolError, ToolRegistry, Typed, TypedTool};
use uno_tools_fs::{MAX_FILE_BYTES, ReadFile, ReadFileArgs};

#[tokio::test]
async fn reads_a_utf8_file_inside_the_workspace() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::create_dir(workspace.path().join("src")).expect("create src directory");
    fs::write(workspace.path().join("src/lib.rs"), "pub fn ready() {}").expect("write source file");
    let tool = ReadFile::new(workspace.path()).expect("create read tool");

    let output = tool
        .run(ReadFileArgs {
            path: "src/lib.rs".into(),
            offset: None,
            limit: None,
        })
        .await
        .expect("read source file");

    assert_eq!(output.content, "pub fn ready() {}");
    assert!(!output.is_error);
}

#[tokio::test]
async fn rejects_parent_directory_components() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let tool = ReadFile::new(workspace.path()).expect("create read tool");

    let error = tool
        .run(ReadFileArgs {
            path: "../secret.txt".into(),
            offset: None,
            limit: None,
        })
        .await
        .expect_err("parent path should be rejected");

    assert!(matches!(error, ToolError::InvalidArguments(_)));
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_symlinks_that_resolve_outside_the_workspace() {
    let parent = tempfile::tempdir().expect("create parent directory");
    let workspace_path = parent.path().join("workspace");
    fs::create_dir(&workspace_path).expect("create workspace");
    fs::write(parent.path().join("secret.txt"), "secret").expect("write outside file");
    symlink(
        parent.path().join("secret.txt"),
        workspace_path.join("outside-link"),
    )
    .expect("create symlink");
    let tool = ReadFile::new(&workspace_path).expect("create read tool");

    let error = tool
        .run(ReadFileArgs {
            path: "outside-link".into(),
            offset: None,
            limit: None,
        })
        .await
        .expect_err("outside symlink should be rejected");

    assert!(matches!(error, ToolError::InvalidArguments(_)));
}

#[tokio::test]
async fn rejects_files_larger_than_the_limit() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::write(
        workspace.path().join("large.txt"),
        vec![b'a'; MAX_FILE_BYTES + 1],
    )
    .expect("write large file");
    let tool = ReadFile::new(workspace.path()).expect("create read tool");

    let error = tool
        .run(ReadFileArgs {
            path: "large.txt".into(),
            offset: None,
            limit: None,
        })
        .await
        .expect_err("oversize file should be rejected");

    assert!(matches!(error, ToolError::Execution(message) if message.contains("limit")));
}

#[tokio::test]
async fn reads_a_one_based_line_range() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::write(
        workspace.path().join("lines.txt"),
        "one\ntwo\nthree\nfour\n",
    )
    .expect("write fixture");
    let tool = ReadFile::new(workspace.path()).expect("create read tool");

    let output = tool
        .run(ReadFileArgs {
            path: "lines.txt".into(),
            offset: Some(2),
            limit: Some(2),
        })
        .await
        .expect("read line range");

    assert_eq!(output.content, "two\nthree\n");
}

#[tokio::test]
async fn rejects_zero_line_range_values_and_returns_empty_past_end() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::write(workspace.path().join("lines.txt"), "one\ntwo\n").expect("write fixture");
    let tool = ReadFile::new(workspace.path()).expect("create read tool");

    for (offset, limit) in [(Some(0), None), (None, Some(0))] {
        let error = tool
            .run(ReadFileArgs {
                path: "lines.txt".into(),
                offset,
                limit,
            })
            .await
            .expect_err("zero range values should be rejected");
        assert!(matches!(error, ToolError::InvalidArguments(_)));
    }

    let output = tool
        .run(ReadFileArgs {
            path: "lines.txt".into(),
            offset: Some(3),
            limit: None,
        })
        .await
        .expect("read beyond end");
    assert!(output.content.is_empty());
}

#[test]
fn registers_as_a_read_only_tool() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let mut registry = ToolRegistry::new();
    registry
        .register(Typed(
            ReadFile::new(workspace.path()).expect("create read tool"),
        ))
        .expect("register read tool");

    let definitions = registry.definitions();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].name, "read_file");
    assert!(registry.is_read_only("read_file"));
}
