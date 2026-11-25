use serde_json::json;
use uno_tools_core::{
    Approval, ApprovalMode, ApprovalPolicy, PolicyDecision, ToolRule, extract_command_prefix,
    is_known_safe_command,
};

#[test]
fn allows_safe_readonly_and_diagnostic_pipelines() {
    assert!(is_known_safe_command("pwd"));
    assert!(is_known_safe_command("ls -la | grep Cargo"));
    assert!(is_known_safe_command("df -h && free -m && uptime"));
    assert!(is_known_safe_command("bash -lc 'ps aux | head -n 10'"));
    assert!(is_known_safe_command("systemctl status systemd-journald"));
    assert!(is_known_safe_command("journalctl -n 50 --no-pager"));
    assert!(is_known_safe_command("sysctl vm.swappiness"));
    assert!(is_known_safe_command("git log --oneline -n 5"));
}

#[test]
fn rejects_mutating_flags_redirections_substitutions_and_local_paths() {
    assert!(!is_known_safe_command("sed -i 's/a/b/' file.txt"));
    assert!(!is_known_safe_command("find . -delete"));
    assert!(!is_known_safe_command("find . -exec rm {} ;"));
    assert!(!is_known_safe_command("sysctl -w vm.swappiness=10"));
    assert!(!is_known_safe_command("systemctl restart nginx"));
    assert!(!is_known_safe_command("git branch -D feature"));
    assert!(!is_known_safe_command("git diff --ext-diff"));
    assert!(!is_known_safe_command("./ls"));
    assert!(!is_known_safe_command("/tmp/cat /etc/passwd"));
    assert!(!is_known_safe_command("cat /etc/passwd"));
    assert!(!is_known_safe_command("head ../secret.txt"));
    assert!(!is_known_safe_command("env"));
    assert!(!is_known_safe_command("printenv"));
    assert!(!is_known_safe_command("cargo check"));
    assert!(!is_known_safe_command("ls > out.txt"));
    assert!(!is_known_safe_command("echo $(whoami)"));
    assert!(!is_known_safe_command("echo `id`"));
    assert!(!is_known_safe_command("sleep 10 &"));
}

#[test]
fn extract_command_prefix_rejects_dangerous_bins_and_flag_bypasses() {
    assert_eq!(extract_command_prefix(&json!({"cmd": "bash -c id"})), None);
    assert_eq!(
        extract_command_prefix(&json!({"cmd": "python3 script.py"})),
        None
    );
    assert_eq!(extract_command_prefix(&json!({"cmd": "rm -rf foo"})), None);
    assert_eq!(
        extract_command_prefix(&json!({"cmd": "git -c core.sshCommand=evil status"})),
        None
    );
    assert_eq!(
        extract_command_prefix(&json!({"cmd": "cargo build"})),
        Some("cargo build".to_owned())
    );
}

#[test]
fn policy_enforces_require_escalated_even_for_safe_commands() {
    let policy = ApprovalPolicy::with_default_safe_commands();

    let safe_default = policy.evaluate(
        "exec_command",
        false,
        Approval::Ask {
            reason: "execute".into(),
        },
        &json!({ "cmd": "df -h | head -n 5" }),
    );
    assert_eq!(safe_default, PolicyDecision::Allow);

    let escalated = policy.evaluate(
        "exec_command",
        false,
        Approval::Ask {
            reason: "[ESCALATED] inspect system logs".into(),
        },
        &json!({
            "cmd": "df -h",
            "sandbox_permissions": "require_escalated",
            "justification": "inspect system logs"
        }),
    );
    assert!(matches!(escalated, PolicyDecision::Ask { .. }));

    let readonly_escalated = ApprovalPolicy::new(ApprovalMode::ReadOnly).evaluate(
        "exec_command",
        false,
        Approval::Ask {
            reason: "escalated".into(),
        },
        &json!({
            "cmd": "df -h",
            "sandbox_permissions": "require_escalated"
        }),
    );
    assert!(matches!(readonly_escalated, PolicyDecision::Deny { .. }));
}

#[test]
fn read_only_modes_preserve_explicit_approval_for_read_only_tools() {
    let approval = Approval::Ask {
        reason: "read sensitive browser data".into(),
    };

    for mode in [ApprovalMode::ReadOnly, ApprovalMode::AutoApproveWorkspace] {
        let policy = ApprovalPolicy::new(mode);
        assert_eq!(
            policy.evaluate("browser_cookies", true, approval.clone(), &json!({})),
            PolicyDecision::Ask {
                reason: "read sensitive browser data".into()
            }
        );
        assert_eq!(
            policy.evaluate("browser_snapshot", true, Approval::Never, &json!({})),
            PolicyDecision::Allow
        );
    }
}

#[test]
fn policy_supports_configurable_rule_lists_and_tool_sets() {
    let policy = ApprovalPolicy::new(ApprovalMode::AutoApproveWorkspace)
        .workspace_write_tools(["custom_edit"])
        .shell_tools(["custom_shell"])
        .allow_command_prefixes(["echo"])
        .with_rules([
            ("web_fetch", ToolRule::Allow),
            (
                "delete_db",
                ToolRule::Deny {
                    reason: "destructive".into(),
                },
            ),
        ])
        .ask_tool("send_email", "confirm recipient");

    assert_eq!(
        policy.evaluate("web_fetch", true, Approval::Never, &json!({})),
        PolicyDecision::Allow
    );
    assert_eq!(
        policy.evaluate("delete_db", false, Approval::Never, &json!({})),
        PolicyDecision::Deny {
            reason: "destructive".into()
        }
    );
    assert_eq!(
        policy.evaluate("send_email", true, Approval::Never, &json!({})),
        PolicyDecision::Ask {
            reason: "confirm recipient".into()
        }
    );
    assert_eq!(
        policy.evaluate(
            "custom_edit",
            false,
            Approval::Ask {
                reason: "write".into()
            },
            &json!({})
        ),
        PolicyDecision::Allow
    );
    assert_eq!(
        policy.evaluate(
            "custom_shell",
            false,
            Approval::Ask {
                reason: "shell".into()
            },
            &json!({ "cmd": "echo hi" })
        ),
        PolicyDecision::Allow
    );
}
