use serde::{Deserialize, Serialize};

/// Причина завершения генерации
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    #[serde(alias = "tool_calls")]
    ToolCall,
    Length,
    ContentFilter,
    #[serde(other)]
    Other,
}

/// Статистика использования токенов
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Токены входного промпта
    pub input_tokens: u32,
    /// Сгенерированные токены ответа
    pub output_tokens: u32,
    /// Токены, прочитанные из кэша промпта
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
}

/// Событие потокового ответа модели
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Дельта обычного текста
    TextDelta { delta: String },
    /// Дельта рассуждений
    ReasoningDelta { delta: String },
    /// Дельта вызова инструмента
    ToolCallDelta {
        index: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        arguments_delta: String,
    },
    /// Сигнатура рассуждений, привязанная к вызову инструмента
    ThoughtSignature { index: usize, signature: String },
    /// Завершение потока
    Finished {
        reason: FinishReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// Ошибка, переданная внутри потока
    Error { message: String, retryable: bool },
}
