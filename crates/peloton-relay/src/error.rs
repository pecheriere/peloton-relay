//! API error responses: `{"error": "<code>", "message": "<text>"}`.
//!
//! Messages are fixed text. Neither upstream response bodies nor tokens ever
//! reach a response.

use std::time::Duration;

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

use crate::upstream::UpstreamError;

/// The JSON body of every error response.
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    /// Machine-readable code, e.g. `invalid_input`.
    pub error: &'static str,
    /// Human-readable explanation.
    pub message: &'static str,
}

/// An error the API reports to its clients.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    /// The request failed validation.
    #[error("invalid input")]
    InvalidInput,
    /// No such route or resource.
    #[error("not found")]
    NotFound,
    /// The route exists but not for this method.
    #[error("method not allowed")]
    MethodNotAllowed,
    /// The relay holds no usable Peloton credential.
    #[error("not authenticated with Peloton")]
    NotAuthenticated,
    /// Peloton asked the relay to slow down.
    #[error("rate limited by Peloton")]
    RateLimited {
        /// How long Peloton asked us to wait, when it said so.
        retry_after: Option<Duration>,
    },
    /// Peloton failed or answered something unusable.
    #[error("upstream error")]
    Upstream,
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            Self::InvalidInput => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            Self::NotAuthenticated | Self::RateLimited { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Upstream => StatusCode::BAD_GATEWAY,
        }
    }

    fn body(&self) -> ErrorBody {
        let (error, message) = match self {
            Self::InvalidInput => ("invalid_input", "The request is invalid."),
            Self::NotFound => ("not_found", "No such resource."),
            Self::MethodNotAllowed => (
                "method_not_allowed",
                "This resource does not support that method.",
            ),
            Self::NotAuthenticated => (
                "not_authenticated",
                "The relay is not authenticated with Peloton.",
            ),
            Self::RateLimited { .. } => (
                "upstream_rate_limited",
                "Peloton is rate limiting the relay; retry later.",
            ),
            Self::Upstream => ("upstream_error", "Peloton failed to answer the request."),
        };
        ErrorBody { error, message }
    }
}

impl From<UpstreamError> for ApiError {
    fn from(error: UpstreamError) -> Self {
        match error {
            UpstreamError::NotAuthenticated => Self::NotAuthenticated,
            UpstreamError::NotFound => Self::NotFound,
            UpstreamError::RateLimited { retry_after } => Self::RateLimited { retry_after },
            UpstreamError::Upstream { .. }
            | UpstreamError::Decode { .. }
            | UpstreamError::Transport => {
                tracing::warn!(%error, "upstream request failed");
                Self::Upstream
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status(), Json(self.body())).into_response();
        if let Self::RateLimited {
            retry_after: Some(wait),
        } = self
        {
            let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        response
    }
}
