use std::sync::Arc;

use async_trait::async_trait;
use protocol::ToolDefinition;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use uno_tools_core::*;

struct Named {
    name: String,
    read_only: bool,
}

fn named(name: &str) -> Named {
    Named {
        name: name.to_owned(),
        read_only: false,
    }
}

#[async_trait]
impl Tool for Named {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: "test tool".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn approval(&self, _arguments: &Value) -> Approval {
        Approval::Never
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(arguments.to_string()))
    }
}

#[derive(Deserialize, JsonSchema)]
struct AddArgs {
    a: i64,
    b: i64,
}

struct Add;

#[async_trait]
impl TypedTool for Add {
    type Args = AddArgs;

    fn name(&self) -> &str {
        "add"
    }

    fn description(&self) -> &str {
        "Adds two numbers"
    }

    fn approval(&self, _args: &AddArgs) -> Approval {
        Approval::Never
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn run(&self, args: AddArgs) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text((args.a + args.b).to_string()))
    }
}

struct ApprovalResponse(bool);

#[async_trait]
impl ApprovalHandler for ApprovalResponse {
    async fn approve(&self, _request: ApprovalRequest) -> Result<bool, String> {
        Ok(self.0)
    }
}

struct DangerousTool {
    executions: Arc<std::sync::atomic::AtomicUsize>,
}

struct DefaultApprovalTool;

#[async_trait]
impl Tool for DefaultApprovalTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "unclassified".into(),
            description: "Uses the default approval policy".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    async fn execute(&self, _arguments: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("executed"))
    }
}

#[async_trait]
impl Tool for DangerousTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "dangerous".into(),
            description: "Requires confirmation".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn approval(&self, _arguments: &Value) -> Approval {
        Approval::Ask {
            reason: "test confirmation".into(),
        }
    }

    async fn execute(&self, _arguments: Value) -> Result<ToolOutput, ToolError> {
        self.executions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ToolOutput::text("executed"))
    }
}

#[derive(Deserialize, JsonSchema)]
struct ShellArgs {
    command: String,
}

struct Shell;

#[async_trait]
impl TypedTool for Shell {
    type Args = ShellArgs;

    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Runs a shell command"
    }

    fn approval(&self, args: &ShellArgs) -> Approval {
        if args.command.contains("rm ") {
            Approval::Ask {
                reason: format!("destructive command: {}", args.command),
            }
        } else {
            Approval::Never
        }
    }

    async fn run(&self, args: ShellArgs) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(format!("ran: {}", args.command)))
    }
}

#[test]
fn rejects_empty_and_blank_names() {
    let mut registry = ToolRegistry::new();

    assert!(matches!(
        registry.register(named("")),
        Err(ToolError::EmptyName)
    ));
    assert!(matches!(
        registry.register(named("   ")),
        Err(ToolError::EmptyName)
    ));
}

#[test]
fn rejects_names_the_api_would_reject() {
    let mut registry = ToolRegistry::new();

    for name in ["bad name", "a.b", "имя", &"a".repeat(65)] {
        assert!(
            matches!(
                registry.register(named(name)),
                Err(ToolError::InvalidName(_))
            ),
            "name {name:?} must be rejected"
        );
    }
}

#[test]
fn accepts_valid_names() {
    let mut registry = ToolRegistry::new();

    registry
        .register(named("read_file"))
        .expect("underscore is valid");
    registry
        .register(named("run-shell"))
        .expect("dash is valid");
    registry
        .register(named(&"a".repeat(64)))
        .expect("64 characters is the limit");
    assert_eq!(registry.len(), 3);
}

#[test]
fn rejects_duplicate_tool() {
    let mut registry = ToolRegistry::new();
    registry
        .register(named("grep"))
        .expect("first registration");

    assert!(matches!(
        registry.register(named("grep")),
        Err(ToolError::DuplicateTool(name)) if name == "grep"
    ));
    assert_eq!(registry.definitions().len(), 1);
}

#[test]
fn definitions_are_sorted_by_name() {
    let mut registry = ToolRegistry::new();
    for name in ["write_file", "grep", "read_file"] {
        registry.register(named(name)).expect("valid tool");
    }

    let names: Vec<&str> = registry
        .definitions()
        .iter()
        .map(|definition| definition.name.as_str())
        .collect();
    assert_eq!(names, ["grep", "read_file", "write_file"]);
}

#[tokio::test]
async fn execute_reports_unknown_tool_as_error() {
    let registry = ToolRegistry::new();

    assert!(matches!(
        registry
            .execute("missing", json!({}), &ApprovalResponse(true))
            .await,
        Err(ToolError::UnknownTool(name)) if name == "missing"
    ));
}

#[tokio::test]
async fn call_turns_unknown_tool_into_model_visible_error() {
    let registry = ToolRegistry::new();

    let output = registry
        .call("missing", json!({}), &ApprovalResponse(true))
        .await;

    assert!(output.is_error);
    assert!(output.content.contains("missing"));
}

#[tokio::test]
async fn call_runs_a_registered_tool() {
    let mut registry = ToolRegistry::new();
    registry.register(named("echo")).expect("valid tool");

    let output = registry
        .call("echo", json!({"x": 1}), &ApprovalResponse(true))
        .await;

    assert!(!output.is_error);
    assert_eq!(output.content, r#"{"x":1}"#);
}

#[test]
fn typed_tool_builds_schema_from_args() {
    let mut registry = ToolRegistry::new();
    registry.register(Typed(Add)).expect("valid tool");

    let definition = &registry.definitions()[0];
    assert_eq!(definition.name, "add");
    assert_eq!(definition.description, "Adds two numbers");
    assert!(definition.input_schema["properties"]["a"].is_object());
    assert!(definition.input_schema["properties"]["b"].is_object());
    assert!(definition.input_schema.get("$schema").is_none());
}

#[tokio::test]
async fn typed_tool_parses_arguments_and_runs() {
    let mut registry = ToolRegistry::new();
    registry.register(Typed(Add)).expect("valid tool");

    let output = registry
        .call("add", json!({"a": 2, "b": 3}), &ApprovalResponse(true))
        .await;

    assert!(!output.is_error);
    assert_eq!(output.content, "5");
}

#[tokio::test]
async fn typed_tool_reports_invalid_arguments_to_the_model() {
    let mut registry = ToolRegistry::new();
    registry.register(Typed(Add)).expect("valid tool");

    let output = registry
        .call("add", json!({"a": "x"}), &ApprovalResponse(true))
        .await;

    assert!(output.is_error);
    assert!(output.content.contains("invalid arguments"));
}

#[test]
fn approval_depends_on_arguments() {
    let mut registry = ToolRegistry::new();
    registry.register(Typed(Shell)).expect("valid tool");

    assert_eq!(
        registry
            .approval("shell", &json!({"command": "ls"}))
            .expect("known tool"),
        Approval::Never
    );
    assert!(matches!(
        registry
            .approval("shell", &json!({"command": "rm -rf build"}))
            .expect("known tool"),
        Approval::Ask { reason } if reason.contains("rm -rf build")
    ));
    assert!(matches!(
        registry.approval("nope", &json!({})),
        Err(ToolError::UnknownTool(_))
    ));
}

#[test]
fn read_only_flag_is_exposed_through_the_registry() {
    let mut registry = ToolRegistry::new();
    registry.register(Typed(Add)).expect("valid tool");
    registry.register(Typed(Shell)).expect("valid tool");

    assert!(registry.is_read_only("add"));
    assert!(!registry.is_read_only("shell"));
    assert!(!registry.is_read_only("missing"));
}

#[tokio::test]
async fn tool_is_not_executed_when_approval_is_denied() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let executions = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry
        .register(DangerousTool {
            executions: executions.clone(),
        })
        .expect("valid tool");

    let result = registry
        .execute("dangerous", json!({}), &ApprovalResponse(false))
        .await;

    assert!(matches!(result, Err(ToolError::ApprovalDenied(name)) if name == "dangerous"));
    assert_eq!(executions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn approval_is_required_by_default() {
    let mut registry = ToolRegistry::new();
    registry.register(DefaultApprovalTool).expect("valid tool");

    assert!(matches!(
        registry.approval("unclassified", &json!({})),
        Ok(Approval::Ask { .. })
    ));
}

#[tokio::test]
async fn policy_read_only_allows_readers_and_denies_mutations_without_prompt() {
    use uno_tools_core::{ApprovalHandler, ApprovalMode, ApprovalPolicy, ApprovalRequest};

    struct NeverPromptHandler;
    #[async_trait]
    impl ApprovalHandler for NeverPromptHandler {
        async fn approve(&self, _request: ApprovalRequest) -> Result<bool, String> {
            Ok(false)
        }
    }

    struct PolicyTestTool {
        name: &'static str,
        read_only: bool,
    }
    #[async_trait]
    impl Tool for PolicyTestTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.name.into(),
                description: "test".into(),
                input_schema: json!({ "type": "object" }),
            }
        }
        fn approval(&self, _arguments: &Value) -> Approval {
            if self.read_only {
                Approval::Never
            } else {
                Approval::Ask {
                    reason: "mutating".into(),
                }
            }
        }
        fn read_only(&self) -> bool {
            self.read_only
        }
        async fn execute(&self, _: Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("ok"))
        }
    }

    let mut registry = ToolRegistry::with_policy(ApprovalPolicy::new(ApprovalMode::ReadOnly));
    registry
        .register(PolicyTestTool {
            name: "read_file",
            read_only: true,
        })
        .expect("valid");
    registry
        .register(PolicyTestTool {
            name: "dangerous",
            read_only: false,
        })
        .expect("valid");

    let observer = NeverPromptHandler;
    let ok = registry
        .execute("read_file", json!({}), &observer)
        .await
        .expect("read_file allowed");
    assert_eq!(ok.content, "ok");

    let denied = registry
        .execute("dangerous", json!({}), &observer)
        .await
        .expect_err("dangerous blocked");
    assert!(matches!(denied, ToolError::ApprovalDenied(msg) if msg.contains("read-only")));
}

#[tokio::test]
async fn policy_auto_approve_workspace_allows_writes_but_asks_for_exec_shell() {
    use uno_tools_core::{ApprovalHandler, ApprovalMode, ApprovalPolicy, ApprovalRequest};

    struct DenyHandler;
    #[async_trait]
    impl ApprovalHandler for DenyHandler {
        async fn approve(&self, _request: ApprovalRequest) -> Result<bool, String> {
            Ok(false)
        }
    }

    struct NamedAskTool(&'static str);
    #[async_trait]
    impl Tool for NamedAskTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.0.into(),
                description: "test".into(),
                input_schema: json!({ "type": "object" }),
            }
        }
        async fn execute(&self, _: Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("ran"))
        }
    }

    let mut registry =
        ToolRegistry::with_policy(ApprovalPolicy::new(ApprovalMode::AutoApproveWorkspace));
    registry
        .register(NamedAskTool("write_file"))
        .expect("valid");
    registry
        .register(NamedAskTool("exec_shell"))
        .expect("valid");

    let observer = DenyHandler;
    let write_out = registry
        .execute("write_file", json!({}), &observer)
        .await
        .expect("write_file auto-approved");
    assert_eq!(write_out.content, "ran");

    let shell_err = registry
        .execute(
            "exec_shell",
            json!({ "command": "rm -rf /tmp/x" }),
            &observer,
        )
        .await
        .expect_err("exec_shell still asks");
    assert!(matches!(shell_err, ToolError::ApprovalDenied(name) if name == "exec_shell"));
}

#[test]
fn policy_safe_command_prefixes_reject_chaining_and_partial_words() {
    use uno_tools_core::{ApprovalPolicy, PolicyDecision};

    let policy = ApprovalPolicy::with_default_safe_commands();
    let ask = Approval::Ask {
        reason: "exec".into(),
    };

    assert_eq!(
        policy.evaluate(
            "exec_shell",
            false,
            ask.clone(),
            &json!({ "command": "git status -s" })
        ),
        PolicyDecision::Allow
    );
    assert!(matches!(
        policy.evaluate(
            "exec_shell",
            false,
            ask.clone(),
            &json!({ "command": "git status && rm -rf /" })
        ),
        PolicyDecision::Ask { .. }
    ));
    assert!(matches!(
        policy.evaluate(
            "exec_shell",
            false,
            ask.clone(),
            &json!({ "command": "lsof -i" })
        ),
        PolicyDecision::Ask { .. }
    ));
    assert!(matches!(
        policy.clone().deny_tool("exec_shell").evaluate(
            "exec_shell",
            false,
            ask,
            &json!({ "command": "pwd" })
        ),
        PolicyDecision::Deny { .. }
    ));
}
