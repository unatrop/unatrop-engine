//! Провайдер моделей GitHub Copilot
//!
//! Обращается к OpenAI-совместимому эндпоинту Copilot с краткоживущим токеном API,
//! переданным напрямую (`GITHUB_COPILOT_API_TOKEN` + `COPILOT_API_URL`) или полученным
//! в обмен на токен GitHub (`GH_TOKEN` / `GITHUB_TOKEN`) через
//! `https://api.github.com/copilot_internal/v2/token`

mod auth;
mod provider;

pub(crate) const COPILOT_USER_AGENT: &str = "GitHubCopilotChat/0.32.0";
pub(crate) const COPILOT_EDITOR_VERSION: &str = "vscode/1.105.0";
pub(crate) const COPILOT_PLUGIN_VERSION: &str = "copilot-chat/0.32.0";

pub use auth::{CopilotAccess, CopilotAccount, CopilotAuth, DeviceFlowPoll, DeviceFlowSession};
pub use provider::{CopilotModel, CopilotProvider};
