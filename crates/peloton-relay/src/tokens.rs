//! One account's Peloton credential, refreshed before it expires.
//!
//! A state file counts only while the account's binding (`binding.json` next
//! to it) names its user: an unbound, mismatched or corrupt binding is
//! refused (logged once as `event="state_refused"`), and a manager that was
//! ready drops its credential rather than keep serving it. A refusal seen
//! without the state lock is read again under it first, since a `seed`
//! replaces the binding and the state one after the other under that lock.
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

use crate::account::{AccountName, user_suffix};
use crate::store::{FileId, Refusal, Snapshot, State, StateFile, StateLock, StoreError, Version};

/// Refresh this long before the access token expires.
pub const DEFAULT_REFRESH_MARGIN: Duration = Duration::from_hours(1);

/// Wait after the first transient refresh failure.
const INITIAL_BACKOFF: Duration = Duration::from_secs(5);
/// Upper bound for the wait between transient refresh failures.
const MAX_BACKOFF: Duration = Duration::from_mins(5);

/// Minimum time between two refreshes forced by 401 responses.
pub const REACTIVE_REFRESH_COOLDOWN: Duration = Duration::from_mins(10);

/// How often [`TokenManager::keep_fresh`] checks the credential.
pub const BACKGROUND_REFRESH_INTERVAL: Duration = Duration::from_mins(15);

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

/// Holds the credential of one account and keeps it fresh.
#[derive(Debug)]
pub struct TokenManager {
    shared: Arc<Shared>,
    /// The detached tasks doing the locked work.
    tasks: TaskTracker,
}

#[derive(Debug)]
struct Shared {
    /// The account whose credential this is; every credential log names it.
    account: AccountName,
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
    /// The Peloton user of the last credential this manager accepted.
    last_user: Option<PelotonId>,
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
    ///
    /// Boxed: a credential is much larger than the other phases.
    Ready(Box<Ready>),
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
    /// The binding version that allowed this state.
    binding_id: FileId,
    /// An external change was ignored because `state` is not saved yet (logged).
    external_ignored: bool,
    /// Set when the current access token came from a refresh forced by a 401.
    reactive: Option<Reactive>,
}

impl Ready {
    fn new(state: State, version: Version) -> Self {
        Self {
            state,
            persist: Persist::Saved,
            backoff: None,
            file_id: Some(version.state),
            binding_id: version.binding,
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

/// What [`Shared::persist`] did with the tokens in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Persisted {
    /// On disk (now or already).
    Saved,
    /// Not written; retried on the next call.
    Failed,
    /// Not written: the binding no longer names their user (logged).
    Refused,
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
    /// A well-formed state the binding does not allow.
    Refused(Refusal),
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
        // Transport and decode failures, and any failure added later, are
        // retried with backoff: only a proven rejection gives up on the token.
        _ => false,
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
    fn apply_loaded(&mut self, account: &AccountName, loaded: io::Result<Snapshot>) {
        let (state, version) = match loaded {
            Ok(Snapshot::Valid(state, version)) => (state, version),
            Ok(Snapshot::Missing) => {
                return self.reported.load_problem(account, LoadProblem::Missing);
            }
            Ok(Snapshot::Corrupt(_)) => {
                return self.reported.load_problem(account, LoadProblem::Corrupt);
            }
            Ok(Snapshot::Refused(refusal)) => {
                return self
                    .reported
                    .load_problem(account, LoadProblem::Refused(refusal));
            }
            Err(err) => {
                return self
                    .reported
                    .load_problem(account, LoadProblem::Unreadable(err.kind()));
            }
        };
        self.reported.load_problem = None;
        if let Phase::Failed { dead_refresh } = &self.phase
            && dead_refresh.expose_secret() == state.tokens.refresh_token.expose_secret()
        {
            // Still the rejected credential: do not retry it.
            return;
        }
        self.accept_user(account, &state.user_id);
        tracing::info!(
            event = "state_loaded",
            account = %account,
            user = %user_suffix(&state.user_id),
            "loaded Peloton credential from the state file"
        );
        self.reported.credential_installed();
        self.phase = Phase::Ready(Box::new(Ready::new(state, version)));
    }

    /// Adopts a state file that another process replaced while ready.
    fn apply_external(&mut self, account: &AccountName, loaded: io::Result<Snapshot>) {
        if !matches!(self.phase, Phase::Ready(_)) {
            return;
        }
        match loaded {
            Ok(Snapshot::Valid(state, version)) => {
                self.accept_user(account, &state.user_id);
                tracing::info!(
                    event = "state_reloaded",
                    account = %account,
                    user = %user_suffix(&state.user_id),
                    "state file was replaced; using the new credential"
                );
                self.phase = Phase::Ready(Box::new(Ready::new(state, version)));
                self.reported.credential_installed();
            }
            Ok(Snapshot::Corrupt(id)) => {
                // Keep the working credential; remember this version so it is not re-parsed.
                if let Phase::Ready(ready) = &mut self.phase {
                    ready.file_id = Some(id);
                }
                self.reported.load_problem(account, LoadProblem::Corrupt);
            }
            Ok(Snapshot::Refused(refusal)) => {
                // Fail closed: the account no longer vouches for the credential in memory.
                self.phase = Phase::Empty;
                self.reported
                    .load_problem(account, LoadProblem::Refused(refusal));
            }
            // Deleted: the next save recreates it.
            Ok(Snapshot::Missing) => {}
            Err(err) => self
                .reported
                .load_problem(account, LoadProblem::Unreadable(err.kind())),
        }
    }

    /// Remembers `user` as the owner of the credential being installed,
    /// logging `event="account_rebound"` first when it is not the previous one.
    fn accept_user(&mut self, account: &AccountName, user: &PelotonId) {
        if let Some(previous) = &self.last_user
            && previous != user
        {
            tracing::warn!(
                event = "account_rebound",
                account = %account,
                previous_user = %user_suffix(previous),
                user = %user_suffix(user),
                "the account now holds the credential of another Peloton user"
            );
        }
        self.last_user = Some(user.clone());
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
    fn load_problem(&mut self, account: &AccountName, problem: LoadProblem) {
        if self.load_problem != Some(problem) {
            match problem {
                LoadProblem::Missing => {
                    tracing::info!(event = "state_missing", account = %account, "no state file yet; run seed");
                }
                LoadProblem::Corrupt => {
                    tracing::warn!(
                        event = "state_corrupt",
                        account = %account,
                        "state file is empty or corrupt; run seed"
                    );
                }
                LoadProblem::Refused(refusal) => {
                    tracing::error!(
                        event = "state_refused",
                        account = %account,
                        reason = refusal.as_str(),
                        "the account's binding does not allow its state file; run seed"
                    );
                }
                LoadProblem::Unreadable(kind) => {
                    tracing::warn!(event = "state_file_unreadable", account = %account, cause = ?kind, "cannot read the state file");
                }
            }
        }
        self.load_problem = Some(problem);
    }

    /// Logs the first failed save of a streak of failures.
    fn save_failed(&mut self, account: &AccountName, err: &StoreError) {
        if !mem::replace(&mut self.save_failing, true) {
            tracing::error!(event = "state_save_failed", account = %account, cause = ?err, "cannot save refreshed tokens; withholding them until saved");
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
    fn save_succeeded(&mut self, account: &AccountName) {
        // The file now holds a valid credential: a later problem is news again.
        self.load_problem = None;
        if mem::take(&mut self.save_failing) {
            tracing::info!(
                event = "state_save_recovered",
                account = %account,
                "saved the Peloton credential"
            );
        }
    }
}

impl TokenManager {
    /// Creates a manager for the credential of `account` in `store`, the
    /// account's state file.
    ///
    /// Does no I/O: the state file is read on the first [`Self::bearer`] call,
    /// so [`Self::is_ready`] is `false` until then.
    #[must_use]
    pub fn new(
        account: AccountName,
        store: StateFile,
        auth: AuthClient,
        refresh_margin: Duration,
    ) -> Self {
        Self::with_timing(
            account,
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
        account: AccountName,
        store: StateFile,
        auth: AuthClient,
        refresh_margin: Duration,
        clock: MonotonicClock,
        lock_wait: Duration,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                account,
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

    /// The account whose credential this manager holds.
    #[must_use]
    pub fn account(&self) -> &AccountName {
        &self.shared.account
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
            self.check_external(&mut inner, None).await;
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
        self.check_external(&mut inner, None).await;
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
                            account = %self.account,
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
                    account = %self.account,
                    "state file or binding was replaced while refreshed tokens are unsaved; keeping the refreshed tokens unless the binding refuses them"
                );
            }
            match self.lock_file().await {
                Ok(lock) => {
                    if self.persist(ready, &mut inner.reported, &lock).await == Persisted::Refused {
                        // The tokens are dropped; what the file holds now is what counts.
                        inner.phase = Phase::Empty;
                        let loaded = self.snapshot(Some(&lock)).await;
                        inner.apply_loaded(&self.account, loaded);
                    }
                }
                Err(err) => inner.reported.save_failed(&self.account, &err),
            }
            return;
        }
        let loaded = self.snapshot(None).await;
        inner.apply_loaded(&self.account, loaded);
    }

    /// While ready with everything saved, picks up a state file replaced by
    /// another process (`seed`); `held` is the state lock if the caller holds it.
    async fn check_external(&self, inner: &mut Inner, held: Option<&StateLock>) {
        let Phase::Ready(ready) = &inner.phase else {
            return;
        };
        if ready.persist != Persist::Saved || !self.file_changed(ready).await {
            return;
        }
        let loaded = self.snapshot(held).await;
        inner.apply_external(&self.account, loaded);
    }

    /// Reads the state file checked against the binding, under `held` if the
    /// caller holds the state lock.
    ///
    /// Without the lock, a refusal may be a `seed` caught between its binding
    /// and its state write, so it is read again under the lock before it
    /// counts. If the lock cannot be taken in time, the unlocked refusal
    /// stands: either way no credential is used.
    async fn snapshot(&self, held: Option<&StateLock>) -> io::Result<Snapshot> {
        if let Some(lock) = held {
            let lock = lock.clone();
            return blocking(move || lock.snapshot()).await.and_then(|r| r);
        }
        let store = self.store.clone();
        let loaded = blocking(move || store.snapshot()).await.and_then(|r| r);
        if !matches!(loaded, Ok(Snapshot::Refused(..))) {
            return loaded;
        }
        match self.lock_file().await {
            Ok(lock) => blocking(move || lock.snapshot()).await.and_then(|r| r),
            Err(err) => {
                tracing::warn!(event = "state_lock_failed", account = %self.account, cause = ?err, "cannot lock the state file to confirm that its binding refuses it");
                loaded
            }
        }
    }

    /// Whether the state file on disk is not the version `ready` came from
    /// or saved, or the binding is not the version that allowed it.
    async fn file_changed(&self, ready: &Ready) -> bool {
        let store = self.store.clone();
        let current =
            blocking(move || Ok::<_, io::Error>((store.current_id()?, store.binding_id()?)))
                .await
                .and_then(|r| r);
        match current {
            // A missing state file has nothing to adopt; a missing binding is a change.
            Ok((state, binding)) => {
                state.is_some_and(|id| ready.file_id != Some(id))
                    || binding != Some(ready.binding_id)
            }
            // Unreadable: nothing to adopt, and every save checks the binding.
            Err(_) => false,
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
                tracing::warn!(event = "state_lock_failed", account = %self.account, cause = ?err, retry_in_s = backoff.delay.as_secs(), "cannot lock the state file to refresh the Peloton access token");
                return false;
            }
        };
        self.check_external(inner, Some(&lock)).await;
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
                tracing::info!(event = "peloton_token_refreshed", account = %self.account, expires_at = %ready.state.tokens.expires_at, "refreshed the Peloton access token");
                if self.persist(ready, &mut inner.reported, &lock).await == Persisted::Refused {
                    inner.phase = Phase::Empty;
                    return false;
                }
                true
            }
            Err(err) if is_terminal(err) => {
                let dead_refresh = ready.state.tokens.refresh_token.clone();
                inner.phase = Phase::Failed { dead_refresh };
                tracing::error!(event = "peloton_auth_failed", account = %self.account, reason = %err, "Peloton rejected the refresh token; run seed to sign in again");
                false
            }
            Err(err) => {
                // The request may have run into its timeout: back off from its end.
                let backoff = Backoff::after_failure(ready.backoff, self.clock.now());
                ready.backoff = Some(backoff);
                tracing::warn!(event = "peloton_refresh_retry", account = %self.account, reason = %err, retry_in_s = backoff.delay.as_secs(), "could not refresh the Peloton access token");
                false
            }
        }
    }

    /// Writes refreshed tokens to the state file, under `lock`, if they are
    /// not there yet and the binding still names their user.
    ///
    /// On [`Persisted::Refused`] the caller drops the tokens (fail closed):
    /// the account now belongs to another user, or to none.
    async fn persist(
        &self,
        ready: &mut Ready,
        reported: &mut Reported,
        lock: &StateLock,
    ) -> Persisted {
        if ready.persist == Persist::Saved {
            return Persisted::Saved;
        }
        let lock = lock.clone();
        let state = ready.state.clone();
        match blocking(move || lock.save_bound(&state))
            .await
            .map_err(StoreError::Io)
            .and_then(|r| r)
        {
            Ok(Ok(id)) => {
                ready.file_id = Some(id);
                ready.external_ignored = false;
                ready.persist = Persist::Saved;
                reported.save_succeeded(&self.account);
                Persisted::Saved
            }
            Ok(Err(refusal)) => {
                reported.load_problem(&self.account, LoadProblem::Refused(refusal));
                Persisted::Refused
            }
            Err(err) => {
                reported.save_failed(&self.account, &err);
                Persisted::Failed
            }
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
        assert!(!is_terminal(AuthError::BodyTooLarge));
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
