//! Client for the read-only subset of the Peloton API.
//!
//! Every call authenticates with the [`TokenManager`]'s current access token.
//! A 401 reports the rejected token back to the manager and retries once when
//! the manager has a different token; otherwise, or on a second 401, the
//! credential is unusable for now. Response
//! bodies never reach errors or logs: a decode failure reports only the path
//! of the offending field.

use std::sync::Arc;
use std::time::Duration;

use peloton::endpoints::{self, PLATFORM_HEADER};
use peloton::ids::PelotonId;
use peloton::upstream::{Me, PerformanceGraph, RideDetails, Workout, WorkoutPage};
use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;
use secrecy::{ExposeSecret as _, SecretString};
use serde::de::DeserializeOwned;
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
}

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
    tokens: Arc<TokenManager>,
    permits: Semaphore,
}

impl Peloton {
    /// Creates a client for the API at `api_base` (e.g.
    /// [`peloton::endpoints::API_BASE`]), allowing `max_in_flight` concurrent
    /// requests (at least one; see [`DEFAULT_MAX_IN_FLIGHT`]).
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
            tokens,
            permits: Semaphore::new(max_in_flight.max(1)),
        }
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

    /// Issues a GET; `path` receives the credential owner's id, which is
    /// returned with the decoded body.
    ///
    /// On 401 the token manager decides whether a retry can help (it refreshed,
    /// or another request already did); a second 401 is reported back too, so
    /// the manager can tell that even a fresh token is rejected.
    async fn get<T: DeserializeOwned>(
        &self,
        path: impl Fn(&PelotonId) -> String,
        params: &[(&str, String)],
    ) -> Result<(T, PelotonId), UpstreamError> {
        let _permit = self.permits.acquire().await.map_err(|_| {
            tracing::error!("upstream concurrency limiter closed");
            UpstreamError::Transport
        })?;
        let (token, owner, response) = self.send(&path, params).await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok((interpret(response).await?, owner));
        }
        if !self.tokens.unauthorized(&token).await {
            return Err(UpstreamError::NotAuthenticated);
        }
        let (token, owner, response) = self.send(&path, params).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.tokens.unauthorized(&token).await;
            return Err(UpstreamError::NotAuthenticated);
        }
        Ok((interpret(response).await?, owner))
    }

    /// Sends one authenticated GET; returns the token and owner it used with
    /// the response.
    async fn send(
        &self,
        path: &impl Fn(&PelotonId) -> String,
        params: &[(&str, String)],
    ) -> Result<(SecretString, PelotonId, reqwest::Response), UpstreamError> {
        let (token, user) = self
            .tokens
            .bearer()
            .await
            .map_err(|_| UpstreamError::NotAuthenticated)?;
        let url = self.url(&path(&user))?;
        let response = self
            .http
            .get(url)
            .query(params)
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
