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

use std::sync::Arc;
use std::time::Duration;

use peloton::endpoints::{self, GRAPHQL_URL, PLATFORM_HEADER};
use peloton::graphql::Op;
use peloton::ids::PelotonId;
use peloton::upstream::{Me, PerformanceGraph, RideDetails, Workout, WorkoutPage};
use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use time::OffsetDateTime;
use time::UtcOffset;
use time::format_description::well_known::Rfc3339;
use tokio::sync::Semaphore;
use url::Url;

use crate::tokens::TokenManager;

/// Default bound on concurrent upstream requests.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 4;

/// Why an upstream call failed. Carries no response bodies and no tokens.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
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

/// Longest Peloton explanation kept in [`UpstreamError::CannotSchedule`].
pub const MAX_CANNOT_SCHEDULE_MESSAGE: usize = 200;

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

    /// Sends GraphQL operation `op` with `variables` and decodes the root
    /// field `data.<op.field>` of the answer as `T`.
    ///
    /// The operation name goes in the URL as `?query=<name>` or
    /// `?mutation=<name>`, the body is `{"query", "variables"}`. A non-empty
    /// `errors` array fails the call even when `data` is present.
    ///
    /// # Errors
    ///
    /// [`UpstreamError::GraphQl`] when the gateway reports errors (only their
    /// `extensions.code`s are logged); [`UpstreamError::Decode`] when `data`,
    /// its root field or a field inside it does not match, with a path such
    /// as `data.addClassToSchedule.id`. See [`UpstreamError`] for the others.
    pub async fn gql<T: DeserializeOwned>(
        &self,
        op: &Op,
        variables: &Value,
    ) -> Result<T, UpstreamError> {
        let mut url = Url::parse(&self.graphql_url).map_err(|_| UpstreamError::Transport)?;
        url.query_pairs_mut()
            .clear()
            .append_pair(op.kind.as_str(), op.name);
        let body = serde_json::json!({ "query": op.document, "variables": variables });
        let (reply, _): (GraphQlReply, _) = self
            .call(|_| Ok(self.http.post(url.clone()).json(&body)))
            .await?;
        if let Some(errors) = reply.errors.filter(|errors| !errors.is_empty()) {
            let codes: Vec<&str> = errors
                .iter()
                .take(MAX_LOGGED_CODES)
                .map(|error| {
                    error
                        .pointer("/extensions/code")
                        .and_then(Value::as_str)
                        .map_or("<none>", loggable_name)
                })
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
        self.call(|user| Ok(self.http.get(self.url(&path(user))?).query(params)))
            .await
    }

    /// Sends the request `build` makes for the credential owner, with the
    /// bearer token and platform header, holding one permit until the body is
    /// decoded as `T`; returns the owner's id with it.
    ///
    /// On 401 the token manager decides whether a retry can help (it refreshed,
    /// or another request already did); a second 401 is reported back too, so
    /// the manager can tell that even a fresh token is rejected.
    async fn call<T: DeserializeOwned>(
        &self,
        build: impl Fn(&PelotonId) -> Result<reqwest::RequestBuilder, UpstreamError>,
    ) -> Result<(T, PelotonId), UpstreamError> {
        let _permit = self.permits.acquire().await.map_err(|_| {
            tracing::error!("upstream concurrency limiter closed");
            UpstreamError::Transport
        })?;
        let (token, owner, response) = self.send(&build).await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok((interpret(response).await?, owner));
        }
        if !self.tokens.unauthorized(&token).await {
            return Err(UpstreamError::NotAuthenticated);
        }
        let (token, owner, response) = self.send(&build).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.tokens.unauthorized(&token).await;
            return Err(UpstreamError::NotAuthenticated);
        }
        Ok((interpret(response).await?, owner))
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

/// Interprets the root field of a schedule mutation or read, a union
/// dispatched on `__typename`: a `ScheduledClass` decodes as `T`.
///
/// # Errors
///
/// - `ScheduledClassDoesNotExistError` → [`UpstreamError::NotFound`].
/// - `CannotScheduleClassError` → [`UpstreamError::CannotSchedule`] with
///   Peloton's `message`, capped at [`MAX_CANNOT_SCHEDULE_MESSAGE`]
///   characters ([`UpstreamError::Decode`] at `message` without one).
/// - Any other member → [`UpstreamError::GraphQl`].
/// - No string `__typename`, or a `ScheduledClass` that does not decode →
///   [`UpstreamError::Decode`] with the path inside `root`.
pub fn schedule_result<T: DeserializeOwned>(root: Value) -> Result<T, UpstreamError> {
    let Some(typename) = root.get("__typename").and_then(Value::as_str) else {
        tracing::warn!("schedule result has no __typename");
        return Err(UpstreamError::Decode {
            path: "__typename".to_owned(),
        });
    };
    match typename {
        "ScheduledClass" => serde_path_to_error::deserialize(root).map_err(|err| {
            let path = err.path().to_string();
            tracing::warn!(%path, "scheduled class did not decode");
            UpstreamError::Decode { path }
        }),
        "ScheduledClassDoesNotExistError" => Err(UpstreamError::NotFound),
        "CannotScheduleClassError" => {
            let code = root
                .get("code")
                .and_then(Value::as_str)
                .map_or("<none>", loggable_name);
            let Some(message) = root.get("message").and_then(Value::as_str) else {
                tracing::warn!(code, "cannot-schedule error has no message");
                return Err(UpstreamError::Decode {
                    path: "message".to_owned(),
                });
            };
            tracing::info!(code, "Peloton cannot schedule the class");
            Err(UpstreamError::CannotSchedule {
                message: message.chars().take(MAX_CANNOT_SCHEDULE_MESSAGE).collect(),
            })
        }
        other => {
            tracing::warn!(
                typename = loggable_name(other),
                "unexpected schedule result member"
            );
            Err(UpstreamError::GraphQl)
        }
    }
}

fn utc_iso8601(instant: OffsetDateTime) -> Result<String, UpstreamError> {
    instant
        .to_offset(UtcOffset::UTC)
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
    let body = response.bytes().await.map_err(|err| {
        tracing::warn!(timeout = err.is_timeout(), "reading upstream body failed");
        UpstreamError::Transport
    })?;
    serde_path_to_error::deserialize(&mut serde_json::Deserializer::from_slice(&body)).map_err(
        |err| {
            // The inner error can quote body content; only the path is safe.
            let path = err.path().to_string();
            tracing::warn!(%path, "upstream response did not decode");
            UpstreamError::Decode { path }
        },
    )
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
