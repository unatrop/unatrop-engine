use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http::Request;
use http::header::{ACCEPT, CONTENT_TYPE, LOCATION, USER_AGENT};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use transport::HttpTransport;
use uno_tools_core::{RetentionConfig, ToolError, ToolOutput, TypedTool, prune_text};

use crate::html::html_to_markdown;
use crate::read_limited_body;
use crate::ssrf::{resolve_and_validate_url, validate_public_url};

const DEFAULT_MAX_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_MAX_CHARS: usize = 24_000;
const MAX_ALLOWED_CHARS: usize = 40_000;
const MAX_REDIRECTS: usize = 5;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Аргументы инструмента `web_fetch`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebFetchArgs {
    /// Публичный HTTP/HTTPS URL страницы или документа
    pub url: String,
    /// Максимальное число символов в итоговом ответе (по умолчанию 24000, не более 40000)
    #[serde(default)]
    pub max_chars: Option<usize>,
}

#[derive(Serialize)]
struct WebFetchResult {
    url: String,
    status: u16,
    untrusted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    content: String,
    truncated: bool,
}

/// Инструмент безопасной загрузки и очистки веб-страниц в Markdown/текст
pub struct WebFetchTool {
    transport: Arc<dyn HttpTransport>,
    max_bytes: usize,
    timeout: Duration,
    allow_private: bool,
    verify_dns: bool,
    user_agent: String,
}

impl WebFetchTool {
    /// Создаёт `WebFetchTool` с защитой от SSRF, таймаутом 15с и лимитом 2 МиБ
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            transport,
            max_bytes: DEFAULT_MAX_BYTES,
            timeout: DEFAULT_TIMEOUT,
            allow_private: false,
            verify_dns: false,
            user_agent: format!("unatrop-web/{}", env!("CARGO_PKG_VERSION")),
        }
    }

    /// Разрешает приватные/локальные адреса (для тестов или внутренних сетей)
    pub fn allow_private(mut self, allow: bool) -> Self {
        self.allow_private = allow;
        self
    }

    /// Включает обязательную DNS-проверку публичности всех разрешённых IP перед HTTP-запросом
    pub fn with_dns_check(mut self, verify: bool) -> Self {
        self.verify_dns = verify;
        self
    }

    async fn check_url(&self, raw: &str) -> Result<url::Url, String> {
        if self.verify_dns {
            resolve_and_validate_url(raw, self.allow_private).await
        } else {
            validate_public_url(raw, self.allow_private)
        }
    }

    /// Переопределяет таймаут HTTP-запроса
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Переопределяет лимит скачиваемого тела ответа в байтах
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }
}

#[async_trait]
impl TypedTool for WebFetchTool {
    type Args = WebFetchArgs;

    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a public HTTP/HTTPS URL and extract clean readable Markdown/text."
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn run(&self, args: Self::Args) -> Result<ToolOutput, ToolError> {
        let mut current_url = match self.check_url(&args.url).await {
            Ok(url) => url,
            Err(err) => return Ok(ToolOutput::error(err)),
        };

        let mut redirects = 0usize;
        let response = loop {
            let request = Request::get(current_url.as_str())
                .header(USER_AGENT, self.user_agent.as_str())
                .header(
                    ACCEPT,
                    "text/html,application/xhtml+xml,text/markdown,text/plain,application/json;q=0.9,*/*;q=0.1",
                )
                .body(Bytes::new())
                .map_err(|err| ToolError::Execution(format!("failed to build request: {err}")))?;

            let resp = tokio::time::timeout(self.timeout, self.transport.send(request))
                .await
                .map_err(|_| ToolError::Execution("web_fetch request timed out".into()))?
                .map_err(|err| ToolError::Execution(err.to_string()))?;

            if resp.status().is_redirection() {
                if redirects >= MAX_REDIRECTS {
                    return Ok(ToolOutput::error("web_fetch exceeded maximum redirects"));
                }
                let Some(loc) = resp.headers().get(LOCATION).and_then(|v| v.to_str().ok()) else {
                    return Ok(ToolOutput::error(
                        "web_fetch received redirect without Location header",
                    ));
                };
                let joined = match current_url.join(loc) {
                    Ok(url) => url,
                    Err(err) => {
                        return Ok(ToolOutput::error(format!("invalid redirect URL: {err}")));
                    }
                };
                current_url = match self.check_url(joined.as_str()).await {
                    Ok(url) => url,
                    Err(err) => {
                        return Ok(ToolOutput::error(format!("blocked redirect target: {err}")));
                    }
                };
                redirects += 1;
                continue;
            }

            break resp;
        };

        let status = response.status();
        let status_code = status.as_u16();
        if !status.is_success() {
            return Ok(ToolOutput::error(format!(
                "web_fetch failed for `{current_url}` with HTTP {status_code}"
            )));
        }

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        if !is_text_content_type(&content_type) {
            return Ok(ToolOutput::error(format!(
                "unsupported binary content-type `{content_type}` for web_fetch"
            )));
        }

        let (bytes, body_truncated) =
            read_limited_body(response.into_body(), self.max_bytes, self.timeout)
                .await
                .map_err(ToolError::Execution)?;

        let raw_text = String::from_utf8_lossy(&bytes);
        let (title, extracted) = if content_type.is_empty()
            || content_type.contains("html")
            || raw_text.trim_start().starts_with('<')
        {
            let html = html_to_markdown(&raw_text);
            (html.title, html.text)
        } else {
            (None, raw_text.into_owned())
        };

        let max_chars = args
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(256, MAX_ALLOWED_CHARS);
        let head = max_chars * 3 / 4;
        let tail = max_chars / 8;
        let retention = RetentionConfig::new(max_chars, head, tail).unwrap_or_default();
        let pruned = prune_text(&extracted, retention);

        let result = WebFetchResult {
            url: current_url.to_string(),
            status: status_code,
            untrusted: true,
            title,
            content: pruned.text,
            truncated: body_truncated || pruned.truncated,
        };
        let json =
            serde_json::to_string(&result).map_err(|err| ToolError::Execution(err.to_string()))?;
        Ok(ToolOutput::text(json))
    }
}

fn is_text_content_type(ct: &str) -> bool {
    if ct.is_empty() {
        return true;
    }
    let mime = ct.split(';').next().unwrap_or("").trim();
    mime.starts_with("text/")
        || mime.contains("json")
        || mime.contains("xml")
        || mime.contains("xhtml")
        || mime == "application/javascript"
}
