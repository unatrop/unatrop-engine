mod auth;
mod gemini_api;
mod provider;
mod request;
mod stream;

pub const DEFAULT_CLOUD_CODE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const DEFAULT_PROJECT: &str = "aicode-consumers";
pub use gemini_api::{DEFAULT_GEMINI_API_BASE_URL, GeminiApiProvider};
pub const COMPILE_TIME_OAUTH_CLIENT_ID: Option<&str> = option_env!("ANTIGRAVITY_OAUTH_CLIENT_ID");
pub const COMPILE_TIME_OAUTH_CLIENT_SECRET: Option<&str> =
    option_env!("ANTIGRAVITY_OAUTH_CLIENT_SECRET");

pub fn resolve_oauth_client_id() -> Option<String> {
    resolve_oauth_env("ANTIGRAVITY_OAUTH_CLIENT_ID", COMPILE_TIME_OAUTH_CLIENT_ID)
}

pub fn resolve_oauth_client_secret() -> Option<String> {
    resolve_oauth_env(
        "ANTIGRAVITY_OAUTH_CLIENT_SECRET",
        COMPILE_TIME_OAUTH_CLIENT_SECRET,
    )
}

fn resolve_oauth_env(var_name: &str, compile_time: Option<&str>) -> Option<String> {
    std::env::var(var_name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            compile_time
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
}

pub(crate) const DEFAULT_USER_AGENT: &str =
    "antigravity/cli/1.5.9 (aidev_client; os_type=linux; arch=amd64; auth_method=consumer)";

pub use auth::{AntigravityAccess, AntigravityAuth};
pub use provider::{
    AntigravityAccount, AntigravityModel, AntigravityProvider, antigravity_request_body,
    gemini_request_body,
};
