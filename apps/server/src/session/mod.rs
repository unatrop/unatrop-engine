//! Session lifecycle, runtime execution, metrics, and event mapping.

mod builder;
mod entry;
mod events;
mod metrics;

pub(crate) use builder::build_session_entry;
pub(crate) use entry::{SESSION_EVENT_CHANNEL_CAPACITY, SessionEntry};
pub(crate) use metrics::SessionMetrics;
