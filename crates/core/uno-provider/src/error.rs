use transport::TransportError;

/// Классифицированный тип ошибки провайдера LLM
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderErrorKind {
    /// Исчерпана квота или баланс аккаунта (не ретраится)
    Quota,
    /// Ошибка аутентификации или прав доступа (401/403, не ретраится)
    Auth,
    /// Временное ограничение частоты запросов (429 rate limit, ретраится)
    RateLimit,
    /// Перегрузка модели или бэкенда (529/503 overloaded, ретраится)
    Overloaded,
    /// Превышена максимальная длина входного контекста (не ретраится)
    ContextLength,
    /// Некорректный запрос или параметры (400/404/422, не ретраится)
    BadRequest,
    /// Внутренняя ошибка сервера провайдера (5xx, ретраится)
    Server,
    /// Сетевая ошибка транспорта
    Transport,
    /// Прочие ошибки (парсинг, конфигурация, отмена)
    Other,
}

impl ProviderErrorKind {
    /// Возвращает `true`, если данный класс ошибки является временным и допускает повтор запроса
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::RateLimit | Self::Overloaded | Self::Server)
    }
}

/// Ошибка обращения к провайдеру LLM
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// Ошибка HTTP-транспорта
    #[error("transport failed: {0}")]
    Transport(#[from] TransportError),

    /// Провайдер вернул неуспешный HTTP-статус
    #[error("provider returned HTTP error {status}: {body}")]
    HttpStatus { status: u16, body: String },

    /// Ошибка, переданная внутри SSE-потока
    #[error("provider reported an error in the stream: {message}")]
    StreamError { message: String, retryable: bool },

    /// Ответ или фрейм потока не соответствует ожидаемому формату
    #[error("provider returned an invalid response: {0}")]
    InvalidResponse(String),

    /// Ошибка конфигурации или отсутствующие учётные данные
    #[error("provider configuration error: {0}")]
    Configuration(String),

    /// Операция отменена вызывающей стороной
    #[error("operation was cancelled")]
    Cancelled,
}

impl ProviderError {
    /// Конструирует `StreamError`, вычисляя `retryable` через классификацию текста и кода ошибки
    pub fn from_stream_error(message: impl Into<String>, fallback_retryable: bool) -> Self {
        let message = message.into();
        let retryable = match classify_error_text(&message) {
            Some(kind) => kind.is_retryable(),
            None => fallback_retryable,
        };
        Self::StreamError { message, retryable }
    }

    /// Возвращает классифицированный тип ошибки [`ProviderErrorKind`]
    pub fn kind(&self) -> ProviderErrorKind {
        match self {
            Self::HttpStatus { status, body } => classify_http_error(*status, body),
            Self::StreamError { message, retryable } => {
                if let Some(kind) = classify_error_text(message) {
                    kind
                } else if *retryable {
                    ProviderErrorKind::Server
                } else {
                    ProviderErrorKind::BadRequest
                }
            }
            Self::Transport(_) => ProviderErrorKind::Transport,
            Self::InvalidResponse(_) | Self::Configuration(_) | Self::Cancelled => {
                ProviderErrorKind::Other
            }
        }
    }

    /// Возвращает `true` для временных ошибок (определяется по [`ProviderErrorKind`], а для транспорта — по `TransportError::is_retryable`)
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(error) => error.is_retryable(),
            Self::StreamError { message, retryable } => match classify_error_text(message) {
                Some(kind) => kind.is_retryable(),
                None => *retryable,
            },
            other => other.kind().is_retryable(),
        }
    }
}

/// Классифицирует HTTP-ответ с ошибкой по статусу и телу ответа
pub fn classify_http_error(status: u16, body: &str) -> ProviderErrorKind {
    if let Some(kind) = classify_error_text(body) {
        // Если тело явно указывает на Quota, ContextLength, Auth, Overloaded или RateLimit — приоритет у точного маркера
        match kind {
            ProviderErrorKind::Quota
            | ProviderErrorKind::ContextLength
            | ProviderErrorKind::Auth
            | ProviderErrorKind::Overloaded
            | ProviderErrorKind::RateLimit => return kind,
            _ => {}
        }
    }

    match status {
        401 | 403 => ProviderErrorKind::Auth,
        402 => ProviderErrorKind::Quota,
        413 => ProviderErrorKind::ContextLength,
        429 => ProviderErrorKind::RateLimit,
        503 | 529 => ProviderErrorKind::Overloaded,
        400..=499 => ProviderErrorKind::BadRequest,
        500..=599 => ProviderErrorKind::Server,
        _ => ProviderErrorKind::Other,
    }
}

/// Распознаёт семантический тип ошибки по тексту/JSON тела или сообщения SSE-потока
pub fn classify_error_text(text: &str) -> Option<ProviderErrorKind> {
    let lower = text.to_ascii_lowercase();

    if [
        "insufficient_quota",
        "quota_exceeded",
        "billing_hard_limit",
        "credit balance is too low",
        "exceeded your current quota",
        "out of credits",
        "payment_required",
    ]
    .iter()
    .any(|m| lower.contains(m))
    {
        return Some(ProviderErrorKind::Quota);
    }

    if [
        "context_length_exceeded",
        "maximum context length",
        "prompt is too long",
        "input is too long",
        "request_too_large",
        "string_above_max_length",
        "too many tokens",
        "exceeds the model's maximum context",
    ]
    .iter()
    .any(|m| lower.contains(m))
    {
        return Some(ProviderErrorKind::ContextLength);
    }

    if [
        "invalid_api_key",
        "authentication_error",
        "unauthenticated",
        "unauthorized",
        "permission_denied",
        "invalid x-api-key",
        "invalid bearer token",
    ]
    .iter()
    .any(|m| lower.contains(m))
    {
        return Some(ProviderErrorKind::Auth);
    }

    if [
        "overloaded_error",
        "overloaded",
        "overload",
        "server_is_overloaded",
        "temporarily_unavailable",
        "model_overloaded",
    ]
    .iter()
    .any(|m| lower.contains(m))
    {
        return Some(ProviderErrorKind::Overloaded);
    }

    if [
        "rate_limit",
        "rate limit",
        "too_many_requests",
        "resource_exhausted",
        "requests per minute",
        "tokens per minute",
    ]
    .iter()
    .any(|m| lower.contains(m))
    {
        return Some(ProviderErrorKind::RateLimit);
    }

    if [
        "invalid_request_error",
        "bad_request",
        "invalid_argument",
        "unknown_parameter",
    ]
    .iter()
    .any(|m| lower.contains(m))
    {
        return Some(ProviderErrorKind::BadRequest);
    }

    if ["server_error", "internal_error", "api_error"]
        .iter()
        .any(|m| lower.contains(m))
    {
        return Some(ProviderErrorKind::Server);
    }

    None
}
