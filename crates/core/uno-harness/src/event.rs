use std::pin::Pin;

use futures_util::Stream;
use protocol::{Message, StreamEvent};
use tool_core::ToolOutput;

use crate::AgentError;

/// Поток событий запуска: фатальные ошибки завершают поток значением `Err(AgentError)`
pub type RunStream<'a> = Pin<Box<dyn Stream<Item = Result<RunEvent, AgentError>> + Send + 'a>>;

/// Текущая фаза выполнения запуска
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    WaitingForModel,
    StreamingResponse,
    /// Включает ожидание подтверждения через `ApprovalHandler`
    HandlingToolCalls,
    Completed,
}

/// События цикла выполнения агента
#[derive(Debug, Clone)]
pub enum RunEvent {
    Started,
    StateChanged(RunState),
    ProviderEvent(StreamEvent),
    /// Новое сообщение для инкрементального обновления истории; `Completed` отдаёт снимок целиком и не должен дописываться повторно
    MessageAppended(Message),
    ToolCallStarted {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolOutputDelta {
        id: String,
        name: String,
        chunk: String,
    },
    ToolCallFinished {
        id: String,
        name: String,
        output: ToolOutput,
    },
    TurnLimitReached {
        max_turns: usize,
    },
    /// Контекст был сжат перед очередным ходом провайдера
    ContextCompacted {
        /// Сколько сообщений вытеснено из истории
        dropped_messages: usize,
        /// Оценка токенов истории после сжатия
        estimated_tokens_after: u64,
    },
    Completed {
        /// Полная история диалога на момент успешного завершения
        messages: Vec<Message>,
        finish_reason: Option<protocol::FinishReason>,
        usage: Option<protocol::Usage>,
    },
}
