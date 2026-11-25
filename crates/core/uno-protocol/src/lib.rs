//! Протокол запросов, потоковых событий и сборки сообщений LLM

mod accumulator;
mod error;
mod message;
mod redact;
mod stream;

pub use accumulator::{AccumulatedMessage, MessageAccumulator};
pub use error::AccumulatorError;
pub use message::{
    CompletionRequest, Content, ContentPart, Message, Role, ThinkingLevel, ToolCall, ToolDefinition,
};
pub use redact::{redact_message_secrets, redact_secrets};
pub use stream::{FinishReason, StreamEvent, Usage};
