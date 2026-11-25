use tempfile::tempdir;
use tool_core::ToolRegistry;
use uno_mcp::{McpServerConfig, register_mcp_tools};

#[test]
fn register_mcp_tools_handles_missing_invalid_and_disabled_configs() {
    let dir = tempdir().expect("tempdir");
    let missing = dir.path().join("does-not-exist.json");
    let invalid = dir.path().join("invalid.json");
    let disabled = dir.path().join("mcp.json");

    std::fs::write(&invalid, "{not valid json").unwrap();
    std::fs::write(
        &disabled,
        r#"{
            "mcpServers": {
                "local": {
                    "command": "nonexistent-mcp-binary",
                    "args": ["--stdio"],
                    "env": {"FOO": "bar"},
                    "disabled": true
                }
            }
        }"#,
    )
    .unwrap();

    let mut registry = ToolRegistry::new();
    register_mcp_tools(&[missing, invalid, disabled], dir.path(), &mut registry);
    assert!(registry.definitions().is_empty());
}

#[test]
fn mcp_server_config_deserializes_camel_case_defaults() {
    let cfg: McpServerConfig =
        serde_json::from_str(r#"{"command":"npx","args":["-y","server"]}"#).unwrap();
    assert_eq!(cfg.command, "npx");
    assert_eq!(cfg.args, vec!["-y", "server"]);
    assert!(cfg.env.is_empty());
    assert!(!cfg.disabled);
}
