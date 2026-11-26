//! Vertical feature modules composing the daemon HTTP/WebSocket surface.

pub(crate) mod accounts;
pub(crate) mod call;
#[cfg(feature = "host-tools")]
pub(crate) mod git;
pub(crate) mod health;
pub(crate) mod sessions;
pub(crate) mod tts;
