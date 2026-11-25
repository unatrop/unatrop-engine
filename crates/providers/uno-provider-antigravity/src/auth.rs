use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::Request;
use http::header::{ACCEPT, CONTENT_TYPE, USER_AGENT};
use provider::{ProviderError, read_checked_body, send_with_retry};
use serde::Deserialize;
use tokio::sync::Mutex;
use transport::HttpTransport;

const DEFAULT_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REFRESH_SKEW_SECS: u64 = 60;

#[derive(Clone, Debug)]
pub struct AntigravityAccess {
    pub token: String,
    pub expires_at: u64,
}

enum TokenOrigin {
    Direct {
        access_token: String,
    },
    Refresh {
        client_id: String,
        client_secret: String,
    },
    Dynamic(Arc<dyn Fn() -> Option<String> + Send + Sync>),
}

struct AuthState {
    cached: Option<AntigravityAccess>,
    refresh_token: Option<String>,
}

pub struct AntigravityAuth {
    transport: Arc<dyn HttpTransport>,
    origin: TokenOrigin,
    state: Arc<Mutex<AuthState>>,
    api_url: String,
    project: String,
    token_url: String,
}

impl AntigravityAuth {
    pub fn from_access_token(
        access_token: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self::new(
            TokenOrigin::Direct {
                access_token: access_token.into(),
            },
            None,
            transport,
        )
    }

    pub fn from_refresh_token(
        refresh_token: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self::from_refresh_token_with_config(
            refresh_token,
            crate::resolve_oauth_client_id().unwrap_or_default(),
            crate::resolve_oauth_client_secret().unwrap_or_default(),
            transport,
        )
    }

    pub fn from_refresh_token_with_config(
        refresh_token: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self::new(
            TokenOrigin::Refresh {
                client_id: client_id.into(),
                client_secret: client_secret.into(),
            },
            Some(refresh_token.into()),
            transport,
        )
    }

    pub fn from_token_provider(
        provider: Arc<dyn Fn() -> Option<String> + Send + Sync>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self::new(TokenOrigin::Dynamic(provider), None, transport)
    }

    fn new(
        origin: TokenOrigin,
        refresh_token: Option<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            transport,
            origin,
            state: Arc::new(Mutex::new(AuthState {
                cached: None,
                refresh_token,
            })),
            api_url: crate::DEFAULT_CLOUD_CODE_URL.to_string(),
            project: crate::DEFAULT_PROJECT.to_string(),
            token_url: DEFAULT_TOKEN_URL.to_string(),
        }
    }

    pub fn from_stored_token(
        json: &str,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, ProviderError> {
        let value: serde_json::Value = serde_json::from_str(json).map_err(|error| {
            ProviderError::InvalidResponse(format!("Failed to parse stored token: {error}"))
        })?;
        let token = value.get("token").unwrap_or(&value);

        let access_token = token
            .get("access_token")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty());
        let refresh_token = token
            .get("refresh_token")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty());

        if let Some(refresh_token) = refresh_token {
            return Ok(Self::from_refresh_token(refresh_token, transport));
        }
        match access_token {
            Some(access_token) => Ok(Self::from_access_token(access_token, transport)),
            None => Err(ProviderError::Configuration(
                "stored token has neither access_token nor refresh_token".into(),
            )),
        }
    }

    pub fn from_env(transport: Arc<dyn HttpTransport>) -> Result<Self, ProviderError> {
        let api_url = env_first(&["ANTIGRAVITY_CLOUD_CODE_URL", "CLOUD_CODE_URL"])
            .unwrap_or_else(|| crate::DEFAULT_CLOUD_CODE_URL.to_string());
        let project = env_first(&["ANTIGRAVITY_PROJECT", "GOOGLE_CLOUD_PROJECT"])
            .unwrap_or_else(|| crate::DEFAULT_PROJECT.to_string());
        let client_id = crate::resolve_oauth_client_id().unwrap_or_default();
        let client_secret = crate::resolve_oauth_client_secret().unwrap_or_default();

        let auth = if let Some(access_token) = env_first(&["ANTIGRAVITY_ACCESS_TOKEN"]) {
            Self::from_access_token(access_token, transport)
        } else if let Some(refresh_token) = env_first(&["ANTIGRAVITY_REFRESH_TOKEN"]) {
            Self::from_refresh_token_with_config(refresh_token, client_id, client_secret, transport)
        } else {
            return Err(ProviderError::Configuration(
                "set ANTIGRAVITY_ACCESS_TOKEN or ANTIGRAVITY_REFRESH_TOKEN".into(),
            ));
        };

        Ok(auth.with_api_url(api_url).with_project(project))
    }

    pub fn with_api_url(mut self, api_url: impl Into<String>) -> Self {
        self.api_url = api_url.into().trim_end_matches('/').to_string();
        self
    }

    pub fn with_project(mut self, project: impl Into<String>) -> Self {
        self.project = project.into();
        self
    }

    pub fn with_token_url(mut self, token_url: impl Into<String>) -> Self {
        self.token_url = token_url.into();
        self
    }

    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    pub async fn access(&self) -> Result<AntigravityAccess, ProviderError> {
        match &self.origin {
            TokenOrigin::Direct { access_token } => {
                return Ok(AntigravityAccess {
                    token: access_token.clone(),
                    expires_at: u64::MAX,
                });
            }
            TokenOrigin::Dynamic(provider) => {
                let token = provider().ok_or_else(|| {
                    ProviderError::Configuration("no Antigravity access token is available".into())
                })?;
                return Ok(AntigravityAccess {
                    token,
                    expires_at: u64::MAX,
                });
            }
            TokenOrigin::Refresh { .. } => {}
        }

        let mut state = self.state.lock().await;
        let now = unix_now();
        if let Some(access) = state.cached.as_ref()
            && now.saturating_add(REFRESH_SKEW_SECS) < access.expires_at
        {
            return Ok(access.clone());
        }

        let refresh_token = state.refresh_token.clone().ok_or_else(|| {
            ProviderError::Configuration("Antigravity refresh token is missing".into())
        })?;
        let (client_id, client_secret) = match &self.origin {
            TokenOrigin::Refresh {
                client_id,
                client_secret,
            } => (client_id.as_str(), client_secret.as_str()),
            _ => unreachable!("other origins return above"),
        };
        if client_id.is_empty() || client_secret.is_empty() {
            return Err(ProviderError::Configuration(
                "set ANTIGRAVITY_OAUTH_CLIENT_ID and ANTIGRAVITY_OAUTH_CLIENT_SECRET to refresh Antigravity OAuth tokens".into(),
            ));
        }

        let body = format!(
            "client_id={}&client_secret={}&grant_type=refresh_token&refresh_token={}",
            form_encode(client_id),
            form_encode(client_secret),
            form_encode(&refresh_token)
        );
        let response = send_with_retry(self.transport.as_ref(), || {
            Request::post(self.token_url.as_str())
                .header(ACCEPT, "application/json")
                .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(USER_AGENT, crate::DEFAULT_USER_AGENT)
                .body(Bytes::from(body.clone()))
                .map_err(|error| {
                    ProviderError::InvalidResponse(format!(
                        "Failed to build token refresh request: {error}"
                    ))
                })
        })
        .await?;

        let text = read_checked_body(response).await?;

        let parsed: TokenResponse = serde_json::from_str(&text).map_err(|error| {
            ProviderError::InvalidResponse(format!("Failed to parse token response: {error}"))
        })?;
        if let Some(refresh_token) = parsed
            .refresh_token
            .filter(|value| !value.trim().is_empty())
        {
            state.refresh_token = Some(refresh_token);
        }

        let expires_at = unix_now().saturating_add(parsed.expires_in.unwrap_or(3600));
        let access = AntigravityAccess {
            token: parsed.access_token,
            expires_at,
        };
        state.cached = Some(access.clone());
        Ok(access)
    }
}

fn form_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn env_first(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    })
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
}
