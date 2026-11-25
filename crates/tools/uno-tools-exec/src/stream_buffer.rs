#[cfg(unix)]
use std::io::Read;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{Mutex, Notify};

/// Максимальный размер внутреннего буфера вывода сессии на каждый поток
pub(crate) const MAX_BUFFER_BYTES: usize = 512 * 1024;

#[derive(Default)]
pub(crate) struct StreamBuffer {
    bytes: Vec<u8>,
    truncated: bool,
}

impl StreamBuffer {
    pub(crate) fn push_chunk(&mut self, chunk: &[u8]) {
        if self.bytes.len() >= MAX_BUFFER_BYTES {
            self.truncated = true;
            return;
        }
        let available = MAX_BUFFER_BYTES - self.bytes.len();
        let take = chunk.len().min(available);
        self.bytes.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            self.truncated = true;
        }
    }

    pub(crate) fn take_capped(&mut self, max_bytes: usize) -> (String, bool) {
        if self.bytes.is_empty() {
            let truncated = std::mem::take(&mut self.truncated);
            return (String::new(), truncated);
        }

        let take = self.bytes.len().min(max_bytes);
        let chunk: Vec<u8> = self.bytes.drain(..take).collect();
        let hit_limit = !self.bytes.is_empty() || std::mem::take(&mut self.truncated);

        let mut text = String::from_utf8_lossy(&chunk).into_owned();
        if hit_limit {
            text.push_str("\n[output truncated]");
        }
        (text, hit_limit)
    }
}

fn emit_delta(
    delta_sink: &std::sync::Mutex<Option<uno_tools_core::OutputDeltaSender>>,
    bytes: &[u8],
) {
    if bytes.is_empty() {
        return;
    }
    if let Ok(mut guard) = delta_sink.lock()
        && let Some(tx) = guard.as_ref()
    {
        let text = String::from_utf8_lossy(bytes).into_owned();
        if tx.send(text).is_err() {
            *guard = None;
        }
    }
}

#[cfg(unix)]
pub(crate) fn pump_blocking_reader(
    mut reader: std::fs::File,
    buffer: Arc<Mutex<StreamBuffer>>,
    notify: Arc<Notify>,
    delta_sink: Arc<std::sync::Mutex<Option<uno_tools_core::OutputDeltaSender>>>,
) {
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => {
                notify.notify_waiters();
                break;
            }
            Ok(n) => {
                buffer.blocking_lock().push_chunk(&chunk[..n]);
                emit_delta(&delta_sink, &chunk[..n]);
                notify.notify_waiters();
            }
        }
    }
}

pub(crate) async fn pump_stream<R: AsyncRead + Unpin>(
    mut reader: R,
    buffer: Arc<Mutex<StreamBuffer>>,
    notify: Arc<Notify>,
    delta_sink: Arc<std::sync::Mutex<Option<uno_tools_core::OutputDeltaSender>>>,
) {
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => {
                notify.notify_waiters();
                break;
            }
            Ok(n) => {
                buffer.lock().await.push_chunk(&chunk[..n]);
                emit_delta(&delta_sink, &chunk[..n]);
                notify.notify_waiters();
            }
        }
    }
}
