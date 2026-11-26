//! Domain & HTTP DTO types for daemon-managed provider accounts.

use serde::{Deserialize, Serialize};

/// Описание одного пресета провайдера, отдаваемое клиентам через `GET /v1/providers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPresetView {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub icon: String,
    pub default_label: String,
    pub default_base_url: String,
    pub default_model: String,
    /// `"oauth_device"`, `"oauth_browser"`, или `"api_key"`.
    pub auth_mode: String,
    pub allows_empty_key: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_start_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_token_auth_kind: Option<String>,
}

/// Учётные данные аккаунта (хранятся только на диске в зашифрованном виде и никогда не отдаются в HTTP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AccountAuth {
    ApiKey { key: String },
    GithubOAuth { token: String },
    GoogleOAuth { token_json: String },
    None,
}

impl AccountAuth {
    pub(crate) const fn auth_kind(&self) -> &'static str {
        match self {
            Self::ApiKey { .. } => "api_key",
            Self::GithubOAuth { .. } | Self::GoogleOAuth { .. } => "oauth",
            Self::None => "none",
        }
    }

    pub(crate) const fn auth_badge(&self) -> &'static str {
        match self {
            Self::ApiKey { .. } => "API",
            Self::GithubOAuth { .. } | Self::GoogleOAuth { .. } => "Account",
            Self::None => "Free",
        }
    }
}

/// Персистентная запись об одном аккаунте/подключении в `accounts.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AccountRecord {
    pub id: String,
    pub label: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    pub auth: AccountAuth,
    #[serde(default)]
    pub models: Vec<String>,
}

/// Снимок одной модели в ответе `/v1/accounts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountModelView {
    pub id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_fraction: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_reset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

/// Общая квота аккаунта (например, GitHub Copilot).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountQuotaView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entitlement: Option<u64>,
    pub unlimited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_date: Option<String>,
}

/// Безопасное представление аккаунта для клиентов (без секретов).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountView {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub icon: String,
    pub auth_kind: String,
    pub auth_badge: String,
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<AccountQuotaView>,
    #[serde(default)]
    pub models: Vec<AccountModelView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Тело `POST /v1/accounts`.
#[derive(Debug, Clone, Deserialize)]
pub struct UpsertAccountRequest {
    #[serde(default)]
    pub id: Option<String>,
    pub label: String,
    pub kind: String,
    #[serde(default)]
    pub base_url: Option<String>,
    /// `"api_key"`, `"google_token"`, `"none"`, или `"keep"` (сохранить текущий секрет при редактировании).
    #[serde(default = "default_auth_kind")]
    pub auth_kind: String,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
}

fn default_auth_kind() -> String {
    "api_key".to_owned()
}

/// Ответ на старт OAuth-авторизации.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthStartResponse {
    pub flow_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_url: Option<String>,
}

/// Ответ опроса статуса OAuth-потока.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthPollResponse {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
