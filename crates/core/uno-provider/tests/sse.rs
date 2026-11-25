use std::{ops::ControlFlow, time::Duration};

use bytes::Bytes;
use futures_util::{StreamExt, stream};
use protocol::{FinishReason, StreamEvent};
use transport::TransportError;
use uno_provider::{ProviderError, SseDecoder, SseParser, sse_stream_with_timeout};

struct EchoDecoder;

impl SseDecoder for EchoDecoder {
    fn on_event(
        &mut self,
        event: &str,
        data: &str,
        out: &mut Vec<Result<StreamEvent, ProviderError>>,
    ) -> ControlFlow<()> {
        out.push(Ok(StreamEvent::TextDelta {
            delta: format!("{event}:{data}"),
        }));
        ControlFlow::Continue(())
    }

    fn on_eof(&mut self, out: &mut Vec<Result<StreamEvent, ProviderError>>) {
        out.push(Ok(StreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        }));
    }
}

#[test]
fn parser_handles_bom_split_crlf_utf8_and_multiline_data() {
    let mut parser = SseParser::new();
    let mut frames = Vec::new();

    let chunks: &[&[u8]] = &[
        b"\xEF\xBB\xBF: comment\r",
        b"\nevent: delta\r",
        b"\ndata: \xD0",
        b"\xBF\xD1\x80\xD0\xB8\ndata: \xD0\xB2\xD0\xB5\xD1\x82\n\n",
    ];

    for chunk in chunks {
        let _ = parser
            .feed_chunk(chunk, |frame| {
                frames.push((frame.event.to_owned(), frame.data.to_owned()));
                ControlFlow::Continue(())
            })
            .expect("valid utf8");
    }
    parser
        .finish(|frame| {
            frames.push((frame.event.to_owned(), frame.data.to_owned()));
            ControlFlow::Continue(())
        })
        .expect("finish");

    assert_eq!(frames, vec![("delta".to_owned(), "при\nвет".to_owned())]);
}

#[tokio::test]
async fn sse_idle_timeout_emits_retryable_transport_error() {
    let pending = stream::pending::<Result<Bytes, TransportError>>();
    let mut events = sse_stream_with_timeout(pending, EchoDecoder, Duration::from_millis(25));

    let first = events.next().await;
    assert!(matches!(first, Some(Err(ref err)) if err.is_retryable()));
    assert!(matches!(
        first,
        Some(Err(ProviderError::Transport(ref err))) if err.to_string().contains("idle timeout")
    ));
    assert!(events.next().await.is_none());
}

#[tokio::test]
async fn sse_transport_error_preserves_retryable_flag() {
    let failing = stream::iter([Result::<Bytes, TransportError>::Err(TransportError::new(
        "connection reset",
        true,
    ))]);
    let mut events = sse_stream_with_timeout(failing, EchoDecoder, Duration::from_secs(1));

    let first = events.next().await;
    assert!(matches!(
        first,
        Some(Err(ProviderError::Transport(ref err))) if err.is_retryable()
    ));
}

#[tokio::test]
async fn sse_done_invokes_on_eof_and_stops() {
    let body = stream::iter([
        Ok::<Bytes, TransportError>(Bytes::from_static(b"data: hi\n\n")),
        Ok::<Bytes, TransportError>(Bytes::from_static(b"data: [DONE]\n\n")),
        Ok::<Bytes, TransportError>(Bytes::from_static(b"data: ignored\n\n")),
    ]);
    let items: Vec<_> = sse_stream_with_timeout(body, EchoDecoder, Duration::from_secs(1))
        .collect()
        .await;

    assert_eq!(items.len(), 2);
    assert!(matches!(
        items[0],
        Ok(StreamEvent::TextDelta { ref delta }) if delta == ":hi"
    ));
    assert!(matches!(
        items[1],
        Ok(StreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None
        })
    ));
}
