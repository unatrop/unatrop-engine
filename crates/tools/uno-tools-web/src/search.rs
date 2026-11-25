use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http::Request;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use transport::HttpTransport;
use uno_tools_core::{ToolError, ToolOutput, TypedTool};
use url::Url;

use crate::read_limited_body;

const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 10;
const DEFAULT_SEARCH_BYTES: usize = 1024 * 1024;
const DEFAULT_SEARCH_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_TOTAL_SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Результат веб-поиска
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Поддерживаемый JSON API веб-поиска (SearXNG, пул SearXNG, Brave, Tavily)
#[derive(Debug, Clone)]
pub enum SearchBackend {
    /// Self-hosted SearXNG (`{base_url}/search?q=...&format=json`), без ключа
    Searxng { base_url: String },
    /// Пул инстансов SearXNG с автоматическим fallback по очереди, без ключа
    SearxngPool { base_urls: Vec<String> },
    /// Brave Search API (требует ключ)
    Brave { api_key: String },
    /// Tavily Search API (требует ключ)
    Tavily { api_key: String },
}

#[derive(Debug, Clone)]
enum AttemptBackend<'a> {
    Searxng { base_url: &'a str },
    Brave { api_key: &'a str },
    Tavily { api_key: &'a str },
}

impl AttemptBackend<'_> {
    fn label(&self) -> &'static str {
        match self {
            Self::Searxng { .. } => "searxng",
            Self::Brave { .. } => "brave",
            Self::Tavily { .. } => "tavily",
        }
    }

    fn target_desc(&self) -> String {
        match self {
            Self::Searxng { base_url } => format!("searxng({base_url})"),
            Self::Brave { .. } => "brave".to_owned(),
            Self::Tavily { .. } => "tavily".to_owned(),
        }
    }
}

/// Аргументы инструмента `web_search`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebSearchArgs {
    /// Поисковый запрос
    pub query: String,
    /// Максимум результатов (по умолчанию 5, не более 10)
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Serialize)]
struct WebSearchResult {
    query: String,
    backend: &'static str,
    results: Vec<SearchHit>,
}

/// Инструмент веб-поиска поверх JSON API (`SearXNG` / `SearXNG Pool` / `Brave` / `Tavily`)
pub struct WebSearchTool {
    transport: Arc<dyn HttpTransport>,
    backends: Vec<SearchBackend>,
    timeout: Duration,
    total_timeout: Duration,
    user_agent: String,
}

impl WebSearchTool {
    /// Создаёт инструмент поиска с основным бэкендом
    pub fn new(transport: Arc<dyn HttpTransport>, backend: SearchBackend) -> Self {
        Self::with_backends(transport, vec![backend])
    }

    /// Создаёт инструмент поиска с fallback-пулом инстансов SearXNG без API-ключей
    pub fn searxng_pool<I, S>(transport: Arc<dyn HttpTransport>, base_urls: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let urls = base_urls.into_iter().map(Into::into).collect();
        Self::new(transport, SearchBackend::SearxngPool { base_urls: urls })
    }

    /// Создаёт инструмент поиска с цепочкой fallback-бэкендов по приоритету
    pub fn with_backends(transport: Arc<dyn HttpTransport>, backends: Vec<SearchBackend>) -> Self {
        Self {
            transport,
            backends,
            timeout: DEFAULT_SEARCH_TIMEOUT,
            total_timeout: DEFAULT_TOTAL_SEARCH_TIMEOUT,
            user_agent: format!("unatrop-web/{}", env!("CARGO_PKG_VERSION")),
        }
    }

    /// Добавляет резервный бэкенд в конец цепочки fallback
    pub fn with_fallback(mut self, backend: SearchBackend) -> Self {
        self.backends.push(backend);
        self
    }

    /// Переопределяет таймаут поискового запроса на каждую попытку
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Переопределяет общий бюджет времени на весь пул fallback-попыток
    pub fn with_total_timeout(mut self, total_timeout: Duration) -> Self {
        self.total_timeout = total_timeout;
        self
    }

    fn expand_attempts(&self) -> Vec<AttemptBackend<'_>> {
        let mut attempts = Vec::new();
        for backend in &self.backends {
            match backend {
                SearchBackend::Searxng { base_url } => {
                    let trimmed = base_url.trim();
                    if !trimmed.is_empty() {
                        attempts.push(AttemptBackend::Searxng { base_url: trimmed });
                    }
                }
                SearchBackend::SearxngPool { base_urls } => {
                    for url in base_urls {
                        let trimmed = url.trim();
                        if !trimmed.is_empty() {
                            attempts.push(AttemptBackend::Searxng { base_url: trimmed });
                        }
                    }
                }
                SearchBackend::Brave { api_key } => {
                    attempts.push(AttemptBackend::Brave { api_key });
                }
                SearchBackend::Tavily { api_key } => {
                    attempts.push(AttemptBackend::Tavily { api_key });
                }
            }
        }
        attempts
    }

    async fn execute_attempt(
        &self,
        attempt: &AttemptBackend<'_>,
        query: &str,
        limit: usize,
        attempt_timeout: Duration,
    ) -> Result<Vec<SearchHit>, String> {
        let request = build_request(attempt, &self.user_agent, query, limit)?;
        let response = tokio::time::timeout(attempt_timeout, self.transport.send(request))
            .await
            .map_err(|_| "request timed out".to_owned())?
            .map_err(|err| err.to_string())?;

        let status = response.status();
        let (bytes, _) =
            read_limited_body(response.into_body(), DEFAULT_SEARCH_BYTES, attempt_timeout).await?;
        let body = String::from_utf8_lossy(&bytes);

        if !status.is_success() {
            let preview: String = body.chars().take(200).collect();
            return Err(format!("HTTP {}: {preview}", status.as_u16()));
        }

        parse_response(attempt, &body, limit)
    }
}

#[async_trait]
impl TypedTool for WebSearchTool {
    type Args = WebSearchArgs;

    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the public web and return a list of titles, URLs, and snippets."
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn run(&self, args: Self::Args) -> Result<ToolOutput, ToolError> {
        let query = args.query.trim();
        if query.is_empty() {
            return Ok(ToolOutput::error("search query cannot be empty"));
        }
        let attempts = self.expand_attempts();
        if attempts.is_empty() {
            return Ok(ToolOutput::error("no search backends configured"));
        }
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

        let mut errors = Vec::new();
        let mut empty_fallback: Option<&'static str> = None;
        let deadline = tokio::time::Instant::now() + self.total_timeout;

        for attempt in &attempts {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                errors.push("total search timeout budget exhausted".to_owned());
                break;
            }
            let attempt_timeout = self.timeout.min(remaining);
            match self
                .execute_attempt(attempt, query, limit, attempt_timeout)
                .await
            {
                Ok(results) if !results.is_empty() => {
                    let out = WebSearchResult {
                        query: query.to_owned(),
                        backend: attempt.label(),
                        results,
                    };
                    let json = serde_json::to_string(&out)
                        .map_err(|err| ToolError::Execution(err.to_string()))?;
                    return Ok(ToolOutput::text(json));
                }
                Ok(_) => {
                    // Успешный, но пустой ответ (частый случай для перегруженного SearXNG) —
                    // запоминаем как запасной вариант и пробуем следующий бэкенд.
                    if empty_fallback.is_none() {
                        empty_fallback = Some(attempt.label());
                    }
                }
                Err(err) => {
                    errors.push(format!("{}: {err}", attempt.target_desc()));
                }
            }
        }

        if let Some(backend) = empty_fallback {
            let out = WebSearchResult {
                query: query.to_owned(),
                backend,
                results: Vec::new(),
            };
            let json =
                serde_json::to_string(&out).map_err(|err| ToolError::Execution(err.to_string()))?;
            return Ok(ToolOutput::text(json));
        }

        Ok(ToolOutput::error(format!(
            "all search backends failed: {}",
            errors.join("; ")
        )))
    }
}

fn build_request(
    backend: &AttemptBackend<'_>,
    user_agent: &str,
    query: &str,
    limit: usize,
) -> Result<Request<Bytes>, String> {
    match backend {
        AttemptBackend::Searxng { base_url } => {
            let base = base_url.trim_end_matches('/');
            let mut url = Url::parse(&format!("{base}/search")).map_err(|e| e.to_string())?;
            url.query_pairs_mut()
                .append_pair("q", query)
                .append_pair("format", "json");
            Request::get(url.as_str())
                .header(USER_AGENT, user_agent)
                .header(ACCEPT, "application/json")
                .body(Bytes::new())
                .map_err(|e| e.to_string())
        }
        AttemptBackend::Brave { api_key } => {
            let mut url = Url::parse("https://api.search.brave.com/res/v1/web/search")
                .map_err(|e| e.to_string())?;
            url.query_pairs_mut()
                .append_pair("q", query)
                .append_pair("count", &limit.to_string());
            Request::get(url.as_str())
                .header(USER_AGENT, user_agent)
                .header(ACCEPT, "application/json")
                .header("X-Subscription-Token", *api_key)
                .body(Bytes::new())
                .map_err(|e| e.to_string())
        }
        AttemptBackend::Tavily { api_key } => {
            let payload = serde_json::json!({
                "query": query,
                "max_results": limit,
            });
            let body = Bytes::from(serde_json::to_vec(&payload).map_err(|e| e.to_string())?);
            Request::post("https://api.tavily.com/search")
                .header(USER_AGENT, user_agent)
                .header(ACCEPT, "application/json")
                .header(CONTENT_TYPE, "application/json")
                .header(AUTHORIZATION, format!("Bearer {api_key}"))
                .body(body)
                .map_err(|e| e.to_string())
        }
    }
}

fn parse_response(
    backend: &AttemptBackend<'_>,
    body: &str,
    limit: usize,
) -> Result<Vec<SearchHit>, String> {
    match backend {
        AttemptBackend::Searxng { .. } => {
            let parsed: SearxngResponse =
                serde_json::from_str(body).map_err(|err| format!("invalid SearXNG JSON: {err}"))?;
            Ok(parsed
                .results
                .into_iter()
                .take(limit)
                .map(|r| SearchHit {
                    title: r.title,
                    url: r.url,
                    snippet: r.content,
                })
                .collect())
        }
        AttemptBackend::Brave { .. } => {
            let parsed: BraveResponse =
                serde_json::from_str(body).map_err(|err| format!("invalid Brave JSON: {err}"))?;
            Ok(parsed
                .web
                .map(|w| w.results)
                .unwrap_or_default()
                .into_iter()
                .take(limit)
                .map(|r| SearchHit {
                    title: r.title,
                    url: r.url,
                    snippet: r.description,
                })
                .collect())
        }
        AttemptBackend::Tavily { .. } => {
            let parsed: TavilyResponse =
                serde_json::from_str(body).map_err(|err| format!("invalid Tavily JSON: {err}"))?;
            Ok(parsed
                .results
                .into_iter()
                .take(limit)
                .map(|r| SearchHit {
                    title: r.title,
                    url: r.url,
                    snippet: r.content,
                })
                .collect())
        }
    }
}

#[derive(Deserialize)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<SearxngItem>,
}

#[derive(Deserialize)]
struct SearxngItem {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

#[derive(Deserialize)]
struct BraveResponse {
    #[serde(default)]
    web: Option<BraveWeb>,
}

#[derive(Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveItem>,
}

#[derive(Deserialize)]
struct BraveItem {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    description: String,
}

#[derive(Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    results: Vec<TavilyItem>,
}

#[derive(Deserialize)]
struct TavilyItem {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}
