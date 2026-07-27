use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug)]
pub enum AppError {
    BadRequest(String),
    NotFound(String),
    MethodNotAllowed(String),
    Upstream(String),
    Internal(anyhow::Error),
}

impl AppError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }

    pub fn method_not_allowed(msg: impl Into<String>) -> Self {
        Self::MethodNotAllowed(msg.into())
    }

    pub fn upstream(msg: impl Into<String>) -> Self {
        Self::Upstream(msg.into())
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(m) => write!(f, "bad request: {m}"),
            Self::NotFound(m) => write!(f, "not found: {m}"),
            Self::MethodNotAllowed(m) => write!(f, "method not allowed: {m}"),
            Self::Upstream(m) => write!(f, "upstream error: {m}"),
            Self::Internal(e) => write!(f, "internal error: {e}"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            Self::NotFound(m) => (StatusCode::NOT_FOUND, m.clone()),
            Self::MethodNotAllowed(m) => (StatusCode::METHOD_NOT_ALLOWED, m.clone()),
            Self::Upstream(m) => (StatusCode::BAD_GATEWAY, m.clone()),
            Self::Internal(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        if matches!(self, Self::Internal(_)) {
            tracing::error!(error = %message, "internal error");
        }
        (status, Json(json!({ "error": message }))).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;
