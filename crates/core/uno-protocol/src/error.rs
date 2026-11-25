/// Ошибка накопления потоковых событий ответа
#[derive(Debug, thiserror::Error)]
pub enum AccumulatorError {
    /// Ошибка, переданная провайдером в потоке
    #[error("stream error: {message} (retryable: {retryable})")]
    Stream { message: String, retryable: bool },
    /// Событие получено после завершения потока
    #[error("received a stream event after the response finished")]
    EventAfterFinish,
    /// Поток завершился без события `Finished`
    #[error("stream ended without a finished event")]
    MissingFinish,
    /// Превышен лимит размера текста
    #[error("text stream exceeded the {limit}-byte limit")]
    TextTooLarge { limit: usize },
    /// Превышен лимит размера рассуждений
    #[error("reasoning stream exceeded the {limit}-byte limit")]
    ReasoningTooLarge { limit: usize },
    /// Превышено максимальное число вызовов инструментов
    #[error("tool call index {index} exceeds the maximum supported index of {max}")]
    TooManyToolCalls { index: usize, max: usize },
    /// Превышен лимит длины `id` или `name` вызова инструмента
    #[error("tool call {index} {field} exceeded the {limit}-byte limit")]
    ToolMetadataTooLarge {
        index: usize,
        field: &'static str,
        limit: usize,
    },
    /// Превышен лимит размера аргументов вызова инструмента
    #[error("arguments for tool call {index} exceeded the {limit}-byte limit")]
    ToolArgumentsTooLarge { index: usize, limit: usize },
    /// Превышен общий лимит буфера сообщения
    #[error("accumulated stream exceeded the {limit}-byte total limit")]
    TotalBufferTooLarge { limit: usize },
    /// Отсутствует обязательное поле `id` или `name` вызова инструмента
    #[error("tool call {index} is missing {field}")]
    MissingToolCallMetadata { index: usize, field: &'static str },
    /// Аргументы вызова инструмента не являются валидным JSON
    #[error("invalid arguments for tool call {index} ({name}): {source}")]
    InvalidToolArguments {
        index: usize,
        name: String,
        #[source]
        source: serde_json::Error,
    },
    /// Аргументы вызова инструмента не являются JSON-объектом
    #[error("arguments for tool call {index} ({name}) must be a JSON object")]
    ToolArgumentsNotObject { index: usize, name: String },
}
