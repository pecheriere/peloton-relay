//! Client for the subset of the Peloton API the relay uses: REST reads and
//! the GraphQL gateway's schedule and Stack operations.
//!
//! Every call authenticates with the [`TokenManager`]'s current access token.
//! A 401 reports the rejected token back to the manager and retries once when
//! the manager has a different token; otherwise, or on a second 401, the
//! credential is unusable for now. Response
//! bodies never reach errors or logs: a decode failure reports only the path
//! of the offending field, and a GraphQL error only its `extensions.code`.
//! The one exception is Peloton's short, user-facing explanation of why a
//! class cannot be scheduled, which [`UpstreamError::CannotSchedule`] carries
//! for the API client and which is never logged.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use peloton::body::{BodyError, MAX_RESPONSE_BODY_BYTES, read_limited};
use peloton::endpoints::{self, GRAPHQL_URL, PLATFORM_HEADER};
use peloton::graphql::{Op, ops};
use peloton::ids::{OnDemandClassId, PelotonId, ScheduledClassId};
use peloton::upstream::{
    BrowseCategories, LibraryPage, Me, PerformanceGraph, RideDetails, RideFilters, ScheduleItem,
    ScheduleList, ScheduleResult, ScheduledClass, StackResponse, UserStack, Workout, WorkoutPage,
};
use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::UtcOffset;
use time::format_description::well_known::Rfc3339;
use tokio::sync::Semaphore;
use url::Url;

use crate::account::AccountName;
use crate::tokens::TokenManager;

/// Default bound on concurrent upstream requests.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 4;

/// Why an upstream call failed. Carries no response bodies and no tokens.
///
/// `Debug` never shows [`UpstreamError::CannotSchedule`]'s message, so
/// logging an error with `?` cannot leak it.
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpstreamError {
    /// No usable credential, or Peloton rejected a freshly refreshed one.
    #[error("not authenticated with Peloton")]
    NotAuthenticated,
    /// Peloton answered 404.
    #[error("not found upstream")]
    NotFound,
    /// Peloton answered 429.
    #[error("rate limited by Peloton")]
    RateLimited {
        /// How long Peloton asked us to wait, when it said so in seconds.
        retry_after: Option<Duration>,
    },
    /// Peloton answered with another unexpected status.
    #[error("upstream returned status {status}")]
    Upstream {
        /// The HTTP status code.
        status: u16,
    },
    /// The response body did not match the expected shape.
    #[error("upstream response did not decode at `{path}`")]
    Decode {
        /// Path of the field that failed, e.g. `data[0].id`.
        path: String,
    },
    /// The response body is larger than [`MAX_RESPONSE_BODY_BYTES`].
    #[error("upstream response body is larger than {MAX_RESPONSE_BODY_BYTES} bytes")]
    BodyTooLarge,
    /// The request could not be built, sent, or its response read.
    #[error("upstream request failed")]
    Transport,
    /// The GraphQL gateway reported errors, or answered with a union member
    /// the relay does not handle.
    #[error("upstream GraphQL operation failed")]
    GraphQl,
    /// Peloton refused to schedule the class.
    #[error("Peloton cannot schedule the class")]
    CannotSchedule {
        /// Peloton's user-facing explanation, at most
        /// [`MAX_CANNOT_SCHEDULE_MESSAGE`] characters.
        message: String,
    },
}

impl fmt::Debug for UpstreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAuthenticated => f.write_str("NotAuthenticated"),
            Self::NotFound => f.write_str("NotFound"),
            Self::RateLimited { retry_after } => f
                .debug_struct("RateLimited")
                .field("retry_after", retry_after)
                .finish(),
            Self::Upstream { status } => {
                f.debug_struct("Upstream").field("status", status).finish()
            }
            Self::Decode { path } => f.debug_struct("Decode").field("path", path).finish(),
            Self::BodyTooLarge => f.write_str("BodyTooLarge"),
            Self::Transport => f.write_str("Transport"),
            Self::GraphQl => f.write_str("GraphQl"),
            Self::CannotSchedule { .. } => f.debug_struct("CannotSchedule").finish_non_exhaustive(),
        }
    }
}

/// Longest Peloton explanation kept in [`UpstreamError::CannotSchedule`].
pub const MAX_CANNOT_SCHEDULE_MESSAGE: usize = 200;

/// How far the window [`Peloton::scheduled_class`] lists to confirm a class
/// reaches beyond the class's own start and end.
pub const SCHEDULE_CONFIRM_MARGIN: time::Duration = time::Duration::minutes(1);

/// Parameters of the workout list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkoutQuery {
    /// Zero-based page index.
    pub page: u32,
    /// Page size.
    pub limit: u32,
    /// Only workouts created at or after this instant.
    pub from: Option<OffsetDateTime>,
    /// Only workouts created at or before this instant.
    pub to: Option<OffsetDateTime>,
}

/// Parameters of the class library search. Values are sent as given; the
/// caller validates them against Peloton's filters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryQuery {
    /// Browse category slug, e.g. `cycling` (`browse_category`).
    pub category: Option<String>,
    /// Class type id (`class_type_id`).
    pub class_type: Option<PelotonId>,
    /// Instructor id (`instructor_id`).
    pub instructor: Option<PelotonId>,
    /// Exact class length in seconds (`duration`).
    pub duration_s: Option<u32>,
    /// Sort order (`sort_by` and `desc`).
    pub sort: Option<LibrarySort>,
    /// Zero-based page index.
    pub page: u32,
    /// Page size.
    pub limit: u32,
}

/// A library sort, as Peloton's filters list it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrarySort {
    /// The field to sort by (`sort_by`), e.g. `original_air_time`.
    pub by: String,
    /// Whether to sort descending (`desc`).
    pub desc: bool,
}

/// The Peloton API, as seen by the relay.
#[derive(Debug)]
pub struct Peloton {
    http: reqwest::Client,
    api_base: Url,
    /// Kept as text and parsed per call, like the REST URLs, so a bad URL is
    /// a request failure rather than a panic.
    graphql_url: String,
    tokens: Arc<TokenManager>,
    permits: Semaphore,
}

impl Peloton {
    /// Creates a client for the API at `api_base` (e.g.
    /// [`peloton::endpoints::API_BASE`]) and the GraphQL gateway at
    /// [`GRAPHQL_URL`], allowing `max_in_flight` concurrent requests (at least
    /// one; see [`DEFAULT_MAX_IN_FLIGHT`]). Both share the limit.
    ///
    /// `http` must be built with `redirect::Policy::none()`, so a redirect can
    /// never carry the bearer token to another host, and with connect and
    /// total timeouts, so a stalled upstream cannot hold a permit forever.
    #[must_use]
    pub fn new(
        http: reqwest::Client,
        api_base: Url,
        tokens: Arc<TokenManager>,
        max_in_flight: usize,
    ) -> Self {
        Self {
            http,
            api_base,
            graphql_url: GRAPHQL_URL.to_owned(),
            tokens,
            permits: Semaphore::new(max_in_flight.max(1)),
        }
    }

    /// The account whose credential every call uses.
    #[must_use]
    pub fn account(&self) -> &AccountName {
        self.tokens.account()
    }

    /// Sends GraphQL operations to `url` instead of [`GRAPHQL_URL`]; any query
    /// string `url` carries is replaced by the operation's.
    #[must_use]
    pub fn with_graphql_url(mut self, url: Url) -> Self {
        self.graphql_url = url.into();
        self
    }

    /// `GET /me`.
    ///
    /// # Errors
    ///
    /// See [`UpstreamError`].
    pub async fn me(&self) -> Result<Me, UpstreamError> {
        self.get(|_| endpoints::me(), &[]).await.map(|(me, _)| me)
    }

    /// `GET /user/{id}/workouts` for the credential's owner, newest first.
    ///
    /// # Errors
    ///
    /// See [`UpstreamError`].
    pub async fn workouts(&self, query: &WorkoutQuery) -> Result<WorkoutPage, UpstreamError> {
        let mut params = vec![
            ("sort_by", "-created".to_owned()),
            ("joins", RIDE_JOINS.to_owned()),
            ("page", query.page.to_string()),
            ("limit", query.limit.to_string()),
        ];
        if let Some(from) = query.from {
            params.push(("from", utc_iso8601(from)?));
        }
        if let Some(to) = query.to {
            params.push(("to", utc_iso8601(to)?));
        }
        self.get(endpoints::user_workouts, &params)
            .await
            .map(|(page, _)| page)
    }

    /// `GET /workout/{id}` with ride and instructor joined, if the workout
    /// belongs to the credential's owner.
    ///
    /// # Errors
    ///
    /// [`UpstreamError::NotFound`] also when the workout is another member's
    /// or names no owner, so the relay does not reveal that it exists. See
    /// [`UpstreamError`] for the others.
    pub async fn workout(&self, id: &PelotonId) -> Result<Workout, UpstreamError> {
        self.owned_workout(id, &[("joins", RIDE_JOINS.to_owned())])
            .await
    }

    /// `GET /workout/{id}/performance_graph`, sampled every `every_n` seconds,
    /// if the workout belongs to the credential's owner.
    ///
    /// The graph names no member, so ownership is checked first with a
    /// `GET /workout/{id}` (no joins).
    ///
    /// # Errors
    ///
    /// As [`Self::workout`].
    pub async fn performance_graph(
        &self,
        id: &PelotonId,
        every_n: u32,
    ) -> Result<PerformanceGraph, UpstreamError> {
        self.owned_workout(id, &[]).await?;
        self.get(
            |_| endpoints::performance_graph(id),
            &[("every_n", every_n.to_string())],
        )
        .await
        .map(|(graph, _)| graph)
    }

    /// `GET /workout/{id}`, or [`UpstreamError::NotFound`] unless its
    /// `user_id` is the credential owner's.
    async fn owned_workout(
        &self,
        id: &PelotonId,
        params: &[(&str, String)],
    ) -> Result<Workout, UpstreamError> {
        let (workout, owner): (Workout, _) = self.get(|_| endpoints::workout(id), params).await?;
        if workout.user_id.as_ref() == Some(&owner) {
            Ok(workout)
        } else {
            Err(UpstreamError::NotFound)
        }
    }

    /// `GET /ride/{id}/details`.
    ///
    /// # Errors
    ///
    /// See [`UpstreamError`].
    pub async fn ride_details(&self, id: &PelotonId) -> Result<RideDetails, UpstreamError> {
        self.get(|_| endpoints::ride_details(id), &[])
            .await
            .map(|(details, _)| details)
    }

    /// `GET /v2/ride/archived`: one page of the on-demand class library,
    /// filtered and sorted upstream so paging stays Peloton's.
    ///
    /// # Errors
    ///
    /// See [`UpstreamError`].
    pub async fn library(&self, query: &LibraryQuery) -> Result<LibraryPage, UpstreamError> {
        let mut params = Vec::with_capacity(8);
        if let Some(category) = &query.category {
            params.push(("browse_category", category.clone()));
        }
        if let Some(class_type) = &query.class_type {
            params.push(("class_type_id", class_type.to_string()));
        }
        if let Some(instructor) = &query.instructor {
            params.push(("instructor_id", instructor.to_string()));
        }
        if let Some(duration) = query.duration_s {
            params.push(("duration", duration.to_string()));
        }
        if let Some(sort) = &query.sort {
            params.push(("sort_by", sort.by.clone()));
            params.push(("desc", sort.desc.to_string()));
        }
        params.push(("page", query.page.to_string()));
        params.push(("limit", query.limit.to_string()));
        self.get(|_| endpoints::library_archived(), &params)
            .await
            .map(|(page, _)| page)
    }

    /// `GET /ride/filters`: the on-demand library's filters and sorts for the
    /// browse category `category`.
    ///
    /// # Errors
    ///
    /// See [`UpstreamError`].
    pub async fn ride_filters(&self, category: &str) -> Result<RideFilters, UpstreamError> {
        let params = [
            ("library_type", ON_DEMAND.to_owned()),
            ("browse_category", category.to_owned()),
        ];
        self.get(|_| endpoints::ride_filters(), &params)
            .await
            .map(|(filters, _)| filters)
    }

    /// `GET /browse_categories`: the on-demand library's categories.
    ///
    /// # Errors
    ///
    /// See [`UpstreamError`].
    pub async fn browse_categories(&self) -> Result<BrowseCategories, UpstreamError> {
        let params = [("library_type", ON_DEMAND.to_owned())];
        self.get(|_| endpoints::browse_categories(), &params)
            .await
            .map(|(categories, _)| categories)
    }

    /// `YourScheduleItems`: everything on the member's schedule between
    /// `from` and `to`, in Peloton's order.
    ///
    /// # Errors
    ///
    /// [`UpstreamError::Decode`] when Peloton answers with anything but the
    /// list, or when an item does not decode: a schedule is never reported as
    /// partial. See [`UpstreamError`] for the others.
    pub async fn schedule(
        &self,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> Result<Vec<ScheduleItem>, UpstreamError> {
        let variables = json!({ "startTime": utc_iso8601(from)?, "endTime": utc_iso8601(to)? });
        let ScheduleList::YourScheduleItemList(items) =
            self.gql(&ops::YOUR_SCHEDULE_ITEMS, &variables).await?;
        Ok(items.your_schedule_items)
    }

    /// `ScheduledClass`: one class on the member's schedule, confirmed by
    /// [`Self::schedule`] over the class's own window.
    ///
    /// Peloton rebuilds a class from its id, which encodes the ride and the
    /// start time, so `ScheduledClass` still answers for a removed class, or
    /// for the old id of a moved one; only the schedule list tells whether
    /// the member still has it. The window runs from
    /// [`SCHEDULE_CONFIRM_MARGIN`] before the class starts to as long after
    /// it ends, so it holds the class whether Peloton bounds the list by
    /// start time or by overlap, inclusively or not.
    ///
    /// # Errors
    ///
    /// [`UpstreamError::NotFound`] when Peloton has no such scheduled class,
    /// or its schedule lists no scheduled class with this id.
    /// [`UpstreamError::Decode`] when the class comes under another id than
    /// `id`, or without its start or end time, without which it cannot be
    /// confirmed. [`UpstreamError::Transport`] when the confirm window runs
    /// past the instants Peloton can be sent. See [`UpstreamError`] for the
    /// others.
    pub async fn scheduled_class(
        &self,
        id: &ScheduledClassId,
    ) -> Result<ScheduledClass, UpstreamError> {
        let op = ops::SCHEDULED_CLASS;
        let result = self.gql(&op, &json!({ "id": id.as_str() })).await?;
        let class = scheduled(&op, result)?;
        if class.id != *id {
            // An answer for another class is no answer about this one.
            return Err(decode_failure(&op, format!("data.{}.id", op.field)));
        }
        let Some(start) = class.scheduled_start_time else {
            return Err(decode_failure(
                &op,
                format!("data.{}.scheduledStartTime", op.field),
            ));
        };
        let Some(end) = class.scheduled_end_time else {
            return Err(decode_failure(
                &op,
                format!("data.{}.scheduledEndTime", op.field),
            ));
        };
        let from = start
            .checked_sub(SCHEDULE_CONFIRM_MARGIN)
            .ok_or(UpstreamError::Transport)?;
        let to = end
            .checked_add(SCHEDULE_CONFIRM_MARGIN)
            .ok_or(UpstreamError::Transport)?;
        let listed = self
            .schedule(from, to)
            .await?
            .iter()
            .any(|item| matches!(item, ScheduleItem::ScheduledClass(c) if c.id == *id));
        if listed {
            Ok(class)
        } else {
            Err(UpstreamError::NotFound)
        }
    }

    /// `AddClassToSchedule`: schedules the on-demand class of `ride` to start
    /// at `start` (sent as UTC). Sent once; never retried except after a
    /// rejected token.
    ///
    /// # Errors
    ///
    /// [`UpstreamError::CannotSchedule`] with Peloton's explanation when it
    /// refuses. See [`UpstreamError`] for the others.
    pub async fn add_to_schedule(
        &self,
        ride: &PelotonId,
        start: OffsetDateTime,
    ) -> Result<ScheduledClass, UpstreamError> {
        let op = ops::ADD_CLASS_TO_SCHEDULE;
        let variables = json!({
            "id": OnDemandClassId::from_ride(ride).as_str(),
            "startTime": utc_iso8601(start)?,
        });
        let result = self.gql(&op, &variables).await?;
        scheduled(&op, result)
    }

    /// `RescheduleClass`: moves the scheduled class `id` to `start` (sent as
    /// UTC). Peloton issues a new id, returned with the class. Sent once, as
    /// [`Self::add_to_schedule`].
    ///
    /// # Errors
    ///
    /// [`UpstreamError::NotFound`]; [`UpstreamError::CannotSchedule`]. See
    /// [`UpstreamError`] for the others.
    pub async fn reschedule(
        &self,
        id: &ScheduledClassId,
        start: OffsetDateTime,
    ) -> Result<ScheduledClass, UpstreamError> {
        let op = ops::RESCHEDULE_CLASS;
        let variables = json!({ "id": id.as_str(), "startTime": utc_iso8601(start)? });
        let result = self.gql(&op, &variables).await?;
        scheduled(&op, result)
    }

    /// `RemoveClassFromSchedule`: removes the scheduled class `id`. Sent once,
    /// as [`Self::add_to_schedule`].
    ///
    /// # Errors
    ///
    /// [`UpstreamError::NotFound`]. See [`UpstreamError`] for the others.
    pub async fn remove_from_schedule(&self, id: &ScheduledClassId) -> Result<(), UpstreamError> {
        let op = ops::REMOVE_CLASS_FROM_SCHEDULE;
        let result = self.gql(&op, &json!({ "id": id.as_str() })).await?;
        scheduled(&op, result).map(|_| ())
    }

    /// `ViewUserStack`: the whole Stack.
    ///
    /// Reads need no serialisation; writes go through
    /// [`StackService`](crate::stack::StackService), which never loses its
    /// own updates.
    ///
    /// # Errors
    ///
    /// [`UpstreamError::Decode`] at `….numClasses` when Peloton's count
    /// differs from the list it sent: a Stack is never reported as partial.
    /// See [`UpstreamError`] for the others.
    pub async fn stack(&self) -> Result<UserStack, UpstreamError> {
        let op = ops::VIEW_USER_STACK;
        let response = self.gql(&op, &json!({})).await?;
        whole_stack(&op, response)
    }

    /// `CheckDoesClassExistInUserStack` for the on-demand class of `ride`.
    ///
    /// # Errors
    ///
    /// See [`UpstreamError`].
    pub async fn class_in_stack(&self, ride: &PelotonId) -> Result<bool, UpstreamError> {
        let variables = json!({ "pelotonClassId": OnDemandClassId::from_ride(ride).as_str() });
        self.gql(&ops::CHECK_CLASS_IN_STACK, &variables).await
    }

    /// `AddClassToStack`: appends the on-demand class of `ride`. Sent once,
    /// as [`Self::add_to_schedule`]; done once Peloton answers success, whatever
    /// Stack it lists with it (read it back with [`Self::stack`]).
    ///
    /// # Errors
    ///
    /// [`UpstreamError::Decode`] at `data.addClassToStack` when Peloton
    /// answers anything but success. See [`UpstreamError`] for the others.
    pub async fn add_to_stack(&self, ride: &PelotonId) -> Result<(), UpstreamError> {
        let variables =
            json!({ "input": { "pelotonClassId": OnDemandClassId::from_ride(ride).as_str() } });
        let StackWrite::StackResponseSuccess =
            self.gql(&ops::ADD_CLASS_TO_STACK, &variables).await?;
        Ok(())
    }

    /// `ModifyStack`: replaces the whole Stack with `ids`, in that order,
    /// sent verbatim (Peloton's own `joinToken`s for classes already in the
    /// Stack). Sent once and done, as [`Self::add_to_stack`].
    ///
    /// # Errors
    ///
    /// As [`Self::add_to_stack`], at `data.modifyStack`.
    pub async fn modify_stack(&self, ids: &[OnDemandClassId]) -> Result<(), UpstreamError> {
        let ids: Vec<&str> = ids.iter().map(OnDemandClassId::as_str).collect();
        let variables = json!({ "input": { "pelotonClassIdList": ids } });
        let StackWrite::StackResponseSuccess = self.gql(&ops::MODIFY_STACK, &variables).await?;
        Ok(())
    }

    /// Sends GraphQL operation `op` with `variables` and decodes the root
    /// field `data.<op.field>` of the answer as `T`.
    ///
    /// The operation name goes in the URL as `?query=<name>` or
    /// `?mutation=<name>`, the body is `{"query", "variables"}`. A non-empty
    /// `errors` array fails the call even when `data` is present; one whose
    /// errors include `UNAUTHENTICATED` is handled as an HTTP 401.
    ///
    /// # Errors
    ///
    /// [`UpstreamError::GraphQl`] when the gateway reports errors (only their
    /// `extensions.code`s are logged); [`UpstreamError::Decode`] when `data`,
    /// its root field or a field inside it does not match, with a path such
    /// as `data.addClassToSchedule.id` (an unknown union member fails at the
    /// root field). See [`UpstreamError`] for the others.
    async fn gql<T: DeserializeOwned>(
        &self,
        op: &Op,
        variables: &Value,
    ) -> Result<T, UpstreamError> {
        let mut url = Url::parse(&self.graphql_url).map_err(|_| UpstreamError::Transport)?;
        url.query_pairs_mut()
            .clear()
            .append_pair(op.kind.as_str(), op.name);
        let body = json!({ "query": op.document, "variables": variables });
        let (reply, _): (GraphQlReply, _) = self
            .call(
                |_| Ok(self.http.post(url.clone()).json(&body)),
                GraphQlReply::unauthenticated,
            )
            .await?;
        if let Some(errors) = reply.errors.filter(|errors| !errors.is_empty()) {
            let codes: Vec<&str> = errors
                .iter()
                .take(MAX_LOGGED_CODES)
                .map(|error| error_code(error).map_or("<none>", loggable_name))
                .collect();
            tracing::warn!(
                op = op.name,
                ?codes,
                "upstream GraphQL operation returned errors"
            );
            return Err(UpstreamError::GraphQl);
        }
        let Some(Value::Object(mut data)) = reply.data else {
            return Err(decode_failure(op, "data".to_owned()));
        };
        let Some(root) = data.remove(op.field) else {
            return Err(decode_failure(op, format!("data.{}", op.field)));
        };
        serde_path_to_error::deserialize(root).map_err(|err| {
            // The inner error can quote values; only the path is safe.
            let inner = err.path().to_string();
            let path = match inner.as_str() {
                "." => format!("data.{}", op.field),
                seq if seq.starts_with('[') => format!("data.{}{seq}", op.field),
                field => format!("data.{}.{field}", op.field),
            };
            decode_failure(op, path)
        })
    }

    /// Issues a GET; `path` receives the credential owner's id, which is
    /// returned with the decoded body.
    async fn get<T: DeserializeOwned>(
        &self,
        path: impl Fn(&PelotonId) -> String,
        params: &[(&str, String)],
    ) -> Result<(T, PelotonId), UpstreamError> {
        self.call(
            |user| Ok(self.http.get(self.url(&path(user))?).query(params)),
            |_| false,
        )
        .await
    }

    /// Sends the request `build` makes for the credential owner, with the
    /// bearer token and platform header, holding one permit until the body is
    /// decoded as `T`; returns the owner's id with it.
    ///
    /// A 401, or a decoded body that `rejected` says refuses the token, is
    /// reported to the token manager, which decides whether one retry can
    /// help (it refreshed, or another request already did); a second
    /// rejection is reported back too, so the manager can tell that even a
    /// fresh token is refused. Nothing else is ever retried: writes are not
    /// idempotent.
    async fn call<T: DeserializeOwned>(
        &self,
        build: impl Fn(&PelotonId) -> Result<reqwest::RequestBuilder, UpstreamError>,
        rejected: impl Fn(&T) -> bool,
    ) -> Result<(T, PelotonId), UpstreamError> {
        let _permit = self.permits.acquire().await.map_err(|_| {
            tracing::error!("upstream concurrency limiter closed");
            UpstreamError::Transport
        })?;
        let (token, owner, body) = self.attempt(&build, &rejected).await?;
        if let Some(body) = body {
            return Ok((body, owner));
        }
        if !self.tokens.unauthorized(&token).await {
            return Err(UpstreamError::NotAuthenticated);
        }
        let (token, owner, body) = self.attempt(&build, &rejected).await?;
        if let Some(body) = body {
            return Ok((body, owner));
        }
        self.tokens.unauthorized(&token).await;
        Err(UpstreamError::NotAuthenticated)
    }

    /// Sends one request and decodes its body; `None` when Peloton refused
    /// the token, with a 401 or in a body `rejected` recognises.
    async fn attempt<T: DeserializeOwned>(
        &self,
        build: &impl Fn(&PelotonId) -> Result<reqwest::RequestBuilder, UpstreamError>,
        rejected: &impl Fn(&T) -> bool,
    ) -> Result<(SecretString, PelotonId, Option<T>), UpstreamError> {
        let (token, owner, response) = self.send(build).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            return Ok((token, owner, None));
        }
        let body: T = interpret(response).await?;
        if rejected(&body) {
            tracing::info!(
                account = %self.account(),
                "upstream refused the token in its answer"
            );
            return Ok((token, owner, None));
        }
        Ok((token, owner, Some(body)))
    }

    /// Sends one authenticated request; returns the token and owner it used
    /// with the response.
    async fn send(
        &self,
        build: &impl Fn(&PelotonId) -> Result<reqwest::RequestBuilder, UpstreamError>,
    ) -> Result<(SecretString, PelotonId, reqwest::Response), UpstreamError> {
        let (token, user) = self
            .tokens
            .bearer()
            .await
            .map_err(|_| UpstreamError::NotAuthenticated)?;
        let response = build(&user)?
            .bearer_auth(token.expose_secret())
            .header(PLATFORM_HEADER.0, PLATFORM_HEADER.1)
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(
                    timeout = err.is_timeout(),
                    connect = err.is_connect(),
                    "upstream request failed"
                );
                UpstreamError::Transport
            })?;
        Ok((token, user, response))
    }

    fn url(&self, path: &str) -> Result<Url, UpstreamError> {
        let joined = format!("{}{path}", self.api_base.as_str().trim_end_matches('/'));
        Url::parse(&joined).map_err(|_| UpstreamError::Transport)
    }
}

const RIDE_JOINS: &str = "ride,ride.instructor";

/// The library the relay searches.
const ON_DEMAND: &str = "on_demand";

/// The GraphQL error code for a refused token.
const UNAUTHENTICATED: &str = "UNAUTHENTICATED";

/// Most GraphQL error codes logged for one answer.
const MAX_LOGGED_CODES: usize = 10;

/// The GraphQL response envelope. `errors` entries are kept as raw values so
/// an unexpected error shape can never hide that there were errors.
#[derive(Deserialize)]
struct GraphQlReply {
    #[serde(default)]
    data: Option<Value>,
    #[serde(default)]
    errors: Option<Vec<Value>>,
}

impl GraphQlReply {
    /// Whether the gateway refused the token in-band.
    fn unauthenticated(&self) -> bool {
        self.errors
            .iter()
            .flatten()
            .any(|error| error_code(error) == Some(UNAUTHENTICATED))
    }
}

/// A GraphQL error's `extensions.code`, if it is a string.
fn error_code(error: &Value) -> Option<&str> {
    error.pointer("/extensions/code").and_then(Value::as_str)
}

fn decode_failure(op: &Op, path: String) -> UpstreamError {
    tracing::warn!(op = op.name, %path, "upstream GraphQL response did not decode");
    UpstreamError::Decode { path }
}

/// `name` when it is a short GraphQL-style name (an error code or type name),
/// which carries no member data and is safe to log; a placeholder otherwise.
fn loggable_name(name: &str) -> &str {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if valid { name } else { "<invalid>" }
}

/// The scheduled class of a schedule read or mutation's union answer.
///
/// # Errors
///
/// - `ScheduledClassDoesNotExistError` → [`UpstreamError::NotFound`].
/// - `CannotScheduleClassError` → [`UpstreamError::CannotSchedule`] with
///   Peloton's `message`, capped at [`MAX_CANNOT_SCHEDULE_MESSAGE`]
///   characters ([`UpstreamError::Decode`] at `….message` without one). Only
///   the error's `code` is logged.
fn scheduled(op: &Op, result: ScheduleResult) -> Result<ScheduledClass, UpstreamError> {
    match result {
        ScheduleResult::ScheduledClass(class) => Ok(*class),
        ScheduleResult::ScheduledClassDoesNotExistError(_) => Err(UpstreamError::NotFound),
        ScheduleResult::CannotScheduleClassError(error) => {
            let code = error.code.as_deref().map_or("<none>", loggable_name);
            let Some(message) = error.message else {
                return Err(decode_failure(op, format!("data.{}.message", op.field)));
            };
            tracing::info!(op = op.name, code, "Peloton cannot schedule the class");
            Err(UpstreamError::CannotSchedule {
                message: message.chars().take(MAX_CANNOT_SCHEDULE_MESSAGE).collect(),
            })
        }
    }
}

/// The answer to a Stack write: only whether it succeeded counts, so a write
/// Peloton applied is never reported as failed over the Stack listed with it.
#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum StackWrite {
    StackResponseSuccess,
}

/// The Stack of a Stack read, refused unless Peloton's count matches the list
/// it sent.
fn whole_stack(op: &Op, response: StackResponse) -> Result<UserStack, UpstreamError> {
    let StackResponse::StackResponseSuccess(stack) = response;
    let listed = u32::try_from(stack.user_stack.stacked_class_list.len()).ok();
    if stack.num_classes.is_none() || stack.num_classes != listed {
        return Err(decode_failure(op, format!("data.{}.numClasses", op.field)));
    }
    Ok(stack)
}

/// `instant` in UTC as RFC 3339, or [`UpstreamError::Transport`] (the
/// request cannot be built) when it is outside the years 0 to 9999 in UTC.
fn utc_iso8601(instant: OffsetDateTime) -> Result<String, UpstreamError> {
    instant
        .checked_to_offset(UtcOffset::UTC)
        .ok_or(UpstreamError::Transport)?
        .format(&Rfc3339)
        .map_err(|_| UpstreamError::Transport)
}

async fn interpret<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, UpstreamError> {
    match response.status() {
        s if s.is_success() => {}
        StatusCode::NOT_FOUND => return Err(UpstreamError::NotFound),
        StatusCode::TOO_MANY_REQUESTS => {
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            return Err(UpstreamError::RateLimited { retry_after });
        }
        other => {
            tracing::warn!(status = other.as_u16(), "upstream returned an error status");
            return Err(UpstreamError::Upstream {
                status: other.as_u16(),
            });
        }
    }
    let body = read_body(response).await?;
    serde_path_to_error::deserialize(&mut serde_json::Deserializer::from_slice(&body)).map_err(
        |err| {
            // The inner error can quote body content; only the path is safe.
            let path = err.path().to_string();
            tracing::warn!(%path, "upstream response did not decode");
            UpstreamError::Decode { path }
        },
    )
}

/// The body of `response`, at most [`MAX_RESPONSE_BODY_BYTES`] long.
async fn read_body(response: reqwest::Response) -> Result<Vec<u8>, UpstreamError> {
    read_limited(response).await.map_err(|err| match err {
        BodyError::TooLarge => {
            tracing::warn!(
                limit = MAX_RESPONSE_BODY_BYTES,
                "upstream response body exceeds the size limit"
            );
            UpstreamError::BodyTooLarge
        }
        BodyError::Transport { timeout } => {
            tracing::warn!(timeout, "reading upstream body failed");
            UpstreamError::Transport
        }
    })
}

#[cfg(test)]
mod tests {
    use super::loggable_name;

    #[test]
    fn loggable_name_keeps_codes_and_type_names() {
        for name in ["BAD_USER_INPUT", "CannotScheduleClassError", "E42"] {
            assert_eq!(loggable_name(name), name);
        }
    }

    #[test]
    fn loggable_name_hides_anything_else() {
        let long = "A".repeat(65);
        for name in ["", "drop me", "a;b", "é", "x\ny", long.as_str()] {
            assert_eq!(loggable_name(name), "<invalid>", "{name:?}");
        }
        assert_eq!(loggable_name(&"A".repeat(64)), "A".repeat(64));
    }
}
