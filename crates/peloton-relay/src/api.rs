//! The relay's HTTP API: reads of the account, its class library, schedule
//! and Stack, and validated, audit-logged writes to the schedule and Stack.

use std::any::Any;
use std::sync::Arc;

use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use peloton::ids::{PelotonId, ScheduledClassId};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, UtcOffset};
use tower_http::catch_panic::CatchPanicLayer;
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::{ObjectBuilder, Schema, Type};
use utoipa::{IntoParams, OpenApi, ToSchema};

use crate::error::{ApiError, ErrorBody};
use crate::model::{
    ClassDetails, LIBRARY_DURATIONS_MIN, LibraryCategory, LibraryFilters, LibraryPage,
    LibrarySortBy, Metrics, Profile, ScheduleList, ScheduledClassView, StackView, WorkoutDetail,
    WorkoutPage,
};
use crate::stack::StackService;
use crate::tokens::TokenManager;
use crate::upstream::{LibraryQuery, LibrarySort, Peloton, WorkoutQuery};

const DEFAULT_LIMIT: u32 = 20;
const MAX_LIMIT: u32 = 100;
const MAX_EVERY_N: u32 = 60;
/// How far ahead a class may be scheduled, by the relay's clock.
const MAX_SCHEDULE_AHEAD: Duration = Duration::days(14);
/// The schedule window listed when the request does not bound it.
const DEFAULT_SCHEDULE_WINDOW: Duration = Duration::days(14);
/// The longest schedule window listed at once.
const MAX_SCHEDULE_WINDOW: Duration = Duration::days(31);
/// The most classes a Stack replacement may name.
const MAX_STACK_CLASSES: usize = 100;

/// The relay's clock, injectable so that time checks are testable.
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> OffsetDateTime + Send + Sync>);

impl Clock {
    /// The system clock.
    #[must_use]
    pub fn system() -> Self {
        Self(Arc::new(OffsetDateTime::now_utc))
    }

    /// A clock stopped at `now`.
    #[must_use]
    pub fn fixed(now: OffsetDateTime) -> Self {
        Self(Arc::new(move || now))
    }

    /// The current time.
    #[must_use]
    pub fn now(&self) -> OffsetDateTime {
        (self.0)()
    }
}

/// Shared state of every handler.
#[derive(Clone)]
pub struct AppState {
    /// The Peloton client.
    pub peloton: Arc<Peloton>,
    /// The credential holder, for readiness.
    pub tokens: Arc<TokenManager>,
    /// The Stack's write path, over the same client.
    pub stack: Arc<StackService<Arc<Peloton>>>,
    /// The clock start times are checked against.
    pub clock: Clock,
}

/// Builds the router for the relay's API.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/me", get(me))
        .route("/v1/workouts", get(workouts))
        .route("/v1/workouts/{id}", get(workout))
        .route("/v1/workouts/{id}/metrics", get(metrics))
        .route("/v1/classes/{id}", get(class))
        .route("/v1/library", get(library))
        .route("/v1/library/filters", get(library_filters))
        .route("/v1/schedule", get(schedule).post(schedule_add))
        .route(
            "/v1/schedule/{scheduled_id}",
            get(scheduled).patch(schedule_move).delete(schedule_remove),
        )
        .route("/v1/stack", get(stack).post(stack_add).put(stack_replace))
        .route("/v1/stack/{class_id}", delete(stack_remove))
        .route("/openapi.json", get(spec))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .fallback(|| async { ApiError::NotFound })
        .method_not_allowed_fallback(|| async { ApiError::MethodNotAllowed })
        .layer(catch_panics())
        .with_state(state)
}

/// Answers a request whose handler panicked with 500 `internal_error`.
///
/// Neither the response nor the log carries the panic's payload, which could
/// hold request or upstream data; the log says only that a handler panicked.
/// The process's panic hook (see [`crate::logging::install_panic_hook`]) does
/// the same for the panic itself.
#[must_use]
pub fn catch_panics() -> CatchPanicLayer<fn(Box<dyn Any + Send + 'static>) -> Response> {
    fn answer(_payload: Box<dyn Any + Send + 'static>) -> Response {
        tracing::error!(event = "handler_panicked", "a request handler panicked");
        ApiError::Internal.into_response()
    }
    CatchPanicLayer::custom(answer as fn(Box<dyn Any + Send + 'static>) -> Response)
}

/// The `OpenAPI` 3.1 description of the API, generated from the handlers.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Peloton relay",
        description = "Relay of a Peloton account: its profile, workouts, metrics and classes, the on-demand class library, and the member's schedule and Stack, which it can also change.\n\nEvery error has an `ErrorBody`. Any path not listed here answers 404 `not_found`, any method a listed path does not list answers 405 `method_not_allowed`, and a request the relay fails to handle answers 500 `internal_error`. Writes (tag `peloton-write`) require the gateway capability `pecheriere.dev/cap/peloton-write`.\n\nExample: find a 20-min low-impact ride and schedule it Monday 08:00 America/Los_Angeles.\n\n1. `GET /v1/library/filters?category=cycling` and take the `id` of the class type named \"Low Impact\".\n2. `GET /v1/library?category=cycling&class_type=<that id>&duration=20` and pick a class `id`.\n3. `POST /v1/schedule` with `{\"class_id\": \"<class id>\", \"start_time\": \"2026-11-02T08:00:00-08:00\"}`: 08:00 on Monday 2 November 2026 in America/Los_Angeles, whose offset that day is -08:00 (always send the offset in effect on that day). The answer holds the class's `scheduled_id`.\n4. `GET /v1/schedule` lists it; `PATCH /v1/schedule/{scheduled_id}` moves it (and gives it a new `scheduled_id`), `DELETE` removes it."
    ),
    paths(
        me,
        workouts,
        workout,
        metrics,
        class,
        library,
        library_filters,
        schedule,
        scheduled,
        schedule_add,
        schedule_move,
        schedule_remove,
        stack,
        stack_add,
        stack_remove,
        stack_replace,
        healthz,
        readyz
    ),
    components(schemas(
        Profile,
        WorkoutPage,
        WorkoutDetail,
        Metrics,
        ClassDetails,
        LibraryPage,
        LibraryFilters,
        ScheduleList,
        ScheduledClassView,
        StackView,
        ScheduleRequest,
        RescheduleRequest,
        StackAddRequest,
        StackReplaceRequest,
        ErrorBody,
        Status
    )),
    tags((
        name = "peloton-write",
        description = "Writes to the member's schedule and Stack. They require the gateway capability `pecheriere.dev/cap/peloton-write` (the read capability alone gets 403 at the gateway), and each one is audit-logged."
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

/// A path `{scheduled_id}`: the relay's form of a scheduled-class id.
struct ScheduledId(ScheduledClassId);

impl<S: Send + Sync> FromRequestParts<S> for ScheduledId {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let Path(raw) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::InvalidInput)?;
        ScheduledClassId::from_url_safe(&raw)
            .map(Self)
            .map_err(|_| ApiError::InvalidInput)
    }
}

/// A JSON object request body deserialised into `T`; rejection is the JSON
/// error.
struct Body<T>(T);

impl<S: Send + Sync, T: serde::de::DeserializeOwned> FromRequest<S> for Body<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, ApiError> {
        let Json(value) = Json::<serde_json::Value>::from_request(request, state)
            .await
            .map_err(|_| ApiError::InvalidInput)?;
        // A derived struct would also accept an array of its fields.
        if !value.is_object() {
            return Err(ApiError::InvalidInput);
        }
        T::deserialize(value)
            .map(Self)
            .map_err(|_| ApiError::InvalidInput)
    }
}

/// An RFC 3339 instant, in UTC.
///
/// Its UTC date must fall in the years 0 to 9999, the range RFC 3339 can
/// express: a valid local time near either end can be outside it in UTC
/// (e.g. `9999-12-31T20:00:00-05:00`), and converting or formatting it later
/// would fail.
fn utc_instant(text: &str) -> Result<OffsetDateTime, ApiError> {
    OffsetDateTime::parse(text, &Rfc3339)
        .ok()
        .and_then(|at| at.checked_to_offset(UtcOffset::UTC))
        .filter(|at| (0..=9999).contains(&at.year()))
        .ok_or(ApiError::InvalidInput)
}

/// An optional RFC 3339 instant, in UTC; see [`utc_instant`].
fn instant(text: Option<&str>) -> Result<Option<OffsetDateTime>, ApiError> {
    text.map(utc_instant).transpose()
}

/// A validated class start time.
struct StartTime {
    /// The instant in UTC, in whole seconds.
    at: OffsetDateTime,
    /// The same instant in UTC, as RFC 3339.
    utc: String,
}

impl StartTime {
    /// Parses `text` as RFC 3339 with an explicit offset (see
    /// [`utc_instant`]) and truncates it to whole seconds; it must then be
    /// after `now` and at most [`MAX_SCHEDULE_AHEAD`] later.
    fn parse(text: &str, now: OffsetDateTime) -> Result<Self, ApiError> {
        let at = utc_instant(text)?.truncate_to_second();
        let latest = now
            .checked_add(MAX_SCHEDULE_AHEAD)
            .ok_or(ApiError::InvalidInput)?;
        if at <= now || at > latest {
            return Err(ApiError::InvalidInput);
        }
        let utc = at.format(&Rfc3339).map_err(|_| ApiError::InvalidInput)?;
        Ok(Self { at, utc })
    }
}

/// One write to the member's account, for the audit log.
#[derive(Default)]
struct Write {
    op: &'static str,
    class_id: Option<PelotonId>,
    class_ids: Option<String>,
    scheduled_id: Option<String>,
    start_time: Option<String>,
}

impl Write {
    fn new(op: &'static str) -> Self {
        Self {
            op,
            ..Self::default()
        }
    }

    /// Logs the `peloton_write` event: the write and its outcome, `ok` or the
    /// error code (never the error's message), with the scheduled-class id
    /// Peloton issued, if any.
    fn log(&self, result: Result<Option<&str>, &ApiError>) {
        let (outcome, new_scheduled_id) = match result {
            Ok(id) => ("ok", id),
            Err(error) => (error.code(), None),
        };
        tracing::info!(
            event = "peloton_write",
            op = self.op,
            class_id = self.class_id.as_ref().map(tracing::field::display),
            class_ids = self.class_ids.as_deref().map(tracing::field::display),
            scheduled_id = self.scheduled_id.as_deref().map(tracing::field::display),
            start_time = self.start_time.as_deref().map(tracing::field::display),
            new_scheduled_id = new_scheduled_id.map(tracing::field::display),
            outcome,
            "Peloton write"
        );
    }
}

/// The answer to a Stack write: the Stack read back, or no content when the
/// write succeeded but the read did not.
enum StackWritten {
    Read(StackView),
    Unread,
}

impl IntoResponse for StackWritten {
    fn into_response(self) -> Response {
        match self {
            Self::Read(stack) => Json(stack).into_response(),
            Self::Unread => StatusCode::NO_CONTENT.into_response(),
        }
    }
}

/// Reads the Stack back after a successful write.
async fn read_back(peloton: &Peloton) -> StackWritten {
    match peloton.stack().await {
        Ok(stack) => StackWritten::Read(StackView::from_upstream(&stack)),
        Err(error) => {
            tracing::warn!(%error, "the Stack was written but could not be read back");
            StackWritten::Unread
        }
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
        let from = instant(self.from.as_deref())?;
        let to = instant(self.to.as_deref())?;
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

/// An exact class length in minutes, one of [`LIBRARY_DURATIONS_MIN`].
#[derive(Debug, Clone, Copy)]
struct DurationMin(u32);

impl<'de> Deserialize<'de> for DurationMin {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let minutes = u32::deserialize(deserializer)?;
        if LIBRARY_DURATIONS_MIN.contains(&minutes) {
            Ok(Self(minutes))
        } else {
            Err(serde::de::Error::custom("not a library class length"))
        }
    }
}

impl utoipa::PartialSchema for DurationMin {
    fn schema() -> RefOr<Schema> {
        ObjectBuilder::new()
            .schema_type(Type::Integer)
            .enum_values(Some(LIBRARY_DURATIONS_MIN))
            .into()
    }
}

impl ToSchema for DurationMin {}

/// Query of `GET /v1/library`.
#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
struct LibraryParams {
    /// Browse category. Peloton's current list is served by `/v1/library/filters`.
    #[param(inline)]
    category: Option<LibraryCategory>,
    /// Class type id (32 lowercase hex), e.g. the id of "Low Impact"; the
    /// current ids of a category are served by `/v1/library/filters`.
    #[param(value_type = Option<String>, pattern = "^[0-9a-f]{32}$")]
    class_type: Option<PelotonId>,
    /// Instructor id (32 lowercase hex); the current ids of a category are
    /// served by `/v1/library/filters`.
    #[param(value_type = Option<String>, pattern = "^[0-9a-f]{32}$")]
    instructor: Option<PelotonId>,
    /// Exact class length in minutes (not a minimum).
    #[param(inline)]
    duration: Option<DurationMin>,
    /// Sort order; with only `desc`, `original_air_time`. Without `sort` and
    /// `desc`, Peloton's default: newest first.
    #[param(inline)]
    sort: Option<LibrarySortBy>,
    /// Whether to sort descending; defaults to `true` (with `false`,
    /// `difficulty` is "Easiest" first).
    desc: Option<bool>,
    /// Zero-based page index; defaults to 0.
    #[param(minimum = 0)]
    page: Option<u32>,
    /// Page size, 1 to 100; defaults to 20.
    #[param(minimum = 1, maximum = 100)]
    limit: Option<u32>,
}

impl LibraryParams {
    fn into_query(self) -> Result<LibraryQuery, ApiError> {
        let limit = self.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(ApiError::InvalidInput);
        }
        let duration_s = self.duration.map(|DurationMin(minutes)| minutes * 60);
        let sort = match (self.sort, self.desc) {
            (None, None) => None,
            (by, desc) => Some(LibrarySort {
                by: by
                    .unwrap_or(LibrarySortBy::OriginalAirTime)
                    .as_str()
                    .to_owned(),
                desc: desc.unwrap_or(true),
            }),
        };
        Ok(LibraryQuery {
            category: self.category.map(|c| c.as_str().to_owned()),
            class_type: self.class_type,
            instructor: self.instructor,
            duration_s,
            sort,
            page: self.page.unwrap_or(0),
            limit,
        })
    }
}

/// Query of `GET /v1/library/filters`.
#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
struct FiltersParams {
    /// Browse category whose filters to list.
    #[param(inline)]
    category: LibraryCategory,
}

/// Query of `GET /v1/schedule`.
#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
struct ScheduleParams {
    /// Start of the window, RFC 3339 with offset; defaults to now.
    #[param(format = DateTime)]
    from: Option<String>,
    /// End of the window, after `from` and at most 31 days later; defaults to
    /// 14 days after `from`.
    #[param(format = DateTime)]
    to: Option<String>,
}

impl ScheduleParams {
    fn window(self, now: OffsetDateTime) -> Result<(OffsetDateTime, OffsetDateTime), ApiError> {
        let from = instant(self.from.as_deref())?.unwrap_or(now);
        let to = match instant(self.to.as_deref())? {
            Some(to) => to,
            None => from
                .checked_add(DEFAULT_SCHEDULE_WINDOW)
                .ok_or(ApiError::InvalidInput)?,
        };
        // Both are in the years 0 to 9999, so the difference cannot overflow.
        if from >= to || to - from > MAX_SCHEDULE_WINDOW {
            return Err(ApiError::InvalidInput);
        }
        Ok((from, to))
    }
}

/// Body of `POST /v1/schedule`.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(example = json!({
    "class_id": "0123456789abcdef0123456789abcdef",
    "start_time": "2026-11-02T08:00:00-08:00"
}))]
struct ScheduleRequest {
    /// Class id (32 lowercase hex) of an on-demand class, from `/v1/library`.
    #[schema(value_type = String, pattern = "^[0-9a-f]{32}$")]
    class_id: PelotonId,
    /// When to take it: RFC 3339 with an explicit offset, after now and at
    /// most 14 days ahead by the relay's clock; fractions of a second are
    /// dropped.
    #[schema(format = DateTime)]
    start_time: String,
}

/// Body of `PATCH /v1/schedule/{scheduled_id}`.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(example = json!({ "start_time": "2026-11-03T07:30:00-08:00" }))]
struct RescheduleRequest {
    /// The new start, as [`ScheduleRequest::start_time`].
    #[schema(format = DateTime)]
    start_time: String,
}

/// Body of `POST /v1/stack`.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct StackAddRequest {
    /// Class id (32 lowercase hex) of an on-demand class, from `/v1/library`.
    #[schema(value_type = String, pattern = "^[0-9a-f]{32}$")]
    class_id: PelotonId,
}

/// Body of `PUT /v1/stack`.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct StackReplaceRequest {
    /// The whole Stack in play order: distinct class ids (32 lowercase hex),
    /// at most 100 (Peloton's own limit still applies); empty clears it.
    #[schema(value_type = Vec<String>, max_items = 100)]
    class_ids: Vec<PelotonId>,
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
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn me(
    State(state): State<AppState>,
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
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn workouts(
    State(state): State<AppState>,
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
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 404, description = "`not_found`: no such workout of the credential's owner.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn workout(
    State(state): State<AppState>,
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
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 404, description = "`not_found`: no such workout of the credential's owner.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn metrics(
    State(state): State<AppState>,
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
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 404, description = "`not_found`: no such class.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn class(
    State(state): State<AppState>,
    Id(id): Id,
    Params(NoParams {}): Params<NoParams>,
) -> Result<Json<ClassDetails>, ApiError> {
    let details = state.peloton.ride_details(&id).await?;
    Ok(Json(ClassDetails::from_upstream(&details)))
}

/// One page of the on-demand class library, filtered and sorted by Peloton.
#[utoipa::path(
    get,
    path = "/v1/library",
    params(LibraryParams),
    responses(
        (status = 200, body = LibraryPage),
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn library(
    State(state): State<AppState>,
    Params(params): Params<LibraryParams>,
) -> Result<Json<LibraryPage>, ApiError> {
    let page = state.peloton.library(&params.into_query()?).await?;
    Ok(Json(LibraryPage::from_upstream(&page)))
}

/// The values `/v1/library` currently accepts for a category, from Peloton:
/// categories, class types, instructors, durations and sorts.
#[utoipa::path(
    get,
    path = "/v1/library/filters",
    params(FiltersParams),
    responses(
        (status = 200, body = LibraryFilters),
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn library_filters(
    State(state): State<AppState>,
    Params(params): Params<FiltersParams>,
) -> Result<Json<LibraryFilters>, ApiError> {
    let category = params.category.as_str();
    let (filters, categories) = tokio::try_join!(
        state.peloton.ride_filters(category),
        state.peloton.browse_categories()
    )?;
    Ok(Json(LibraryFilters::from_upstream(
        category,
        &filters,
        &categories,
    )))
}

/// Everything on the member's schedule in a window, of every kind.
#[utoipa::path(
    get,
    path = "/v1/schedule",
    params(ScheduleParams),
    responses(
        (status = 200, body = ScheduleList),
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn schedule(
    State(state): State<AppState>,
    Params(params): Params<ScheduleParams>,
) -> Result<Json<ScheduleList>, ApiError> {
    let (from, to) = params.window(state.clock.now())?;
    let items = state.peloton.schedule(from, to).await?;
    Ok(Json(ScheduleList::from_upstream(&items)))
}

/// One class on the member's schedule.
#[utoipa::path(
    get,
    path = "/v1/schedule/{scheduled_id}",
    params(("scheduled_id" = String, Path, pattern = "^[A-Za-z0-9_-]+$", max_length = 512, description = "Scheduled-class id as the relay issues it (`scheduled_id`): URL-safe base64 without padding")),
    responses(
        (status = 200, body = ScheduledClassView),
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 404, description = "`not_found`: no such scheduled class.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn scheduled(
    State(state): State<AppState>,
    ScheduledId(id): ScheduledId,
    Params(NoParams {}): Params<NoParams>,
) -> Result<Json<ScheduledClassView>, ApiError> {
    let class = state.peloton.scheduled_class(&id).await?;
    Ok(Json(ScheduledClassView::from_upstream(&class)))
}

/// Schedules an on-demand class at a day and time.
///
/// Requires the gateway capability `pecheriere.dev/cap/peloton-write`. Audit-logged.
#[utoipa::path(
    post,
    path = "/v1/schedule",
    tag = "peloton-write",
    request_body = ScheduleRequest,
    responses(
        (status = 201, description = "Scheduled; `Location` is the class's `/v1/schedule/{scheduled_id}`.", body = ScheduledClassView,
            headers(("Location" = String, description = "The scheduled class's path."))
        ),
        (status = 400, description = "`invalid_input`: a path, query or body value is malformed or out of range, the query has a parameter, or the body has an unknown field.", body = ErrorBody),
        (status = 422, description = "`cannot_schedule`: Peloton refused, e.g. a conflict or a time outside its window; the message is Peloton's explanation.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected; whether the write happened is unknown, so read before retrying.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn schedule_add(
    State(state): State<AppState>,
    Params(NoParams {}): Params<NoParams>,
    Body(request): Body<ScheduleRequest>,
) -> Result<Response, ApiError> {
    let start = StartTime::parse(&request.start_time, state.clock.now())?;
    let write = Write {
        class_id: Some(request.class_id.clone()),
        start_time: Some(start.utc),
        ..Write::new("schedule_add")
    };
    let result = state
        .peloton
        .add_to_schedule(&request.class_id, start.at)
        .await
        .map_err(ApiError::from);
    write.log(result.as_ref().map(|class| Some(class.id.url_safe())));
    let class = result?;
    let location = format!("/v1/schedule/{}", class.id.url_safe());
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(ScheduledClassView::from_upstream(&class)),
    )
        .into_response())
}

/// Moves a scheduled class to another time. Peloton gives it a new
/// `scheduled_id`; the old one stops working.
///
/// Requires the gateway capability `pecheriere.dev/cap/peloton-write`. Audit-logged.
#[utoipa::path(
    patch,
    path = "/v1/schedule/{scheduled_id}",
    tag = "peloton-write",
    params(("scheduled_id" = String, Path, pattern = "^[A-Za-z0-9_-]+$", max_length = 512, description = "Scheduled-class id as the relay issues it (`scheduled_id`): URL-safe base64 without padding")),
    request_body = RescheduleRequest,
    responses(
        (status = 200, description = "Moved; the class with its new `scheduled_id`.", body = ScheduledClassView),
        (status = 400, description = "`invalid_input`: a path, query or body value is malformed or out of range, the query has a parameter, or the body has an unknown field.", body = ErrorBody),
        (status = 404, description = "`not_found`: no such scheduled class.", body = ErrorBody),
        (status = 422, description = "`cannot_schedule`: Peloton refused, e.g. a conflict or a time outside its window; the message is Peloton's explanation.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected; whether the write happened is unknown, so read before retrying.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn schedule_move(
    State(state): State<AppState>,
    ScheduledId(id): ScheduledId,
    Params(NoParams {}): Params<NoParams>,
    Body(request): Body<RescheduleRequest>,
) -> Result<Json<ScheduledClassView>, ApiError> {
    let start = StartTime::parse(&request.start_time, state.clock.now())?;
    let write = Write {
        scheduled_id: Some(id.url_safe().to_owned()),
        start_time: Some(start.utc),
        ..Write::new("schedule_move")
    };
    let result = state
        .peloton
        .reschedule(&id, start.at)
        .await
        .map_err(ApiError::from);
    write.log(result.as_ref().map(|class| Some(class.id.url_safe())));
    Ok(Json(ScheduledClassView::from_upstream(&result?)))
}

/// Removes a class from the member's schedule.
///
/// Requires the gateway capability `pecheriere.dev/cap/peloton-write`. Audit-logged.
#[utoipa::path(
    delete,
    path = "/v1/schedule/{scheduled_id}",
    tag = "peloton-write",
    params(("scheduled_id" = String, Path, pattern = "^[A-Za-z0-9_-]+$", max_length = 512, description = "Scheduled-class id as the relay issues it (`scheduled_id`): URL-safe base64 without padding")),
    responses(
        (status = 204, description = "Removed."),
        (status = 400, description = "`invalid_input`: a path, query or body value is malformed or out of range, the query has a parameter, or the body has an unknown field.", body = ErrorBody),
        (status = 404, description = "`not_found`: no such scheduled class.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected; whether the write happened is unknown, so read before retrying.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn schedule_remove(
    State(state): State<AppState>,
    ScheduledId(id): ScheduledId,
    Params(NoParams {}): Params<NoParams>,
) -> Result<StatusCode, ApiError> {
    let write = Write {
        scheduled_id: Some(id.url_safe().to_owned()),
        ..Write::new("schedule_remove")
    };
    let result = state
        .peloton
        .remove_from_schedule(&id)
        .await
        .map_err(ApiError::from);
    write.log(result.as_ref().map(|()| None));
    result?;
    Ok(StatusCode::NO_CONTENT)
}

/// The Stack: the classes queued to play on the bike, in play order.
#[utoipa::path(
    get,
    path = "/v1/stack",
    responses(
        (status = 200, body = StackView),
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn stack(
    State(state): State<AppState>,
    Params(NoParams {}): Params<NoParams>,
) -> Result<Json<StackView>, ApiError> {
    let stack = state.peloton.stack().await?;
    Ok(Json(StackView::from_upstream(&stack)))
}

/// Appends a class to the Stack.
///
/// Requires the gateway capability `pecheriere.dev/cap/peloton-write`. Audit-logged.
#[utoipa::path(
    post,
    path = "/v1/stack",
    tag = "peloton-write",
    request_body = StackAddRequest,
    responses(
        (status = 200, description = "Appended; the Stack as it now is.", body = StackView),
        (status = 204, description = "The write succeeded but reading the Stack back failed; `GET /v1/stack` to see it."),
        (status = 400, description = "`invalid_input`: a path, query or body value is malformed or out of range, the query has a parameter, or the body has an unknown field.", body = ErrorBody),
        (status = 409, description = "`already_in_stack`: the class is already in the Stack; nothing was written.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected; whether the write happened is unknown, so read before retrying.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn stack_add(
    State(state): State<AppState>,
    Params(NoParams {}): Params<NoParams>,
    Body(request): Body<StackAddRequest>,
) -> Result<StackWritten, ApiError> {
    let write = Write {
        class_id: Some(request.class_id.clone()),
        ..Write::new("stack_add")
    };
    let result = state
        .stack
        .add(&request.class_id)
        .await
        .map_err(ApiError::from);
    write.log(result.as_ref().map(|()| None));
    result?;
    Ok(read_back(&state.peloton).await)
}

/// Removes a class from the Stack, keeping the others in order.
///
/// Requires the gateway capability `pecheriere.dev/cap/peloton-write`. Audit-logged.
#[utoipa::path(
    delete,
    path = "/v1/stack/{class_id}",
    tag = "peloton-write",
    params(("class_id" = String, Path, description = "Class id: 32 lowercase hex characters")),
    responses(
        (status = 200, description = "Removed; the Stack as it now is.", body = StackView),
        (status = 204, description = "The write succeeded but reading the Stack back failed; `GET /v1/stack` to see it."),
        (status = 400, description = "`invalid_input`: a path, query or body value is malformed or out of range, the query has a parameter, or the body has an unknown field.", body = ErrorBody),
        (status = 404, description = "`not_found`: the class is not in the Stack; nothing was written.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected, or the Stack holds an item the relay cannot write back faithfully (nothing is written then).", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn stack_remove(
    State(state): State<AppState>,
    Id(id): Id,
    Params(NoParams {}): Params<NoParams>,
) -> Result<StackWritten, ApiError> {
    let write = Write {
        class_id: Some(id.clone()),
        ..Write::new("stack_remove")
    };
    let result = state.stack.remove(&id).await.map_err(ApiError::from);
    write.log(result.as_ref().map(|()| None));
    result?;
    Ok(read_back(&state.peloton).await)
}

/// Replaces the whole Stack: its classes and their order.
///
/// Requires the gateway capability `pecheriere.dev/cap/peloton-write`. Audit-logged.
#[utoipa::path(
    put,
    path = "/v1/stack",
    tag = "peloton-write",
    request_body = StackReplaceRequest,
    responses(
        (status = 200, description = "Replaced; the Stack as it now is.", body = StackView),
        (status = 204, description = "The write succeeded but reading the Stack back failed; `GET /v1/stack` to see it."),
        (status = 400, description = "`invalid_input`: a value is malformed, a class is named twice, there are more than 100, the query has a parameter, or the body has an unknown field.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected, or the Stack holds an item the relay cannot write back faithfully (nothing is written then).", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn stack_replace(
    State(state): State<AppState>,
    Params(NoParams {}): Params<NoParams>,
    Body(request): Body<StackReplaceRequest>,
) -> Result<StackWritten, ApiError> {
    if request.class_ids.len() > MAX_STACK_CLASSES {
        return Err(ApiError::InvalidInput);
    }
    let ids: Vec<String> = request.class_ids.iter().map(ToString::to_string).collect();
    let write = Write {
        class_ids: Some(ids.join(",")),
        ..Write::new("stack_replace")
    };
    let result = state
        .stack
        .replace(&request.class_ids)
        .await
        .map_err(ApiError::from);
    write.log(result.as_ref().map(|()| None));
    result?;
    Ok(read_back(&state.peloton).await)
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
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential.", body = ErrorBody)
    )
)]
async fn readyz(State(state): State<AppState>) -> Result<Json<Status>, ApiError> {
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
