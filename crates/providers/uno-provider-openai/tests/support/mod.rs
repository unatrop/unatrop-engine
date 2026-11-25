use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream;
use http::{Request, Response, StatusCode};
use protocol::{CompletionRequest, Message};
use provider::HttpTransport;
use transport::{ByteStream, TransportError};
use uno_provider_openai::OpenAiCompatibleProvider;

pub type CapturedRequest = Arc<Mutex<Option<Request<Bytes>>>>;

struct MockTransport {
    status: StatusCode,
    body: Bytes,
    captured_request: CapturedRequest,
}

pub fn provider_with_response(
    status: StatusCode,
    body: impl Into<Bytes>,
) -> (OpenAiCompatibleProvider, CapturedRequest) {
    let captured_request = Arc::new(Mutex::new(None));
    let transport = MockTransport {
        status,
        body: body.into(),
        captured_request: captured_request.clone(),
    };
    let provider = OpenAiCompatibleProvider::new(
        "mock",
        "https://example.test/v1",
        "test-key",
        Arc::new(transport),
    );
    (provider, captured_request)
}

pub fn request_with(messages: Vec<Message>) -> CompletionRequest {
    CompletionRequest {
        model: "test-model".into(),
        messages,
        tools: vec![],
        temperature: None,
        max_tokens: None,
        thinking_level: None,
        ..CompletionRequest::default()
    }
}

#[async_trait]
impl HttpTransport for MockTransport {
    async fn send(&self, request: Request<Bytes>) -> Result<Response<ByteStream>, TransportError> {
        *self
            .captured_request
            .lock()
            .expect("request lock should not be poisoned") = Some(request);

        let body: ByteStream = Box::pin(stream::iter([Ok(self.body.clone())]));
        let mut response = Response::new(body);
        *response.status_mut() = self.status;
        Ok(response)
    }
}
