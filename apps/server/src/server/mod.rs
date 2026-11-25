//! HTTP/Axum infrastructure: shared state, error mapping, and auth middleware.

pub(crate) mod error;
pub(crate) mod middleware;
pub(crate) mod state;

pub(crate) use error::DaemonError;
pub(crate) use middleware::auth_middleware;
pub use state::DaemonState;
pub(crate) use state::validate_session_id;
