//! API error type. Every failure is a specific, bounded JSON error — never a
//! silent fallback or an infinite-loading state.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Minimal boxed-error helper so the dependency list stays small.
pub type BoxErr = Box<dyn std::error::Error + Send + Sync + 'static>;

pub fn err_msg(m: impl Into<String>) -> BoxErr {
    #[derive(Debug)]
    struct Msg(String);
    impl std::fmt::Display for Msg {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for Msg {}
    Box::new(Msg(m.into()))
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("unauthenticated")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("rate limited; slow down and retry")]
    RateLimited,
    #[error("upstream error: {0}")]
    Upstream(String),
    #[error("not configured: {0}")]
    NotConfigured(String),
    #[error("timeout waiting for {0}")]
    Timeout(String),
    #[error("internal error: {0}")]
    Internal(#[source] BoxErr),
}

impl ApiError {
    pub fn internal<E: std::error::Error + Send + Sync + 'static>(e: E) -> Self {
        Self::Internal(Box::new(e))
    }
    pub fn internal_msg(msg: impl Into<String>) -> Self {
        Self::Internal(err_msg(msg))
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::Upstream(_) => StatusCode::BAD_GATEWAY,
            Self::NotConfigured(_) => StatusCode::PRECONDITION_FAILED,
            Self::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "bad_request",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden(_) => "forbidden",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::RateLimited => "rate_limited",
            Self::Upstream(_) => "upstream_error",
            Self::NotConfigured(_) => "not_configured",
            Self::Timeout(_) => "timeout",
            Self::Internal(_) => "internal",
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        if matches!(self, Self::Internal(_)) {
            tracing::error!(error = %self, "internal api error");
        }
        let message = match &self {
            Self::Internal(_) => "internal server error".to_string(),
            other => other.to_string(),
        };
        (
            status,
            Json(json!({ "error": { "code": self.code(), "message": message } })),
        )
            .into_response()
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        match e {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ErrorCode::ConstraintViolation,
                    ..
                },
                _,
            ) => Self::Conflict("constraint violation".into()),
            other => Self::internal(other),
        }
    }
}
