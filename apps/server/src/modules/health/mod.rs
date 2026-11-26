//! Health check feature module (`GET /health`).

mod handlers;
mod router;

pub(crate) use router::router;
