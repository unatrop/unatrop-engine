use std::sync::Arc;

use async_trait::async_trait;
use futures_util::stream;
use harness::Harness;
use protocol::{CompletionRequest, Message, StreamEvent, ToolCall};
use provider::{BoxStream, Provider, ProviderError};
use serde_json::json;
use session::Session;
use tempfile::{TempDir, tempdir};
use tool_core::{ApprovalHandler, ApprovalRequest, ToolRegistry};
use uno_session_jsonl::{JsonlSessionStore, load_session_json, save_session_json};

struct DummyProvider;

#[async_trait]
impl Provider for DummyProvider {
    fn name(&self) -> &'static str {
        "dummy"
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        Ok(Box::pin(stream::empty()))
    }
}

struct DummyApproval;

#[async_trait]
impl ApprovalHandler for DummyApproval {
    async fn approve(&self, _request: ApprovalRequest) -> Result<bool, String> {
        Ok(true)
    }
}

fn dummy_harness() -> Arc<Harness> {
    Arc::new(
        Harness::new(
            Arc::new(DummyProvider),
            ToolRegistry::new(),
            Arc::new(DummyApproval),
            4,
        )
        .expect("build harness"),
    )
}

#[test]
fn jsonl_store_appends_flushes_and_reloads_messages() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nested/session.jsonl");
    let store = JsonlSessionStore::open(&path).unwrap();

    store.append(&Message::system("sys")).unwrap();
    store.append(&Message::user("hello")).unwrap();
    store.append(&Message::assistant("world")).unwrap();
    store.flush().unwrap();

    let messages = JsonlSessionStore::load_messages(&path).unwrap();
    assert_eq!(messages.len(), 3);
}

#[test]
fn saves_and_loads_session_history_and_repairs_dangling_calls() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("nested/session.json");

    let mut assistant = Message::assistant("calling");
    assistant.tool_calls.push(ToolCall {
        id: "call_1".into(),
        name: "read_file".into(),
        arguments: json!({"path": "Cargo.toml"}),
        signature: None,
    });

    let session = Session::new(dummy_harness(), vec![Message::user("hello"), assistant]);
    save_session_json(&session, &path).expect("save session");

    let loaded = load_session_json(dummy_harness(), &path).expect("load session");
    let history = loaded.history().expect("history");
    assert_eq!(history.len(), 3);
    assert_eq!(
        history[2],
        Message::tool_result("call_1", "Tool error: run was cancelled")
    );
}
