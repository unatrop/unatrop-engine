use protocol::{ContentPart, Message};
use uno_harness::xml_recovery::{has_truncated_tool_call, recover_xml_tool_calls};

#[test]
fn recovers_qwen_xml_tool_call_from_text() {
    let mut msg = Message::assistant(
        "Проверим:\n<tool_call>\n<function=exec_command>\n<parameter=cmd>\ncargo test\n</parameter>\n<parameter=yield_time_ms>\n60000\n</parameter>\n<parameter=tty>\ntrue\n</parameter>\n</function>\n</tool_call>",
    );
    recover_xml_tool_calls(&mut msg, 3);

    assert_eq!(msg.tool_calls.len(), 1);
    assert_eq!(msg.tool_calls[0].id, "xml_call_3_0");
    assert_eq!(msg.tool_calls[0].name, "exec_command");
    assert_eq!(msg.tool_calls[0].arguments["cmd"], "cargo test");
    assert_eq!(msg.tool_calls[0].arguments["yield_time_ms"], 60_000);
    assert_eq!(msg.tool_calls[0].arguments["tty"], true);
    assert_eq!(msg.content.0, vec![ContentPart::text("Проверим:")]);
}

#[test]
fn detects_truncated_xml_tool_call_fragments() {
    let truncated = Message::assistant("текст\n</parameter>\n</function>\n</tool_call>");
    assert!(has_truncated_tool_call(&truncated));

    let normal = Message::assistant("Обычный финальный ответ.");
    assert!(!has_truncated_tool_call(&normal));
}
