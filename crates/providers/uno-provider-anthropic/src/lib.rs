//! Провайдер Anthropic Messages API (`/v1/messages`) и совместимых эндпоинтов (в т.ч. DeepSeek Anthropic).

mod provider;
mod request;
mod stream;

pub use provider::{AnthropicProvider, DEFAULT_ANTHROPIC_BASE_URL, DEFAULT_ANTHROPIC_VERSION};
