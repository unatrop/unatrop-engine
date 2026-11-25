//! Agent wiring: high-level client, tool approvals, prompts, skills/MCP config.

pub(crate) mod approval;
pub(crate) mod client;
pub(crate) mod config;
pub(crate) mod prompts;

pub(crate) use approval::DaemonApprovalHandler;
pub(crate) use client::{Client, ClientBuilder, ClientConfig};
