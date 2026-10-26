//! The relay's only Peloton credential, refreshed before it expires.
//!
//! All state sits behind one async mutex that stays locked across a refresh,
//! so concurrent callers share a single refresh request (single-flight) and
//! the rotated refresh token is on disk before anyone gets the new access
//! token. The locked work runs in its own task: a caller that goes away
//! mid-refresh cannot drop a rotated refresh token on the floor.

use std::future::Future;
use std::io;
use std::panic;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use peloton::ids::PelotonId;
use peloton::oauth::{AuthClient, AuthError};
use secrecy::{ExposeSecret as _, SecretString};
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tracing::Instrument as _;
use tracing::instrument::WithSubscriber as _;

use crate::store::{State, StateFile, StoreError};

/// Refresh this long before the access token expires.
pub const DEFAULT_REFRESH_MARGIN: Duration = Duration::from_secs(60 * 60);

/// Wait after the first transient refresh failure.
const INITIAL_BACKOFF: Duration = Duration::from_secs(5);
/// Upper bound for the wait between transient refresh failures.
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// No usable Peloton credential: not seeded yet, unreadable, rejected, or
/// expired while the token endpoint is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not authenticated with Peloton")]
pub struct NotAuthenticated;

/// Holds the credential and keeps it fresh.
#[derive(Debug)]
pub struct TokenManager {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    store: StateFile,
    auth: AuthClient,
    refresh_margin: Duration,
    inner: Mutex<Inner>,
    /// Mirror of [`Inner::is_ready`] for synchronous readers.
    ready: AtomicBool,
}

#[derive(Debug, Default)]
struct Inner {
    phase: Phase,
    /// Last state file problem reported, so it is logged once, not per request.
    load_problem: Option<LoadProblem>,
}

#[derive(Debug, Default)]
enum Phase {
    /// No credential in memory: file missing, unreadable, corrupt, or not read yet.
    #[default]
    Empty,
    /// A credential that has not been rejected.
    Ready(Ready),
    /// The refresh token was rejected; waits for a seed with a different one.
    Failed { dead_refresh: SecretString },
}

#[derive(Debug)]
struct Ready {
    state: State,
    persist: Persist,
    /// Set after a transient refresh failure; no refresh before it allows.
    backoff: Option<Backoff>,
}

/// Whether the in-memory state is on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Persist {
    Saved,
    /// Refreshed tokens not written yet.
    Pending,
    /// Writing failed (already logged); retried on every call.
    Failing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadProblem {
    Missing,
    Corrupt,
    Unreadable(io::ErrorKind),
}

/// Exponential backoff between transient refresh failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Backoff {
    delay: Duration,
    not_before: Instant,
}

impl Backoff {
    /// The backoff after one more failure at `now`.
    fn after_failure(previous: Option<Self>, now: Instant) -> Self {
        let delay = previous.map_or(INITIAL_BACKOFF, |b| (b.delay * 2).min(MAX_BACKOFF));
        Self {
            delay,
            not_before: now + delay,
        }
    }

    fn allows(self, now: Instant) -> bool {
        now >= self.not_before
    }
}

/// A rejected refresh token stays rejected; anything else may pass on retry.
fn is_terminal(err: AuthError) -> bool {
    match err {
        AuthError::InvalidGrant => true,
        AuthError::Rejected { status } => (400..500).contains(&status) && status != 429,
        AuthError::Transport | AuthError::Decode => false,
    }
}

fn needs_refresh(state: &State, now: OffsetDateTime, margin: Duration) -> bool {
    state.tokens.expires_at - now < margin
}

impl Inner {
    fn is_ready(&self) -> bool {
        matches!(&self.phase, Phase::Ready(ready) if ready.persist == Persist::Saved)
    }

    /// Applies a fresh read of the state file while not ready.
    fn apply_loaded(&mut self, loaded: Result<Option<State>, StoreError>) {
        let state = match loaded {
            Ok(Some(state)) => state,
            Ok(None) => return self.note_load_problem(LoadProblem::Missing),
            Err(StoreError::Corrupt) => return self.note_load_problem(LoadProblem::Corrupt),
            Err(StoreError::Io(err)) => {
                return self.note_load_problem(LoadProblem::Unreadable(err.kind()));
            }
        };
        self.load_problem = None;
        if let Phase::Failed { dead_refresh } = &self.phase
            && dead_refresh.expose_secret() == state.tokens.refresh_token.expose_secret()
        {
            // Still the rejected credential: do not retry it.
            return;
        }
        tracing::info!(
            event = "state_loaded",
            "loaded Peloton credential from the state file"
        );
        self.phase = Phase::Ready(Ready {
            state,
            persist: Persist::Saved,
            backoff: None,
        });
    }

    fn note_load_problem(&mut self, problem: LoadProblem) {
        if self.load_problem != Some(problem) {
            match problem {
                LoadProblem::Missing => {
                    tracing::info!(event = "state_missing", "no state file yet; run seed");
                }
                LoadProblem::Corrupt => {
                    tracing::warn!(
                        event = "state_corrupt",
                        "state file is empty or corrupt; run seed"
                    );
                }
                LoadProblem::Unreadable(kind) => {
                    tracing::warn!(event = "state_file_unreadable", cause = ?kind, "cannot read the state file");
                }
            }
        }
        self.load_problem = Some(problem);
    }

    /// The current access token, if it is persisted and has not expired.
    fn usable(&self, now: OffsetDateTime) -> Result<(SecretString, PelotonId), NotAuthenticated> {
        match &self.phase {
            Phase::Ready(ready)
                if ready.persist == Persist::Saved && ready.state.tokens.expires_at > now =>
            {
                Ok((
                    ready.state.tokens.access_token.clone(),
                    ready.state.user_id.clone(),
                ))
            }
            _ => Err(NotAuthenticated),
        }
    }
}

impl TokenManager {
    /// Creates a manager for the credential in `store`.
    ///
    /// Does no I/O: the state file is read on the first [`Self::bearer`] call,
    /// so [`Self::is_ready`] is `false` until then.
    #[must_use]
    pub fn new(store: StateFile, auth: AuthClient, refresh_margin: Duration) -> Self {
        Self {
            shared: Arc::new(Shared {
                store,
                auth,
                refresh_margin,
                inner: Mutex::new(Inner::default()),
                ready: AtomicBool::new(false),
            }),
        }
    }

    /// A valid access token and the owner's user id, refreshing first if the
    /// token expires within the refresh margin.
    ///
    /// While not ready, re-reads the state file, so a new seed takes effect
    /// without a restart.
    ///
    /// # Errors
    ///
    /// [`NotAuthenticated`] when no usable credential is available.
    pub async fn bearer(&self) -> Result<(SecretString, PelotonId), NotAuthenticated> {
        let shared = Arc::clone(&self.shared);
        detached(async move { shared.bearer().await })
            .await
            .unwrap_or(Err(NotAuthenticated))
    }

    /// Reports that Peloton rejected `used` with 401.
    ///
    /// Refreshes only if `used` is still the current access token, so a burst
    /// of failed requests causes at most one refresh.
    pub async fn unauthorized(&self, used: &SecretString) {
        let shared = Arc::clone(&self.shared);
        let used = used.clone();
        // `None` only when the runtime is shutting down; nothing left to do then.
        let _ = detached(async move { shared.unauthorized(&used).await }).await;
    }

    /// Whether a credential is loaded, persisted and not rejected.
    ///
    /// The access token may still be expired while the token endpoint is
    /// unavailable; [`Self::bearer`] is the authoritative check.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.shared.ready.load(Ordering::Acquire)
    }
}

/// Runs `work` in its own task so it completes even if the caller is dropped.
///
/// The task keeps the caller's span and subscriber. Panics propagate to the
/// caller; `None` means the runtime cancelled the task.
async fn detached<T, F>(work: F) -> Option<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::spawn(work.in_current_span().with_current_subscriber()).await {
        Ok(value) => Some(value),
        Err(err) => match err.try_into_panic() {
            Ok(payload) => panic::resume_unwind(payload),
            Err(_) => None,
        },
    }
}

impl Shared {
    async fn bearer(&self) -> Result<(SecretString, PelotonId), NotAuthenticated> {
        let mut inner = self.inner.lock().await;
        if !inner.is_ready() {
            self.reload(&mut inner).await;
        }
        let now = OffsetDateTime::now_utc();
        let due = matches!(&inner.phase, Phase::Ready(ready) if needs_refresh(&ready.state, now, self.refresh_margin));
        if due {
            self.refresh(&mut inner).await;
        }
        self.publish(&inner);
        inner.usable(OffsetDateTime::now_utc())
    }

    async fn unauthorized(&self, used: &SecretString) {
        let mut inner = self.inner.lock().await;
        let current = matches!(&inner.phase, Phase::Ready(ready)
            if ready.state.tokens.access_token.expose_secret() == used.expose_secret());
        if current {
            self.refresh(&mut inner).await;
        }
        self.publish(&inner);
    }

    fn publish(&self, inner: &Inner) {
        self.ready.store(inner.is_ready(), Ordering::Release);
    }

    /// Re-reads the state file, or retries a failed save of refreshed tokens.
    async fn reload(&self, inner: &mut Inner) {
        if let Phase::Ready(ready) = &mut inner.phase {
            // Memory holds newer tokens than the file; never overwrite them with it.
            self.persist(ready).await;
            return;
        }
        let store = self.store.clone();
        let loaded = blocking(move || store.load()).await.and_then(|r| r);
        inner.apply_loaded(loaded);
    }

    /// Refreshes the credential unless a backoff is in effect.
    async fn refresh(&self, inner: &mut Inner) {
        let Phase::Ready(ready) = &mut inner.phase else {
            return;
        };
        let now = Instant::now();
        if ready.backoff.is_some_and(|b| !b.allows(now)) {
            return;
        }
        match self.auth.refresh(&ready.state.tokens.refresh_token).await {
            Ok(tokens) => {
                ready.state.tokens = tokens;
                ready.backoff = None;
                ready.persist = Persist::Pending;
                tracing::info!(event = "peloton_token_refreshed", expires_at = %ready.state.tokens.expires_at, "refreshed the Peloton access token");
                self.persist(ready).await;
            }
            Err(err) if is_terminal(err) => {
                let dead_refresh = ready.state.tokens.refresh_token.clone();
                inner.phase = Phase::Failed { dead_refresh };
                tracing::error!(event = "peloton_auth_failed", reason = %err, "Peloton rejected the refresh token; run seed to sign in again");
            }
            Err(err) => {
                let backoff = Backoff::after_failure(ready.backoff, now);
                ready.backoff = Some(backoff);
                tracing::warn!(event = "peloton_refresh_retry", reason = %err, retry_in_s = backoff.delay.as_secs(), "could not refresh the Peloton access token");
            }
        }
    }

    /// Writes refreshed tokens to the state file if they are not there yet.
    async fn persist(&self, ready: &mut Ready) {
        if ready.persist == Persist::Saved {
            return;
        }
        let store = self.store.clone();
        let state = ready.state.clone();
        match blocking(move || store.save(&state)).await.and_then(|r| r) {
            Ok(()) => {
                if ready.persist == Persist::Failing {
                    tracing::info!(
                        event = "state_save_recovered",
                        "saved the Peloton credential"
                    );
                }
                ready.persist = Persist::Saved;
            }
            Err(err) => {
                if ready.persist == Persist::Pending {
                    tracing::error!(event = "state_save_failed", cause = ?err, "cannot save refreshed tokens; withholding them until saved");
                }
                ready.persist = Persist::Failing;
            }
        }
    }
}

/// Runs blocking file I/O off the async worker threads.
async fn blocking<T, F>(f: F) -> Result<T, StoreError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| StoreError::Io(io::Error::other("state file task did not complete")))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use peloton::oauth::AuthError;

    use super::{Backoff, INITIAL_BACKOFF, MAX_BACKOFF, is_terminal};

    #[test]
    fn rejected_grants_and_client_errors_are_terminal() {
        assert!(is_terminal(AuthError::InvalidGrant));
        for status in [400, 401, 403, 404, 499] {
            assert!(is_terminal(AuthError::Rejected { status }), "{status}");
        }
    }

    #[test]
    fn rate_limits_server_errors_and_transport_are_transient() {
        for status in [429, 500, 502, 503, 599, 302] {
            assert!(!is_terminal(AuthError::Rejected { status }), "{status}");
        }
        assert!(!is_terminal(AuthError::Transport));
        assert!(!is_terminal(AuthError::Decode));
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let now = Instant::now();
        let first = Backoff::after_failure(None, now);
        assert_eq!(first.delay, INITIAL_BACKOFF);
        assert!(!first.allows(now));
        assert!(first.allows(now + INITIAL_BACKOFF));

        let second = Backoff::after_failure(Some(first), now);
        assert_eq!(second.delay, INITIAL_BACKOFF * 2);
        assert_eq!(second.not_before, now + INITIAL_BACKOFF * 2);

        let mut backoff = second;
        for _ in 0..20 {
            backoff = Backoff::after_failure(Some(backoff), now);
        }
        assert_eq!(backoff.delay, MAX_BACKOFF);
        assert!(backoff.allows(now + MAX_BACKOFF));
        let just_before = (now + MAX_BACKOFF)
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        assert!(!backoff.allows(just_before));
    }
}
