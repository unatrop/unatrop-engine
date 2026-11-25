use std::sync::Arc;

use tempfile::TempDir;
use uno_tools_core::TypedTool;
use uno_tools_fs::{
    ApplyPatch, ApplyPatchArgs, FileObservationTracker, ReadFile, ReadFileArgs, WriteFile,
    WriteFileArgs,
};

#[tokio::test]
async fn edit_requires_prior_read_and_detects_external_modification() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("demo.txt");
    std::fs::write(&file_path, "hello world").unwrap();

    let tracker = Arc::new(FileObservationTracker::new());
    let read_tool = ReadFile::new(dir.path())
        .unwrap()
        .with_observation_tracker(Arc::clone(&tracker));
    let patch_tool = ApplyPatch::new(dir.path())
        .unwrap()
        .with_observation_tracker(Arc::clone(&tracker));
    let write_tool = WriteFile::new(dir.path())
        .unwrap()
        .with_observation_tracker(Arc::clone(&tracker));

    let err = patch_tool
        .run(ApplyPatchArgs {
            path: "demo.txt".into(),
            old_text: "hello".into(),
            new_text: "hi".into(),
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("reading"));

    read_tool
        .run(ReadFileArgs {
            path: "demo.txt".into(),
            offset: None,
            limit: None,
        })
        .await
        .unwrap();
    patch_tool
        .run(ApplyPatchArgs {
            path: "demo.txt".into(),
            old_text: "hello".into(),
            new_text: "hi".into(),
        })
        .await
        .unwrap();

    std::fs::write(&file_path, "external edit!!").unwrap();
    let conflict = write_tool
        .run(WriteFileArgs {
            path: "demo.txt".into(),
            content: "overwrite".into(),
        })
        .await
        .unwrap_err();
    assert!(conflict.to_string().contains("changed on disk"));

    write_tool
        .run(WriteFileArgs {
            path: "new.txt".into(),
            content: "brand new".into(),
        })
        .await
        .unwrap();
}
