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

use crate::stack::StackError;
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
    /// The class is already in the Stack.
    #[error("the class is already in the Stack")]
    AlreadyInStack,
    /// Peloton refused to schedule the class.
    #[error("Peloton cannot schedule the class")]
    CannotSchedule {
        /// Peloton's explanation (already capped by the upstream client).
        message: String,
    },
    /// Peloton failed or answered something unusable.
    #[error("upstream error")]
    Upstream,
    /// The relay failed while handling the request (a handler panicked).
    #[error("internal error")]
    Internal,
}

impl fmt::Debug for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput => f.write_str("InvalidInput"),
            Self::NotFound => f.write_str("NotFound"),
            Self::MethodNotAllowed => f.write_str("MethodNotAllowed"),
            Self::NotAuthenticated => f.write_str("NotAuthenticated"),
            Self::AlreadyInStack => f.write_str("AlreadyInStack"),
            Self::RateLimited { retry_after } => f
                .debug_struct("RateLimited")
                .field("retry_after", retry_after)
                .finish(),
            Self::CannotSchedule { .. } => f.debug_struct("CannotSchedule").finish_non_exhaustive(),
            Self::Upstream => f.write_str("Upstream"),
            Self::Internal => f.write_str("Internal"),
        }
    }
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            Self::InvalidInput => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            Self::AlreadyInStack => StatusCode::CONFLICT,
            Self::CannotSchedule { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            Self::NotAuthenticated | Self::RateLimited { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Upstream => StatusCode::BAD_GATEWAY,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The machine-readable code of the error body, e.g. `invalid_input`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::NotFound => "not_found",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::NotAuthenticated => "not_authenticated",
            Self::RateLimited { .. } => "upstream_rate_limited",
            Self::AlreadyInStack => "already_in_stack",
            Self::CannotSchedule { .. } => "cannot_schedule",
            Self::Upstream => "upstream_error",
            Self::Internal => "internal_error",
        }
    }

    fn body(&self) -> ErrorBody {
        let message = match self {
            Self::InvalidInput => Cow::Borrowed("The request is invalid."),
            Self::NotFound => Cow::Borrowed("No such resource."),
            Self::MethodNotAllowed => Cow::Borrowed("This resource does not support that method."),
            Self::NotAuthenticated => Cow::Borrowed("The relay is not authenticated with Peloton."),
            Self::RateLimited { .. } => {
                Cow::Borrowed("Peloton is rate limiting the relay; retry later.")
            }
            Self::AlreadyInStack => Cow::Borrowed("The class is already in the Stack."),
            Self::CannotSchedule { message } => Cow::Owned(message.clone()),
            Self::Upstream => Cow::Borrowed("Peloton failed to answer the request."),
            Self::Internal => Cow::Borrowed("internal error"),
        };
        ErrorBody {
            error: self.code(),
            message,
        }
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

impl From<StackError> for ApiError {
    fn from(error: StackError) -> Self {
        match error {
            StackError::AlreadyInStack => Self::AlreadyInStack,
            StackError::NotInStack => Self::NotFound,
            StackError::Duplicate => Self::InvalidInput,
            StackError::Undecodable => {
                tracing::warn!(%error, "Stack write refused");
                Self::Upstream
            }
            StackError::Upstream(error) => error.into(),
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
