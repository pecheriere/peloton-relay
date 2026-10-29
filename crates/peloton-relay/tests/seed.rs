//! Tests for `peloton-relay seed`.

#[allow(
    dead_code,
    reason = "shared by several test crates, each using a subset of the helpers"
)]
mod common;

use peloton::body::MAX_RESPONSE_BODY_BYTES;
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

    let username = run_seed(TOKENS.as_bytes(), &store, &common::http(), &base(&server))
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

    let err = run_seed(&input[..], &store, &common::http(), &base(&server))
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

    let err = run_seed(TOKENS.as_bytes(), &store, &common::http(), &base(&server))
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

    let err = run_seed(TOKENS.as_bytes(), &store, &common::http(), &base(&server))
        .await
        .unwrap_err();

    assert!(matches!(err, SeedError::Unexpected { status: 503 }));
    assert_eq!(err.to_string(), "Peloton answered with status 503");
    assert!(!store.path.exists());
}

/// A `/me` answer padded with trailing whitespace to `len` bytes.
fn me_of_len(len: usize) -> String {
    let me = format!(r#"{{"id":"{}","username":"rider"}}"#, common::USER);
    format!("{me}{}", " ".repeat(len - me.len()))
}

async fn me_answering(body: String) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn seed_reads_a_profile_at_the_size_limit() {
    let server = me_answering(me_of_len(MAX_RESPONSE_BODY_BYTES)).await;
    let dir = tempfile::tempdir().unwrap();
    let store = common::state_file(dir.path());

    let username = run_seed(TOKENS.as_bytes(), &store, &common::http(), &base(&server))
        .await
        .unwrap();

    assert_eq!(username, "rider");
}

#[tokio::test]
async fn seed_rejects_a_profile_over_the_size_limit() {
    let server = me_answering(me_of_len(MAX_RESPONSE_BODY_BYTES + 1)).await;
    let dir = tempfile::tempdir().unwrap();
    let store = common::state_file(dir.path());

    let err = run_seed(TOKENS.as_bytes(), &store, &common::http(), &base(&server))
        .await
        .unwrap_err();

    assert!(matches!(err, SeedError::Upstream));
    assert!(!store.path.exists());
}
