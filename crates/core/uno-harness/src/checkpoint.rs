//! Fail-closed чекпоинт состояния сессии перед сайд-эффектами (LLM-запрос и вызов инструмента)

use async_trait::async_trait;

/// Хук принудительной фиксации журнала/состояния сессии.
/// При возврате `Err(...)` цикл агента останавливается до отправки запроса в LLM или вызова инструмента.
#[async_trait]
pub trait SessionCheckpoint: Send + Sync {
    async fn flush(&self) -> Result<(), String>;
}
