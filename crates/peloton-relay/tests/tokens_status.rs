//! Tests of what a token manager reports about its credential, and of
//! revoking the credential.

use peloton_relay_test_support::common;
use std::fs;
use std::path::Path;
use std::time::Duration as StdDuration;

use peloton::oauth::AuthClient;
use peloton_relay::account::Accounts;
use peloton_relay::store::StoreError;
use peloton_relay::tokens::{
    Health, IncidentKind, MAX_INCIDENTS, MonotonicClock, NotAuthenticated, TokenManager,
};
use secrecy::ExposeSecret as _;
use time::{Duration, OffsetDateTime};
use url::Url;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use peloton_relay_test_support::common::{
    ACCOUNT, LOCK_WAIT, account, account_state_file, state, state_file, user,
};

const MARGIN: StdDuration = StdDuration::from_secs(3600);

fn manager(dir: &Path, server: &MockServer) -> TokenManager {
    manager_waiting(dir, server, LOCK_WAIT)
}

fn manager_waiting(dir: &Path, server: &MockServer, lock_wait: StdDuration) -> TokenManager {
    let auth = AuthClient::new(common::http(), Url::parse(&server.uri()).unwrap());
    TokenManager::with_timing(
        account(ACCOUNT),
        state_file(dir),
        Vec::new(),
        auth,
        MARGIN,
        MonotonicClock::system(),
        lock_wait,
    )
}

async fn refresh_answers(server: &MockServer, answer: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(answer)
        .mount(server)
        .await;
}

fn seed(dir: &Path, access: &str, refresh: &str, expires_in: Duration) {
    state_file(dir)
        .save(&state(access, refresh, expires_in), LOCK_WAIT)
        .unwrap();
}

#[tokio::test]
async fn a_new_manager_needs_sign_in_until_it_reads_a_credential() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let tokens = manager(dir.path(), &server);
    let status = tokens.status();
    assert_eq!(status.health, Health::NeedsSignIn);
    assert_eq!(status.last_refresh, None);
    assert_eq!(status.expires_at, None);
    assert_eq!(status.incidents, []);

    assert!(tokens.bearer().await.is_err());
    assert_eq!(tokens.status().health, Health::NeedsSignIn);
    // A missing state file is the normal state before a seed, not a problem.
    assert_eq!(tokens.status().incidents, []);
}

#[tokio::test]
async fn a_loaded_credential_is_healthy_with_its_expiry() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "access", "refresh", Duration::hours(48));
    let tokens = manager(dir.path(), &server);
    tokens.bearer().await.unwrap();
    let status = tokens.status();
    assert_eq!(status.health, Health::Ok);
    let expires_at = status.expires_at.unwrap();
    assert!(expires_at > OffsetDateTime::now_utc() + Duration::hours(47));
    // Loaded, not refreshed.
    assert_eq!(status.last_refresh, None);
}

#[tokio::test]
async fn a_refresh_is_recorded() {
    let server = MockServer::start().await;
    refresh_answers(
        &server,
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "new-access",
            "refresh_token": "rotated-refresh",
            "expires_in": 172_800,
            "token_type": "Bearer",
        })),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    seed(
        dir.path(),
        "old-access",
        "old-refresh",
        Duration::minutes(5),
    );
    let tokens = manager(dir.path(), &server);
    let before = OffsetDateTime::now_utc();
    tokens.bearer().await.unwrap();
    let status = tokens.status();
    assert_eq!(status.health, Health::Ok);
    let refreshed = status.last_refresh.unwrap();
    assert!(refreshed >= before && refreshed <= OffsetDateTime::now_utc());
}

#[tokio::test]
async fn a_rejected_refresh_token_needs_sign_in_and_is_an_incident() {
    let server = MockServer::start().await;
    refresh_answers(
        &server,
        ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": "invalid_grant",
            "error_description": "Unknown or invalid refresh token.",
        })),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    seed(
        dir.path(),
        "old-access",
        "old-refresh",
        -Duration::minutes(5),
    );
    let tokens = manager(dir.path(), &server);
    assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
    let status = tokens.status();
    assert_eq!(status.health, Health::NeedsSignIn);
    assert_eq!(status.expires_at, None);
    let kinds: Vec<_> = status.incidents.iter().map(|i| i.kind).collect();
    assert_eq!(kinds, [IncidentKind::RefreshRejected]);
    let message = IncidentKind::RefreshRejected.message();
    assert!(!message.contains("old-refresh") && !message.is_empty());
}

#[tokio::test]
async fn a_corrupt_state_file_is_unreadable() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let file = state_file(dir.path());
    fs::write(&file.path, "{").unwrap();
    let tokens = manager(dir.path(), &server);
    assert!(tokens.bearer().await.is_err());
    let status = tokens.status();
    assert_eq!(status.health, Health::Unreadable);
    let kinds: Vec<_> = status.incidents.iter().map(|i| i.kind).collect();
    assert_eq!(kinds, [IncidentKind::StateCorrupt]);
}

#[tokio::test]
async fn incidents_are_bounded_and_newest_first() {
    let server = MockServer::start().await;
    refresh_answers(&server, ResponseTemplate::new(503)).await;
    let dir = tempfile::tempdir().unwrap();
    seed(
        dir.path(),
        "old-access",
        "old-refresh",
        Duration::minutes(5),
    );
    let file = state_file(dir.path());
    // A failed refresh, then one corrupt state file per round: more
    // incidents than the manager keeps.
    let tokens = manager(dir.path(), &server);
    tokens.bearer().await.unwrap();
    for round in 0..MAX_INCIDENTS {
        tokens.revoke().await.unwrap();
        fs::write(&file.path, "{").unwrap();
        let _ = tokens.bearer().await;
        seed(
            dir.path(),
            &format!("access-{round}"),
            "old-refresh",
            Duration::hours(48),
        );
        tokens.bearer().await.unwrap();
    }
    let status = tokens.status();
    assert_eq!(status.incidents.len(), MAX_INCIDENTS);
    assert!(
        status
            .incidents
            .windows(2)
            .all(|pair| pair[0].at >= pair[1].at)
    );
}

#[tokio::test]
async fn revoke_deletes_the_credential_and_keeps_the_binding() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "access", "refresh", Duration::hours(48));
    let file = state_file(dir.path());
    let tokens = manager(dir.path(), &server);
    let (access, _) = tokens.bearer().await.unwrap();
    assert_eq!(access.expose_secret(), "access");

    assert!(tokens.revoke().await.unwrap());

    assert!(!file.path.exists());
    assert!(file.read_binding().unwrap().is_some());
    assert!(!tokens.is_ready());
    assert_eq!(tokens.status().health, Health::NeedsSignIn);
    assert!(matches!(tokens.bearer().await, Err(NotAuthenticated)));
    // Nothing left to delete.
    assert!(!tokens.revoke().await.unwrap());
}

#[tokio::test]
async fn revoke_of_an_account_without_a_directory_deletes_nothing() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let tokens = manager(&dir.path().join("absent"), &server);
    assert!(!tokens.revoke().await.unwrap());
    assert_eq!(tokens.status().health, Health::NeedsSignIn);
}

#[tokio::test]
async fn revoke_waits_for_the_state_lock() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "access", "refresh", Duration::hours(48));
    let file = state_file(dir.path());
    let tokens = manager_waiting(dir.path(), &server, StdDuration::from_millis(200));
    tokens.bearer().await.unwrap();

    let held = file.lock(LOCK_WAIT).unwrap();
    assert!(matches!(tokens.revoke().await, Err(StoreError::Locked)));
    // Not revoked: the credential is still on disk and in use.
    assert!(file.path.exists());
    assert!(tokens.bearer().await.is_ok());
    drop(held);

    assert!(tokens.revoke().await.unwrap());
    assert!(!file.path.exists());
}

/// alice holding a state of `user()`, with bob as her one peer; returns
/// alice's manager.
fn alice_with_bob(root: &Path, server: &MockServer) -> TokenManager {
    let accounts = Accounts::new(root.to_owned(), vec![account("alice"), account("bob")]).unwrap();
    let file = account_state_file(root, "alice", &user());
    file.save(&state("access", "refresh", Duration::hours(48)), LOCK_WAIT)
        .unwrap();
    let auth = AuthClient::new(common::http(), Url::parse(&server.uri()).unwrap());
    TokenManager::new(
        account("alice"),
        file,
        accounts.peers_of(&account("alice")),
        auth,
        MARGIN,
    )
}

#[tokio::test]
async fn a_user_bound_to_another_account_needs_sign_in() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _bob = account_state_file(dir.path(), "bob", &user());
    let tokens = alice_with_bob(dir.path(), &server);

    assert!(tokens.bearer().await.is_err());

    let status = tokens.status();
    assert_eq!(status.health, Health::NeedsSignIn);
    let kinds: Vec<_> = status.incidents.iter().map(|i| i.kind).collect();
    assert_eq!(kinds, [IncidentKind::BoundElsewhere]);
}

#[tokio::test]
async fn an_unreadable_peer_binding_is_unreadable() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let bob = account_state_file(
        dir.path(),
        "bob",
        &"000000000000000000000000000000b2".parse().unwrap(),
    );
    fs::write(bob.binding_path(), "{").unwrap();
    let tokens = alice_with_bob(dir.path(), &server);

    assert!(tokens.bearer().await.is_err());

    let status = tokens.status();
    assert_eq!(status.health, Health::Unreadable);
    let kinds: Vec<_> = status.incidents.iter().map(|i| i.kind).collect();
    assert_eq!(kinds, [IncidentKind::PeerBindingUnreadable]);
}
