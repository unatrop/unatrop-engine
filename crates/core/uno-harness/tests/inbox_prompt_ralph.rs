use protocol::Message;
use uno_harness::{
    AgentInbox, MIN_DEDUPE_TOOL_CHARS, RalphReport, RalphStatus, SystemPromptBuilder,
    dedupe_consecutive_tool_result, section_order,
};

#[test]
fn inbox_orders_next_step_and_next_turn_separately() {
    let inbox = AgentInbox::new();
    inbox.push_next_step(Message::user("steer 1"));
    inbox.push_next_turn(Message::user("turn 1"));
    inbox.push_next_step(Message::user("steer 2"));

    assert!(inbox.has_pending());
    assert_eq!(inbox.drain_next_step().len(), 2);
    assert!(inbox.drain_next_step().is_empty());
    assert!(inbox.pop_next_turn().is_some());
    assert!(!inbox.has_pending());
}

#[test]
fn prompt_builder_sorts_by_order_and_replaces_named_slot() {
    let mut builder = SystemPromptBuilder::new();
    builder
        .set_section("suffix", section_order::PERSONA_SUFFIX, "End")
        .set_section("identity", section_order::HARNESS_IDENTITY, "Base")
        .set_section("persona", section_order::PERSONA_PREFIX, "Old persona")
        .set_section("persona", section_order::PERSONA_PREFIX, "New persona");

    assert_eq!(builder.build(), "Base\n\nNew persona\n\nEnd");
}

#[test]
fn ralph_report_enforces_status_invariants() {
    let valid_continue = RalphReport {
        status: RalphStatus::Continue,
        summary: "step 1 done".into(),
        evidence: vec![],
        next_steps: vec!["step 2".into()],
        blocker: String::new(),
    };
    assert!(valid_continue.validate(4096).is_ok());

    let bad_complete = RalphReport {
        status: RalphStatus::Complete,
        summary: "done".into(),
        evidence: vec![],
        next_steps: vec![],
        blocker: String::new(),
    };
    assert!(bad_complete.validate(4096).is_err());
}

#[test]
fn dedupes_identical_consecutive_tool_results() {
    let text = "X".repeat(MIN_DEDUPE_TOOL_CHARS + 10);
    let messages = vec![Message::tool_result("a", text.clone())];

    let deduped = dedupe_consecutive_tool_result(&messages, text);

    assert!(deduped.starts_with("[duplicate of tool result a"));
    assert!(deduped.len() < 120);
}

#[test]
fn keeps_different_or_small_tool_results() {
    let messages = vec![Message::tool_result("a", "same")];

    assert_eq!(
        dedupe_consecutive_tool_result(&messages, "other".to_string()),
        "other"
    );
    assert_eq!(
        dedupe_consecutive_tool_result(&messages, "same".to_string()),
        "same"
    );
}
