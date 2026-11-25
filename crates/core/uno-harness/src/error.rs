use protocol::AccumulatorError;
use provider::ProviderError;

/// Машиночитаемый код ошибки цикла агента
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentErrorCode {
    Provider,
    Accumulator,
    ProviderStream,
    IncompleteProviderStream,
    EventAfterFinish,
    InvalidToolCall,
    InvalidMaxProviderTurns,
    MaxTurnsExceeded,
    TokenBudgetExceeded,
    CheckpointFailed,
}

impl AgentErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Accumulator => "accumulator",
            Self::ProviderStream => "provider_stream",
            Self::IncompleteProviderStream => "incomplete_provider_stream",
            Self::EventAfterFinish => "event_after_finish",
            Self::InvalidToolCall => "invalid_tool_call",
            Self::InvalidMaxProviderTurns => "invalid_max_provider_turns",
            Self::MaxTurnsExceeded => "max_turns_exceeded",
            Self::TokenBudgetExceeded => "token_budget_exceeded",
            Self::CheckpointFailed => "checkpoint_failed",
        }
    }
}

/// Ошибки цикла выполнения агента
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Accumulator(#[from] AccumulatorError),
    #[error("provider stream error: {message} (retryable: {retryable})")]
    ProviderStream { message: String, retryable: bool },
    #[error("provider stream ended without a finished event (retryable: {retryable})")]
    IncompleteProviderStream { retryable: bool },
    #[error("provider emitted an event after finishing the response")]
    EventAfterFinish,
    #[error("tool call has an empty id or name")]
    InvalidToolCall,
    #[error("tool call id is duplicated: {id}")]
    DuplicateToolCallId { id: String },
    #[error("max_provider_turns must be greater than zero")]
    InvalidMaxProviderTurns,
    #[error("run exceeded the limit of {max_turns} provider turns")]
    MaxTurnsExceeded { max_turns: usize },
    #[error("run exceeded token budget ({used_tokens} > {max_tokens})")]
    TokenBudgetExceeded { used_tokens: u64, max_tokens: u64 },
    #[error("session checkpoint failed: {0}")]
    CheckpointFailed(String),
}

impl AgentError {
    pub fn code(&self) -> AgentErrorCode {
        match self {
            Self::Provider(_) => AgentErrorCode::Provider,
            Self::Accumulator(_) => AgentErrorCode::Accumulator,
            Self::ProviderStream { .. } => AgentErrorCode::ProviderStream,
            Self::IncompleteProviderStream { .. } => AgentErrorCode::IncompleteProviderStream,
            Self::EventAfterFinish => AgentErrorCode::EventAfterFinish,
            Self::InvalidToolCall | Self::DuplicateToolCallId { .. } => {
                AgentErrorCode::InvalidToolCall
            }
            Self::InvalidMaxProviderTurns => AgentErrorCode::InvalidMaxProviderTurns,
            Self::MaxTurnsExceeded { .. } => AgentErrorCode::MaxTurnsExceeded,
            Self::TokenBudgetExceeded { .. } => AgentErrorCode::TokenBudgetExceeded,
            Self::CheckpointFailed(_) => AgentErrorCode::CheckpointFailed,
        }
    }

    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Provider(error) => error.is_retryable(),
            Self::ProviderStream { retryable, .. } => *retryable,
            Self::IncompleteProviderStream { retryable } => *retryable,
            Self::Accumulator(_)
            | Self::EventAfterFinish
            | Self::InvalidToolCall
            | Self::DuplicateToolCallId { .. }
            | Self::InvalidMaxProviderTurns
            | Self::MaxTurnsExceeded { .. }
            | Self::TokenBudgetExceeded { .. }
            | Self::CheckpointFailed(_) => false,
        }
    }
}
