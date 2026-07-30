//! API error type rendered as RFC 9457 `application/problem+json`.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

const ERROR_BASE: &str = "https://suede.gameshow.pro/errors/";

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// Malformed request body or query.
    #[error("{0}")]
    BadRequest(String),
    /// Missing or invalid bearer token.
    #[error("authentication required")]
    Unauthorized,
    /// Understood but refused, e.g. a non-loopback heartbeat.
    #[error("{0}")]
    Forbidden(String),
    /// Unknown resource.
    #[error("{0}")]
    NotFound(String),
    /// Revision mismatch on a conditional write.
    #[error("{0}")]
    Conflict(String),
    /// Schema-valid but semantically invalid desired state.
    #[error("{0}")]
    Validation(String),
    /// Sway IPC (or another hard dependency) is unreachable.
    #[error("{0}")]
    Unavailable(String),
    /// Anything unexpected.
    #[error("{0}")]
    Internal(String),
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn slug(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "bad-request",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden(_) => "forbidden",
            Self::NotFound(_) => "not-found",
            Self::Conflict(_) => "conflict",
            Self::Validation(_) => "validation",
            Self::Unavailable(_) => "unavailable",
            Self::Internal(_) => "internal",
        }
    }

    fn title(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "Malformed request",
            Self::Unauthorized => "Authentication required",
            Self::Forbidden(_) => "Forbidden",
            Self::NotFound(_) => "Not found",
            Self::Conflict(_) => "Revision conflict",
            Self::Validation(_) => "Validation failed",
            Self::Unavailable(_) => "Dependency unavailable",
            Self::Internal(_) => "Internal error",
        }
    }
}

/// RFC 9457 problem document.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct Problem {
    /// URI reference identifying the error type.
    pub r#type: String,
    /// Short human-readable summary.
    pub title: String,
    /// HTTP status code.
    pub status: u16,
    /// Human-readable explanation specific to this occurrence.
    pub detail: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        } else {
            tracing::debug!(error = %self, status = status.as_u16(), "request rejected");
        }
        let problem = Problem {
            r#type: format!("{ERROR_BASE}{}", self.slug()),
            title: self.title().to_string(),
            status: status.as_u16(),
            detail: self.to_string(),
        };
        let body = serde_json::to_string(&problem)
            .unwrap_or_else(|_| r#"{"title":"Internal error","status":500}"#.to_string());
        (
            status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            body,
        )
            .into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
