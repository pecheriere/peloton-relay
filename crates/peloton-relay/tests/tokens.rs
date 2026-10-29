//! Tests for the state file and the refreshing token manager.

use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use peloton::ids::PelotonId;
use peloton::oauth::AuthClient;
use peloton_relay::account::Accounts;
use peloton_relay::seed::run_seed;
use peloton_relay::store::{State, StateFile, StoreError};
use peloton_relay::tokens::{
    DEFAULT_STATE_LOCK_WAIT, MonotonicClock, NotAuthenticated, REACTIVE_REFRESH_COOLDOWN,
    TokenManager,
};
use secrecy::{ExposeSecret, SecretString};
use time::{Duration, OffsetDateTime};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;
use url::Url;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

use common::{
    ACCOUNT, LOCK_WAIT, USER, account, account_state_file, captured_logs, state, state_file, user,
};

const MARGIN: StdDuration = StdDuration::from_secs(3600);

fn manager(dir: &Path, server: &MockServer) -> TokenManager {
    let auth = AuthClient::new(common::http(), Url::parse(&server.uri()).unwrap());
    TokenManager::new(account(ACCOUNT), state_file(dir), auth, MARGIN)
}

fn refresh_mock(refresh_token: &str) -> wiremock::MockBuilder {
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains(format!(
            "refresh_token={refresh_token}"
        )))
}

fn rotated_tokens() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "access_token": "new-access",
        "refresh_token": "rotated-refresh",
        "expires_in": 172_800,
        "token_type": "Bearer",
    }))
}

async fn refresh_calls(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

#[test]
fn save_is_atomic_and_0600() {
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    fs::write(&file.path, b"previous contents").unwrap();
    fs::set_permissions(&file.path, fs::Permissions::from_mode(0o644)).unwrap();
    let old_inode = fs::metadata(&file.path).unwrap().ino();

    file.save(&state("access", "refresh", Duration::hours(48)), LOCK_WAIT)
        .unwrap();

    let meta = fs::metadata(&file.path).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    // Replaced by rename, not rewritten in place.
    assert_ne!(meta.ino(), old_inode);
    let mut entries: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    entries.sort();
    assert_eq!(entries, [".state.json.lock", "binding.json", "state.json"]);
    let lock = fs::metadata(dir.path().join(".state.json.lock")).unwrap();
    assert_eq!(lock.permissions().mode() & 0o777, 0o600);

    let loaded = file.load().unwrap().unwrap();
    assert_eq!(loaded.tokens.access_token.expose_secret(), "access");
    assert_eq!(loaded.tokens.refresh_token.expose_secret(), "refresh");
    assert_eq!(loaded.user_id, user());
}

#[test]
fn save_removes_stale_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    let stale = dir.path().join(".state.json.123.0.tmp");
    let live = dir.path().join(".state.json.456.1.tmp");
    let unrelated = [
        ".state.json.bak",
        ".state.json.x.0.tmp",
        ".state.json.1.tmp",
        ".other.json.1.0.tmp",
        "state.json.1.0.tmp",
    ];
    for name in unrelated {
        fs::write(dir.path().join(name), b"keep").unwrap();
    }
    fs::write(&live, b"another writer, mid-save").unwrap();
    fs::write(&stale, b"crashed writer").unwrap();
    let long_ago = std::time::SystemTime::now() - StdDuration::from_secs(600);
    fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();

    file.save(&state("access", "refresh", Duration::hours(48)), LOCK_WAIT)
        .unwrap();

    assert!(!stale.exists(), "stale temp file left behind");
    assert!(live.exists(), "a concurrent writer's temp file was removed");
    for name in unrelated {
        assert!(dir.path().join(name).exists(), "{name} was removed");
    }
}

#[test]
fn save_into_missing_directory_fails_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(&dir.path().join("absent"));
    let err = file
        .save(&state("access", "refresh", Duration::hours(48)), LOCK_WAIT)
        .unwrap_err();
    assert!(matches!(err, StoreError::Io(_)));
    assert!(!err.to_string().contains("access"));
}

#[tokio::test]
async fn missing_state_is_not_authenticated() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    assert!(state_file(dir.path()).load().unwrap().is_none());

    let tokens = manager(dir.path(), &server);
    assert!(!tokens.is_ready());
    assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
    assert!(!tokens.is_ready());
    assert_eq!(refresh_calls(&server).await, 0);
}

#[tokio::test]
async fn corrupt_state_is_not_authenticated() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    for contents in ["", "{", "null", "{\"tokens\": 1}", "\u{0}\u{1}garbage"] {
        fs::write(&file.path, contents).unwrap();
        assert!(
            matches!(file.load(), Err(StoreError::Corrupt)),
            "{contents:?} must be corrupt"
        );
        let tokens = manager(dir.path(), &server);
        assert!(!tokens.is_ready());
        assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
        assert!(!tokens.is_ready());
    }
    assert_eq!(refresh_calls(&server).await, 0);
}

#[tokio::test]
async fn picks_up_seeded_state() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let tokens = manager(dir.path(), &server);
    assert!(tokens.bearer().await.is_err());

    // Corrupt in between: still not authenticated, still re-read later.
    fs::write(&state_file(dir.path()).path, "{").unwrap();
    assert!(tokens.bearer().await.is_err());

    state_file(dir.path())
        .save(
            &state("seeded-access", "seeded-refresh", Duration::hours(48)),
            LOCK_WAIT,
        )
        .unwrap();
    let (access, user_id) = tokens.bearer().await.unwrap();
    assert_eq!(access.expose_secret(), "seeded-access");
    assert_eq!(user_id, user());
    assert!(tokens.is_ready());
    assert_eq!(refresh_calls(&server).await, 0);
}

#[tokio::test]
async fn fresh_token_is_used_without_refresh() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    state_file(dir.path())
        .save(
            &state("old-access", "old-refresh", Duration::hours(2)),
            LOCK_WAIT,
        )
        .unwrap();
    let tokens = manager(dir.path(), &server);
    // Construction does no I/O; the first call reads the file.
    assert!(!tokens.is_ready());
    for _ in 0..3 {
        let (access, _) = tokens.bearer().await.unwrap();
        assert_eq!(access.expose_secret(), "old-access");
    }
    assert!(tokens.is_ready());
    assert_eq!(refresh_calls(&server).await, 0);
}

#[tokio::test]
async fn refreshes_before_expiry() {
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens())
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    state_file(dir.path())
        .save(
            &state("old-access", "old-refresh", Duration::minutes(30)),
            LOCK_WAIT,
        )
        .unwrap();
    let tokens = manager(dir.path(), &server);

    for _ in 0..2 {
        let (access, _) = tokens.bearer().await.unwrap();
        assert_eq!(access.expose_secret(), "new-access");
    }
    assert_eq!(refresh_calls(&server).await, 1);
}

#[tokio::test]
async fn rotated_refresh_token_persisted_before_use() {
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens())
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", -Duration::minutes(5)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = manager(dir.path(), &server);

    let (access, _) = tokens.bearer().await.unwrap();
    assert_eq!(access.expose_secret(), "new-access");
    let on_disk = file.load().unwrap().unwrap();
    assert_eq!(
        on_disk.tokens.refresh_token.expose_secret(),
        "rotated-refresh"
    );
    assert_eq!(on_disk.tokens.access_token.expose_secret(), "new-access");
    assert!(on_disk.tokens.expires_at > OffsetDateTime::now_utc() + Duration::hours(47));
    assert_eq!(on_disk.user_id, user());
    assert_eq!(
        fs::metadata(&file.path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_share_one_refresh() {
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens().set_delay(StdDuration::from_millis(200)))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    state_file(dir.path())
        .save(
            &state("old-access", "old-refresh", -Duration::minutes(5)),
            LOCK_WAIT,
        )
        .unwrap();
    let tokens = Arc::new(manager(dir.path(), &server));

    let handles: Vec<_> = (0..50)
        .map(|_| {
            let tokens = Arc::clone(&tokens);
            tokio::spawn(async move { tokens.bearer().await })
        })
        .collect();
    for handle in handles {
        let (access, _) = handle.await.unwrap().unwrap();
        assert_eq!(access.expose_secret(), "new-access");
    }
    assert_eq!(refresh_calls(&server).await, 1);
}

#[tokio::test]
async fn invalid_grant_logs_event_once() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": "invalid_grant",
            "error_description": "Unknown or invalid refresh token.",
        })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    state_file(dir.path())
        .save(
            &state("old-access", "old-refresh", -Duration::minutes(5)),
            LOCK_WAIT,
        )
        .unwrap();
    let tokens = manager(dir.path(), &server);

    async {
        // The dead refresh token is still on disk; it must not be retried.
        for _ in 0..3 {
            assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
            assert!(!tokens.is_ready());
        }
        tokens.unauthorized(&SecretString::from("old-access")).await;
    }
    .instrument(tracing::info_span!("case", name = "invalid_grant_once"))
    .await;
    assert_eq!(refresh_calls(&server).await, 1);

    let output = logs.contents();
    let failures = output
        .lines()
        .filter(|line| line.contains("invalid_grant_once") && line.contains("peloton_auth_failed"))
        .count();
    assert_eq!(failures, 1, "{output}");
    for secret in ["old-access", "old-refresh", "new-access", "rotated-refresh"] {
        assert!(!output.contains(secret), "log leaks {secret}: {output}");
    }
}

#[tokio::test]
async fn other_4xx_rejection_is_terminal() {
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(
            ResponseTemplate::new(401).set_body_json(serde_json::json!({"error": "access_denied"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    state_file(dir.path())
        .save(
            &state("old-access", "old-refresh", Duration::minutes(30)),
            LOCK_WAIT,
        )
        .unwrap();
    let tokens = manager(dir.path(), &server);

    // Terminal even though the access token has not expired yet.
    assert!(tokens.bearer().await.is_err());
    assert!(tokens.bearer().await.is_err());
    assert!(!tokens.is_ready());
    assert_eq!(refresh_calls(&server).await, 1);
}

#[tokio::test]
async fn failed_state_recovers_after_new_seed() {
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(
            ResponseTemplate::new(403).set_body_json(serde_json::json!({"error": "invalid_grant"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", -Duration::minutes(5)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = manager(dir.path(), &server);
    assert!(tokens.bearer().await.is_err());

    // Re-seeding the same dead token does not resurrect it.
    file.save(
        &state("other-access", "old-refresh", Duration::hours(48)),
        LOCK_WAIT,
    )
    .unwrap();
    assert!(tokens.bearer().await.is_err());
    assert!(!tokens.is_ready());

    file.save(
        &state("seeded-access", "seeded-refresh", Duration::hours(48)),
        LOCK_WAIT,
    )
    .unwrap();
    let (access, _) = tokens.bearer().await.unwrap();
    assert_eq!(access.expose_secret(), "seeded-access");
    assert!(tokens.is_ready());
    assert_eq!(refresh_calls(&server).await, 1);
}

#[tokio::test]
async fn transient_refresh_failure_keeps_valid_token() {
    for status in [429, 500, 503] {
        let server = MockServer::start().await;
        refresh_mock("old-refresh")
            .respond_with(ResponseTemplate::new(status))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        state_file(dir.path())
            .save(
                &state("old-access", "old-refresh", Duration::minutes(30)),
                LOCK_WAIT,
            )
            .unwrap();
        let tokens = manager(dir.path(), &server);

        // Second call falls inside the backoff window: no tight retry loop.
        for _ in 0..2 {
            let (access, _) = tokens.bearer().await.unwrap();
            assert_eq!(access.expose_secret(), "old-access", "status {status}");
        }
        assert!(tokens.is_ready());
        assert_eq!(refresh_calls(&server).await, 1, "status {status}");
    }
}

#[tokio::test]
async fn transient_failure_with_expired_token_backs_off() {
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(ResponseTemplate::new(502))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    state_file(dir.path())
        .save(
            &state("old-access", "old-refresh", -Duration::minutes(5)),
            LOCK_WAIT,
        )
        .unwrap();
    let tokens = manager(dir.path(), &server);

    for _ in 0..3 {
        assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
    }
    // Not dead, only temporarily unable to refresh.
    assert!(tokens.is_ready());
    assert_eq!(refresh_calls(&server).await, 1);
}

#[tokio::test]
async fn unauthorized_with_stale_token_does_not_refresh_again() {
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens())
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    state_file(dir.path())
        .save(
            &state("old-access", "old-refresh", Duration::hours(48)),
            LOCK_WAIT,
        )
        .unwrap();
    let tokens = manager(dir.path(), &server);
    let (used, _) = tokens.bearer().await.unwrap();
    assert_eq!(used.expose_secret(), "old-access");

    tokens.unauthorized(&used).await;
    let (current, _) = tokens.bearer().await.unwrap();
    assert_eq!(current.expose_secret(), "new-access");

    // A second request that also failed with the old token must not refresh again.
    tokens.unauthorized(&used).await;
    let (current, _) = tokens.bearer().await.unwrap();
    assert_eq!(current.expose_secret(), "new-access");
    assert_eq!(refresh_calls(&server).await, 1);
}

#[tokio::test]
async fn refresh_survives_cancelled_caller() {
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens().set_delay(StdDuration::from_millis(300)))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", -Duration::minutes(5)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = manager(dir.path(), &server);

    // The caller gives up while the refresh is in flight (client disconnect).
    let abandoned = tokio::time::timeout(StdDuration::from_millis(50), tokens.bearer()).await;
    assert!(abandoned.is_err());

    // The rotated refresh token must still land on disk, not be lost.
    let (access, _) = tokens.bearer().await.unwrap();
    assert_eq!(access.expose_secret(), "new-access");
    let on_disk = file.load().unwrap().unwrap();
    assert_eq!(
        on_disk.tokens.refresh_token.expose_secret(),
        "rotated-refresh"
    );
    assert_eq!(refresh_calls(&server).await, 1);
}

#[tokio::test]
async fn adopts_state_seeded_while_ready() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", Duration::hours(48)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = manager(dir.path(), &server);
    let (access, _) = tokens.bearer().await.unwrap();
    assert_eq!(access.expose_secret(), "old-access");

    // `seed` from another process replaces the file of a healthy relay.
    file.save(
        &state("seeded-access", "seeded-refresh", Duration::hours(48)),
        LOCK_WAIT,
    )
    .unwrap();
    let (access, _) = tokens.bearer().await.unwrap();
    assert_eq!(access.expose_secret(), "seeded-access");
    assert!(tokens.is_ready());

    // A corrupt replacement does not knock out the working credential.
    fs::write(&file.path, "{").unwrap();
    let (access, _) = tokens.bearer().await.unwrap();
    assert_eq!(access.expose_secret(), "seeded-access");
    assert_eq!(refresh_calls(&server).await, 0);
}

/// Makes `dir` read-only and checks that this actually blocks writes
/// (it does not for root, which this test cannot run as).
fn make_read_only(dir: &Path) {
    fs::set_permissions(dir, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(
        fs::File::create(dir.join("probe")).is_err(),
        "directory permissions are not enforced (running as root?)"
    );
}

fn make_writable(dir: &Path) {
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
}

#[tokio::test]
async fn save_failure_withholds_tokens_until_saved() {
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens())
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", -Duration::minutes(5)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = manager(dir.path(), &server);

    make_read_only(dir.path());
    assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
    assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
    assert!(!tokens.is_ready());
    make_writable(dir.path());
    let on_disk = file.load().unwrap().unwrap();
    assert_eq!(on_disk.tokens.refresh_token.expose_secret(), "old-refresh");

    // The rotated tokens were kept in memory, not replaced by the stale file.
    let (access, _) = tokens.bearer().await.unwrap();
    assert_eq!(access.expose_secret(), "new-access");
    assert!(tokens.is_ready());
    let on_disk = file.load().unwrap().unwrap();
    assert_eq!(
        on_disk.tokens.refresh_token.expose_secret(),
        "rotated-refresh"
    );
    assert_eq!(refresh_calls(&server).await, 1);
}

#[tokio::test]
async fn unsaved_tokens_win_over_external_seed() {
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens())
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", -Duration::minutes(5)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = manager(dir.path(), &server);

    make_read_only(dir.path());
    assert!(tokens.bearer().await.is_err());
    make_writable(dir.path());
    file.save(
        &state("seeded-access", "seeded-refresh", Duration::hours(48)),
        LOCK_WAIT,
    )
    .unwrap();

    // The rotated refresh token exists only in memory: it is saved, not discarded.
    let (access, _) = tokens.bearer().await.unwrap();
    assert_eq!(access.expose_secret(), "new-access");
    let on_disk = file.load().unwrap().unwrap();
    assert_eq!(
        on_disk.tokens.refresh_token.expose_secret(),
        "rotated-refresh"
    );
    assert_eq!(refresh_calls(&server).await, 1);
}

/// Polls `condition` every 10 ms for up to five seconds.
async fn eventually(mut condition: impl AsyncFnMut() -> bool) -> bool {
    for _ in 0..500 {
        if condition().await {
            return true;
        }
        tokio::time::sleep(StdDuration::from_millis(10)).await;
    }
    false
}

#[tokio::test]
async fn background_loop_refreshes_without_traffic() {
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens())
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", Duration::minutes(30)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = Arc::new(manager(dir.path(), &server));
    let shutdown = CancellationToken::new();
    let refresher = tokio::spawn({
        let tokens = Arc::clone(&tokens);
        let shutdown = shutdown.clone();
        async move {
            tokens
                .keep_fresh(StdDuration::from_secs(3600), &shutdown)
                .await;
        }
    });

    // No request is made; the loop's first tick alone refreshes the due token.
    assert!(eventually(async || refresh_calls(&server).await == 1).await);
    assert!(
        eventually(async || file
            .load()
            .unwrap()
            .unwrap()
            .tokens
            .refresh_token
            .expose_secret()
            == "rotated-refresh")
        .await
    );
    shutdown.cancel();
    tokio::time::timeout(StdDuration::from_secs(1), refresher)
        .await
        .expect("loop stops on shutdown")
        .unwrap();
}

#[tokio::test]
async fn background_loop_keeps_checking() {
    let server = MockServer::start().await;
    refresh_mock("seeded-refresh")
        .respond_with(rotated_tokens())
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", Duration::hours(48)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = Arc::new(manager(dir.path(), &server));
    let shutdown = CancellationToken::new();
    let refresher = tokio::spawn({
        let tokens = Arc::clone(&tokens);
        let shutdown = shutdown.clone();
        async move {
            tokens
                .keep_fresh(StdDuration::from_millis(20), &shutdown)
                .await;
        }
    });
    assert!(eventually(async || tokens.is_ready()).await);
    assert_eq!(refresh_calls(&server).await, 0);

    // A later tick picks up a due credential and refreshes it.
    file.save(
        &state("seeded-access", "seeded-refresh", Duration::minutes(30)),
        LOCK_WAIT,
    )
    .unwrap();
    assert!(eventually(async || refresh_calls(&server).await == 1).await);
    shutdown.cancel();
    tokio::time::timeout(StdDuration::from_secs(1), refresher)
        .await
        .expect("loop stops on shutdown")
        .unwrap();
}

#[tokio::test]
async fn drain_waits_for_an_abandoned_refresh() {
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens().set_delay(StdDuration::from_millis(300)))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", -Duration::minutes(5)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = manager(dir.path(), &server);

    let abandoned = tokio::time::timeout(StdDuration::from_millis(50), tokens.bearer()).await;
    assert!(abandoned.is_err());

    // Shutdown: the rotated refresh token must be on disk once drained.
    assert!(tokens.drain(StdDuration::from_secs(5)).await);
    let on_disk = file.load().unwrap().unwrap();
    assert_eq!(
        on_disk.tokens.refresh_token.expose_secret(),
        "rotated-refresh"
    );
}

#[tokio::test]
async fn drain_gives_up_at_the_deadline() {
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens().set_delay(StdDuration::from_secs(2)))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    state_file(dir.path())
        .save(
            &state("old-access", "old-refresh", -Duration::minutes(5)),
            LOCK_WAIT,
        )
        .unwrap();
    let tokens = manager(dir.path(), &server);
    let abandoned = tokio::time::timeout(StdDuration::from_millis(50), tokens.bearer()).await;
    assert!(abandoned.is_err());
    assert!(!tokens.drain(StdDuration::from_millis(50)).await);
}

/// A monotonic clock the test moves forward by hand.
#[derive(Clone)]
struct TestClock {
    start: Instant,
    offset: Arc<Mutex<StdDuration>>,
}

impl TestClock {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            offset: Arc::default(),
        }
    }

    fn advance(&self, by: StdDuration) {
        *self.offset.lock().unwrap() += by;
    }

    fn clock(&self) -> MonotonicClock {
        let clock = self.clone();
        MonotonicClock::from_fn(move || clock.start + *clock.offset.lock().unwrap())
    }
}

fn manager_at(dir: &Path, auth_base: Url, clock: &TestClock) -> TokenManager {
    manager_waiting(dir, auth_base, clock, DEFAULT_STATE_LOCK_WAIT)
}

fn manager_waiting(
    dir: &Path,
    auth_base: Url,
    clock: &TestClock,
    lock_wait: StdDuration,
) -> TokenManager {
    let auth = AuthClient::new(common::http(), auth_base);
    TokenManager::with_timing(
        account(ACCOUNT),
        state_file(dir),
        auth,
        MARGIN,
        clock.clock(),
        lock_wait,
    )
}

/// Moves `clock` forward by `by` once `after` has passed, from another thread.
fn advance_later(clock: &TestClock, after: StdDuration, by: StdDuration) -> thread::JoinHandle<()> {
    let clock = clock.clone();
    thread::spawn(move || {
        thread::sleep(after);
        clock.advance(by);
    })
}

fn server_url(server: &MockServer) -> Url {
    Url::parse(&server.uri()).unwrap()
}

/// Log lines of the test case `case` that contain `needle`.
fn logged(output: &str, case: &str, needle: &str) -> usize {
    output
        .lines()
        .filter(|line| line.contains(case) && line.contains(needle))
        .count()
}

/// A token endpoint that accepts connections and closes them unanswered,
/// and the number of connections it accepted.
async fn closing_endpoint() -> (Url, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    tokio::spawn({
        let accepted = Arc::clone(&accepted);
        async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                accepted.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        }
    });
    (url, accepted)
}

#[tokio::test]
async fn transport_failure_keeps_valid_token_and_retries_after_backoff() {
    let logs = captured_logs();
    let (url, accepted) = closing_endpoint().await;
    let dir = tempfile::tempdir().unwrap();
    state_file(dir.path())
        .save(
            &state("old-access", "old-refresh", Duration::minutes(30)),
            LOCK_WAIT,
        )
        .unwrap();
    let clock = TestClock::new();
    let tokens = manager_at(dir.path(), url, &clock);

    async {
        // Second call falls inside the backoff window: no tight retry loop.
        for _ in 0..2 {
            let (access, _) = tokens.bearer().await.unwrap();
            assert_eq!(access.expose_secret(), "old-access");
        }
        assert_eq!(accepted.load(Ordering::SeqCst), 1);

        clock.advance(REACTIVE_REFRESH_COOLDOWN);
        let (access, _) = tokens.bearer().await.unwrap();
        assert_eq!(access.expose_secret(), "old-access");
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
    }
    .instrument(tracing::info_span!("case", name = "transport_retry"))
    .await;
    assert!(tokens.is_ready());

    let output = logs.contents();
    assert_eq!(
        logged(
            &output,
            "transport_retry",
            "could not reach the token endpoint"
        ),
        2,
        "{output}"
    );
}

#[tokio::test]
async fn undecodable_refresh_answer_keeps_valid_token_and_retries_after_backoff() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(ResponseTemplate::new(200).set_body_string("not a token set"))
        .expect(2)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    state_file(dir.path())
        .save(
            &state("old-access", "old-refresh", Duration::minutes(30)),
            LOCK_WAIT,
        )
        .unwrap();
    let clock = TestClock::new();
    let tokens = manager_at(dir.path(), server_url(&server), &clock);

    async {
        for _ in 0..2 {
            let (access, _) = tokens.bearer().await.unwrap();
            assert_eq!(access.expose_secret(), "old-access");
        }
        assert_eq!(refresh_calls(&server).await, 1);

        clock.advance(REACTIVE_REFRESH_COOLDOWN);
        let (access, _) = tokens.bearer().await.unwrap();
        assert_eq!(access.expose_secret(), "old-access");
        assert_eq!(refresh_calls(&server).await, 2);
    }
    .instrument(tracing::info_span!("case", name = "decode_retry"))
    .await;
    assert!(tokens.is_ready());

    let output = logs.contents();
    assert_eq!(
        logged(
            &output,
            "decode_retry",
            "token endpoint response could not be decoded"
        ),
        2,
        "{output}"
    );
}

fn third_tokens() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "access_token": "third-access",
        "refresh_token": "third-refresh",
        "expires_in": 172_800,
        "token_type": "Bearer",
    }))
}

#[tokio::test]
async fn forced_refresh_waits_out_the_cooldown() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens())
        .expect(1)
        .mount(&server)
        .await;
    refresh_mock("rotated-refresh")
        .respond_with(third_tokens())
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    state_file(dir.path())
        .save(
            &state("old-access", "old-refresh", Duration::hours(48)),
            LOCK_WAIT,
        )
        .unwrap();
    let clock = TestClock::new();
    let tokens = manager_at(dir.path(), server_url(&server), &clock);
    let minted = SecretString::from("new-access");

    async {
        let (used, _) = tokens.bearer().await.unwrap();
        assert!(tokens.unauthorized(&used).await);

        // Peloton rejects the token that refresh minted too: no rotation
        // within the cooldown, however often it is reported.
        assert!(!tokens.unauthorized(&minted).await);
        clock.advance(
            REACTIVE_REFRESH_COOLDOWN
                .checked_sub(StdDuration::from_secs(1))
                .unwrap(),
        );
        assert!(!tokens.unauthorized(&minted).await);
        assert_eq!(refresh_calls(&server).await, 1);

        clock.advance(StdDuration::from_secs(1));
        assert!(tokens.unauthorized(&minted).await);
        let (access, _) = tokens.bearer().await.unwrap();
        assert_eq!(access.expose_secret(), "third-access");
    }
    .instrument(tracing::info_span!("case", name = "cooldown_expiry"))
    .await;
    assert_eq!(refresh_calls(&server).await, 2);

    let output = logs.contents();
    assert_eq!(
        logged(
            &output,
            "cooldown_expiry",
            "peloton_unauthorized_after_refresh"
        ),
        1,
        "{output}"
    );
    assert_eq!(
        logged(&output, "cooldown_expiry", r#""retry_in_s":600"#),
        1,
        "{output}"
    );
}

#[tokio::test]
async fn rejection_after_the_cooldown_refreshes_without_warning() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens())
        .expect(1)
        .mount(&server)
        .await;
    refresh_mock("rotated-refresh")
        .respond_with(third_tokens())
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    state_file(dir.path())
        .save(
            &state("old-access", "old-refresh", Duration::hours(48)),
            LOCK_WAIT,
        )
        .unwrap();
    let clock = TestClock::new();
    let tokens = manager_at(dir.path(), server_url(&server), &clock);

    async {
        let (used, _) = tokens.bearer().await.unwrap();
        assert!(tokens.unauthorized(&used).await);
        // The minted token worked for longer than the cooldown before a 401.
        clock.advance(REACTIVE_REFRESH_COOLDOWN);
        assert!(tokens.unauthorized(&SecretString::from("new-access")).await);
    }
    .instrument(tracing::info_span!("case", name = "late_rejection"))
    .await;
    assert_eq!(refresh_calls(&server).await, 2);

    let output = logs.contents();
    assert_eq!(
        logged(
            &output,
            "late_rejection",
            "peloton_unauthorized_after_refresh"
        ),
        0,
        "{output}"
    );
}

#[tokio::test]
async fn corruption_after_a_good_save_is_logged_again() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens())
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", Duration::hours(48)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = manager(dir.path(), &server);

    async {
        let (used, _) = tokens.bearer().await.unwrap();
        fs::write(&file.path, "{").unwrap();
        tokens.bearer().await.unwrap();
        // A forced refresh saves a good file over the corrupt one.
        assert!(tokens.unauthorized(&used).await);
        assert_eq!(
            file.load()
                .unwrap()
                .unwrap()
                .tokens
                .refresh_token
                .expose_secret(),
            "rotated-refresh"
        );
        fs::write(&file.path, "{").unwrap();
        let (access, _) = tokens.bearer().await.unwrap();
        assert_eq!(access.expose_secret(), "new-access");
    }
    .instrument(tracing::info_span!("case", name = "corrupt_twice"))
    .await;

    let output = logs.contents();
    assert_eq!(
        logged(&output, "corrupt_twice", "state_corrupt"),
        2,
        "{output}"
    );
}

#[tokio::test]
async fn save_failure_is_logged_once_per_streak() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    // Tokens that are due again at once, so every call refreshes.
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "short-access",
            "refresh_token": "short-refresh",
            "expires_in": 1800,
            "token_type": "Bearer",
        })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", -Duration::minutes(5)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = manager(dir.path(), &server);

    async {
        make_read_only(dir.path());
        for _ in 0..3 {
            assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
        }
        make_writable(dir.path());
        let (access, _) = tokens.bearer().await.unwrap();
        assert_eq!(access.expose_secret(), "short-access");
    }
    .instrument(tracing::info_span!("case", name = "save_streak"))
    .await;
    assert!(refresh_calls(&server).await >= 3);

    let output = logs.contents();
    assert_eq!(
        logged(&output, "save_streak", "state_save_failed"),
        1,
        "{output}"
    );
    assert_eq!(
        logged(&output, "save_streak", "state_save_recovered"),
        1,
        "{output}"
    );
}

#[tokio::test]
async fn unauthorized_adopts_a_new_seed_instead_of_refreshing() {
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens())
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", Duration::hours(48)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = manager(dir.path(), &server);
    let (used, _) = tokens.bearer().await.unwrap();

    // A request with the old token fails after a seed landed.
    file.save(
        &state("seeded-access", "seeded-refresh", Duration::hours(48)),
        LOCK_WAIT,
    )
    .unwrap();
    assert!(tokens.unauthorized(&used).await);

    assert_eq!(refresh_calls(&server).await, 0);
    let (access, _) = tokens.bearer().await.unwrap();
    assert_eq!(access.expose_secret(), "seeded-access");
    let on_disk = file.load().unwrap().unwrap();
    assert_eq!(
        on_disk.tokens.refresh_token.expose_secret(),
        "seeded-refresh"
    );
}

#[tokio::test]
async fn unauthorized_within_the_cooldown_still_adopts_a_new_seed() {
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens())
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", Duration::hours(48)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = manager(dir.path(), &server);
    let (used, _) = tokens.bearer().await.unwrap();
    assert!(tokens.unauthorized(&used).await);

    // The refreshed token is rejected too, so no refresh is allowed now; the
    // owner seeds again, and the failed request may retry with that seed.
    file.save(
        &state("seeded-access", "seeded-refresh", Duration::hours(48)),
        LOCK_WAIT,
    )
    .unwrap();
    assert!(tokens.unauthorized(&SecretString::from("new-access")).await);
    assert_eq!(refresh_calls(&server).await, 1);
    let (access, _) = tokens.bearer().await.unwrap();
    assert_eq!(access.expose_secret(), "seeded-access");
}

#[test]
fn state_lock_excludes_a_second_holder_until_released() {
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    let held = file.lock(LOCK_WAIT).unwrap();

    let started = Instant::now();
    assert!(matches!(
        file.lock(StdDuration::from_millis(100)),
        Err(StoreError::Locked)
    ));
    assert!(started.elapsed() >= StdDuration::from_millis(100));
    assert!(matches!(
        file.save(
            &state("access", "refresh", Duration::hours(48)),
            StdDuration::from_millis(50)
        ),
        Err(StoreError::Locked)
    ));
    assert!(!file.path.exists());

    // Clones share the hold: it ends with the last one.
    let clone = held.clone();
    drop(held);
    assert!(matches!(
        file.lock(StdDuration::ZERO),
        Err(StoreError::Locked)
    ));
    drop(clone);
    file.lock(StdDuration::ZERO).unwrap();
}

#[test]
fn save_waits_for_the_lock_holder() {
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    let held = file.lock(LOCK_WAIT).unwrap();
    let releaser = thread::spawn(move || {
        thread::sleep(StdDuration::from_millis(200));
        drop(held);
    });

    let started = Instant::now();
    file.save(&state("access", "refresh", Duration::hours(48)), LOCK_WAIT)
        .unwrap();
    assert!(started.elapsed() >= StdDuration::from_millis(150));
    releaser.join().unwrap();
    assert_eq!(
        file.load()
            .unwrap()
            .unwrap()
            .tokens
            .refresh_token
            .expose_secret(),
        "refresh"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_uses_a_seed_that_landed_while_it_waited_for_the_lock() {
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens())
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", Duration::minutes(30)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = Arc::new(manager(dir.path(), &server));

    // `seed` holds the lock while it writes; the due refresh has to wait.
    let seeding = file.lock(LOCK_WAIT).unwrap();
    let refreshing = tokio::spawn({
        let tokens = Arc::clone(&tokens);
        async move { tokens.bearer().await }
    });
    tokio::time::sleep(StdDuration::from_millis(300)).await;
    assert!(
        !refreshing.is_finished(),
        "refresh did not wait for the lock"
    );
    seeding
        .save(&state(
            "seeded-access",
            "seeded-refresh",
            Duration::hours(48),
        ))
        .unwrap();
    drop(seeding);

    let (access, _) = refreshing.await.unwrap().unwrap();
    assert_eq!(access.expose_secret(), "seeded-access");
    assert_eq!(refresh_calls(&server).await, 0);
    let on_disk = file.load().unwrap().unwrap();
    assert_eq!(
        on_disk.tokens.refresh_token.expose_secret(),
        "seeded-refresh"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seed_during_a_refresh_lands_after_the_rotated_tokens() {
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens().set_delay(StdDuration::from_millis(500)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": common::USER,
            "username": "rider"
        })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let accounts = Accounts::new(dir.path().to_owned(), vec![account(ACCOUNT)]).unwrap();
    let file = account_state_file(dir.path(), ACCOUNT, &user());
    file.save(
        &state("old-access", "old-refresh", -Duration::minutes(5)),
        LOCK_WAIT,
    )
    .unwrap();
    let auth = AuthClient::new(common::http(), server_url(&server));
    let tokens = Arc::new(TokenManager::new(
        account(ACCOUNT),
        file.clone(),
        auth,
        MARGIN,
    ));

    let refreshing = tokio::spawn({
        let tokens = Arc::clone(&tokens);
        async move { tokens.bearer().await }
    });
    // The refresh request is in flight before seed starts.
    assert!(eventually(async || refresh_calls(&server).await == 1).await);
    let seeded = br#"{"access_token":"seeded-access","refresh_token":"seeded-refresh","expires_at":"2099-01-01T00:00:00Z"}"#;
    run_seed(
        &seeded[..],
        &accounts,
        &account(ACCOUNT),
        false,
        &common::http(),
        &server_url(&server),
        LOCK_WAIT,
    )
    .await
    .unwrap();

    let (access, _) = refreshing.await.unwrap().unwrap();
    assert_eq!(access.expose_secret(), "new-access");
    // The seed is the last write, and the relay switches to it.
    let on_disk = file.load().unwrap().unwrap();
    assert_eq!(
        on_disk.tokens.refresh_token.expose_secret(),
        "seeded-refresh"
    );
    let (access, _) = tokens.bearer().await.unwrap();
    assert_eq!(access.expose_secret(), "seeded-access");
}

#[tokio::test]
async fn lock_timeout_backs_off_from_the_end_of_the_wait() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens())
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", Duration::minutes(30)),
        LOCK_WAIT,
    )
    .unwrap();
    let clock = TestClock::new();
    let tokens = manager_waiting(
        dir.path(),
        server_url(&server),
        &clock,
        StdDuration::from_millis(300),
    );

    async {
        let held = file.lock(LOCK_WAIT).unwrap();
        // A minute passes while the refresh waits for the lock.
        let waiting = advance_later(
            &clock,
            StdDuration::from_millis(50),
            StdDuration::from_secs(60),
        );
        let (access, _) = tokens.bearer().await.unwrap();
        assert_eq!(access.expose_secret(), "old-access");
        waiting.join().unwrap();
        drop(held);

        // Still inside the backoff counted from the end of the wait.
        clock.advance(StdDuration::from_secs(1));
        let (access, _) = tokens.bearer().await.unwrap();
        assert_eq!(access.expose_secret(), "old-access");
        assert_eq!(refresh_calls(&server).await, 0);

        clock.advance(StdDuration::from_secs(5));
        let (access, _) = tokens.bearer().await.unwrap();
        assert_eq!(access.expose_secret(), "new-access");
    }
    .instrument(tracing::info_span!("case", name = "lock_timeout"))
    .await;
    assert_eq!(refresh_calls(&server).await, 1);

    let output = logs.contents();
    assert_eq!(
        logged(&output, "lock_timeout", "state_lock_failed"),
        1,
        "{output}"
    );
}

#[tokio::test]
async fn slow_refresh_failure_backs_off_from_when_it_failed() {
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(ResponseTemplate::new(503).set_delay(StdDuration::from_millis(300)))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens())
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    state_file(dir.path())
        .save(
            &state("old-access", "old-refresh", Duration::minutes(30)),
            LOCK_WAIT,
        )
        .unwrap();
    let clock = TestClock::new();
    let tokens = manager_at(dir.path(), server_url(&server), &clock);

    // A minute passes while the token endpoint takes its time to fail.
    let waiting = advance_later(
        &clock,
        StdDuration::from_millis(50),
        StdDuration::from_secs(60),
    );
    let (access, _) = tokens.bearer().await.unwrap();
    assert_eq!(access.expose_secret(), "old-access");
    waiting.join().unwrap();

    clock.advance(StdDuration::from_secs(1));
    tokens.bearer().await.unwrap();
    assert_eq!(refresh_calls(&server).await, 1);

    clock.advance(StdDuration::from_secs(5));
    let (access, _) = tokens.bearer().await.unwrap();
    assert_eq!(access.expose_secret(), "new-access");
    assert_eq!(refresh_calls(&server).await, 2);
}

#[tokio::test]
async fn new_seed_starts_a_fresh_save_failure_streak() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "short-access",
            "refresh_token": "short-refresh",
            "expires_in": 1800,
            "token_type": "Bearer",
        })))
        .mount(&server)
        .await;
    refresh_mock("short-refresh")
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": "invalid_grant",
        })))
        .mount(&server)
        .await;
    refresh_mock("seeded-refresh")
        .respond_with(rotated_tokens())
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", -Duration::minutes(5)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = manager(dir.path(), &server);

    async {
        // The refreshed tokens cannot be saved, then their refresh token dies.
        make_read_only(dir.path());
        assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
        assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
        make_writable(dir.path());

        // A new seed whose refreshed tokens cannot be saved either.
        file.save(
            &state("seeded-access", "seeded-refresh", -Duration::minutes(5)),
            LOCK_WAIT,
        )
        .unwrap();
        make_read_only(dir.path());
        assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
        make_writable(dir.path());
        let (access, _) = tokens.bearer().await.unwrap();
        assert_eq!(access.expose_secret(), "new-access");
    }
    .instrument(tracing::info_span!("case", name = "seed_streak"))
    .await;

    let output = logs.contents();
    assert_eq!(
        logged(&output, "seed_streak", "peloton_auth_failed"),
        1,
        "{output}"
    );
    assert_eq!(
        logged(&output, "seed_streak", "state_save_failed"),
        2,
        "{output}"
    );
    assert_eq!(
        logged(&output, "seed_streak", "state_save_recovered"),
        1,
        "{output}"
    );
}

#[tokio::test]
async fn new_seed_after_a_failed_streak_claims_no_recovery() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "short-access",
            "refresh_token": "short-refresh",
            "expires_in": 1800,
            "token_type": "Bearer",
        })))
        .mount(&server)
        .await;
    refresh_mock("short-refresh")
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": "invalid_grant",
        })))
        .mount(&server)
        .await;
    refresh_mock("seeded-refresh")
        .respond_with(rotated_tokens())
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", -Duration::minutes(5)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = manager(dir.path(), &server);

    async {
        make_read_only(dir.path());
        assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
        assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
        make_writable(dir.path());

        // The seed's refreshed tokens save at once: nothing recovered.
        file.save(
            &state("seeded-access", "seeded-refresh", -Duration::minutes(5)),
            LOCK_WAIT,
        )
        .unwrap();
        let (access, _) = tokens.bearer().await.unwrap();
        assert_eq!(access.expose_secret(), "new-access");
    }
    .instrument(tracing::info_span!("case", name = "seed_no_recovery"))
    .await;

    let output = logs.contents();
    assert_eq!(
        logged(&output, "seed_no_recovery", "state_save_failed"),
        1,
        "{output}"
    );
    assert_eq!(
        logged(&output, "seed_no_recovery", "state_save_recovered"),
        0,
        "{output}"
    );
}

/// Another Peloton user than [`user`].
const OTHER_USER: &str = "0000000000000000000000000000b0b2";

fn other_user() -> PelotonId {
    OTHER_USER.parse().unwrap()
}

/// [`state`] of `owner` instead of [`user`].
fn state_of(owner: &PelotonId, access: &str, refresh: &str, expires_in: Duration) -> State {
    State {
        user_id: owner.clone(),
        ..state(access, refresh, expires_in)
    }
}

/// A state file of [`user`] in `dir` with no binding (unlike [`state_file`]).
fn unbound_state_file(dir: &Path) -> StateFile {
    let file = StateFile {
        path: dir.join("state.json"),
    };
    file.save(
        &state("old-access", "old-refresh", Duration::hours(48)),
        LOCK_WAIT,
    )
    .unwrap();
    file
}

/// Calls `bearer()` three times within the case span `case`, expecting
/// `NotAuthenticated` each time and no token endpoint call, and checks that
/// exactly one `state_refused` with `reason` was logged for account alice.
async fn assert_refused(dir: &Path, case: &'static str, reason: &str) {
    let logs = captured_logs();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(rotated_tokens())
        .expect(0)
        .mount(&server)
        .await;
    // Not `manager`: its `state_file` would bind the account.
    let tokens = TokenManager::new(
        account(ACCOUNT),
        StateFile {
            path: dir.join("state.json"),
        },
        AuthClient::new(common::http(), server_url(&server)),
        MARGIN,
    );
    async {
        for _ in 0..3 {
            assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
            assert!(!tokens.is_ready());
        }
    }
    .instrument(tracing::info_span!("case", name = case))
    .await;

    let output = logs.contents();
    let refused: Vec<serde_json::Value> = output
        .lines()
        .filter(|line| line.contains(case) && line.contains("state_refused"))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(refused.len(), 1, "{output}");
    assert_eq!(refused[0]["event"], "state_refused");
    assert_eq!(refused[0]["level"], "ERROR");
    assert_eq!(refused[0]["account"], ACCOUNT);
    assert_eq!(refused[0]["reason"], reason);
    for secret in ["old-access", "old-refresh", USER, OTHER_USER] {
        assert!(!output.contains(secret), "log leaks {secret}: {output}");
    }
}

#[tokio::test]
async fn unbound_state_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    unbound_state_file(dir.path());
    assert_refused(dir.path(), "refused_unbound", "unbound").await;
}

#[tokio::test]
async fn mismatched_state_is_not_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let file = unbound_state_file(dir.path());
    file.lock(LOCK_WAIT).unwrap().bind(&other_user()).unwrap();
    assert_refused(dir.path(), "refused_mismatch", "user_mismatch").await;
}

#[tokio::test]
async fn corrupt_binding_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let file = unbound_state_file(dir.path());
    fs::write(file.binding_path(), "{").unwrap();
    assert_refused(dir.path(), "refused_corrupt", "binding_corrupt").await;
}

#[tokio::test]
async fn ready_manager_drops_a_replaced_mismatched_state() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", Duration::hours(48)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = manager(dir.path(), &server);

    async {
        tokens.bearer().await.unwrap();
        assert!(tokens.is_ready());
        let lock = file.lock(LOCK_WAIT).unwrap();
        lock.bind(&other_user()).unwrap();
        lock.save(&state("new-access", "new-refresh", Duration::hours(48)))
            .unwrap();
        drop(lock);
        // Fail closed: neither the new state nor the old credential is used.
        assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
        assert!(!tokens.is_ready());
        assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
    }
    .instrument(tracing::info_span!("case", name = "ready_mismatch"))
    .await;
    assert_eq!(refresh_calls(&server).await, 0);

    let output = logs.contents();
    assert_eq!(
        logged(&output, "ready_mismatch", "\"reason\":\"user_mismatch\""),
        1,
        "{output}"
    );
    assert_eq!(
        logged(&output, "ready_mismatch", "state_refused"),
        1,
        "{output}"
    );
}

#[tokio::test]
async fn rebound_state_is_adopted_and_audited() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", Duration::hours(48)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = manager(dir.path(), &server);

    async {
        tokens.bearer().await.unwrap();
        let lock = file.lock(LOCK_WAIT).unwrap();
        lock.bind(&other_user()).unwrap();
        lock.save(&state_of(
            &other_user(),
            "other-access",
            "other-refresh",
            Duration::hours(48),
        ))
        .unwrap();
        drop(lock);
        let (access, owner) = tokens.bearer().await.unwrap();
        assert_eq!(access.expose_secret(), "other-access");
        assert_eq!(owner, other_user());
    }
    .instrument(tracing::info_span!("case", name = "rebound"))
    .await;
    assert_eq!(refresh_calls(&server).await, 0);

    let output = logs.contents();
    let events: Vec<serde_json::Value> = output
        .lines()
        .filter(|line| line.contains("\"rebound\""))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let names: Vec<&str> = events
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["state_loaded", "account_rebound", "state_reloaded"],
        "{output}"
    );
    assert_eq!(events[0]["user"], "\u{2026}00f2");
    assert_eq!(events[1]["level"], "WARN");
    assert_eq!(events[1]["account"], ACCOUNT);
    assert_eq!(events[1]["previous_user"], "\u{2026}00f2");
    assert_eq!(events[1]["user"], "\u{2026}b0b2");
    assert_eq!(events[2]["account"], ACCOUNT);
    assert_eq!(events[2]["user"], "\u{2026}b0b2");
    for secret in [USER, OTHER_USER] {
        assert!(!output.contains(secret), "log leaks {secret}: {output}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusal_seen_mid_seed_is_rechecked_under_the_lock() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", Duration::hours(48)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = Arc::new(manager(dir.path(), &server));

    async {
        // A seed for another user, between its binding and its state write.
        let seeding = file.lock(LOCK_WAIT).unwrap();
        seeding.bind(&other_user()).unwrap();
        let reading = tokio::spawn(
            {
                let tokens = Arc::clone(&tokens);
                async move { tokens.bearer().await }
            }
            .in_current_span(),
        );
        tokio::time::sleep(StdDuration::from_millis(300)).await;
        assert!(
            !reading.is_finished(),
            "the refusal was not rechecked under the lock"
        );
        seeding
            .save(&state_of(
                &other_user(),
                "other-access",
                "other-refresh",
                Duration::hours(48),
            ))
            .unwrap();
        drop(seeding);

        let (access, owner) = reading.await.unwrap().unwrap();
        assert_eq!(access.expose_secret(), "other-access");
        assert_eq!(owner, other_user());
    }
    .instrument(tracing::info_span!("case", name = "mid_seed"))
    .await;

    let output = logs.contents();
    assert_eq!(logged(&output, "mid_seed", "state_refused"), 0, "{output}");
    assert_eq!(logged(&output, "mid_seed", "state_loaded"), 1, "{output}");
}

#[tokio::test]
async fn credential_events_carry_the_account() {
    const EVENTS: [&str; 6] = [
        "state_missing",
        "state_loaded",
        "peloton_refresh_retry",
        "peloton_token_refreshed",
        "peloton_auth_failed",
        "state_save_failed",
    ];
    let logs = captured_logs();
    let server = MockServer::start().await;
    refresh_mock("first-refresh")
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    refresh_mock("first-refresh")
        .respond_with(rotated_tokens())
        .mount(&server)
        .await;
    refresh_mock("rotated-refresh")
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": "invalid_grant",
        })))
        .mount(&server)
        .await;
    refresh_mock("seeded-refresh")
        .respond_with(third_tokens())
        .mount(&server)
        .await;
    let root = tempfile::tempdir().unwrap();
    let file = account_state_file(root.path(), "bob", &user());
    let dir = file.path.parent().unwrap().to_path_buf();
    let clock = TestClock::new();
    let tokens = TokenManager::with_timing(
        account("bob"),
        file.clone(),
        AuthClient::new(common::http(), server_url(&server)),
        MARGIN,
        clock.clock(),
        LOCK_WAIT,
    );
    assert_eq!(tokens.account().as_str(), "bob");

    async {
        assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
        file.save(
            &state("first-access", "first-refresh", Duration::minutes(30)),
            LOCK_WAIT,
        )
        .unwrap();
        // Loaded and due: the first refresh fails transiently.
        let (access, _) = tokens.bearer().await.unwrap();
        assert_eq!(access.expose_secret(), "first-access");
        clock.advance(StdDuration::from_secs(3600));
        let (access, _) = tokens.bearer().await.unwrap();
        assert_eq!(access.expose_secret(), "new-access");
        // A forced refresh finds the rotated refresh token rejected.
        assert!(!tokens.unauthorized(&access).await);
        // A new seed is due at once, but its rotated tokens cannot be saved.
        file.save(
            &state("seeded-access", "seeded-refresh", Duration::minutes(30)),
            LOCK_WAIT,
        )
        .unwrap();
        make_read_only(&dir);
        let withheld = tokens.bearer().await;
        make_writable(&dir);
        assert!(matches!(withheld, Err(NotAuthenticated)));
    }
    .instrument(tracing::info_span!("case", name = "bob_events"))
    .await;

    let output = logs.contents();
    for event in EVENTS {
        let lines: Vec<serde_json::Value> = output
            .lines()
            .filter(|line| line.contains("bob_events"))
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|json| json["event"] == event)
            .collect();
        assert!(!lines.is_empty(), "no {event}: {output}");
        for json in lines {
            assert_eq!(json["account"], "bob", "{json}");
        }
    }
}

/// A token manager of account `bob` in a fresh state directory under `root`,
/// bound to [`user`], with its own refresh token and the given lock wait.
fn bob_manager(
    root: &Path,
    server: &MockServer,
    clock: &TestClock,
    lock_wait: StdDuration,
) -> (TokenManager, StateFile) {
    let file = account_state_file(root, "bob", &user());
    let tokens = TokenManager::with_timing(
        account("bob"),
        file.clone(),
        AuthClient::new(common::http(), server_url(server)),
        MARGIN,
        clock.clock(),
        lock_wait,
    );
    (tokens, file)
}

#[tokio::test]
async fn state_and_refresh_problem_events_carry_the_account() {
    const EVENTS: [&str; 7] = [
        "state_corrupt",
        "state_file_unreadable",
        "state_save_failed",
        "state_external_change_ignored",
        "state_save_recovered",
        "state_lock_failed",
        "peloton_unauthorized_after_refresh",
    ];
    let logs = captured_logs();
    let server = MockServer::start().await;
    refresh_mock("unsaved-refresh")
        .respond_with(rotated_tokens())
        .mount(&server)
        .await;
    refresh_mock("forced-refresh")
        .respond_with(third_tokens())
        .mount(&server)
        .await;
    let clock = TestClock::new();

    async {
        // A corrupt state file.
        let root = tempfile::tempdir().unwrap();
        let (tokens, file) = bob_manager(root.path(), &server, &clock, LOCK_WAIT);
        fs::write(&file.path, "{").unwrap();
        assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));

        // An account directory that cannot be searched (not as root).
        let root = tempfile::tempdir().unwrap();
        let (tokens, file) = bob_manager(root.path(), &server, &clock, LOCK_WAIT);
        let dir = file.path.parent().unwrap().to_path_buf();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
        let blocked = fs::metadata(&file.path).is_err();
        let refused = tokens.bearer().await;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(blocked, "tests must not run as root");
        assert!(matches!(refused, Err(NotAuthenticated)));

        // Refreshed tokens that cannot be saved, a seed landing meanwhile,
        // then a save that succeeds.
        let root = tempfile::tempdir().unwrap();
        let (tokens, file) = bob_manager(root.path(), &server, &clock, LOCK_WAIT);
        let dir = file.path.parent().unwrap().to_path_buf();
        file.save(
            &state("unsaved-access", "unsaved-refresh", -Duration::minutes(5)),
            LOCK_WAIT,
        )
        .unwrap();
        make_read_only(&dir);
        let withheld = tokens.bearer().await;
        make_writable(&dir);
        assert!(matches!(withheld, Err(NotAuthenticated)));
        file.save(
            &state("seeded-access", "seeded-refresh", Duration::hours(48)),
            LOCK_WAIT,
        )
        .unwrap();
        let (access, _) = tokens.bearer().await.unwrap();
        assert_eq!(access.expose_secret(), "new-access");

        // The state lock stays held while a refresh is due.
        let root = tempfile::tempdir().unwrap();
        let (tokens, file) =
            bob_manager(root.path(), &server, &clock, StdDuration::from_millis(100));
        file.save(
            &state("held-access", "held-refresh", Duration::minutes(30)),
            LOCK_WAIT,
        )
        .unwrap();
        let held = file.lock(LOCK_WAIT).unwrap();
        let (access, _) = tokens.bearer().await.unwrap();
        drop(held);
        assert_eq!(access.expose_secret(), "held-access");

        // Peloton rejects the token a forced refresh just minted.
        let root = tempfile::tempdir().unwrap();
        let (tokens, file) = bob_manager(root.path(), &server, &clock, LOCK_WAIT);
        file.save(
            &state("forced-access", "forced-refresh", Duration::hours(48)),
            LOCK_WAIT,
        )
        .unwrap();
        let (used, _) = tokens.bearer().await.unwrap();
        assert!(tokens.unauthorized(&used).await);
        assert!(
            !tokens
                .unauthorized(&SecretString::from("third-access"))
                .await
        );
    }
    .instrument(tracing::info_span!("case", name = "bob_problem_events"))
    .await;

    let output = logs.contents();
    for event in EVENTS {
        let lines: Vec<serde_json::Value> = output
            .lines()
            .filter(|line| line.contains("bob_problem_events"))
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|json| json["event"] == event)
            .collect();
        assert!(!lines.is_empty(), "no {event}: {output}");
        for json in lines {
            assert_eq!(json["account"], "bob", "{json}");
        }
    }
}

/// Lines of the case `case` that log `state_refused` with `reason` for alice.
fn refusals(output: &str, case: &str, reason: &str) -> usize {
    output
        .lines()
        .filter(|line| line.contains(case) && line.contains("state_refused"))
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|json| json["account"] == ACCOUNT && json["reason"] == reason)
        .count()
}

#[tokio::test]
async fn pending_save_does_not_overwrite_a_rebound_account() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens())
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", Duration::minutes(30)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = manager(dir.path(), &server);

    async {
        // The refreshed tokens of the old user cannot be saved yet.
        make_read_only(dir.path());
        let withheld = tokens.bearer().await;
        make_writable(dir.path());
        assert!(matches!(withheld, Err(NotAuthenticated)));

        // A seed rebinds the account to another user and saves their state.
        let seeding = file.lock(LOCK_WAIT).unwrap();
        seeding.bind(&other_user()).unwrap();
        seeding
            .save(&state_of(
                &other_user(),
                "other-access",
                "other-refresh",
                Duration::hours(48),
            ))
            .unwrap();
        drop(seeding);

        // Saving works again, but the old user's tokens are dropped, not
        // written; the rebound account's credential is used.
        for _ in 0..2 {
            let (access, owner) = tokens.bearer().await.unwrap();
            assert_eq!(access.expose_secret(), "other-access");
            assert_eq!(owner, other_user());
            let on_disk = file.load().unwrap().unwrap();
            assert_eq!(on_disk.user_id, other_user());
            assert_eq!(
                on_disk.tokens.refresh_token.expose_secret(),
                "other-refresh"
            );
        }
    }
    .instrument(tracing::info_span!("case", name = "pending_rebound"))
    .await;

    let output = logs.contents();
    assert_eq!(
        refusals(&output, "pending_rebound", "user_mismatch"),
        1,
        "{output}"
    );
    assert_eq!(
        logged(&output, "pending_rebound", "account_rebound"),
        1,
        "{output}"
    );
    assert!(
        !fs::read_to_string(&file.path)
            .unwrap()
            .contains("rotated-refresh")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_refuses_to_save_under_another_users_binding() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    refresh_mock("old-refresh")
        .respond_with(rotated_tokens().set_delay(StdDuration::from_millis(500)))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", Duration::minutes(30)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = Arc::new(manager(dir.path(), &server));

    async {
        let refreshing = tokio::spawn(
            {
                let tokens = Arc::clone(&tokens);
                async move { tokens.bearer().await }
            }
            .in_current_span(),
        );
        // While the token request is in flight, a writer that ignores the
        // state lock binds the account to another user.
        tokio::time::sleep(StdDuration::from_millis(200)).await;
        assert!(!refreshing.is_finished());
        fs::write(
            file.binding_path(),
            serde_json::json!({ "user_id": OTHER_USER }).to_string(),
        )
        .unwrap();

        assert!(matches!(refreshing.await.unwrap(), Err(NotAuthenticated)));
        assert!(!tokens.is_ready());
        assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
    }
    .instrument(tracing::info_span!("case", name = "refresh_rebound"))
    .await;

    let on_disk = file.load().unwrap().unwrap();
    assert_eq!(on_disk.tokens.refresh_token.expose_secret(), "old-refresh");
    let output = logs.contents();
    assert_eq!(
        refusals(&output, "refresh_rebound", "user_mismatch"),
        1,
        "{output}"
    );
}

#[tokio::test]
async fn binding_only_change_is_noticed() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    file.save(
        &state("old-access", "old-refresh", Duration::hours(48)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = manager(dir.path(), &server);

    async {
        tokens.bearer().await.unwrap();
        // Only the binding changes: the state file is the version already loaded.
        file.lock(LOCK_WAIT).unwrap().bind(&other_user()).unwrap();
        assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
        assert!(!tokens.is_ready());

        // Bound back to the user of the state: it is used again.
        file.lock(LOCK_WAIT).unwrap().bind(&user()).unwrap();
        let (access, _) = tokens.bearer().await.unwrap();
        assert_eq!(access.expose_secret(), "old-access");

        // A removed binding is noticed too.
        fs::remove_file(file.binding_path()).unwrap();
        assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
    }
    .instrument(tracing::info_span!("case", name = "binding_only"))
    .await;
    assert_eq!(refresh_calls(&server).await, 0);

    let output = logs.contents();
    assert_eq!(
        refusals(&output, "binding_only", "user_mismatch"),
        1,
        "{output}"
    );
    assert_eq!(refusals(&output, "binding_only", "unbound"), 1, "{output}");
}

/// A manager ready on [`user`]'s credential, and its state file, in `dir`.
async fn ready_manager(dir: &Path, server: &MockServer) -> (TokenManager, StateFile) {
    let file = state_file(dir);
    file.save(
        &state("old-access", "old-refresh", Duration::hours(48)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = manager(dir, server);
    tokens.bearer().await.unwrap();
    assert!(tokens.is_ready());
    (tokens, file)
}

#[tokio::test]
async fn rebinding_while_the_state_file_is_missing_is_refused() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (tokens, file) = ready_manager(dir.path(), &server).await;

    async {
        fs::remove_file(&file.path).unwrap();
        file.lock(LOCK_WAIT).unwrap().bind(&other_user()).unwrap();
        for _ in 0..2 {
            assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
            assert!(!tokens.is_ready());
        }
    }
    .instrument(tracing::info_span!("case", name = "missing_rebound"))
    .await;
    assert_eq!(refresh_calls(&server).await, 0);

    let output = logs.contents();
    assert_eq!(
        refusals(&output, "missing_rebound", "user_mismatch"),
        1,
        "{output}"
    );
}

#[tokio::test]
async fn unbinding_while_the_state_file_is_missing_is_refused() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (tokens, file) = ready_manager(dir.path(), &server).await;

    async {
        fs::remove_file(&file.path).unwrap();
        fs::remove_file(file.binding_path()).unwrap();
        for _ in 0..2 {
            assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
        }
    }
    .instrument(tracing::info_span!("case", name = "missing_unbound"))
    .await;

    let output = logs.contents();
    assert_eq!(
        refusals(&output, "missing_unbound", "unbound"),
        1,
        "{output}"
    );
}

#[tokio::test]
async fn same_user_rebind_while_the_state_file_is_missing_keeps_the_credential() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (tokens, file) = ready_manager(dir.path(), &server).await;

    async {
        fs::remove_file(&file.path).unwrap();
        file.lock(LOCK_WAIT).unwrap().bind(&user()).unwrap();
        for _ in 0..2 {
            let (access, _) = tokens.bearer().await.unwrap();
            assert_eq!(access.expose_secret(), "old-access");
        }
    }
    .instrument(tracing::info_span!("case", name = "missing_same_user"))
    .await;

    let output = logs.contents();
    assert_eq!(
        logged(&output, "missing_same_user", "state_refused"),
        0,
        "{output}"
    );
}

#[tokio::test]
async fn binding_that_cannot_be_checked_fails_closed() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    let root = tempfile::tempdir().unwrap();
    let file = account_state_file(root.path(), ACCOUNT, &user());
    let dir = file.path.parent().unwrap().to_path_buf();
    file.save(
        &state("old-access", "old-refresh", Duration::hours(48)),
        LOCK_WAIT,
    )
    .unwrap();
    let tokens = TokenManager::new(
        account(ACCOUNT),
        file.clone(),
        AuthClient::new(common::http(), server_url(&server)),
        MARGIN,
    );
    tokens.bearer().await.unwrap();

    async {
        // Without search permission on the account directory, neither file
        // can even be stat'ed (this does not hold for root).
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
        let blocked = fs::metadata(file.binding_path()).is_err();
        let refused = tokens.bearer().await;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(blocked, "tests must not run as root");
        assert!(matches!(refused, Err(NotAuthenticated)));
        assert!(!tokens.is_ready());

        // Readable again: the bound credential is used again.
        let (access, _) = tokens.bearer().await.unwrap();
        assert_eq!(access.expose_secret(), "old-access");
    }
    .instrument(tracing::info_span!("case", name = "binding_unreadable"))
    .await;

    let output = logs.contents();
    let unreadable: Vec<serde_json::Value> = output
        .lines()
        .filter(|line| {
            line.contains("binding_unreadable") && line.contains("state_file_unreadable")
        })
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(unreadable.len(), 1, "{output}");
    assert_eq!(unreadable[0]["account"], ACCOUNT);
}

#[test]
fn temp_cleanup_failures_name_the_account_directory() {
    let logs = captured_logs();
    let root = tempfile::tempdir().unwrap();
    let file = account_state_file(root.path(), "bob", &user());
    // A stale directory with a temp file's name: removing it as a file fails.
    let stuck = root.path().join("bob").join(".state.json.123.0.tmp");
    fs::create_dir(&stuck).unwrap();
    fs::File::open(&stuck)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - StdDuration::from_secs(600))
        .unwrap();

    tracing::info_span!("case", name = "stuck_temp").in_scope(|| {
        file.save(&state("access", "refresh", Duration::hours(48)), LOCK_WAIT)
            .unwrap();
    });

    let output = logs.contents();
    let failures: Vec<serde_json::Value> = output
        .lines()
        .filter(|line| line.contains("stuck_temp") && line.contains("state_temp_cleanup_failed"))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(failures.len(), 1, "{output}");
    assert_eq!(failures[0]["directory"], "bob");
}
