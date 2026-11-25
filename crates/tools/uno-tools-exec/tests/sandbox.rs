use std::path::Path;

use uno_tools_exec::{
    SandboxBackend, SandboxFailureKind, build_seatbelt_profile, classify_sandbox_failure,
};

#[test]
fn seatbelt_profile_quotes_paths_and_allows_dev_null() {
    let ws = Path::new("/Users/dev/my \"project\"");
    let profile = build_seatbelt_profile(&[ws]);
    assert!(profile.contains("(deny file-write*)"));
    assert!(profile.contains("(literal \"/dev/null\")"));
    assert!(profile.contains("(subpath \"/Users/dev/my \\\"project\\\"\")"));
}

#[test]
fn classifies_runner_failure_vs_policy_denial() {
    assert_eq!(
        classify_sandbox_failure(
            SandboxBackend::Bubblewrap,
            Some(1),
            "touch: cannot touch '/etc/x': Read-only file system"
        ),
        Some(SandboxFailureKind::Denied)
    );
    assert_eq!(
        classify_sandbox_failure(
            SandboxBackend::Bubblewrap,
            Some(1),
            "bwrap: setting up uid map: Permission denied / namespace error"
        ),
        Some(SandboxFailureKind::RunnerUnavailable)
    );
    assert_eq!(
        classify_sandbox_failure(
            SandboxBackend::Seatbelt,
            Some(1),
            "sh: /etc/test: Operation not permitted"
        ),
        Some(SandboxFailureKind::Denied)
    );
}
