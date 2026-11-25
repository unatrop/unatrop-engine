/// Ошибки регистрации, валидации и запуска инструмента
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("tool name cannot be empty")]
    EmptyName,
    #[error("invalid tool name `{0}`: use 1-64 characters from a-z, A-Z, 0-9, `_` and `-`")]
    InvalidName(String),
    #[error("tool is already registered: {0}")]
    DuplicateTool(String),
    #[error("tool not found: {0}")]
    UnknownTool(String),
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("tool execution failed: {0}")]
    Execution(String),
    #[error("approval denied for tool: {0}")]
    ApprovalDenied(String),
    #[error("approval handler failed: {0}")]
    ApprovalFailed(String),
}
