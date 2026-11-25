/// Ошибка сетевого или HTTP-транспорта
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct TransportError {
    message: String,
    retryable: bool,
}

impl TransportError {
    /// Создаёт ошибку транспорта с флагом возможности повтора
    pub fn new(message: impl Into<String>, retryable: bool) -> Self {
        Self {
            message: message.into(),
            retryable,
        }
    }

    /// Возвращает `true`, если запрос имеет смысл повторить
    pub fn is_retryable(&self) -> bool {
        self.retryable
    }
}
