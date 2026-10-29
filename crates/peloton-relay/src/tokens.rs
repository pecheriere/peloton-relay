//! The relay's only Peloton credential, refreshed before it expires.
//!
//! All state sits behind one async mutex that stays locked across a refresh,
//! so concurrent callers share a single refresh request (single-flight) and
//! the rotated refresh token is on disk before anyone gets the new access
//! token. The locked work runs in its own tracked task: a caller that goes
//! away mid-refresh cannot drop a rotated refresh token on the floor, and
//! [`TokenManager::drain`] lets shutdown wait for that work to finish.
//!
//! A refresh also holds the state file lock ([`StateFile::lock`]) from
//! re-reading the file to saving the rotated tokens, so a `seed` in another
//! process either lands first and is used, or waits until the rotated tokens
//! are saved and then replaces them.
//!
//! A 401 forces a refresh at most once per [`REACTIVE_REFRESH_COOLDOWN`]; when
//! Peloton also rejects the token that refresh minted before the cooldown
//! ends, the manager logs `event="peloton_unauthorized_after_refresh"` instead
//! of rotating again.

use std::fmt;
use std::future::Future;
use std::io;
use std::mem;
use std::panic;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use peloton::ids::PelotonId;
use peloton::oauth::{AuthClient, AuthError};
use secrecy::{ExposeSecret as _, SecretString};
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::Instrument as _;
use tracing::instrument::WithSubscriber as _;

use crate::store::{FileId, Snapshot, State, StateFile, StateLock, StoreError};

/// Refresh this long before the access token expires.
pub const DEFAULT_REFRESH_MARGIN: Duration = Duration::from_secs(60 * 60);

/// Wait after the first transient refresh failure.
const INITIAL_BACKOFF: Duration = Duration::from_secs(5);
/// Upper bound for the wait between transient refresh failures.
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// Minimum time between two refreshes forced by 401 responses.
pub const REACTIVE_REFRESH_COOLDOWN: Duration = Duration::from_secs(10 * 60);

/// How often [`TokenManager::keep_fresh`] checks the credential.
pub const BACKGROUND_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// How long a refresh waits for the state file lock by default; `seed`
/// holds it only while it writes the file.
pub const DEFAULT_STATE_LOCK_WAIT: Duration = Duration::from_secs(10);

/// The monotonic clock that backoff and cooldown decisions read.
#[derive(Clone)]
pub struct MonotonicClock(Arc<dyn Fn() -> Instant + Send + Sync>);

impl MonotonicClock {
    /// The system's monotonic clock.
    #[must_use]
    pub fn system() -> Self {
        Self(Arc::new(Instant::now))
    }

    /// A clock that reads `now`, so tests can move time forward.
    #[must_use]
    pub fn from_fn(now: impl Fn() -> Instant + Send + Sync + 'static) -> Self {
        Self(Arc::new(now))
    }

    fn now(&self) -> Instant {
        (self.0)()
    }
}

impl fmt::Debug for MonotonicClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MonotonicClock")
    }
}

/// No usable Peloton credential: not seeded yet, unreadable, rejected, or
/// expired while the token endpoint is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not authenticated with Peloton")]
pub struct NotAuthenticated;

/// Holds the credential and keeps it fresh.
#[derive(Debug)]
pub struct TokenManager {
    shared: Arc<Shared>,
    /// The detached tasks doing the locked work.
    tasks: TaskTracker,
}

#[derive(Debug)]
struct Shared {
    store: StateFile,
    auth: AuthClient,
    refresh_margin: Duration,
    clock: MonotonicClock,
    /// How long a refresh waits for the state file lock.
    lock_wait: Duration,
    inner: Mutex<Inner>,
    /// Mirror of [`Inner::is_ready`] for synchronous readers.
    ready: AtomicBool,
}

#[derive(Debug, Default)]
struct Inner {
    phase: Phase,
    reported: Reported,
}

/// State file problems already logged, so each is logged once, not per request.
#[derive(Debug, Default)]
struct Reported {
    /// Last problem reading the file; cleared once the file is valid again.
    load_problem: Option<LoadProblem>,
    /// A failed save was logged and no save has succeeded since.
    save_failing: bool,
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
    /// The state file version this state came from or was saved as.
    file_id: Option<FileId>,
    /// An external change was ignored because `state` is not saved yet (logged).
    external_ignored: bool,
    /// Set when the current access token came from a refresh forced by a 401.
    reactive: Option<Reactive>,
}

impl Ready {
    fn new(state: State, file_id: FileId) -> Self {
        Self {
            state,
            persist: Persist::Saved,
            backoff: None,
            file_id: Some(file_id),
            external_ignored: false,
            reactive: None,
        }
    }
}

/// A refresh forced by a 401 response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reactive {
    /// When it happened.
    at: Instant,
    /// Whether the rejection of the token it minted was logged.
    warned: bool,
}

/// Whether another refresh forced by a 401 is allowed at `now`.
fn cooldown_elapsed(last: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last) >= REACTIVE_REFRESH_COOLDOWN
}

/// Whether the in-memory state is on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Persist {
    Saved,
    /// Refreshed tokens not written yet; the save is retried on every call.
    Pending,
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
    fn apply_loaded(&mut self, loaded: io::Result<Snapshot>) {
        let (state, id) = match loaded {
            Ok(Snapshot::Valid(state, id)) => (state, id),
            Ok(Snapshot::Missing) => return self.reported.load_problem(LoadProblem::Missing),
            Ok(Snapshot::Corrupt(_)) => return self.reported.load_problem(LoadProblem::Corrupt),
            Err(err) => {
                return self
                    .reported
                    .load_problem(LoadProblem::Unreadable(err.kind()));
            }
        };
        self.reported.load_problem = None;
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
        self.reported.credential_installed();
        self.phase = Phase::Ready(Ready::new(state, id));
    }

    /// Adopts a state file that another process replaced while ready.
    fn apply_external(&mut self, loaded: io::Result<Snapshot>) {
        let Phase::Ready(ready) = &mut self.phase else {
            return;
        };
        match loaded {
            Ok(Snapshot::Valid(state, id)) => {
                tracing::info!(
                    event = "state_reloaded",
                    "state file was replaced; using the new credential"
                );
                *ready = Ready::new(state, id);
                self.reported.credential_installed();
            }
            Ok(Snapshot::Corrupt(id)) => {
                // Keep the working credential; remember this version so it is not re-parsed.
                ready.file_id = Some(id);
                self.reported.load_problem(LoadProblem::Corrupt);
            }
            // Deleted: the next save recreates it.
            Ok(Snapshot::Missing) => {}
            Err(err) => self
                .reported
                .load_problem(LoadProblem::Unreadable(err.kind())),
        }
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

impl Reported {
    /// Logs `problem` unless it is the one logged last.
    fn load_problem(&mut self, problem: LoadProblem) {
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

    /// Logs the first failed save of a streak of failures.
    fn save_failed(&mut self, err: &StoreError) {
        if !mem::replace(&mut self.save_failing, true) {
            tracing::error!(event = "state_save_failed", cause = ?err, "cannot save refreshed tokens; withholding them until saved");
        }
    }

    /// Forgets what was logged about the credential `phase` held before: a
    /// new one from the file starts clean, so its first failed save is logged
    /// and no recovery is claimed for tokens that were dropped unsaved.
    fn credential_installed(&mut self) {
        self.load_problem = None;
        self.save_failing = false;
    }

    /// Records a successful save, logging the end of a failure streak.
    fn save_succeeded(&mut self) {
        // The file now holds a valid credential: a later problem is news again.
        self.load_problem = None;
        if mem::take(&mut self.save_failing) {
            tracing::info!(
                event = "state_save_recovered",
                "saved the Peloton credential"
            );
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
        Self::with_timing(
            store,
            auth,
            refresh_margin,
            MonotonicClock::system(),
            DEFAULT_STATE_LOCK_WAIT,
        )
    }

    /// [`Self::new`], with backoff and cooldown decisions made on `clock`,
    /// and refreshes waiting at most `lock_wait` for the state file lock.
    #[must_use]
    pub fn with_timing(
        store: StateFile,
        auth: AuthClient,
        refresh_margin: Duration,
        clock: MonotonicClock,
        lock_wait: Duration,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                store,
                auth,
                refresh_margin,
                clock,
                lock_wait,
                inner: Mutex::new(Inner::default()),
                ready: AtomicBool::new(false),
            }),
            tasks: TaskTracker::new(),
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
        self.detached(async move { shared.bearer().await })
            .await
            .unwrap_or(Err(NotAuthenticated))
    }

    /// Reports that Peloton rejected `used` with 401, and returns whether a
    /// retry may succeed: the current access token is no longer `used`.
    ///
    /// First picks up a state file that `seed` replaced, so a new seed is
    /// used, not rotated away. Refreshes only if `used` is still the current
    /// access token, so a burst of failed requests causes at most one refresh,
    /// and at most once per [`REACTIVE_REFRESH_COOLDOWN`], so a token Peloton
    /// keeps rejecting does not rotate the credential on every request.
    pub async fn unauthorized(&self, used: &SecretString) -> bool {
        let shared = Arc::clone(&self.shared);
        let used = used.clone();
        // `None` only when the runtime is shutting down; nothing left to retry then.
        self.detached(async move { shared.unauthorized(&used).await })
            .await
            .unwrap_or(false)
    }

    /// Calls [`Self::bearer`] every `every` (first right away) until
    /// `shutdown` is cancelled, so an idle relay still refreshes on time and
    /// a dead credential is reported without waiting for traffic.
    ///
    /// Stopping mid-refresh is safe: the refresh itself is a tracked task.
    pub async fn keep_fresh(&self, every: Duration, shutdown: &CancellationToken) {
        let mut ticks = tokio::time::interval(every);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = async {
                    ticks.tick().await;
                    // Failures are logged by the manager; there is no caller to tell.
                    let _ = self.bearer().await;
                } => {}
            }
        }
    }

    /// Waits up to `timeout` for detached refresh and save work to finish,
    /// so a rotated refresh token reaches the state file before the process
    /// exits. Returns `false` when work was still running at the deadline.
    pub async fn drain(&self, timeout: Duration) -> bool {
        self.tasks.close();
        tokio::time::timeout(timeout, self.tasks.wait())
            .await
            .is_ok()
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

impl TokenManager {
    /// Runs `work` in its own tracked task so it completes even if the caller
    /// is dropped, and [`Self::drain`] can wait for it.
    ///
    /// The task keeps the caller's span and subscriber. Panics propagate to the
    /// caller; `None` means the runtime cancelled the task.
    async fn detached<T, F>(&self, work: F) -> Option<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        match self
            .tasks
            .spawn(work.in_current_span().with_current_subscriber())
            .await
        {
            Ok(value) => Some(value),
            Err(err) => match err.try_into_panic() {
                Ok(payload) => panic::resume_unwind(payload),
                Err(_) => None,
            },
        }
    }
}

impl Shared {
    async fn bearer(&self) -> Result<(SecretString, PelotonId), NotAuthenticated> {
        let mut inner = self.inner.lock().await;
        if inner.is_ready() {
            self.check_external(&mut inner).await;
        } else {
            self.reload(&mut inner).await;
        }
        let margin = self.refresh_margin;
        self.refresh(&mut inner, |ready| {
            needs_refresh(&ready.state, OffsetDateTime::now_utc(), margin)
        })
        .await;
        self.publish(&inner);
        inner.usable(OffsetDateTime::now_utc())
    }

    async fn unauthorized(&self, used: &SecretString) -> bool {
        let mut inner = self.inner.lock().await;
        // A seed that landed since `used` was handed out is the retry's token.
        self.check_external(&mut inner).await;
        let still_used =
            |ready: &Ready| ready.state.tokens.access_token.expose_secret() == used.expose_secret();
        let now = self.clock.now();
        let force = match &mut inner.phase {
            Phase::Ready(ready) if still_used(ready) => match &mut ready.reactive {
                None => true,
                Some(reactive) if cooldown_elapsed(reactive.at, now) => true,
                Some(reactive) => {
                    if !reactive.warned {
                        reactive.warned = true;
                        let wait = REACTIVE_REFRESH_COOLDOWN
                            .saturating_sub(now.saturating_duration_since(reactive.at));
                        tracing::warn!(
                            event = "peloton_unauthorized_after_refresh",
                            retry_in_s = wait.as_secs(),
                            "Peloton rejected the access token of a refresh forced by a 401; no forced refresh until the cooldown ends"
                        );
                    }
                    false
                }
            },
            _ => false,
        };
        if force
            && self.refresh(&mut inner, still_used).await
            && let Phase::Ready(ready) = &mut inner.phase
        {
            ready.reactive = Some(Reactive {
                at: now,
                warned: false,
            });
        }
        self.publish(&inner);
        matches!(&inner.phase, Phase::Ready(ready) if !still_used(ready))
    }

    fn publish(&self, inner: &Inner) {
        self.ready.store(inner.is_ready(), Ordering::Release);
    }

    /// Re-reads the state file, or retries a failed save of refreshed tokens.
    async fn reload(&self, inner: &mut Inner) {
        if let Phase::Ready(ready) = &mut inner.phase {
            // Memory holds newer tokens than the file (the old refresh token may
            // already be rotated away): save them, never replace them with the file.
            if !ready.external_ignored && self.file_changed(ready).await {
                ready.external_ignored = true;
                tracing::warn!(
                    event = "state_external_change_ignored",
                    "state file was replaced while refreshed tokens are unsaved; keeping the refreshed tokens"
                );
            }
            match self.lock_file().await {
                Ok(lock) => self.persist(ready, &mut inner.reported, &lock).await,
                Err(err) => inner.reported.save_failed(&err),
            }
            return;
        }
        let store = self.store.clone();
        let loaded = blocking(move || store.snapshot()).await.and_then(|r| r);
        inner.apply_loaded(loaded);
    }

    /// While ready with everything saved, picks up a state file replaced by
    /// another process (`seed`).
    async fn check_external(&self, inner: &mut Inner) {
        let Phase::Ready(ready) = &inner.phase else {
            return;
        };
        if ready.persist != Persist::Saved || !self.file_changed(ready).await {
            return;
        }
        let store = self.store.clone();
        let loaded = blocking(move || store.snapshot()).await.and_then(|r| r);
        inner.apply_external(loaded);
    }

    /// Whether the file on disk is not the version `ready` came from or saved.
    async fn file_changed(&self, ready: &Ready) -> bool {
        let store = self.store.clone();
        match blocking(move || store.current_id()).await.and_then(|r| r) {
            Ok(Some(id)) => ready.file_id != Some(id),
            // Missing or unreadable: nothing to adopt.
            Ok(None) | Err(_) => false,
        }
    }

    /// Takes the state file lock without blocking an async worker.
    async fn lock_file(&self) -> Result<StateLock, StoreError> {
        let store = self.store.clone();
        let wait = self.lock_wait;
        blocking(move || store.lock(wait))
            .await
            .map_err(StoreError::Io)
            .and_then(|r| r)
    }

    /// Refreshes the credential if `wanted` holds and no backoff is in
    /// effect; returns whether Peloton issued new tokens.
    ///
    /// Holds the state file lock from re-reading the file to saving the new
    /// tokens, and checks `wanted` again on what it re-read, so a seed that
    /// landed first is used instead of being refreshed away.
    async fn refresh(&self, inner: &mut Inner, wanted: impl Fn(&Ready) -> bool) -> bool {
        let now = self.clock.now();
        let Phase::Ready(ready) = &mut inner.phase else {
            return false;
        };
        if !wanted(ready) || ready.backoff.is_some_and(|b| !b.allows(now)) {
            return false;
        }
        let lock = match self.lock_file().await {
            Ok(lock) => lock,
            Err(err) => {
                // The wait for the lock took time: back off from its end.
                let backoff = Backoff::after_failure(ready.backoff, self.clock.now());
                ready.backoff = Some(backoff);
                tracing::warn!(event = "state_lock_failed", cause = ?err, retry_in_s = backoff.delay.as_secs(), "cannot lock the state file to refresh the Peloton access token");
                return false;
            }
        };
        self.check_external(inner).await;
        let Phase::Ready(ready) = &mut inner.phase else {
            return false;
        };
        if !wanted(ready) {
            return false;
        }
        match self.auth.refresh(&ready.state.tokens.refresh_token).await {
            Ok(tokens) => {
                ready.state.tokens = tokens;
                ready.backoff = None;
                ready.reactive = None;
                ready.persist = Persist::Pending;
                tracing::info!(event = "peloton_token_refreshed", expires_at = %ready.state.tokens.expires_at, "refreshed the Peloton access token");
                self.persist(ready, &mut inner.reported, &lock).await;
                true
            }
            Err(err) if is_terminal(err) => {
                let dead_refresh = ready.state.tokens.refresh_token.clone();
                inner.phase = Phase::Failed { dead_refresh };
                tracing::error!(event = "peloton_auth_failed", reason = %err, "Peloton rejected the refresh token; run seed to sign in again");
                false
            }
            Err(err) => {
                // The request may have run into its timeout: back off from its end.
                let backoff = Backoff::after_failure(ready.backoff, self.clock.now());
                ready.backoff = Some(backoff);
                tracing::warn!(event = "peloton_refresh_retry", reason = %err, retry_in_s = backoff.delay.as_secs(), "could not refresh the Peloton access token");
                false
            }
        }
    }

    /// Writes refreshed tokens to the state file, under `lock`, if they are
    /// not there yet.
    async fn persist(&self, ready: &mut Ready, reported: &mut Reported, lock: &StateLock) {
        if ready.persist == Persist::Saved {
            return;
        }
        let lock = lock.clone();
        let state = ready.state.clone();
        match blocking(move || lock.save_with_id(&state))
            .await
            .map_err(StoreError::Io)
            .and_then(|r| r)
        {
            Ok(id) => {
                ready.file_id = Some(id);
                ready.external_ignored = false;
                ready.persist = Persist::Saved;
                reported.save_succeeded();
            }
            Err(err) => reported.save_failed(&err),
        }
    }
}

/// Runs blocking file I/O off the async worker threads.
async fn blocking<T, F>(f: F) -> io::Result<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| io::Error::other("state file task did not complete"))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use peloton::oauth::AuthError;

    use super::{
        Backoff, INITIAL_BACKOFF, MAX_BACKOFF, REACTIVE_REFRESH_COOLDOWN, cooldown_elapsed,
        is_terminal,
    };

    #[test]
    fn reactive_refresh_waits_for_the_cooldown() {
        let last = Instant::now();
        assert!(!cooldown_elapsed(last, last));
        let just_before = (last + REACTIVE_REFRESH_COOLDOWN)
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        assert!(!cooldown_elapsed(last, just_before));
        assert!(cooldown_elapsed(last, last + REACTIVE_REFRESH_COOLDOWN));
        // A clock reading before `last` never allows a refresh.
        assert!(!cooldown_elapsed(last + REACTIVE_REFRESH_COOLDOWN, last));
    }

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
