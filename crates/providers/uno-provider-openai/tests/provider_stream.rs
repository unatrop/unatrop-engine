mod support;

use bytes::Bytes;
use futures_util::StreamExt;
use http::StatusCode;
use protocol::{FinishReason, Message, StreamEvent};
use provider::{Provider, ProviderError};
use serde_json::Value;
use support::{provider_with_response, request_with};

const MAX_ERROR_BODY_BYTES: usize = 4 * 1024;

#[tokio::test]
async fn provider_streams_events_over_the_injected_transport() {
    let response = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (provider, captured_request) =
        provider_with_response(StatusCode::OK, Bytes::from(response));

    let events = provider
        .stream(request_with(vec![Message::user("hi")]))
        .await
        .expect("provider should return a stream")
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        &events[..],
        [
            Ok(StreamEvent::TextDelta { delta }),
            Ok(StreamEvent::Finished {
                reason: FinishReason::Stop,
                ..
            })
        ] if delta == "hello"
    ));

    let captured = captured_request
        .lock()
        .expect("request lock should not be poisoned");
    let request = captured.as_ref().expect("transport should receive request");
    assert_eq!(
        request.uri().to_string(),
        "https://example.test/v1/chat/completions"
    );
    assert_eq!(request.headers()["authorization"], "Bearer test-key");

    let body: Value =
        serde_json::from_slice(request.body()).expect("request body should be valid JSON");
    assert_eq!(body["stream"], true);
    assert_eq!(body["messages"][0]["content"], "hi");
}

#[tokio::test]
async fn http_error_body_is_capped() {
    let (provider, _) = provider_with_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        Bytes::from(vec![b'x'; MAX_ERROR_BODY_BYTES + 16]),
    );

    let result = provider
        .stream(request_with(vec![Message::user("hi")]))
        .await;
    let Err(ProviderError::HttpStatus { status, body }) = result else {
        panic!("expected an HTTP status error");
    };

    assert_eq!(status, 500);
    assert_eq!(
        body.len(),
        MAX_ERROR_BODY_BYTES + "\n[error body truncated]".len()
    );
    assert!(body.ends_with("[error body truncated]"));
}
