//! Git workspace diff/revert feature module mounted under `/v1/sessions`.

mod handlers;
mod router;
mod service;

pub(crate) use router::router;
