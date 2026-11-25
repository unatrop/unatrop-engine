use std::{error::Error, sync::Arc};

use futures_util::StreamExt;
use protocol::{CompletionRequest, Message, StreamEvent};
use provider::Provider;
use reqwest_transport::ReqwestTransport;
use uno_provider_openai::OpenAiCompatibleProvider;

#[tokio::test]
#[ignore = "live API test; requires OPENROUTER_BASE_URL, OPENROUTER_API_KEY and a free OPENROUTER_MODEL"]
async fn openrouter_free_model_stream_smoke_test() -> Result<(), Box<dyn Error>> {
    let base_url = std::env::var("OPENROUTER_BASE_URL")?;
    let api_key = std::env::var("OPENROUTER_API_KEY")?;
    let model = std::env::var("OPENROUTER_MODEL")?;
    if base_url.trim().is_empty() || api_key.trim().is_empty() {
        return Err("OPENROUTER_BASE_URL and OPENROUTER_API_KEY must not be empty".into());
    }
    if model.trim().is_empty() || (model != "openrouter/free" && !model.ends_with(":free")) {
        return Err("OPENROUTER_MODEL must be openrouter/free or end in :free".into());
    }

    let transport = Arc::new(ReqwestTransport::new()?);
    let provider = OpenAiCompatibleProvider::new("openrouter", base_url, api_key, transport);
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
