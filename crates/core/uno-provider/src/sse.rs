//! Быстрый потоковый SSE-движок на базе `BytesMut` и `memchr`

use std::fmt::Display;
use std::ops::ControlFlow;
use std::time::Duration;

use bytes::{Buf, Bytes, BytesMut};
use futures_util::{Stream, StreamExt, stream};
use memchr::{memchr, memchr2};
use protocol::StreamEvent;
use tokio::sync::mpsc;
use tokio::time::timeout;
use transport::{ByteStream, TransportError};

use crate::{BoxStream, ProviderError};

/// Стандартный таймаут простоя между SSE-чанками
pub const DEFAULT_SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const SSE_CHANNEL_CAPACITY: usize = 1024;

/// Событие SSE, заимствованное из внутренних буферов [`SseParser`]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SseFrame<'a> {
    /// Тип события (по умолчанию `"message"` или пустая строка до первого `event:`)
    pub event: &'a str,
    /// Полезная нагрузка из полей `data:`
    pub data: &'a str,
}

/// Декодер конкретного диалекта SSE (OpenAI, Anthropic и т.д.)
pub trait SseDecoder: Send + 'static {
    /// Обрабатывает одно SSE-событие и добавляет 0..N [`StreamEvent`] в `out`
    fn on_event(
        &mut self,
        event: &str,
        data: &str,
        out: &mut Vec<Result<StreamEvent, ProviderError>>,
    ) -> ControlFlow<()>;

    /// Вызывается при `[DONE]` или штатном закрытии потока (EOF)
    fn on_eof(&mut self, _out: &mut Vec<Result<StreamEvent, ProviderError>>) {}
}

#[derive(Default)]
struct SseFrameState {
    event_buf: String,
    data_buf: String,
    has_data: bool,
}

/// Инкрементальный zero-copy (по строкам) парсер Server-Sent Events
#[derive(Default)]
pub struct SseParser {
    buf: BytesMut,
    state: SseFrameState,
    bom_checked: bool,
    pending_cr: bool,
}

impl SseParser {
    /// Создаёт новый парсер SSE
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Подаёт очередной чанк байтов и вызывает `on_frame` для каждого завершённого события
    pub fn feed_chunk(
        &mut self,
        chunk: &[u8],
        mut on_frame: impl FnMut(SseFrame<'_>) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, ProviderError> {
        let mut slice = chunk;
        if !self.bom_checked && !slice.is_empty() {
            if self.buf.is_empty() && slice.starts_with(b"\xEF\xBB\xBF") {
                slice = &slice[3..];
                self.bom_checked = true;
            } else if self.buf.len() + slice.len() >= 3 {
                self.buf.extend_from_slice(slice);
                self.bom_checked = true;
                if self.buf.starts_with(b"\xEF\xBB\xBF") {
                    self.buf.advance(3);
                }
                return self.drain_lines(&mut on_frame);
            } else {
                self.buf.extend_from_slice(slice);
                return Ok(ControlFlow::Continue(()));
            }
        }

        if self.pending_cr {
            self.pending_cr = false;
            if slice.starts_with(b"\n") {
                slice = &slice[1..];
            }
        }

        self.buf.extend_from_slice(slice);
        self.drain_lines(&mut on_frame)
    }

    /// Завершает поток на EOF, сбрасывая незакрытую последнюю строку/фрейм
    pub fn finish(
        &mut self,
        mut on_frame: impl FnMut(SseFrame<'_>) -> ControlFlow<()>,
    ) -> Result<(), ProviderError> {
        if !self.buf.is_empty() {
            let line = std::str::from_utf8(&self.buf).map_err(|err| {
                ProviderError::InvalidResponse(format!("invalid UTF-8 in SSE stream: {err}"))
            })?;
            let line = line.strip_suffix('\r').unwrap_or(line);
            let _ = self.state.process_line(line, &mut on_frame);
            self.buf.clear();
        }

        if self.state.has_data {
            let _ = self.state.dispatch_frame(&mut on_frame);
        }
        Ok(())
    }

    fn drain_lines(
        &mut self,
        on_frame: &mut impl FnMut(SseFrame<'_>) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, ProviderError> {
        let mut consumed = 0;

        while let Some(rel_pos) = memchr2(b'\n', b'\r', &self.buf[consumed..]) {
            let line_end = consumed + rel_pos;
            let delim = self.buf[line_end];

            // Если `\r` стоит в самом конце буфера, ждём следующий байт, чтобы не разорвать `\r\n`
            if delim == b'\r' && line_end + 1 == self.buf.len() {
                let line = std::str::from_utf8(&self.buf[consumed..line_end]).map_err(|err| {
                    ProviderError::InvalidResponse(format!("invalid UTF-8 in SSE stream: {err}"))
                })?;
                let flow = self.state.process_line(line, on_frame);
                self.buf.advance(line_end + 1);
                self.pending_cr = true;
                return Ok(flow);
            }

            let line = std::str::from_utf8(&self.buf[consumed..line_end]).map_err(|err| {
                ProviderError::InvalidResponse(format!("invalid UTF-8 in SSE stream: {err}"))
            })?;
            let flow = self.state.process_line(line, on_frame);

            consumed = if delim == b'\r' && self.buf.get(line_end + 1) == Some(&b'\n') {
                line_end + 2
            } else {
                line_end + 1
            };

            if flow.is_break() {
                self.buf.advance(consumed);
                return Ok(ControlFlow::Break(()));
            }
        }

        if consumed > 0 {
            self.buf.advance(consumed);
        }

        Ok(ControlFlow::Continue(()))
    }
}

impl SseFrameState {
    fn process_line(
        &mut self,
        line: &str,
        on_frame: &mut impl FnMut(SseFrame<'_>) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        if line.is_empty() {
            return self.dispatch_frame(on_frame);
        }

        if line.starts_with(':') {
            return ControlFlow::Continue(());
        }

        let (field, value) = match memchr(b':', line.as_bytes()) {
            Some(colon) => {
                let field = &line[..colon];
                let raw_value = &line[colon + 1..];
                let value = raw_value.strip_prefix(' ').unwrap_or(raw_value);
                (field, value)
            }
            None => (line, ""),
        };

        match field {
            "event" => {
                self.event_buf.clear();
                self.event_buf.push_str(value);
            }
            "data" => {
                if self.has_data {
                    self.data_buf.push('\n');
                }
                self.data_buf.push_str(value);
                self.has_data = true;
            }
            // `id` и `retry` в LLM-стримах не требуются; неизвестные поля игнорируются по спецификации SSE
            _ => {}
        }

        ControlFlow::Continue(())
    }

    fn dispatch_frame(
        &mut self,
        on_frame: &mut impl FnMut(SseFrame<'_>) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        if !self.has_data {
            self.event_buf.clear();
            return ControlFlow::Continue(());
        }

        let flow = on_frame(SseFrame {
            event: &self.event_buf,
            data: &self.data_buf,
        });

        self.event_buf.clear();
        self.data_buf.clear();
        self.has_data = false;
        flow
    }
}

/// Запускает декодирование SSE-потока со стандартным таймаутом простоя
pub fn sse_stream<D>(
    byte_stream: ByteStream,
    decoder: D,
) -> BoxStream<'static, Result<StreamEvent, ProviderError>>
where
    D: SseDecoder,
{
    sse_stream_with_timeout(byte_stream, decoder, DEFAULT_SSE_IDLE_TIMEOUT)
}

/// Запускает декодирование SSE-потока с настраиваемым таймаутом простоя и отменой при drop
pub fn sse_stream_with_timeout<S, E, D>(
    mut byte_stream: S,
    mut decoder: D,
    idle_timeout: Duration,
) -> BoxStream<'static, Result<StreamEvent, ProviderError>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + Unpin + 'static,
    E: Into<TransportError> + Display + Send + 'static,
    D: SseDecoder,
{
    let (tx, rx) = mpsc::channel::<Result<StreamEvent, ProviderError>>(SSE_CHANNEL_CAPACITY);

    tokio::spawn(async move {
        let mut parser = SseParser::new();
        let mut out = Vec::with_capacity(4);
        let mut stopped = false;

        loop {
            let next = tokio::select! {
                biased;
                _ = tx.closed() => return,
                res = timeout(idle_timeout, byte_stream.next()) => res,
            };

            match next {
                Ok(Some(Ok(chunk))) => {
                    let feed_res = parser.feed_chunk(&chunk, |frame| {
                        let data = frame.data.trim();
                        if data.is_empty() {
                            return ControlFlow::Continue(());
                        }
                        if data == "[DONE]" {
                            decoder.on_eof(&mut out);
                            stopped = true;
                            return ControlFlow::Break(());
                        }
                        let flow = decoder.on_event(frame.event, data, &mut out);
                        if flow.is_break() {
                            stopped = true;
                        }
                        flow
                    });

                    if let Err(err) = feed_res {
                        let _ = tx.send(Err(err)).await;
                        return;
                    }

                    for item in out.drain(..) {
                        if tx.send(item).await.is_err() {
                            return;
                        }
                    }

                    if stopped {
                        return;
                    }
                }
                Ok(Some(Err(err))) => {
                    let _ = tx.send(Err(ProviderError::Transport(err.into()))).await;
                    return;
                }
                Ok(None) => {
                    if let Err(err) = parser.finish(|frame| {
                        let data = frame.data.trim();
                        if data.is_empty() {
                            return ControlFlow::Continue(());
                        }
                        if data == "[DONE]" {
                            stopped = true;
                            return ControlFlow::Break(());
                        }
                        decoder.on_event(frame.event, data, &mut out)
                    }) {
                        let _ = tx.send(Err(err)).await;
                        return;
                    }

                    decoder.on_eof(&mut out);
                    for item in out.drain(..) {
                        if tx.send(item).await.is_err() {
                            return;
                        }
                    }
                    return;
                }
                Err(_) => {
                    let _ = tx
                        .send(Err(ProviderError::Transport(TransportError::new(
                            "idle timeout waiting for SSE",
                            true,
                        ))))
                        .await;
                    return;
                }
            }
        }
    });

    Box::pin(stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}
