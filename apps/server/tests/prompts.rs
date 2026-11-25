use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use uno_daemon::build_default_system_prompt;

#[test]
fn builds_layered_prompt_with_only_unatrop_md_and_environment_last() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is valid")
        .as_nanos();
    let workspace =
        std::env::temp_dir().join(format!("uno-daemon-prompt-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&workspace).expect("create fixture workspace");
    fs::write(workspace.join("UNATROP.md"), "Unatrop instructions").expect("write UNATROP.md");

    let prompt =
        build_default_system_prompt(&workspace, "claude-sonnet-4").expect("build layered prompt");

    let base_idx = prompt.find("You are Unatrop").expect("base prompt");
    let project_idx = prompt.find("Unatrop instructions").expect("UNATROP.md");
    let env_idx = prompt.find("# Environment").expect("environment section");

    assert!(base_idx < project_idx);
    assert!(project_idx < env_idx);
    assert!(prompt.contains("parallel"));

    let _ = fs::remove_dir_all(&workspace);
}
