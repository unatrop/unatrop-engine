use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::Request;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use provider::{ProviderError, read_checked_body, send_with_retry};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use transport::HttpTransport;

const DEFAULT_GITHUB_API_BASE: &str = "https://api.github.com";
const DEFAULT_GITHUB_OAUTH_BASE: &str = "https://github.com";
const DEFAULT_COPILOT_API_URL: &str = "https://api.githubcopilot.com";
const DEFAULT_COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const REFRESH_SKEW_SECS: u64 = 60;

/// Токен доступа и базовый URL для одного запроса к Copilot API
#[derive(Clone, Debug)]
pub struct CopilotAccess {
    /// Короткоживущий токен Copilot API
    pub token: String,
    /// Базовый URL эндпоинта
    pub api_url: String,
    /// Unix-время истечения токена в секундах (`u64::MAX`, если не задано)
    pub expires_at: u64,
}

/// Аккаунт GitHub и тариф Copilot, к которому он подключён
#[derive(Clone, Debug, PartialEq)]
pub struct CopilotAccount {
    /// GitHub login пользователя
    pub login: Option<String>,
    /// Человекочитаемое название тарифа (`Copilot Free`, `Copilot Pro`, ...)
    pub plan: Option<String>,
    /// Оставшаяся квота премиум-запросов в процентах (`0.0..=100.0`)
    pub quota_percent: Option<f64>,
    /// Оставшееся количество запросов
    pub quota_remaining: Option<u64>,
    /// Общий лимит запросов в периоде
    pub quota_entitlement: Option<u64>,
    /// Признак безлимитной квоты
    pub quota_unlimited: bool,
    /// Дата сброса квоты (если вернул API)
    pub quota_reset_date: Option<String>,
}

#[derive(Clone)]
enum TokenOrigin {
    /// Обмен токена GitHub на токен Copilot API
    Exchange { github_token: String },
    /// Динамическое чтение токена GitHub (например, из хранилища настроек)
    Dynamic(Arc<dyn Fn() -> Option<String> + Send + Sync>),
    /// Прямое использование выданного токена Copilot API
    Direct { token: String, api_url: String },
}

/// Получает и кэширует токены Copilot API до истечения срока действия
#[derive(Clone)]
pub struct CopilotAuth {
    transport: Arc<dyn HttpTransport>,
    origin: TokenOrigin,
    github_api_base: String,
    cached: Arc<Mutex<Option<CopilotAccess>>>,
}

/// Сессия GitHub OAuth Device Flow для получения `ghu_...` токена
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceFlowSession {
    /// Код устройства для опроса `access_token`
    pub device_code: String,
    /// Короткий код, который пользователь вводит в браузере
    pub user_code: String,
    /// Страница подтверждения (`https://github.com/login/device`)
    pub verification_uri: String,
    /// Срок жизни `device_code` в секундах
    pub expires_in: u64,
    /// Рекомендуемый интервал опроса в секундах
    pub interval: u64,
    #[serde(default = "default_oauth_base")]
    oauth_base: String,
    #[serde(default = "default_client_id")]
    client_id: String,
}

fn default_oauth_base() -> String {
    DEFAULT_GITHUB_OAUTH_BASE.to_owned()
}

fn default_client_id() -> String {
    DEFAULT_COPILOT_CLIENT_ID.to_owned()
}

/// Результат одного шага опроса GitHub OAuth Device Flow
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceFlowPoll {
    /// Пользователь ещё не подтвердил код
    Pending,
    /// GitHub просит увеличить интервал опроса на `extra_secs` секунд
    SlowDown { extra_secs: u64 },
    /// Авторизация завершена, получен OAuth-токен GitHub (`ghu_...`)
    Authorized { github_token: String },
}

impl DeviceFlowSession {
    /// Выполняет один опрос `login/oauth/access_token`
    pub async fn poll_once(
        &self,
        transport: &dyn HttpTransport,
    ) -> Result<DeviceFlowPoll, ProviderError> {
        let url = format!("{}/login/oauth/access_token", self.oauth_base);
        let payload = serde_json::json!({
            "client_id": self.client_id,
            "device_code": self.device_code,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
        });
        let body = Bytes::from(serde_json::to_vec(&payload).map_err(|err| {
            ProviderError::InvalidResponse(format!("Failed to serialize OAuth poll body: {err}"))
        })?);

        let request = Request::post(url)
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, "application/json")
            .header(USER_AGENT, crate::COPILOT_USER_AGENT)
            .body(body)
            .map_err(|err| {
                ProviderError::InvalidResponse(format!("Failed to build OAuth poll request: {err}"))
            })?;

        let response = transport.send(request).await?;
        let resp_body = read_checked_body(response).await?;

        let parsed: DeviceTokenResponse = serde_json::from_str(&resp_body).map_err(|err| {
            ProviderError::InvalidResponse(format!("Failed to parse OAuth poll response: {err}"))
        })?;

        if let Some(token) = parsed.access_token.filter(|t| !t.trim().is_empty()) {
            return Ok(DeviceFlowPoll::Authorized {
                github_token: token,
            });
        }

        match parsed.error.as_deref() {
            Some("authorization_pending") => Ok(DeviceFlowPoll::Pending),
            Some("slow_down") => Ok(DeviceFlowPoll::SlowDown { extra_secs: 5 }),
            Some(err_code) => Err(ProviderError::InvalidResponse(format!(
                "GitHub OAuth device flow failed: {err_code}{}",
                parsed
                    .error_description
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default()
            ))),
            None => Err(ProviderError::InvalidResponse(
                "GitHub OAuth poll response missing access_token and error".into(),
            )),
        }
    }

    /// Опрашивает GitHub до подтверждения пользователем и возвращает готовый [`CopilotAuth`]
    pub async fn wait_for_auth(
        &self,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<CopilotAuth, ProviderError> {
        let mut interval_secs = self.interval.max(1);
        let deadline = unix_now().saturating_add(self.expires_in);

        loop {
            if unix_now() >= deadline {
                return Err(ProviderError::InvalidResponse(
                    "GitHub OAuth device code expired".into(),
                ));
            }
            tokio::time::sleep(Duration::from_secs(interval_secs)).await;

            match self.poll_once(transport.as_ref()).await? {
                DeviceFlowPoll::Pending => {}
                DeviceFlowPoll::SlowDown { extra_secs } => {
                    interval_secs = interval_secs.saturating_add(extra_secs);
                }
                DeviceFlowPoll::Authorized { github_token } => {
                    return Ok(CopilotAuth::from_github_token(github_token, transport));
                }
            }
        }
    }
}

impl CopilotAuth {
    /// Запускает GitHub OAuth Device Flow со стандартным client_id Copilot
    pub async fn start_device_flow(
        transport: &dyn HttpTransport,
    ) -> Result<DeviceFlowSession, ProviderError> {
        Self::start_device_flow_with_config(
            transport,
            DEFAULT_GITHUB_OAUTH_BASE,
            DEFAULT_COPILOT_CLIENT_ID,
        )
        .await
    }

    /// Запускает GitHub OAuth Device Flow с пользовательским OAuth URL и `client_id`
    pub async fn start_device_flow_with_config(
        transport: &dyn HttpTransport,
        oauth_base: &str,
        client_id: &str,
    ) -> Result<DeviceFlowSession, ProviderError> {
        let oauth_base = oauth_base.trim_end_matches('/');
        let url = format!("{oauth_base}/login/device/code");
        let payload = serde_json::json!({
            "client_id": client_id,
            "scope": "read:user",
        });
        let body = Bytes::from(serde_json::to_vec(&payload).map_err(|err| {
            ProviderError::InvalidResponse(format!(
                "Failed to serialize OAuth device/code body: {err}"
            ))
        })?);

        let request = Request::post(url)
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, "application/json")
            .header(USER_AGENT, crate::COPILOT_USER_AGENT)
            .body(body)
            .map_err(|err| {
                ProviderError::InvalidResponse(format!(
                    "Failed to build OAuth device/code request: {err}"
                ))
            })?;

        let response = transport.send(request).await?;
        let resp_body = read_checked_body(response).await?;

        let parsed: DeviceCodeResponse = serde_json::from_str(&resp_body).map_err(|err| {
            ProviderError::InvalidResponse(format!(
                "Failed to parse OAuth device/code response: {err}"
            ))
        })?;

        Ok(DeviceFlowSession {
            device_code: parsed.device_code,
            user_code: parsed.user_code,
            verification_uri: parsed.verification_uri,
            expires_in: parsed.expires_in,
            interval: parsed.interval.max(1),
            oauth_base: oauth_base.to_string(),
            client_id: client_id.to_string(),
        })
    }

    /// Читает учётные данные из переменных окружения
    ///
    /// Приоритет: `GITHUB_COPILOT_API_TOKEN` (+ опциональный `COPILOT_API_URL`),
    /// затем `GH_TOKEN`, затем `GITHUB_TOKEN`
    pub fn from_env(transport: Arc<dyn HttpTransport>) -> Result<Self, ProviderError> {
        if let Ok(token) = std::env::var("GITHUB_COPILOT_API_TOKEN")
            && !token.trim().is_empty()
        {
            let api_url = std::env::var("COPILOT_API_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_COPILOT_API_URL.to_string());
            return Ok(Self::from_api_token(token, api_url, transport));
        }

        for name in ["GH_TOKEN", "GITHUB_TOKEN"] {
            if let Ok(token) = std::env::var(name)
                && !token.trim().is_empty()
            {
                return Ok(Self::from_github_token(token, transport));
            }
        }

        Err(ProviderError::Configuration(
            "set GITHUB_COPILOT_API_TOKEN (with optional COPILOT_API_URL) or GH_TOKEN/GITHUB_TOKEN"
                .into(),
        ))
    }

    /// Обменивает токен GitHub на токен Copilot API
    pub fn from_github_token(
        github_token: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            transport,
            origin: TokenOrigin::Exchange {
                github_token: github_token.into(),
            },
            github_api_base: DEFAULT_GITHUB_API_BASE.to_string(),
            cached: Arc::new(Mutex::new(None)),
        }
    }

    /// Использует готовый токен Copilot API без обмена
    pub fn from_api_token(
        token: impl Into<String>,
        api_url: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            transport,
            origin: TokenOrigin::Direct {
                token: token.into(),
                api_url: api_url.into().trim_end_matches('/').to_string(),
            },
            github_api_base: DEFAULT_GITHUB_API_BASE.to_string(),
            cached: Arc::new(Mutex::new(None)),
        }
    }

    /// Лениво читает токен GitHub из переданного поставщика при каждом запросе доступа
    pub fn from_token_provider(
        provider: Arc<dyn Fn() -> Option<String> + Send + Sync>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            transport,
            origin: TokenOrigin::Dynamic(provider),
            github_api_base: DEFAULT_GITHUB_API_BASE.to_string(),
            cached: Arc::new(Mutex::new(None)),
        }
    }

    /// Переопределяет базовый URL GitHub API для обмена токена
    pub fn with_github_api_base(mut self, base: impl Into<String>) -> Self {
        self.github_api_base = base.into().trim_end_matches('/').to_string();
        self
    }

    /// Возвращает действующий токен доступа, обновляя его при необходимости
    pub async fn access(&self) -> Result<CopilotAccess, ProviderError> {
        if let TokenOrigin::Direct { token, api_url } = &self.origin {
            return Ok(CopilotAccess {
                token: token.clone(),
                api_url: api_url.clone(),
                expires_at: u64::MAX,
            });
        }

        let mut cached = self.cached.lock().await;
        if let TokenOrigin::Dynamic(provider) = &self.origin
            && provider().is_none()
        {
            *cached = None;
            return Err(ProviderError::Configuration(
                "GitHub Copilot is not connected; sign in via Settings or set GH_TOKEN/GITHUB_TOKEN"
                    .into(),
            ));
        }

        let now = unix_now();
        if let Some(access) = cached.as_ref()
            && (access.expires_at == u64::MAX
                || now.saturating_add(REFRESH_SKEW_SECS) < access.expires_at)
        {
            return Ok(access.clone());
        }

        let github_token = match &self.origin {
            TokenOrigin::Exchange { github_token } => github_token.clone(),
            TokenOrigin::Dynamic(provider) => provider().ok_or_else(|| {
                ProviderError::Configuration(
                    "GitHub Copilot is not connected; sign in via Settings or set GH_TOKEN/GITHUB_TOKEN"
                        .into(),
                )
            })?,
            TokenOrigin::Direct { .. } => unreachable!("direct tokens return above"),
        };
        let access = self.exchange(&github_token).await?;
        *cached = Some(access.clone());
        Ok(access)
    }

    /// Запрашивает GitHub login текущего авторизованного аккаунта через `GET /user`
    pub async fn fetch_github_login(&self) -> Result<Option<String>, ProviderError> {
        let github_token = match &self.origin {
            TokenOrigin::Exchange { github_token } => github_token.clone(),
            TokenOrigin::Dynamic(provider) => match provider() {
                Some(token) => token,
                None => return Ok(None),
            },
            TokenOrigin::Direct { .. } => return Ok(None),
        };

        let url = format!("{}/user", self.github_api_base);
        let response = send_with_retry(self.transport.as_ref(), || {
            Request::get(url.as_str())
                .header(AUTHORIZATION, format!("token {github_token}"))
                .header(ACCEPT, "application/vnd.github+json")
                .header(USER_AGENT, crate::COPILOT_USER_AGENT)
                .header("X-GitHub-Api-Version", "2022-11-28")
                .body(Bytes::new())
                .map_err(|error| {
                    ProviderError::InvalidResponse(format!("Failed to build user request: {error}"))
                })
        })
        .await?;

        let body = read_checked_body(response).await?;

        #[derive(Deserialize)]
        struct GitHubUser {
            login: String,
        }
        let parsed: GitHubUser = serde_json::from_str(&body).map_err(|error| {
            ProviderError::InvalidResponse(format!("Failed to parse GitHub user response: {error}"))
        })?;
        Ok(Some(parsed.login))
    }

    /// Запрашивает login и тариф Copilot через `GET /copilot_internal/user`
    pub async fn fetch_account(&self) -> Result<Option<CopilotAccount>, ProviderError> {
        let github_token = match &self.origin {
            TokenOrigin::Exchange { github_token } => github_token.clone(),
            TokenOrigin::Dynamic(provider) => match provider() {
                Some(token) => token,
                None => return Ok(None),
            },
            TokenOrigin::Direct { .. } => return Ok(None),
        };

        let url = format!("{}/copilot_internal/user", self.github_api_base);
        let response = send_with_retry(self.transport.as_ref(), || {
            Request::get(url.as_str())
                .header(AUTHORIZATION, format!("token {github_token}"))
                .header(ACCEPT, "application/vnd.github+json")
                .header(USER_AGENT, crate::COPILOT_USER_AGENT)
                .header("Editor-Version", crate::COPILOT_EDITOR_VERSION)
                .header("Editor-Plugin-Version", crate::COPILOT_PLUGIN_VERSION)
                .header("X-GitHub-Api-Version", "2022-11-28")
                .body(Bytes::new())
                .map_err(|error| {
                    ProviderError::InvalidResponse(format!(
                        "Failed to build account request: {error}"
                    ))
                })
        })
        .await?;

        let body = read_checked_body(response).await?;

        #[derive(Deserialize)]
        struct CopilotQuotaSnapshot {
            #[serde(default)]
            percent_remaining: Option<f64>,
            #[serde(default)]
            remaining: Option<f64>,
            #[serde(default)]
            entitlement: Option<f64>,
            #[serde(default)]
            unlimited: Option<bool>,
        }
        #[derive(Deserialize)]
        struct CopilotQuotaSnapshots {
            #[serde(default)]
            premium_interactions: Option<CopilotQuotaSnapshot>,
            #[serde(default)]
            chat: Option<CopilotQuotaSnapshot>,
        }
        #[derive(Deserialize)]
        struct CopilotUser {
            #[serde(default)]
            login: Option<String>,
            #[serde(default)]
            copilot_plan: Option<String>,
            #[serde(default)]
            access_type_sku: Option<String>,
            #[serde(default)]
            quota_snapshots: Option<CopilotQuotaSnapshots>,
            #[serde(default)]
            quota_reset_date: Option<String>,
        }
        let parsed: CopilotUser = serde_json::from_str(&body).map_err(|error| {
            ProviderError::InvalidResponse(format!("Failed to parse account response: {error}"))
        })?;
        let snapshot = parsed
            .quota_snapshots
            .as_ref()
            .and_then(|s| s.premium_interactions.as_ref().or(s.chat.as_ref()));
        let quota_unlimited = snapshot.and_then(|s| s.unlimited).unwrap_or(false);
        let quota_percent = snapshot.and_then(|s| {
            s.percent_remaining.or_else(|| {
                let rem = s.remaining?;
                let ent = s.entitlement?;
                (ent > 0.0).then(|| (rem / ent * 100.0).clamp(0.0, 100.0))
            })
        });
        let quota_remaining = snapshot
            .and_then(|s| s.remaining)
            .filter(|v| *v >= 0.0)
            .map(|v| v.round() as u64);
        let quota_entitlement = snapshot
            .and_then(|s| s.entitlement)
            .filter(|v| *v > 0.0)
            .map(|v| v.round() as u64);
        Ok(Some(CopilotAccount {
            login: parsed.login,
            plan: copilot_plan_label(
                parsed.copilot_plan.as_deref(),
                parsed.access_type_sku.as_deref(),
            ),
            quota_percent,
            quota_remaining,
            quota_entitlement,
            quota_unlimited,
            quota_reset_date: parsed.quota_reset_date,
        }))
    }

    async fn exchange(&self, github_token: &str) -> Result<CopilotAccess, ProviderError> {
        let url = format!("{}/copilot_internal/v2/token", self.github_api_base);
        let response = send_with_retry(self.transport.as_ref(), || {
            Request::get(url.as_str())
                .header(AUTHORIZATION, format!("token {github_token}"))
                .header(ACCEPT, "application/vnd.github+json")
                .header(USER_AGENT, crate::COPILOT_USER_AGENT)
                .header("Editor-Version", crate::COPILOT_EDITOR_VERSION)
                .header("Editor-Plugin-Version", crate::COPILOT_PLUGIN_VERSION)
                .header("X-GitHub-Api-Version", "2022-11-28")
                .body(Bytes::new())
                .map_err(|error| {
                    ProviderError::InvalidResponse(format!(
                        "Failed to build token request: {error}"
                    ))
                })
        })
        .await?;
        let body = read_checked_body(response).await?;

        let parsed: TokenResponse = serde_json::from_str(&body).map_err(|error| {
            ProviderError::InvalidResponse(format!(
                "Failed to parse Copilot token response: {error}"
            ))
        })?;
        let api_url = parsed
            .endpoints
            .and_then(|endpoints| endpoints.api)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_COPILOT_API_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        let expires_at = if parsed.expires_at == 0 {
            u64::MAX
        } else {
            parsed.expires_at
        };

        Ok(CopilotAccess {
            token: parsed.token,
            api_url,
            expires_at,
        })
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
    #[serde(default)]
    expires_at: u64,
    #[serde(default)]
    endpoints: Option<Endpoints>,
}

#[derive(Deserialize)]
struct Endpoints {
    #[serde(default)]
    api: Option<String>,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

fn default_expires_in() -> u64 {
    900
}

fn default_interval() -> u64 {
    5
}

#[derive(Deserialize)]
struct DeviceTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

fn copilot_plan_label(plan: Option<&str>, sku: Option<&str>) -> Option<String> {
    let sku = sku.unwrap_or_default().to_ascii_lowercase();
    if sku.contains("free") {
        return Some("Copilot Free".to_string());
    }
    if sku.contains("business") {
        return Some("Copilot Business".to_string());
    }
    if sku.contains("enterprise") {
        return Some("Copilot Enterprise".to_string());
    }
    match plan.map(str::to_ascii_lowercase).as_deref() {
        Some("individual") => Some("Copilot Pro".to_string()),
        Some("business") => Some("Copilot Business".to_string()),
        Some("enterprise") => Some("Copilot Enterprise".to_string()),
        Some(other) if !other.is_empty() => Some(format!("Copilot {other}")),
        _ => None,
    }
}
