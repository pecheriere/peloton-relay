//! API error responses: `{"error": "<code>", "message": "<text>"}`.
//!
//! Messages are fixed text, except that `cannot_schedule` carries Peloton's
//! short user-facing explanation. Neither upstream response bodies nor tokens
//! ever reach a response.

use std::borrow::Cow;
use std::fmt;
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
    pub message: Cow<'static, str>,
}

/// An error the API reports to its clients.
///
/// `Debug` never shows [`ApiError::CannotSchedule`]'s message, so logging an
/// error with `?` cannot leak it.
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
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
    /// Peloton refused to schedule the class.
    #[error("Peloton cannot schedule the class")]
    CannotSchedule {
        /// Peloton's explanation (already capped by the upstream client).
        message: String,
    },
    /// Peloton failed or answered something unusable.
    #[error("upstream error")]
    Upstream,
}

impl fmt::Debug for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput => f.write_str("InvalidInput"),
            Self::NotFound => f.write_str("NotFound"),
            Self::MethodNotAllowed => f.write_str("MethodNotAllowed"),
            Self::NotAuthenticated => f.write_str("NotAuthenticated"),
            Self::RateLimited { retry_after } => f
                .debug_struct("RateLimited")
                .field("retry_after", retry_after)
                .finish(),
            Self::CannotSchedule { .. } => f.debug_struct("CannotSchedule").finish_non_exhaustive(),
            Self::Upstream => f.write_str("Upstream"),
        }
    }
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            Self::InvalidInput => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            Self::CannotSchedule { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            Self::NotAuthenticated | Self::RateLimited { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Upstream => StatusCode::BAD_GATEWAY,
        }
    }

    fn body(&self) -> ErrorBody {
        let fixed = |error, message| (error, Cow::Borrowed(message));
        let (error, message) = match self {
            Self::InvalidInput => fixed("invalid_input", "The request is invalid."),
            Self::NotFound => fixed("not_found", "No such resource."),
            Self::MethodNotAllowed => fixed(
                "method_not_allowed",
                "This resource does not support that method.",
            ),
            Self::NotAuthenticated => fixed(
                "not_authenticated",
                "The relay is not authenticated with Peloton.",
            ),
            Self::RateLimited { .. } => fixed(
                "upstream_rate_limited",
                "Peloton is rate limiting the relay; retry later.",
            ),
            Self::CannotSchedule { message } => ("cannot_schedule", Cow::Owned(message.clone())),
            Self::Upstream => fixed("upstream_error", "Peloton failed to answer the request."),
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
            UpstreamError::CannotSchedule { message } => Self::CannotSchedule { message },
            UpstreamError::Upstream { .. }
            | UpstreamError::Decode { .. }
            | UpstreamError::Transport
            | UpstreamError::GraphQl => {
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
