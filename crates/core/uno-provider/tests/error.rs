use uno_provider::{ProviderError, ProviderErrorKind};

#[test]
fn classifies_quota_vs_rate_limit_on_429() {
    let quota_err = ProviderError::HttpStatus {
        status: 429,
        body:
            r#"{"error":{"code":"insufficient_quota","message":"You exceeded your current quota"}}"#
                .into(),
    };
    assert_eq!(quota_err.kind(), ProviderErrorKind::Quota);
    assert!(!quota_err.is_retryable());

    let rate_err = ProviderError::HttpStatus {
        status: 429,
        body: r#"{"error":{"code":"rate_limit_exceeded","message":"Rate limit reached"}}"#.into(),
    };
    assert_eq!(rate_err.kind(), ProviderErrorKind::RateLimit);
    assert!(rate_err.is_retryable());
}

#[test]
fn classifies_auth_context_overloaded_and_bad_request() {
    let auth = ProviderError::HttpStatus {
        status: 401,
        body: "Unauthorized".into(),
    };
    assert_eq!(auth.kind(), ProviderErrorKind::Auth);
    assert!(!auth.is_retryable());

    let ctx = ProviderError::HttpStatus {
        status: 400,
        body: r#"{"error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#.into(),
    };
    assert_eq!(ctx.kind(), ProviderErrorKind::ContextLength);
    assert!(!ctx.is_retryable());

    let overloaded = ProviderError::HttpStatus {
        status: 529,
        body: r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
            .into(),
    };
    assert_eq!(overloaded.kind(), ProviderErrorKind::Overloaded);
    assert!(overloaded.is_retryable());

    let stream_overloaded =
        ProviderError::from_stream_error("overloaded_error: backend busy", false);
    assert_eq!(stream_overloaded.kind(), ProviderErrorKind::Overloaded);
    assert!(stream_overloaded.is_retryable());

    let stream_quota = ProviderError::from_stream_error("insufficient_quota: billing limit", true);
    assert_eq!(stream_quota.kind(), ProviderErrorKind::Quota);
    assert!(!stream_quota.is_retryable());
}
