use serde_json::json;
use tempfile::tempdir;
use tool_core::{Approval, Tool};
use uno_skills::{ReadSkillTool, discover_skills, format_skills_section};

#[tokio::test]
async fn discovers_skills_with_override_and_loads_via_tool() {
    let base = tempdir().expect("tempdir");
    let global_dir = base.path().join("global");
    let local_dir = base.path().join("local");
    std::fs::create_dir_all(global_dir.join("deploy")).unwrap();
    std::fs::create_dir_all(global_dir.join("empty")).unwrap();
    std::fs::create_dir_all(local_dir.join("deploy")).unwrap();
    std::fs::create_dir_all(local_dir.join("plain")).unwrap();

    std::fs::write(
        global_dir.join("deploy/SKILL.md"),
        "---\nname: deploy\ndescription: Global deploy\n---\nGlobal steps",
    )
    .unwrap();
    std::fs::write(global_dir.join("empty/SKILL.md"), "   \n").unwrap();
    std::fs::write(
        local_dir.join("deploy/SKILL.md"),
        "---\nname: deploy\ndescription: Local deploy\n---\nLocal steps",
    )
    .unwrap();
    std::fs::write(
        local_dir.join("plain/SKILL.md"),
        "# Plain\nFallback description line\nMore details",
    )
    .unwrap();

    let skills = discover_skills(&[global_dir, local_dir]);
    assert_eq!(skills.len(), 2);
    assert_eq!(skills[0].name, "deploy");
    assert_eq!(skills[0].description, "Local deploy");
    assert!(skills[0].content.contains("Local steps"));
    assert_eq!(skills[1].name, "plain");
    assert_eq!(skills[1].description, "Fallback description line");

    let section = format_skills_section(&skills).expect("skills section");
    assert!(section.contains("# Available Skills"));
    assert!(section.contains("- `deploy`: Local deploy"));
    assert!(format_skills_section(&[]).is_none());

    let tool = ReadSkillTool::new(skills);
    assert_eq!(tool.definition().name, "read_skill");
    assert!(tool.read_only());
    assert_eq!(tool.approval(&json!({"name": "deploy"})), Approval::Never);

    let out = tool
        .execute(json!({"name": "deploy"}))
        .await
        .expect("read_skill");
    assert!(!out.is_error);
    assert!(out.content.contains("Local steps"));

    let missing = tool
        .execute(json!({"name": "nonexistent"}))
        .await
        .expect("missing skill returns tool error output");
    assert!(missing.is_error);
    assert!(missing.content.contains("not found"));
}
