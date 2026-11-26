//! Accounts feature module backed by pluggable `ProviderDriver`s.

mod crypto;
mod drivers;
mod dto;
mod handlers;
mod router;
mod service;
mod store;

pub(crate) use router::router;
pub(crate) use service::AccountService;
