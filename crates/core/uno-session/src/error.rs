/// Ошибки управления сессией диалога
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("a run is already active for this session")]
    RunAlreadyActive,
    #[error("session history lock was poisoned")]
    HistoryPoisoned,
    #[error("failed to access session file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to serialize or parse session history: {0}")]
    Serialization(#[from] serde_json::Error),
}
