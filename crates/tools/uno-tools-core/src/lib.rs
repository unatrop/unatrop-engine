//! Базовые абстракции, реестр и политики подтверждения инструментов

mod command_safety;
mod error;
mod policy;
mod registry;
pub mod retention;
pub mod spill;
mod tool;

pub use command_safety::{is_known_safe_command, split_shell_commands};
pub use error::ToolError;
pub use policy::{
    ApprovalMode, ApprovalPolicy, PolicyDecision, ToolRule, extract_command_prefix,
    matches_safe_command_prefix,
};
pub use registry::{ToolRegistry, UNLISTED_TOOLS_MARKER};
pub use retention::{
    ItemRetainer, PRUNE_MARKER, PrunedText, RetainedItems, RetentionConfig, prune_text,
};
pub use spill::{InMemorySpillStore, SpillRef, SpillStore, encode_safe_segment};
pub use tool::{
    Approval, ApprovalHandler, ApprovalRequest, OutputDeltaSender, Tool, ToolOutput, Typed,
    TypedTool,
};
