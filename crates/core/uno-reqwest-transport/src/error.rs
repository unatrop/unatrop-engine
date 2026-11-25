use std::error::Error as _;

use transport::TransportError;

pub(crate) fn map_reqwest_error(error: reqwest::Error) -> TransportError {
    let mut message = error.to_string();
    let mut current = error.source();
    while let Some(source) = current {
        message.push_str(": ");
        message.push_str(&source.to_string());
        current = source.source();
    }
    let lower = message.to_ascii_lowercase();
    let transient_socket = lower.contains("connection reset")
        || lower.contains("os error 104")
        || lower.contains("unexpected eof")
        || lower.contains("broken pipe");
    let retryable = error.is_timeout() || error.is_connect() || transient_socket;
    TransportError::new(message, retryable)
}
