use tempfile::TempDir;
use uno_tools_core::SpillStore;
use uno_tools_fs::LocalSpillStore;

#[tokio::test]
async fn local_spill_store_writes_isolated_file() {
    let dir = TempDir::new().unwrap();
    let store = LocalSpillStore::new(dir.path());
    let saved = store.save("../evil", "out.log", "payload").await.unwrap();
    assert_eq!(saved.bytes, 7);
    assert!(
        saved
            .location
            .starts_with(&dir.path().to_string_lossy().into_owned())
    );
    assert_eq!(std::fs::read_to_string(&saved.location).unwrap(), "payload");
}
