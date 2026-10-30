//! The relay's HTTP API: reads of an account, its class library, schedule
//! and Stack, and validated, audit-logged writes to the schedule and Stack.
//!
//! Each request is served from the one account its `X-Peloton-Account`
//! header names (see [`ACCOUNT_HEADER`]); any other is refused. The admin
//! routes under `/admin` (see [`crate::admin`]) take no account and are
//! refused unless the request is marked as admin instead.

use std::any::Any;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashSet};
use std::future;
use std::sync::Arc;

use axum::extract::{FromRequest, FromRequestParts, MatchedPath, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use peloton::ids::{PelotonId, ScheduledClassId};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, UtcOffset};
use tower_http::catch_panic::CatchPanicLayer;
use tracing::Instrument as _;
use utoipa::openapi::schema::{ObjectBuilder, Schema, Type};
use utoipa::openapi::{ContentBuilder, Ref, RefOr, ResponseBuilder};
use utoipa::{IntoParams, Modify, OpenApi, ToSchema};

use crate::account::AccountName;
use crate::admin::{self, AdminState};
use crate::error::{ApiError, ErrorBody};
use crate::model::{
    ClassDetails, LIBRARY_DURATIONS_MIN, LibraryCategory, LibraryFilters, LibraryPage,
    LibrarySortBy, Metrics, Profile, ScheduleList, ScheduledClassView, StackView, WorkoutDetail,
    WorkoutPage,
};
use crate::stack::StackService;
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

/// Header naming the account a request is served from. The proxy in front of
/// the relay sets it from the caller's identity.
pub const ACCOUNT_HEADER: HeaderName = HeaderName::from_static("x-peloton-account");

/// The only paths served without an account.
const ACCOUNT_FREE_PATHS: [&str; 3] = ["/healthz", "/readyz", "/openapi.json"];

/// Everything one account's requests are served with.
///
/// Each account has its own client, so its own limit on concurrent upstream
/// requests, and its own Stack write path, so its Stack writes run one at a
/// time without waiting for another account's.
#[derive(Debug)]
pub struct AccountContext {
    /// The account.
    pub name: AccountName,
    /// The Peloton client, with the account's credential.
    pub peloton: Arc<Peloton>,
    /// The account's Stack write path, over the same client.
    pub stack: Arc<StackService<Arc<Peloton>>>,
}

impl AccountContext {
    /// The context of the account whose credential `peloton` uses.
    #[must_use]
    pub fn new(peloton: Arc<Peloton>) -> Self {
        Self {
            name: peloton.account().clone(),
            stack: Arc::new(StackService::new(Arc::clone(&peloton))),
            peloton,
        }
    }
}

/// Two contexts serve the same account.
#[derive(Debug, thiserror::Error)]
#[error("account configured twice")]
pub struct DuplicateAccount;

/// The accounts the relay serves, by name.
#[derive(Debug)]
pub struct AccountRegistry(BTreeMap<AccountName, Arc<AccountContext>>);

impl AccountRegistry {
    /// The registry of `contexts`.
    ///
    /// # Errors
    ///
    /// [`DuplicateAccount`] when two contexts serve the same account.
    pub fn new(contexts: Vec<AccountContext>) -> Result<Self, DuplicateAccount> {
        let mut accounts = BTreeMap::new();
        for context in contexts {
            match accounts.entry(context.name.clone()) {
                Entry::Occupied(_) => return Err(DuplicateAccount),
                Entry::Vacant(slot) => {
                    slot.insert(Arc::new(context));
                }
            }
        }
        Ok(Self(accounts))
    }

    /// The context of account `name`, if the relay serves it.
    #[must_use]
    pub fn get(&self, name: &AccountName) -> Option<Arc<AccountContext>> {
        self.0.get(name).cloned()
    }

    /// Every account's context, by name.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<AccountContext>> {
        self.0.values()
    }
}

/// Shared state of every handler.
#[derive(Clone)]
pub struct AppState {
    /// The accounts requests are served from.
    pub accounts: Arc<AccountRegistry>,
    /// The clock start times are checked against, and the admin page's
    /// times are shown against.
    pub clock: Clock,
    /// The state of the admin routes.
    pub admin: Arc<AdminState>,
}

/// Builds the router for the relay's API.
pub fn router(state: AppState) -> Router {
    let routes = Router::new()
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
        .merge(admin::routes())
        .fallback(|| async { ApiError::NotFound })
        .method_not_allowed_fallback(|| async { ApiError::MethodNotAllowed });
    request_handling(routes, Arc::clone(&state.accounts)).with_state(state)
}

/// Wraps `routes` in the relay's request handling.
///
/// Each request runs in a span with its id, method and matched route (never
/// the query string or the path's own ids) and is answered with an
/// `x-request-id` header. A panicking handler is answered with 500 (see
/// [`catch_panics`]) inside that span, so the panic's log line carries the
/// request's id and route too. Inside both, every request but those of the
/// account-free paths and the admin paths must name an account of `accounts`
/// (see [`require_account`]) before it reaches its route or a fallback; an
/// admin path must be marked as admin instead (see [`admin::gate`]).
///
/// Public only so that the integration tests can wrap their own routes.
#[doc(hidden)]
pub fn request_handling<S: Clone + Send + Sync + 'static>(
    routes: Router<S>,
    accounts: Arc<AccountRegistry>,
) -> Router<S> {
    routes
        .layer(middleware::from_fn_with_state(accounts, require_account))
        .layer(catch_panics())
        .layer(middleware::from_fn(request_context))
}

/// Header carrying the id the relay gives each request.
const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// The `route` logged for a request that matched no route, so that a log line
/// never holds the path of an arbitrary request.
const UNMATCHED_ROUTE: &str = "unmatched";

/// Gives the request a fresh id, returned as `x-request-id`, and runs it in a
/// span with that id, its method and its matched route (the path template,
/// e.g. `/v1/workouts/{id}`), so that every log line of the handler, such as
/// an upstream failure, carries them; [`require_account`] adds the account
/// the request is served from.
///
/// An id sent by the client is ignored. The span holds neither the query
/// string nor the path's own ids.
async fn request_context(request: Request, next: Next) -> Response {
    let id = HeaderValue::from_str(&format!("{:032x}", rand::random::<u128>())).ok();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or(UNMATCHED_ROUTE, MatchedPath::as_str);
    // An error-level span, so that it stays enabled (and its fields reach the
    // log lines) under any `RUST_LOG` that enables the warnings logged in it.
    let span = tracing::error_span!(
        "request",
        method = %request.method(),
        route,
        request_id = id.as_ref().and_then(|id| id.to_str().ok()),
        account = tracing::field::Empty,
    );
    let mut response = next.run(request).instrument(span).await;
    if let Some(id) = id {
        response.headers_mut().insert(REQUEST_ID_HEADER, id);
    }
    response
}

/// Why a request names no account the relay serves; logged as `reason`.
enum Refusal {
    /// No account header.
    Missing,
    /// More than one account header.
    Ambiguous,
    /// The header is not a valid account name.
    Invalid,
    /// A valid name the relay does not serve.
    Unknown(AccountName),
}

impl Refusal {
    /// Logs the `account_refused` event; the account only when it is a
    /// valid name, so a log line never holds arbitrary header bytes.
    fn log(&self) {
        let reason = match self {
            Self::Missing => "missing",
            Self::Ambiguous => "ambiguous",
            Self::Invalid => "invalid",
            Self::Unknown(name) => {
                tracing::warn!(
                    event = "account_refused",
                    reason = "unknown",
                    account = %name,
                    "request refused: the relay does not serve its account"
                );
                return;
            }
        };
        tracing::warn!(
            event = "account_refused",
            reason,
            "request refused: it names no valid account"
        );
    }
}

/// The context of the account `request` names: its one [`ACCOUNT_HEADER`]
/// value, a valid name of an account of `accounts`.
fn named_account(
    request: &Request,
    accounts: &AccountRegistry,
) -> Result<Arc<AccountContext>, Refusal> {
    let mut values = request.headers().get_all(ACCOUNT_HEADER).iter();
    let value = values.next().ok_or(Refusal::Missing)?;
    if values.next().is_some() {
        return Err(Refusal::Ambiguous);
    }
    let name: AccountName = value
        .to_str()
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or(Refusal::Invalid)?;
    accounts.get(&name).ok_or(Refusal::Unknown(name))
}

/// Lets through a request of an account-free path, or one naming an account
/// of `accounts`, whose context it adds to the request's extensions (see
/// [`Account`]) and whose name it records on the request span. Anything else
/// is answered with 403 `forbidden` before routing, validation or any
/// upstream call, and logged with the reason (see [`Refusal::log`]). An admin
/// path goes through [`admin::gate`] instead: the admin header, never the
/// account header, decides.
async fn require_account(
    State(accounts): State<Arc<AccountRegistry>>,
    mut request: Request,
    next: Next,
) -> Response {
    if admin::is_admin_path(request.uri().path()) {
        return admin::gate(request, next).await;
    }
    if ACCOUNT_FREE_PATHS.contains(&request.uri().path()) {
        return next.run(request).await;
    }
    match named_account(&request, &accounts) {
        Ok(context) => {
            tracing::Span::current().record("account", context.name.as_str());
            request.extensions_mut().insert(context);
            next.run(request).await
        }
        Err(refusal) => {
            refusal.log();
            ApiError::Forbidden.into_response()
        }
    }
}

/// The account a request is served from, added by [`require_account`].
struct Account(Arc<AccountContext>);

impl<S: Send + Sync> FromRequestParts<S> for Account {
    type Rejection = ApiError;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, ApiError>> + Send {
        future::ready(
            parts
                .extensions
                .get::<Arc<AccountContext>>()
                .cloned()
                .map(Self)
                .ok_or(ApiError::Forbidden),
        )
    }
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
        description = "Relay of a Peloton account: its profile, workouts, metrics and classes, the on-demand class library, and the member's schedule and Stack, which it can also change.\n\nEvery error has an `ErrorBody`. Any path not listed here answers 404 `not_found` (to a request naming a configured account; others answer 403 `forbidden`), any method a listed path does not list answers 405 `method_not_allowed`, and a request the relay fails to handle answers 500 `internal_error`. Writes (tag `peloton-write`) change the member's account; a proxy in front of the relay can authorise them separately from reads.\n\nExample: find a 20-min low-impact ride and schedule it Monday 08:00 Europe/Berlin.\n\n1. `GET /v1/library/filters?category=cycling` and take the `id` of the class type named \"Low Impact\".\n2. `GET /v1/library?category=cycling&class_type=<that id>&duration=20` and pick a class `id`.\n3. `POST /v1/schedule` with `{\"class_id\": \"<class id>\", \"start_time\": \"2026-11-02T08:00:00+01:00\"}`: 08:00 on Monday 2 November 2026 in Europe/Berlin, whose offset that day is +01:00 (always send the offset in effect on that day). The answer holds the class's `scheduled_id`.\n4. `GET /v1/schedule` lists it; `PATCH /v1/schedule/{scheduled_id}` moves it (and gives it a new `scheduled_id`), `DELETE` removes it.\n\nEvery route except `/healthz`, `/readyz` and `/openapi.json` serves one Peloton account, named by the `X-Peloton-Account` request header. The proxy in front of the relay sets that header from the caller's identity; clients cannot choose it. A request without a configured account answers 403 `forbidden`."
    ),
    modifiers(&AccountRequirement),
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
        description = "Writes to the member's schedule and Stack. Every write is a POST, PATCH, PUT or DELETE under `/v1/schedule` or `/v1/stack` (every read is a GET), so a proxy in front of the relay can authorise writes separately from reads. Each one is audit-logged."
    ))
)]
struct ApiDoc;

/// Documents the 403 `forbidden` answer of every operation under `/v1/`.
struct AccountRequirement;

impl Modify for AccountRequirement {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let forbidden = ResponseBuilder::new()
            .description("`forbidden`: the request does not name an account this relay serves.")
            .content(
                "application/json",
                ContentBuilder::new()
                    .schema(Some(Ref::from_schema_name("ErrorBody")))
                    .build(),
            )
            .build();
        for (route, item) in &mut openapi.paths.paths {
            if !route.starts_with("/v1/") {
                continue;
            }
            let operations = [
                &mut item.get,
                &mut item.put,
                &mut item.post,
                &mut item.delete,
                &mut item.options,
                &mut item.head,
                &mut item.patch,
                &mut item.trace,
                &mut item.query,
            ]
            .into_iter()
            .filter_map(Option::as_mut)
            .chain(item.additional_operations.values_mut());
            for operation in operations {
                operation
                    .responses
                    .responses
                    .insert("403".to_owned(), RefOr::T(forbidden.clone()));
            }
        }
    }
}

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

impl<S: Send + Sync, T: serde::de::DeserializeOwned + Send> FromRequestParts<S> for Params<T> {
    type Rejection = ApiError;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, ApiError>> + Send {
        future::ready(
            Query::<T>::try_from_uri(&parts.uri)
                .map(|Query(value)| Self(value))
                .map_err(|_| ApiError::InvalidInput),
        )
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
struct Write {
    op: &'static str,
    account: AccountName,
    class_id: Option<PelotonId>,
    class_ids: Option<String>,
    scheduled_id: Option<String>,
    start_time: Option<String>,
}

impl Write {
    fn new(op: &'static str, account: &AccountName) -> Self {
        Self {
            op,
            account: account.clone(),
            class_id: None,
            class_ids: None,
            scheduled_id: None,
            start_time: None,
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
            account = %self.account,
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
    "start_time": "2026-11-02T08:00:00+01:00"
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
#[schema(example = json!({ "start_time": "2026-11-03T07:30:00+01:00" }))]
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
        (status = 200, description = "The credential owner's profile.", body = Profile),
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential for this account; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn me(
    Account(account): Account,
    Params(NoParams {}): Params<NoParams>,
) -> Result<Json<Profile>, ApiError> {
    let me = account.peloton.me().await?;
    Ok(Json(Profile::from_upstream(&me)))
}

/// Workouts, newest first, with paging information.
#[utoipa::path(
    get,
    path = "/v1/workouts",
    params(WorkoutsParams),
    responses(
        (status = 200, description = "One page of the member's workouts, newest first.", body = WorkoutPage),
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential for this account; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn workouts(
    Account(account): Account,
    Params(params): Params<WorkoutsParams>,
) -> Result<Json<WorkoutPage>, ApiError> {
    let page = account.peloton.workouts(&params.into_query()?).await?;
    Ok(Json(WorkoutPage::from_upstream(&page)))
}

/// One workout with its class and instructor.
#[utoipa::path(
    get,
    path = "/v1/workouts/{id}",
    params(("id" = String, Path, description = "Workout id: 32 lowercase hex characters")),
    responses(
        (status = 200, description = "The workout with its ride and instructor.", body = WorkoutDetail),
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 404, description = "`not_found`: no such workout of the credential's owner.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential for this account; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn workout(
    Account(account): Account,
    Id(id): Id,
    Params(NoParams {}): Params<NoParams>,
) -> Result<Json<WorkoutDetail>, ApiError> {
    let workout = account.peloton.workout(&id).await?;
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
        (status = 200, description = "The workout's performance metrics.", body = Metrics),
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 404, description = "`not_found`: no such workout of the credential's owner.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential for this account; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn metrics(
    Account(account): Account,
    Id(id): Id,
    Params(params): Params<MetricsParams>,
) -> Result<Json<Metrics>, ApiError> {
    let every_n = params.every_n.unwrap_or(1);
    if !(1..=MAX_EVERY_N).contains(&every_n) {
        return Err(ApiError::InvalidInput);
    }
    let graph = account.peloton.performance_graph(&id, every_n).await?;
    Ok(Json(Metrics::from_upstream(&graph)))
}

/// A class with its structure and music.
#[utoipa::path(
    get,
    path = "/v1/classes/{id}",
    params(("id" = String, Path, description = "Class id: 32 lowercase hex characters")),
    responses(
        (status = 200, description = "The on-demand class's details.", body = ClassDetails),
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 404, description = "`not_found`: no such class.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential for this account; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn class(
    Account(account): Account,
    Id(id): Id,
    Params(NoParams {}): Params<NoParams>,
) -> Result<Json<ClassDetails>, ApiError> {
    let details = account.peloton.ride_details(&id).await?;
    Ok(Json(ClassDetails::from_upstream(&details)))
}

/// One page of the on-demand class library, filtered and sorted by Peloton.
#[utoipa::path(
    get,
    path = "/v1/library",
    params(LibraryParams),
    responses(
        (status = 200, description = "One page of on-demand classes matching the query.", body = LibraryPage),
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential for this account; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn library(
    Account(account): Account,
    Params(params): Params<LibraryParams>,
) -> Result<Json<LibraryPage>, ApiError> {
    let page = account.peloton.library(&params.into_query()?).await?;
    Ok(Json(LibraryPage::from_upstream(&page)))
}

/// The values `/v1/library` currently accepts for a category, from Peloton:
/// categories, class types, instructors, durations and sorts.
#[utoipa::path(
    get,
    path = "/v1/library/filters",
    params(FiltersParams),
    responses(
        (status = 200, description = "The filters the library search accepts.", body = LibraryFilters),
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential for this account; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn library_filters(
    Account(account): Account,
    Params(params): Params<FiltersParams>,
) -> Result<Json<LibraryFilters>, ApiError> {
    let category = params.category.as_str();
    let (filters, categories) = tokio::try_join!(
        account.peloton.ride_filters(category),
        account.peloton.browse_categories()
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
        (status = 200, description = "The member's scheduled classes.", body = ScheduleList),
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential for this account; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn schedule(
    Account(account): Account,
    State(state): State<AppState>,
    Params(params): Params<ScheduleParams>,
) -> Result<Json<ScheduleList>, ApiError> {
    let (from, to) = params.window(state.clock.now())?;
    let items = account.peloton.schedule(from, to).await?;
    Ok(Json(ScheduleList::from_upstream(&items)))
}

/// One class on the member's schedule.
///
/// Only a class the schedule still lists is found: after a remove, and for
/// the old id after a move, the id answers 404.
#[utoipa::path(
    get,
    path = "/v1/schedule/{scheduled_id}",
    params(("scheduled_id" = String, Path, pattern = "^[A-Za-z0-9_-]+$", max_length = 512, description = "Scheduled-class id as the relay issues it (`scheduled_id`): URL-safe base64 without padding")),
    responses(
        (status = 200, description = "The scheduled class.", body = ScheduledClassView),
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 404, description = "`not_found`: no such scheduled class, or it is no longer on the schedule (removed, or moved to a new id).", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential for this account; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn scheduled(
    Account(account): Account,
    ScheduledId(id): ScheduledId,
    Params(NoParams {}): Params<NoParams>,
) -> Result<Json<ScheduledClassView>, ApiError> {
    let class = account.peloton.scheduled_class(&id).await?;
    Ok(Json(ScheduledClassView::from_upstream(&class)))
}

/// Schedules an on-demand class at a day and time.
///
/// Write route: deployments should only allow it for clients granted write
/// access (see the README, Security model). Audit-logged.
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
        (status = 404, description = "`not_found`: Peloton has no such on-demand class.", body = ErrorBody),
        (status = 422, description = "`cannot_schedule`: Peloton refused, e.g. a conflict or a time outside its window; the message is Peloton's explanation.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected; whether the write happened is unknown, so read before retrying.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential for this account; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn schedule_add(
    Account(account): Account,
    State(state): State<AppState>,
    Params(NoParams {}): Params<NoParams>,
    Body(request): Body<ScheduleRequest>,
) -> Result<Response, ApiError> {
    let start = StartTime::parse(&request.start_time, state.clock.now())?;
    let write = Write {
        class_id: Some(request.class_id.clone()),
        start_time: Some(start.utc),
        ..Write::new("schedule_add", &account.name)
    };
    let result = account
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
/// Write route: deployments should only allow it for clients granted write
/// access (see the README, Security model). Audit-logged.
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
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential for this account; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn schedule_move(
    Account(account): Account,
    State(state): State<AppState>,
    ScheduledId(id): ScheduledId,
    Params(NoParams {}): Params<NoParams>,
    Body(request): Body<RescheduleRequest>,
) -> Result<Json<ScheduledClassView>, ApiError> {
    let start = StartTime::parse(&request.start_time, state.clock.now())?;
    let write = Write {
        scheduled_id: Some(id.url_safe().to_owned()),
        start_time: Some(start.utc),
        ..Write::new("schedule_move", &account.name)
    };
    let result = account
        .peloton
        .reschedule(&id, start.at)
        .await
        .map_err(ApiError::from);
    write.log(result.as_ref().map(|class| Some(class.id.url_safe())));
    Ok(Json(ScheduledClassView::from_upstream(&result?)))
}

/// Removes a class from the member's schedule.
///
/// Write route: deployments should only allow it for clients granted write
/// access (see the README, Security model). Audit-logged.
#[utoipa::path(
    delete,
    path = "/v1/schedule/{scheduled_id}",
    tag = "peloton-write",
    params(("scheduled_id" = String, Path, pattern = "^[A-Za-z0-9_-]+$", max_length = 512, description = "Scheduled-class id as the relay issues it (`scheduled_id`): URL-safe base64 without padding")),
    responses(
        (status = 204, description = "Removed."),
        (status = 400, description = "`invalid_input`: a path, query or body value is malformed or out of range, the query has a parameter, or the body has an unknown field.", body = ErrorBody),
        (status = 404, description = "`not_found`: no such scheduled class.", body = ErrorBody),
        (status = 422, description = "`cannot_schedule`: Peloton refused to remove the class; the message is Peloton's explanation.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected; whether the write happened is unknown, so read before retrying.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential for this account; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn schedule_remove(
    Account(account): Account,
    ScheduledId(id): ScheduledId,
    Params(NoParams {}): Params<NoParams>,
) -> Result<StatusCode, ApiError> {
    let write = Write {
        scheduled_id: Some(id.url_safe().to_owned()),
        ..Write::new("schedule_remove", &account.name)
    };
    let result = account
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
        (status = 200, description = "The Stack as it is.", body = StackView),
        (status = 400, description = "`invalid_input`: a path or query value is malformed, or the query has an unknown parameter.", body = ErrorBody),
        (status = 502, description = "`upstream_error`: Peloton failed or answered something unexpected.", body = ErrorBody),
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential for this account; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn stack(
    Account(account): Account,
    Params(NoParams {}): Params<NoParams>,
) -> Result<Json<StackView>, ApiError> {
    let stack = account.peloton.stack().await?;
    Ok(Json(StackView::from_upstream(&stack)))
}

/// Appends a class to the Stack.
///
/// Write route: deployments should only allow it for clients granted write
/// access (see the README, Security model). Audit-logged.
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
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential for this account; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn stack_add(
    Account(account): Account,
    Params(NoParams {}): Params<NoParams>,
    Body(request): Body<StackAddRequest>,
) -> Result<StackWritten, ApiError> {
    let write = Write {
        class_id: Some(request.class_id.clone()),
        ..Write::new("stack_add", &account.name)
    };
    let result = account
        .stack
        .add(&request.class_id)
        .await
        .map_err(ApiError::from);
    write.log(result.as_ref().map(|()| None));
    result?;
    Ok(read_back(&account.peloton).await)
}

/// Removes a class from the Stack, keeping the others in order.
///
/// Write route: deployments should only allow it for clients granted write
/// access (see the README, Security model). Audit-logged.
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
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential for this account; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn stack_remove(
    Account(account): Account,
    Id(id): Id,
    Params(NoParams {}): Params<NoParams>,
) -> Result<StackWritten, ApiError> {
    let write = Write {
        class_id: Some(id.clone()),
        ..Write::new("stack_remove", &account.name)
    };
    let result = account.stack.remove(&id).await.map_err(ApiError::from);
    write.log(result.as_ref().map(|()| None));
    result?;
    Ok(read_back(&account.peloton).await)
}

/// Replaces the whole Stack: its classes and their order.
///
/// Write route: deployments should only allow it for clients granted write
/// access (see the README, Security model). Audit-logged.
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
        (status = 503, description = "`not_authenticated`: the relay holds no usable Peloton credential for this account; or `upstream_rate_limited`: Peloton is rate limiting the relay.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "Seconds to wait; only with `upstream_rate_limited`, when Peloton said."))
        )
    )
)]
async fn stack_replace(
    Account(account): Account,
    Params(NoParams {}): Params<NoParams>,
    Body(request): Body<StackReplaceRequest>,
) -> Result<StackWritten, ApiError> {
    let mut seen = HashSet::with_capacity(request.class_ids.len());
    let distinct = request.class_ids.iter().all(|id| seen.insert(id));
    // Input validation, so not a write; `StackService` checks it again.
    if request.class_ids.len() > MAX_STACK_CLASSES || !distinct {
        return Err(ApiError::InvalidInput);
    }
    let ids: Vec<String> = request.class_ids.iter().map(ToString::to_string).collect();
    let write = Write {
        class_ids: Some(ids.join(",")),
        ..Write::new("stack_replace", &account.name)
    };
    let result = account
        .stack
        .replace(&request.class_ids)
        .await
        .map_err(ApiError::from);
    write.log(result.as_ref().map(|()| None));
    result?;
    Ok(read_back(&account.peloton).await)
}

/// Liveness: the process is up. Never calls Peloton.
#[utoipa::path(
    get,
    path = "/healthz",
    responses((status = 200, description = "The service status.", body = Status))
)]
async fn healthz() -> Json<Status> {
    Json(Status { status: "ok" })
}

/// Readiness: the server is up. Account credentials are reported in the logs, not here.
#[utoipa::path(
    get,
    path = "/readyz",
    responses((status = 200, description = "The server is up.", body = Status))
)]
async fn readyz() -> Json<Status> {
    Json(Status { status: "ready" })
}

/// The `OpenAPI` document.
async fn spec() -> Json<utoipa::openapi::OpenApi> {
    Json(openapi())
}
