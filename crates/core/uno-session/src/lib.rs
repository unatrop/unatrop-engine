//! Управление сессиями диалога и восстановлением истории сообщений

mod error;
mod history;
mod session;

pub use error::SessionError;
pub use history::repair_history;
pub use session::Session;
