use std::{error::Error, sync::Arc};

use futures_util::StreamExt;
use protocol::{CompletionRequest, Message, StreamEvent};
use provider::Provider;
use reqwest_transport::ReqwestTransport;
use uno_provider_openai::OpenAiCompatibleProvider;

const GEMINI_OPENAI_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta/openai";

#[tokio::test]
#[ignore = "live API test; requires GEMINI_API_KEY and GEMINI_MODEL"]
async fn gemini_openai_compatible_stream_smoke_test() -> Result<(), Box<dyn Error>> {
    let api_key = std::env::var("GEMINI_API_KEY")?;
    let model = std::env::var("GEMINI_MODEL")?;
    if api_key.trim().is_empty() || model.trim().is_empty() {
        return Err("GEMINI_API_KEY and GEMINI_MODEL must not be empty".into());
    }

    let transport = Arc::new(ReqwestTransport::new()?);

    let provider =
        OpenAiCompatibleProvider::new("gemini", GEMINI_OPENAI_ENDPOINT, api_key, transport);

    let request = CompletionRequest {
        model,
        messages: vec![Message::user("Reply with exactly PONG and nothing else.")],
        tools: vec![],
        temperature: Some(0.0),
        max_tokens: Some(16),
        thinking_level: None,
        ..CompletionRequest::default()
    };

    let mut stream = provider.stream(request).await?;
    let mut text = String::new();
    let mut finished = false;

    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::TextDelta { delta } => text.push_str(&delta),
            StreamEvent::Error { message, .. } => return Err(message.into()),
            StreamEvent::Finished { .. } => finished = true,
            _ => {}
        }
    }

    assert!(finished, "provider stream should finish");
    assert!(
        text.to_ascii_lowercase().contains("pong"),
        "unexpected model response: {text:?}"
    );

    Ok(())
}
