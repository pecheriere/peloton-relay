//! The relay's read-only HTTP API.

use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, Query};
use axum::http::request::Parts;
use axum::routing::get;
use axum::{Json, Router};
use peloton::ids::PelotonId;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::{IntoParams, OpenApi, ToSchema};

use crate::error::{ApiError, ErrorBody};
use crate::model::{ClassDetails, Metrics, Profile, WorkoutDetail, WorkoutPage};
use crate::tokens::TokenManager;
use crate::upstream::{Peloton, WorkoutQuery};

const DEFAULT_LIMIT: u32 = 20;
const MAX_LIMIT: u32 = 100;
const MAX_EVERY_N: u32 = 60;

/// Shared state of every handler.
#[derive(Clone)]
pub struct AppState {
    /// The Peloton client.
    pub peloton: Arc<Peloton>,
    /// The credential holder, for readiness.
    pub tokens: Arc<TokenManager>,
}

/// Builds the router for the relay's API.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/me", get(me))
        .route("/v1/workouts", get(workouts))
        .route("/v1/workouts/{id}", get(workout))
        .route("/v1/workouts/{id}/metrics", get(metrics))
        .route("/v1/classes/{id}", get(class))
        .route("/openapi.json", get(spec))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .fallback(|| async { ApiError::NotFound })
        .method_not_allowed_fallback(|| async { ApiError::MethodNotAllowed })
        .with_state(state)
}

/// The `OpenAPI` 3.1 description of the API, generated from the handlers.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Peloton relay",
        description = "Read-only relay of a Peloton account's profile, workouts, metrics and classes."
    ),
    paths(me, workouts, workout, metrics, class, healthz, readyz),
    components(schemas(
        Profile,
        WorkoutPage,
        WorkoutDetail,
        Metrics,
        ClassDetails,
        ErrorBody,
        Status
    ))
)]
struct ApiDoc;

/// The generated `OpenAPI` document.
#[must_use]
pub fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

/// A path `{id}` that must be a valid Peloton id.
struct Id(PelotonId);

impl<S: Send + Sync> FromRequestParts<S> for Id {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let Path(raw) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::InvalidInput)?;
        raw.parse().map(Self).map_err(|_| ApiError::InvalidInput)
    }
}

/// A query string deserialised into `T`; rejection is the JSON error.
struct Params<T>(T);

impl<S: Send + Sync, T: serde::de::DeserializeOwned> FromRequestParts<S> for Params<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, ApiError> {
        Query::<T>::try_from_uri(&parts.uri)
            .map(|Query(value)| Self(value))
            .map_err(|_| ApiError::InvalidInput)
    }
}

/// Routes that take no query parameters; any parameter is rejected.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoParams {}

/// Query of `GET /v1/workouts`.
#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
struct WorkoutsParams {
    /// Zero-based page index; defaults to 0.
    #[param(minimum = 0)]
    page: Option<u32>,
    /// Page size, 1 to 100; defaults to 20.
    #[param(minimum = 1, maximum = 100)]
    limit: Option<u32>,
    /// Only workouts created at or after this RFC 3339 instant.
    #[param(format = DateTime)]
    from: Option<String>,
    /// Only workouts created at or before this RFC 3339 instant; after `from`.
    #[param(format = DateTime)]
    to: Option<String>,
}

impl WorkoutsParams {
    fn into_query(self) -> Result<WorkoutQuery, ApiError> {
        let limit = self.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(ApiError::InvalidInput);
        }
        let instant = |text: Option<String>| {
            text.map(|t| OffsetDateTime::parse(&t, &Rfc3339).map_err(|_| ApiError::InvalidInput))
                .transpose()
        };
        let from = instant(self.from)?;
        let to = instant(self.to)?;
        if let (Some(from), Some(to)) = (from, to)
            && from >= to
        {
            return Err(ApiError::InvalidInput);
        }
        Ok(WorkoutQuery {
            page: self.page.unwrap_or(0),
            limit,
            from,
            to,
        })
    }
}

/// Query of `GET /v1/workouts/{id}/metrics`.
#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
struct MetricsParams {
    /// Sample spacing in seconds, 1 to 60; defaults to 1.
    #[param(minimum = 1, maximum = 60)]
    every_n: Option<u32>,
}

/// Body of a successful probe.
#[derive(Serialize, ToSchema)]
struct Status {
    /// Always `ok` or `ready`.
    status: &'static str,
}

/// The authenticated user's profile.
#[utoipa::path(
    get,
    path = "/v1/me",
    responses(
        (status = 200, body = Profile),
        (status = 502, body = ErrorBody),
        (status = 503, body = ErrorBody)
    )
)]
async fn me(
    axum::extract::State(state): axum::extract::State<AppState>,
    Params(NoParams {}): Params<NoParams>,
) -> Result<Json<Profile>, ApiError> {
    let me = state.peloton.me().await?;
    Ok(Json(Profile::from_upstream(&me)))
}

/// Workouts, newest first, with paging information.
#[utoipa::path(
    get,
    path = "/v1/workouts",
    params(WorkoutsParams),
    responses(
        (status = 200, body = WorkoutPage),
        (status = 400, body = ErrorBody),
        (status = 502, body = ErrorBody),
        (status = 503, body = ErrorBody)
    )
)]
async fn workouts(
    axum::extract::State(state): axum::extract::State<AppState>,
    Params(params): Params<WorkoutsParams>,
) -> Result<Json<WorkoutPage>, ApiError> {
    let page = state.peloton.workouts(&params.into_query()?).await?;
    Ok(Json(WorkoutPage::from_upstream(&page)))
}

/// One workout with its class and instructor.
#[utoipa::path(
    get,
    path = "/v1/workouts/{id}",
    params(("id" = String, Path, description = "Workout id: 32 lowercase hex characters")),
    responses(
        (status = 200, body = WorkoutDetail),
        (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody),
        (status = 502, body = ErrorBody),
        (status = 503, body = ErrorBody)
    )
)]
async fn workout(
    axum::extract::State(state): axum::extract::State<AppState>,
    Id(id): Id,
    Params(NoParams {}): Params<NoParams>,
) -> Result<Json<WorkoutDetail>, ApiError> {
    let workout = state.peloton.workout(&id).await?;
    Ok(Json(WorkoutDetail::from_upstream(&workout)))
}

/// Time series recorded during a workout.
#[utoipa::path(
    get,
    path = "/v1/workouts/{id}/metrics",
    params(
        ("id" = String, Path, description = "Workout id: 32 lowercase hex characters"),
        MetricsParams
    ),
    responses(
        (status = 200, body = Metrics),
        (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody),
        (status = 502, body = ErrorBody),
        (status = 503, body = ErrorBody)
    )
)]
async fn metrics(
    axum::extract::State(state): axum::extract::State<AppState>,
    Id(id): Id,
    Params(params): Params<MetricsParams>,
) -> Result<Json<Metrics>, ApiError> {
    let every_n = params.every_n.unwrap_or(1);
    if !(1..=MAX_EVERY_N).contains(&every_n) {
        return Err(ApiError::InvalidInput);
    }
    let graph = state.peloton.performance_graph(&id, every_n).await?;
    Ok(Json(Metrics::from_upstream(&graph)))
}

/// A class with its structure and music.
#[utoipa::path(
    get,
    path = "/v1/classes/{id}",
    params(("id" = String, Path, description = "Class id: 32 lowercase hex characters")),
    responses(
        (status = 200, body = ClassDetails),
        (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody),
        (status = 502, body = ErrorBody),
        (status = 503, body = ErrorBody)
    )
)]
async fn class(
    axum::extract::State(state): axum::extract::State<AppState>,
    Id(id): Id,
    Params(NoParams {}): Params<NoParams>,
) -> Result<Json<ClassDetails>, ApiError> {
    let details = state.peloton.ride_details(&id).await?;
    Ok(Json(ClassDetails::from_upstream(&details)))
}

/// Liveness: the process is up. Never calls Peloton.
#[utoipa::path(
    get,
    path = "/healthz",
    responses((status = 200, body = Status))
)]
async fn healthz() -> Json<Status> {
    Json(Status { status: "ok" })
}

/// Readiness: a usable Peloton credential is held. Never exposes it.
#[utoipa::path(
    get,
    path = "/readyz",
    responses(
        (status = 200, body = Status),
        (status = 503, body = ErrorBody)
    )
)]
async fn readyz(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Json<Status>, ApiError> {
    // `bearer` rather than `is_ready`: the latter is false until first use.
    state
        .tokens
        .bearer()
        .await
        .map_err(|_| ApiError::NotAuthenticated)?;
    Ok(Json(Status { status: "ready" }))
}

/// The `OpenAPI` document.
async fn spec() -> Json<utoipa::openapi::OpenApi> {
    Json(openapi())
}
