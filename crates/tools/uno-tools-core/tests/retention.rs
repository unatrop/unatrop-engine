use uno_tools_core::{ItemRetainer, PRUNE_MARKER, RetentionConfig, prune_text};

#[test]
fn prune_text_preserves_short_and_utf8_boundaries() {
    let cfg = RetentionConfig::new(80, 10, 10).unwrap();
    let short = "Привет, мир!";
    let res = prune_text(short, cfg);
    assert!(!res.truncated);
    assert_eq!(res.text, short);

    let long = "абвгдеёжзи".repeat(20);
    let pruned = prune_text(&long, cfg);
    assert!(pruned.truncated);
    assert!(pruned.text.starts_with("абвгдеёжзи"));
    assert!(pruned.text.ends_with("абвгдеёжзи"));
    assert!(pruned.text.contains(PRUNE_MARKER));
}

#[test]
fn item_retainer_counts_seen_and_omitted_accurately() {
    let mut retainer = ItemRetainer::new(2);
    assert!(retainer.push("a"));
    assert!(retainer.push("b"));
    assert!(!retainer.push("c"));
    assert!(!retainer.push("d"));
    let res = retainer.finish();
    assert!(res.truncated);
    assert_eq!(res.items, vec!["a", "b"]);
    assert_eq!(res.seen, 4);
    assert_eq!(res.omitted_count, 2);
}
