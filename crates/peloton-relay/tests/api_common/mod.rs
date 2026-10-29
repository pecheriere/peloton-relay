//! Helpers shared by the API tests: the relay's router against a mock
//! Peloton, requests, and fixture answers.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt as _;
use peloton_relay::api::{AppState, Clock, router};
use peloton_relay::stack::StackService;
use peloton_relay::tokens::TokenManager;
use peloton_relay::upstream::Peloton;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt as _;
use url::Url;

use crate::common::{ACCOUNT, LOCK_WAIT, account, http, state, state_file};

/// The access token the relay under test holds.
pub const ACCESS: &str = "access-secret-1";
/// The refresh token the relay under test holds.
pub const REFRESH: &str = "refresh-secret-1";

/// A scrubbed fixture of the `peloton` crate.
pub fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../peloton/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// The relay's router with every upstream on `server` (REST under `/api`,
/// GraphQL at `/graphql`) and its clock stopped at `now`; `seeded` gives it
/// the tokens [`ACCESS`] and [`REFRESH`].
pub fn relay(
    dir: &tempfile::TempDir,
    server: &wiremock::MockServer,
    seeded: bool,
    now: OffsetDateTime,
) -> Router {
    if seeded {
        state_file(dir.path())
            .save(&state(ACCESS, REFRESH, Duration::hours(48)), LOCK_WAIT)
            .unwrap();
    }
    let auth = peloton::oauth::AuthClient::new(http(), Url::parse(&server.uri()).unwrap());
    let tokens = Arc::new(TokenManager::new(
        account(ACCOUNT),
        state_file(dir.path()),
        auth,
        StdDuration::from_secs(3600),
    ));
    let api = Url::parse(&format!("{}/api", server.uri())).unwrap();
    let graphql = Url::parse(&format!("{}/graphql", server.uri())).unwrap();
    let peloton =
        Arc::new(Peloton::new(http(), api, Arc::clone(&tokens), 4).with_graphql_url(graphql));
    router(AppState {
        stack: Arc::new(StackService::new(Arc::clone(&peloton))),
        peloton,
        tokens,
        clock: Clock::fixed(now),
    })
}

/// Sends `request` to `app`: status, headers and body text.
pub async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

/// A request with a JSON body.
pub fn with_json(method: &str, uri: &str, body: &serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// A request without a body.
pub fn bare(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

/// Asserts `body` is an error body with code `code` and some message.
pub fn assert_error(body: &str, code: &str) {
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(value["error"], code, "{body}");
    assert!(value["message"].as_str().is_some_and(|m| !m.is_empty()));
}

/// A scrubbed fixture of the `peloton` crate, as JSON.
pub fn fixture_json(name: &str) -> serde_json::Value {
    serde_json::from_str(&fixture(name)).unwrap()
}

/// Answers GraphQL operation `op` on `server` with `body`, expecting
/// `calls` requests when given.
pub async fn mount_op(
    server: &wiremock::MockServer,
    op: &peloton::graphql::Op,
    body: &serde_json::Value,
    calls: Option<u64>,
) {
    use wiremock::matchers::{method, path, query_param};

    let mock = wiremock::Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param(op.kind.as_str(), op.name))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body));
    match calls {
        Some(calls) => mock.expect(calls),
        None => mock,
    }
    .mount(server)
    .await;
}

/// A `ViewUserStack` answer holding the one class of `stack_add.json`.
pub fn stack_with_one_class() -> serde_json::Value {
    let added = fixture_json("stack_add.json");
    serde_json::json!({ "data": { "viewUserStack": added["data"]["addClassToStack"] } })
}

/// The scheduled-class id of `schedule_add.json` and its relay form.
pub fn fixture_scheduled_id() -> (peloton::ids::ScheduledClassId, String) {
    let id: peloton::ids::ScheduledClassId =
        fixture_json("schedule_add.json")["data"]["addClassToSchedule"]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
    let url_safe = id.url_safe().to_owned();
    (id, url_safe)
}

/// Answers every library and GraphQL call of the relay with its fixture.
pub async fn mount_every_new_upstream(server: &wiremock::MockServer) {
    use peloton::graphql::ops;
    use wiremock::matchers::{method, path};

    for (route, name) in [
        ("/api/v2/ride/archived", "library_archived.json"),
        ("/api/ride/filters", "ride_filters.json"),
        ("/api/browse_categories", "browse_categories.json"),
    ] {
        wiremock::Mock::given(method("GET"))
            .and(path(route))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(fixture_json(name)))
            .mount(server)
            .await;
    }
    for (op, body) in [
        (
            ops::YOUR_SCHEDULE_ITEMS,
            fixture_json("schedule_during.json"),
        ),
        (ops::SCHEDULED_CLASS, fixture_json("scheduled_class.json")),
        (
            ops::ADD_CLASS_TO_SCHEDULE,
            fixture_json("schedule_add.json"),
        ),
        (
            ops::RESCHEDULE_CLASS,
            fixture_json("schedule_reschedule.json"),
        ),
        (
            ops::REMOVE_CLASS_FROM_SCHEDULE,
            fixture_json("schedule_remove.json"),
        ),
        (ops::VIEW_USER_STACK, stack_with_one_class()),
        (ops::ADD_CLASS_TO_STACK, fixture_json("stack_add.json")),
        (ops::MODIFY_STACK, fixture_json("stack_modify.json")),
        (
            ops::CHECK_CLASS_IN_STACK,
            fixture_json("check_class_in_stack.json"),
        ),
    ] {
        mount_op(server, &op, &body, None).await;
    }
}
