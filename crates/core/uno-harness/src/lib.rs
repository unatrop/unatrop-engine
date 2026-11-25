//! Цикл выполнения агента: запросы к провайдеру и вызов инструментов

pub mod checkpoint;
pub mod compaction;
mod error;
mod event;
mod harness;
mod helpers;
pub mod inbox;
pub use uno_mcp as mcp;
pub mod plan;
pub mod prompt;
pub mod ralph;
pub use uno_skills as skills;
pub mod subagent;
pub mod xml_recovery;

pub use checkpoint::SessionCheckpoint;
pub use error::{AgentError, AgentErrorCode};
pub use event::{RunEvent, RunState, RunStream};
pub use harness::{Harness, MIN_DEDUPE_TOOL_CHARS, dedupe_consecutive_tool_result};
pub use inbox::AgentInbox;
pub use prompt::{SystemPromptBuilder, section_order};
pub use ralph::{RalphConfig, RalphError, RalphOutcome, RalphReport, RalphRunner, RalphStatus};
