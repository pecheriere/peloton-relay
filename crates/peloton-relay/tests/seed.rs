//! Tests for `peloton-relay seed`.

#[allow(
    dead_code,
    reason = "shared by several test crates, each using a subset of the helpers"
)]
mod common;

use std::time::Duration;

use peloton_relay::seed::{SeedError, run_seed};
use secrecy::ExposeSecret as _;
use url::Url;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKENS: &str = r#"{"access_token":"access-secret","refresh_token":"refresh-secret","expires_at":"2030-01-01T00:00:00Z"}"#;

fn base(server: &MockServer) -> Url {
    server.uri().parse().unwrap()
}

#[tokio::test]
async fn seed_writes_state_with_user_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .and(header("authorization", "Bearer access-secret"))
        .and(header("peloton-platform", "web"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": common::USER,
            "username": "rider"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let store = common::state_file(dir.path());

    let username = run_seed(
        TOKENS.as_bytes(),
        &store,
        &common::http(),
        &base(&server),
        common::LOCK_WAIT,
    )
    .await
    .unwrap();

    assert_eq!(username, "rider");
    let state = store.load().unwrap().unwrap();
    assert_eq!(state.user_id, common::user());
    assert_eq!(state.tokens.access_token.expose_secret(), "access-secret");
    assert_eq!(state.tokens.refresh_token.expose_secret(), "refresh-secret");
}

#[tokio::test]
async fn seed_rejects_invalid_json() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store = common::state_file(dir.path());

    let err = run_seed(
        &b"{\"access_token\": \"leaky-secret\""[..],
        &store,
        &common::http(),
        &base(&server),
        common::LOCK_WAIT,
    )
    .await
    .unwrap_err();

    assert!(matches!(err, SeedError::InvalidInput));
    assert!(!format!("{err} {err:?}").contains("leaky-secret"));
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(!store.path.exists());
}

#[tokio::test]
async fn seed_rejects_oversized_input() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store = common::state_file(dir.path());
    let input = vec![b' '; 128 * 1024];

    let err = run_seed(
        &input[..],
        &store,
        &common::http(),
        &base(&server),
        common::LOCK_WAIT,
    )
    .await
    .unwrap_err();

    assert!(matches!(err, SeedError::InvalidInput));
    assert!(!store.path.exists());
}

#[tokio::test]
async fn seed_rejects_token_peloton_refuses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .respond_with(ResponseTemplate::new(401).set_body_string("body-secret"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let store = common::state_file(dir.path());

    let err = run_seed(
        TOKENS.as_bytes(),
        &store,
        &common::http(),
        &base(&server),
        common::LOCK_WAIT,
    )
    .await
    .unwrap_err();

    assert!(matches!(err, SeedError::Rejected { status: 401 }));
    let shown = format!("{err} {err:?}");
    assert!(!shown.contains("body-secret") && !shown.contains("access-secret"));
    assert!(!store.path.exists());
}

#[tokio::test]
async fn seed_reports_other_statuses_neutrally() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let store = common::state_file(dir.path());

    let err = run_seed(
        TOKENS.as_bytes(),
        &store,
        &common::http(),
        &base(&server),
        common::LOCK_WAIT,
    )
    .await
    .unwrap_err();

    assert!(matches!(err, SeedError::Unexpected { status: 503 }));
    assert_eq!(err.to_string(), "Peloton answered with status 503");
    assert!(!store.path.exists());
}

fn me_mock() -> Mock {
    Mock::given(method("GET")).and(path("/me")).respond_with(
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": common::USER,
            "username": "rider"
        })),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seed_waits_for_the_state_lock_holder() {
    let server = MockServer::start().await;
    me_mock().expect(1).mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let store = common::state_file(dir.path());
    // The relay, mid-refresh, holds the lock through its own handle.
    let refreshing = store.lock(common::LOCK_WAIT).unwrap();

    let seeding = tokio::spawn({
        let store = store.clone();
        let base = base(&server);
        async move {
            run_seed(
                TOKENS.as_bytes(),
                &store,
                &common::http(),
                &base,
                common::LOCK_WAIT,
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!seeding.is_finished(), "seed did not wait for the lock");
    assert!(!store.path.exists());

    drop(refreshing);
    assert_eq!(seeding.await.unwrap().unwrap(), "rider");
    let state = store.load().unwrap().unwrap();
    assert_eq!(state.tokens.refresh_token.expose_secret(), "refresh-secret");
}

#[tokio::test]
async fn seed_gives_up_clearly_when_the_lock_stays_held() {
    let server = MockServer::start().await;
    me_mock().mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let store = common::state_file(dir.path());
    let _refreshing = store.lock(common::LOCK_WAIT).unwrap();

    let err = run_seed(
        TOKENS.as_bytes(),
        &store,
        &common::http(),
        &base(&server),
        Duration::from_millis(100),
    )
    .await
    .unwrap_err();

    assert!(matches!(err, SeedError::Busy), "{err:?}");
    assert_eq!(
        err.to_string(),
        "the relay kept the state file locked (a token refresh in flight); seed again"
    );
    assert!(!store.path.exists());
}
