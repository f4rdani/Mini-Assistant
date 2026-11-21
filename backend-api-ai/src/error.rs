use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug)]
pub enum AppError {
    BadRequest(String),
    Unauthorized(String),
    Upstream(String),
    Internal(String),
    NoAccount,
    Busy,
    TooLarge(String),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::BadRequest(m) => write!(f, "bad request: {m}"),
            AppError::Unauthorized(m) => write!(f, "unauthorized: {m}"),
            AppError::Upstream(m) => write!(f, "upstream: {m}"),
            AppError::Internal(m) => write!(f, "internal error: {m}"),
            AppError::NoAccount => write!(f, "no account configured"),
            AppError::Busy => write!(f, "all accounts busy"),
            AppError::TooLarge(m) => write!(f, "too large: {m}"),
        }
    }
}

impl std::error::Error for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, kind, msg) = match self {
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, "invalid_request_error", m),
            AppError::Unauthorized(m) => (StatusCode::UNAUTHORIZED, "authentication_error", m),
            AppError::Upstream(m) => (StatusCode::BAD_GATEWAY, "upstream_error", m),
            AppError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error", m),
            AppError::Busy => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_exceeded",
                "All accounts are busy, retry shortly.".to_string(),
            ),
            AppError::TooLarge(m) => (StatusCode::PAYLOAD_TOO_LARGE, "invalid_request_error", m),
            AppError::NoAccount => (
                StatusCode::SERVICE_UNAVAILABLE,
                "no_account",
                "No DeepSeek account configured. POST /admin/accounts with {name, cookie, token}.".to_string(),
            ),
        };
        tracing::warn!("{status} {kind}: {msg}");
        (
            status,
            Json(json!({ "error": { "message": msg, "type": kind, "code": kind } })),
        )
            .into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError::Upstream(format!("{e:#}"))
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        AppError::Upstream(e.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError::BadRequest(e.to_string())
    }
}
