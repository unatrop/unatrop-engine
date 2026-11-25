use std::fs;

use uno_tools_core::{ToolError, ToolRegistry, Typed, TypedTool};
use uno_tools_fs::{ListFiles, ListFilesArgs, MAX_DIRECTORY_ENTRIES};

#[tokio::test]
async fn lists_only_immediate_entries_in_sorted_order() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::create_dir(workspace.path().join("src")).expect("create src directory");
    fs::write(workspace.path().join("z.txt"), "z").expect("write z");
    fs::write(workspace.path().join("a.txt"), "a").expect("write a");
    fs::write(workspace.path().join("src/lib.rs"), "mod").expect("write nested file");
    let tool = ListFiles::new(workspace.path()).expect("create list tool");

    let output = tool
        .run(ListFilesArgs {
            path: None,
            recursive: None,
            depth: None,
            limit: None,
        })
        .await
        .expect("list root");

    assert_eq!(
        output.content,
        "Entries in \".\" (non-recursive):\na.txt\nsrc/\nz.txt"
    );
}

#[tokio::test]
async fn lists_a_workspace_relative_directory() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::create_dir(workspace.path().join("src")).expect("create src directory");
    fs::write(workspace.path().join("src/lib.rs"), "mod").expect("write nested file");
    let tool = ListFiles::new(workspace.path()).expect("create list tool");

    let output = tool
        .run(ListFilesArgs {
            path: Some("src".into()),
            recursive: None,
            depth: None,
            limit: None,
        })
        .await
        .expect("list src");

    assert_eq!(
        output.content,
        "Entries in \"src\" (non-recursive):\nlib.rs"
    );
}

#[tokio::test]
async fn rejects_parent_paths_and_files() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::write(workspace.path().join("file.txt"), "content").expect("write file");
    let tool = ListFiles::new(workspace.path()).expect("create list tool");

    let traversal = tool
        .run(ListFilesArgs {
            path: Some("../".into()),
            recursive: None,
            depth: None,
            limit: None,
        })
        .await
        .expect_err("parent path should be rejected");
    assert!(matches!(traversal, ToolError::InvalidArguments(_)));

    let file = tool
        .run(ListFilesArgs {
            path: Some("file.txt".into()),
            recursive: None,
            depth: None,
            limit: None,
        })
        .await
        .expect_err("file path should be rejected");
    assert!(matches!(file, ToolError::InvalidArguments(_)));
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_directories_that_resolve_outside_the_workspace() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().expect("create parent directory");
    let workspace_path = parent.path().join("workspace");
    fs::create_dir(&workspace_path).expect("create workspace");
    fs::create_dir(parent.path().join("outside")).expect("create outside directory");
    symlink(
        parent.path().join("outside"),
        workspace_path.join("outside-link"),
    )
    .expect("create symlink");
    let tool = ListFiles::new(&workspace_path).expect("create list tool");

    let error = tool
        .run(ListFilesArgs {
            path: Some("outside-link".into()),
            recursive: None,
            depth: None,
            limit: None,
        })
        .await
        .expect_err("outside directory should be rejected");

    assert!(matches!(error, ToolError::InvalidArguments(_)));
}

#[tokio::test]
async fn truncates_large_directories_and_registers_read_only() {
    let workspace = tempfile::tempdir().expect("create workspace");
    for index in 0..=MAX_DIRECTORY_ENTRIES {
        fs::write(workspace.path().join(format!("file-{index:03}.txt")), "").expect("write file");
    }
    let mut registry = ToolRegistry::new();
    registry
        .register(Typed(
            ListFiles::new(workspace.path()).expect("create list tool"),
        ))
        .expect("register list tool");

    let definitions = registry.definitions();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].name, "list_files");
    assert!(registry.is_read_only("list_files"));

    let output = ListFiles::new(workspace.path())
        .expect("create list tool")
        .run(ListFilesArgs {
            path: None,
            recursive: None,
            depth: None,
            limit: None,
        })
        .await
        .expect("list root");
    assert!(output.content.contains(&format!(
        "additional entries omitted (limit {MAX_DIRECTORY_ENTRIES})"
    )));
}

#[tokio::test]
async fn lists_recursively_with_depth_limit_and_skips_build_directories() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::create_dir_all(workspace.path().join("src/nested")).expect("create nested source dirs");
    fs::create_dir_all(workspace.path().join("target/debug")).expect("create target dirs");
    fs::write(workspace.path().join("src/lib.rs"), "mod").expect("write source");
    fs::write(workspace.path().join("src/nested/deep.rs"), "mod").expect("write nested source");
    fs::write(workspace.path().join("target/debug/cache"), "ignored").expect("write cache");
    let tool = ListFiles::new(workspace.path()).expect("create list tool");

    let output = tool
        .run(ListFilesArgs {
            path: None,
            recursive: Some(true),
            depth: Some(2),
            limit: Some(100),
        })
        .await
        .expect("list recursively");

    assert_eq!(
        output.content.replace("\\\\", "/").replace('\\', "/"),
        "Entries in \".\" (recursive, depth 2):\nsrc/\nsrc/lib.rs\nsrc/nested/"
    );
    assert!(!output.content.contains("target"));
}

#[tokio::test]
async fn validates_recursive_listing_bounds() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let tool = ListFiles::new(workspace.path()).expect("create list tool");

    for args in [
        ListFilesArgs {
            path: None,
            recursive: Some(true),
            depth: Some(0),
            limit: None,
        },
        ListFilesArgs {
            path: None,
            recursive: Some(true),
            depth: None,
            limit: Some(0),
        },
    ] {
        let error = tool
            .run(args)
            .await
            .expect_err("invalid bounds should be rejected");
        assert!(matches!(error, ToolError::InvalidArguments(_)));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn recursive_listing_shows_but_does_not_follow_symlink_directories() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().expect("create parent directory");
    let workspace_path = parent.path().join("workspace");
    let outside = parent.path().join("outside");
    fs::create_dir(&workspace_path).expect("create workspace");
    fs::create_dir(&outside).expect("create outside directory");
    fs::write(outside.join("secret.txt"), "outside").expect("write outside file");
    symlink(&outside, workspace_path.join("linked")).expect("create directory symlink");
    let tool = ListFiles::new(&workspace_path).expect("create list tool");

    let output = tool
        .run(ListFilesArgs {
            path: None,
            recursive: Some(true),
            depth: Some(3),
            limit: Some(100),
        })
        .await
        .expect("list recursively");

    assert!(output.content.contains("linked@"));
    assert!(!output.content.contains("secret.txt"));
}
