use uno_tools_core::{InMemorySpillStore, SpillStore, encode_safe_segment};

#[test]
fn encode_safe_segment_blocks_traversal_and_is_injective() {
    assert_eq!(encode_safe_segment(""), "~");
    assert_eq!(encode_safe_segment("."), "~002E");
    assert_eq!(encode_safe_segment(".."), "~002E~002E");
    assert!(!encode_safe_segment("../../etc/passwd").contains('/'));
    assert!(!encode_safe_segment("a~b").contains("a~b"));
    assert_ne!(encode_safe_segment("a/b"), encode_safe_segment("a_b"));
}

#[tokio::test]
async fn in_memory_spill_store_roundtrips() {
    let store = InMemorySpillStore::new();
    let r = store
        .save("sess-1", "exec.log", "hello world")
        .await
        .unwrap();
    assert_eq!(r.bytes, 11);
    assert_eq!(store.get(&r.location).as_deref(), Some("hello world"));
}
