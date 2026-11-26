//! HTTP middleware for `uno-daemon`.

use axum::{
    extract::{Request, State},
    http::{StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::{IntoResponse, Response},
};

use super::state::DaemonState;

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Enforces `Authorization: Bearer <token>` on all routes except `/health`.
pub(crate) async fn auth_middleware(
    State(state): State<DaemonState>,
    req: Request,
    next: Next,
) -> Response {
    if req.uri().path() == "/health" {
        return next.run(req).await;
    }

    let Some(expected) = state.auth_token.as_deref() else {
        return next.run(req).await;
    };

    let authorized = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|tok| constant_time_eq(tok.as_bytes(), expected.as_bytes()));

    if authorized {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    }
}

pub(crate) fn is_loopback_origin(origin: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(origin) else {
        return false;
    };
    let Some(rest) = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))
    else {
        return false;
    };
    let check_port = |after: &str| {
        after.is_empty()
            || after
                .strip_prefix(':')
                .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    };
    if let Some(after) = rest.strip_prefix("localhost") {
        check_port(after)
    } else if let Some(after) = rest.strip_prefix("127.0.0.1") {
        check_port(after)
    } else if let Some(after) = rest.strip_prefix("[::1]") {
        check_port(after)
    } else {
        false
    }
}
