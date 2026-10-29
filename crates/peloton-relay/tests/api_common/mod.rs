//! Helpers shared by the API tests: the relay's router against a mock
//! Peloton, requests, and fixture answers.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt as _;
use peloton::ids::PelotonId;
use peloton_relay::account::AccountName;
use peloton_relay::api::{AccountContext, AccountRegistry, AppState, Clock, router};
use peloton_relay::store::StateFile;
use peloton_relay::tokens::TokenManager;
use peloton_relay::upstream::Peloton;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt as _;
use url::Url;

use crate::common::{ACCOUNT, LOCK_WAIT, account, account_state_file, http, state, state_file};

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

/// The account every helper request names.
pub const ACCOUNT_HEADER: &str = "x-peloton-account";

/// The sample ids of [`V1_ROUTES`], as literals so that `concat!` takes them.
macro_rules! sample_id {
    () => {
        "fedcba9876543210fedcba9876543210"
    };
}
macro_rules! sample_class {
    () => {
        "00000000000000000000000000000f1d"
    };
}
macro_rules! sample_scheduled {
    () => {
        "eyJob21lX3BlbG90b25faWQiOiAiMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDBmM2IiLCAicmlkZV9pZCI6ICIwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMGYxZCIsICJzY2hlZHVsZWRfc3RhcnRfdGltZSI6IDE4OTQwMTc2MDAsICJzdHVkaW9fcGVsb3Rvbl9pZCI6IG51bGwsICJ0eXBlIjogImV2ZW50In0"
    };
}

/// A workout or class id the `/v1` samples use for `{id}`.
pub const SAMPLE_ID: &str = sample_id!();
/// A class id the `/v1` samples use for `{class_id}`.
pub const SAMPLE_CLASS: &str = sample_class!();
/// The relay form of the scheduled-class id of `schedule_add.json`, which the
/// `/v1` samples use for `{scheduled_id}`.
pub const SAMPLE_SCHEDULED: &str = sample_scheduled!();

/// Every `/v1` route: its method and a path with the sample ids above.
pub const V1_ROUTES: &[(&str, &str)] = &[
    ("GET", "/v1/me"),
    ("GET", "/v1/workouts"),
    ("GET", concat!("/v1/workouts/", sample_id!())),
    ("GET", concat!("/v1/workouts/", sample_id!(), "/metrics")),
    ("GET", concat!("/v1/classes/", sample_id!())),
    ("GET", "/v1/library"),
    ("GET", "/v1/library/filters?category=cycling"),
    ("GET", "/v1/schedule"),
    ("POST", "/v1/schedule"),
    ("GET", concat!("/v1/schedule/", sample_scheduled!())),
    ("PATCH", concat!("/v1/schedule/", sample_scheduled!())),
    ("DELETE", concat!("/v1/schedule/", sample_scheduled!())),
    ("GET", "/v1/stack"),
    ("POST", "/v1/stack"),
    ("PUT", "/v1/stack"),
    ("DELETE", concat!("/v1/stack/", sample_class!())),
];

/// A body the write route `method` `uri` of [`V1_ROUTES`] accepts at a
/// relay clock before 2030-01-07; `None` for a route that takes none.
pub fn sample_body(method: &str, uri: &str) -> Option<serde_json::Value> {
    let start = "2030-01-07T12:00:00Z";
    match (method, uri) {
        ("POST", "/v1/schedule") => Some(serde_json::json!({
            "class_id": SAMPLE_CLASS, "start_time": start
        })),
        ("PATCH", _) => Some(serde_json::json!({ "start_time": start })),
        ("POST", "/v1/stack") => Some(serde_json::json!({ "class_id": SAMPLE_CLASS })),
        ("PUT", "/v1/stack") => Some(serde_json::json!({ "class_ids": [SAMPLE_CLASS] })),
        _ => None,
    }
}

/// The context of account `name` with every upstream on `server` (REST under
/// `/api`, GraphQL at `/graphql`) and at most `max_in_flight` upstream
/// requests at once. Its state lives under `root`, bound to `user`; `tokens`
/// (access, refresh) seeds it with a token set valid for 48 hours.
pub fn account_context(
    root: &std::path::Path,
    server: &wiremock::MockServer,
    name: &str,
    tokens: Option<(&str, &str)>,
    user: &PelotonId,
    max_in_flight: usize,
) -> AccountContext {
    let file = account_state_file(root, name, user);
    if let Some((access, refresh)) = tokens {
        let mut seeded = state(access, refresh, Duration::hours(48));
        seeded.user_id = user.clone();
        file.save(&seeded, LOCK_WAIT).unwrap();
    }
    context(server, account(name), file, max_in_flight)
}

fn context(
    server: &wiremock::MockServer,
    name: AccountName,
    file: StateFile,
    max_in_flight: usize,
) -> AccountContext {
    let auth = peloton::oauth::AuthClient::new(http(), Url::parse(&server.uri()).unwrap());
    let tokens = Arc::new(TokenManager::new(
        name,
        file,
        auth,
        StdDuration::from_secs(3600),
    ));
    let api = Url::parse(&format!("{}/api", server.uri())).unwrap();
    let graphql = Url::parse(&format!("{}/graphql", server.uri())).unwrap();
    let peloton = Peloton::new(http(), api, tokens, max_in_flight).with_graphql_url(graphql);
    AccountContext::new(Arc::new(peloton))
}

/// The relay's router serving `contexts`, its clock stopped at `now`.
pub fn router_of(contexts: Vec<AccountContext>, now: OffsetDateTime) -> Router {
    router(AppState {
        accounts: Arc::new(AccountRegistry::new(contexts).unwrap()),
        clock: Clock::fixed(now),
    })
}

/// One account of [`relay_with`]: its name, the (access, refresh) tokens to
/// seed it with, if any, and the Peloton user it is bound to.
pub type AccountSpec<'a> = (&'a str, Option<(&'a str, &'a str)>, &'a PelotonId);

/// The relay's router serving `accounts` (name, tokens to seed, bound user;
/// see [`account_context`]) under `dir`, its clock stopped at `now`.
pub fn relay_with(
    dir: &tempfile::TempDir,
    server: &wiremock::MockServer,
    now: OffsetDateTime,
    accounts: &[AccountSpec<'_>],
) -> Router {
    let contexts = accounts
        .iter()
        .map(|(name, tokens, user)| account_context(dir.path(), server, name, *tokens, user, 4))
        .collect();
    router_of(contexts, now)
}

/// The context of account [`ACCOUNT`], its state file directly in `dir`;
/// `seeded` gives it the tokens [`ACCESS`] and [`REFRESH`].
pub fn alice(
    dir: &tempfile::TempDir,
    server: &wiremock::MockServer,
    seeded: bool,
) -> AccountContext {
    if seeded {
        state_file(dir.path())
            .save(&state(ACCESS, REFRESH, Duration::hours(48)), LOCK_WAIT)
            .unwrap();
    }
    context(server, account(ACCOUNT), state_file(dir.path()), 4)
}

/// The relay's router serving the one account [`ACCOUNT`] with every
/// upstream on `server` (REST under `/api`, GraphQL at `/graphql`) and its
/// clock stopped at `now`; `seeded` gives it the tokens [`ACCESS`] and
/// [`REFRESH`].
pub fn relay(
    dir: &tempfile::TempDir,
    server: &wiremock::MockServer,
    seeded: bool,
    now: OffsetDateTime,
) -> Router {
    router_of(vec![alice(dir, server, seeded)], now)
}

/// A request naming `account` (none when `None`), with `body` as JSON.
pub fn request_as(
    account: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<&serde_json::Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(account) = account {
        builder = builder.header(ACCOUNT_HEADER, account);
    }
    match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())),
        None => builder.body(Body::empty()),
    }
    .unwrap()
}

/// Sends `request` to `app`: status, headers and body text.
pub async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

/// A request of [`ACCOUNT`] with a JSON body.
pub fn with_json(method: &str, uri: &str, body: &serde_json::Value) -> Request<Body> {
    request_as(Some(ACCOUNT), method, uri, Some(body))
}

/// A request of [`ACCOUNT`] without a body.
pub fn bare(method: &str, uri: &str) -> Request<Body> {
    request_as(Some(ACCOUNT), method, uri, None)
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
