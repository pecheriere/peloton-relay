//! Tests for the state file and the refreshing token manager.

use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use peloton::oauth::AuthClient;
use peloton_relay::store::StoreError;
use peloton_relay::tokens::{NotAuthenticated, TokenManager};
use secrecy::{ExposeSecret, SecretString};
use time::{Duration, OffsetDateTime};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;
use url::Url;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

use common::{captured_logs, state, state_file, user};

const MARGIN: StdDuration = StdDuration::from_secs(3600);

fn manager(dir: &Path, server: &MockServer) -> TokenManager {
    let auth = AuthClient::new(common::http(), Url::parse(&server.uri()).unwrap());
    TokenManager::new(state_file(dir), auth, MARGIN)
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

    file.save(&state("access", "refresh", Duration::hours(48)))
        .unwrap();

    let meta = fs::metadata(&file.path).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    // Replaced by rename, not rewritten in place.
    assert_ne!(meta.ino(), old_inode);
    let entries: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("state.json")]);

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

    file.save(&state("access", "refresh", Duration::hours(48)))
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
        .save(&state("access", "refresh", Duration::hours(48)))
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
        .save(&state(
            "seeded-access",
            "seeded-refresh",
            Duration::hours(48),
        ))
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
        .save(&state("old-access", "old-refresh", Duration::hours(2)))
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
        .save(&state("old-access", "old-refresh", Duration::minutes(30)))
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
    file.save(&state("old-access", "old-refresh", -Duration::minutes(5)))
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
        .save(&state("old-access", "old-refresh", -Duration::minutes(5)))
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
        .save(&state("old-access", "old-refresh", -Duration::minutes(5)))
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
        .save(&state("old-access", "old-refresh", Duration::minutes(30)))
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
    file.save(&state("old-access", "old-refresh", -Duration::minutes(5)))
        .unwrap();
    let tokens = manager(dir.path(), &server);
    assert!(tokens.bearer().await.is_err());

    // Re-seeding the same dead token does not resurrect it.
    file.save(&state("other-access", "old-refresh", Duration::hours(48)))
        .unwrap();
    assert!(tokens.bearer().await.is_err());
    assert!(!tokens.is_ready());

    file.save(&state(
        "seeded-access",
        "seeded-refresh",
        Duration::hours(48),
    ))
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
            .save(&state("old-access", "old-refresh", Duration::minutes(30)))
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
        .save(&state("old-access", "old-refresh", -Duration::minutes(5)))
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
        .save(&state("old-access", "old-refresh", Duration::hours(48)))
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
    file.save(&state("old-access", "old-refresh", -Duration::minutes(5)))
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
    file.save(&state("old-access", "old-refresh", Duration::hours(48)))
        .unwrap();
    let tokens = manager(dir.path(), &server);
    let (access, _) = tokens.bearer().await.unwrap();
    assert_eq!(access.expose_secret(), "old-access");

    // `seed` from another process replaces the file of a healthy relay.
    file.save(&state(
        "seeded-access",
        "seeded-refresh",
        Duration::hours(48),
    ))
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
    file.save(&state("old-access", "old-refresh", -Duration::minutes(5)))
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
    file.save(&state("old-access", "old-refresh", -Duration::minutes(5)))
        .unwrap();
    let tokens = manager(dir.path(), &server);

    make_read_only(dir.path());
    assert!(tokens.bearer().await.is_err());
    make_writable(dir.path());
    file.save(&state(
        "seeded-access",
        "seeded-refresh",
        Duration::hours(48),
    ))
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
    file.save(&state("old-access", "old-refresh", Duration::minutes(30)))
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
    file.save(&state("old-access", "old-refresh", Duration::hours(48)))
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
    file.save(&state(
        "seeded-access",
        "seeded-refresh",
        Duration::minutes(30),
    ))
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
    file.save(&state("old-access", "old-refresh", -Duration::minutes(5)))
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
        .save(&state("old-access", "old-refresh", -Duration::minutes(5)))
        .unwrap();
    let tokens = manager(dir.path(), &server);
    let abandoned = tokio::time::timeout(StdDuration::from_millis(50), tokens.bearer()).await;
    assert!(abandoned.is_err());
    assert!(!tokens.drain(StdDuration::from_millis(50)).await);
}
