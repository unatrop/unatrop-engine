//! Кроссплатформенные веб-инструменты агента (`web_fetch`, `web_search`) поверх [`HttpTransport`]

use std::time::Duration;

use futures_util::StreamExt;
use transport::ByteStream;

mod fetch;
mod html;
mod search;
mod ssrf;

pub use fetch::{WebFetchArgs, WebFetchTool};
pub use html::{ExtractedHtml, html_to_markdown};
pub use search::{SearchBackend, SearchHit, WebSearchArgs, WebSearchTool};
pub use ssrf::{
    is_public_ip, resolve_and_validate_url, validate_public_url, validate_resolved_ips,
};

pub(crate) async fn read_limited_body(
    mut body: ByteStream,
    max_bytes: usize,
    timeout: Duration,
) -> Result<(Vec<u8>, bool), String> {
    let mut out = Vec::new();
    let mut truncated = false;

    let read_fut = async {
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|err| err.to_string())?;
            let remaining = max_bytes.saturating_sub(out.len());
            if remaining == 0 {
                truncated = true;
                break;
            }
            let copy = chunk.len().min(remaining);
            out.extend_from_slice(&chunk[..copy]);
            if copy < chunk.len() {
                truncated = true;
                break;
            }
        }
        Ok::<_, String>(())
    };

    tokio::time::timeout(timeout, read_fut)
        .await
        .map_err(|_| "timed out reading response body".to_owned())??;

    Ok((out, truncated))
}
