//! `peloton-relay seed`: validates a token set against Peloton and stores it.

use std::io::{self, Read};
use std::time::Duration;

use peloton::body::read_limited;
use peloton::endpoints::PLATFORM_HEADER;
use peloton::oauth::TokenSet;
use peloton::upstream::Me;
use secrecy::ExposeSecret as _;
use url::Url;

use crate::store::{State, StateFile, StoreError};

/// Upper bound for the token JSON read from the input.
const MAX_INPUT_BYTES: u64 = 64 * 1024;

/// Why seeding failed. Never carries tokens or response bodies.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SeedError {
    /// The input could not be read, is too large or is not a token set.
    #[error("input is not a valid token set")]
    InvalidInput,
    /// Peloton could not be reached or its answer was not understood.
    #[error("could not fetch the profile from Peloton")]
    Upstream,
    /// Peloton refused the access token.
    #[error("Peloton rejected the access token with status {status}")]
    Rejected {
        /// HTTP status code.
        status: u16,
    },
    /// Peloton answered with a status other than success or a rejection.
    #[error("Peloton answered with status {status}")]
    Unexpected {
        /// HTTP status code.
        status: u16,
    },
    /// The running relay kept the state file locked for the whole wait.
    #[error("the relay kept the state file locked (a token refresh in flight); seed again")]
    Busy,
    /// The state file could not be written.
    #[error("could not write the state file")]
    Store(#[source] StoreError),
}

impl From<StoreError> for SeedError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::Locked => Self::Busy,
            other => Self::Store(other),
        }
    }
}

/// Reads a [`TokenSet`] from `input`, looks up its owner with `GET /me` on
/// `api_base`, and saves both to `state`. Returns the owner's username (or id
/// when Peloton reports none). Nothing is written unless `/me` succeeded.
///
/// The save waits up to `lock_wait` for a refresh the relay has in flight
/// (see [`StateFile::lock`]), so the two never overwrite each other.
///
/// # Errors
///
/// See [`SeedError`].
pub async fn run_seed(
    input: impl Read,
    state: &StateFile,
    http: &reqwest::Client,
    api_base: &Url,
    lock_wait: Duration,
) -> Result<String, SeedError> {
    let tokens = read_tokens(input)?;
    let me = fetch_me(http, api_base, &tokens).await?;
    let label = me.username.unwrap_or_else(|| me.id.to_string());
    let store = state.clone();
    let seeded = State {
        tokens,
        user_id: me.id,
    };
    // Waiting for the lock blocks a thread; keep it off the async workers.
    tokio::task::spawn_blocking(move || store.save(&seeded, lock_wait))
        .await
        .map_err(|_| {
            SeedError::Store(StoreError::Io(io::Error::other(
                "state file task did not complete",
            )))
        })??;
    Ok(label)
}

fn read_tokens(input: impl Read) -> Result<TokenSet, SeedError> {
    let mut bytes = Vec::new();
    input
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| SeedError::InvalidInput)?;
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return Err(SeedError::InvalidInput);
    }
    // The parse error is dropped on purpose: its message can quote the input.
    serde_json::from_slice(&bytes).map_err(|_| SeedError::InvalidInput)
}

async fn fetch_me(
    http: &reqwest::Client,
    api_base: &Url,
    tokens: &TokenSet,
) -> Result<Me, SeedError> {
    let mut url = api_base.clone();
    url.path_segments_mut()
        .map_err(|()| SeedError::Upstream)?
        .pop_if_empty()
        .push("me");
    let response = http
        .get(url)
        .bearer_auth(tokens.access_token.expose_secret())
        .header(PLATFORM_HEADER.0, PLATFORM_HEADER.1)
        .send()
        .await
        .map_err(|_| SeedError::Upstream)?;
    let status = response.status();
    if !status.is_success() {
        let status = status.as_u16();
        return Err(if matches!(status, 401 | 403) {
            SeedError::Rejected { status }
        } else {
            SeedError::Unexpected { status }
        });
    }
    let body = read_limited(response)
        .await
        .map_err(|_| SeedError::Upstream)?;
    serde_json::from_slice(&body).map_err(|_| SeedError::Upstream)
}
