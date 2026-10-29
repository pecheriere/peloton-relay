//! Tests of account routing: every `/v1` request is served from exactly the
//! account its `X-Peloton-Account` header names, and refused otherwise; each
//! account's credential, upstream limit and Stack writes are its own.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::time::{Duration as StdDuration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode};
use peloton::graphql::{Op, ops};
use peloton::ids::PelotonId;
use peloton_relay::api::openapi;
use peloton_relay::store::StateFile;
use serde_json::{Value, json};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};
use tracing::Instrument as _;
use wiremock::matchers::{any, body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[allow(
    dead_code,
    reason = "shared by several test crates, each using a subset of the helpers"
)]
mod common;

#[allow(
    dead_code,
    reason = "shared by several test crates, each using a subset of the helpers"
)]
mod api_common;

use api_common::{
    ACCOUNT_HEADER, SAMPLE_CLASS, SAMPLE_ID, SAMPLE_SCHEDULED, V1_ROUTES, account_context,
    assert_error, fixture_json, fixture_scheduled_id, relay_with, request_as, router_of,
    sample_body, send, stack_with_one_class,
};
use common::{LOCK_WAIT, captured_logs};

/// The relay clock: a week before the samples' start time.
const NOW: OffsetDateTime = datetime!(2030-01-01 00:00 UTC);
const ALICE_ACCESS: &str = "access-alice";
const ALICE_REFRESH: &str = "refresh-alice";
const BOB_ACCESS: &str = "access-bob";
const BOB_REFRESH: &str = "refresh-bob";

fn user_a() -> PelotonId {
    common::user()
}

fn user_b() -> PelotonId {
    "000000000000000000000000000000b2".parse().unwrap()
}

fn parse(body: &str) -> Value {
    serde_json::from_str(body).unwrap()
}

/// alice (user A) and bob (user B), both seeded.
fn both(dir: &tempfile::TempDir, server: &MockServer) -> Router {
    relay_with(
        dir,
        server,
        NOW,
        &[
            ("alice", Some((ALICE_ACCESS, ALICE_REFRESH)), &user_a()),
            ("bob", Some((BOB_ACCESS, BOB_REFRESH)), &user_b()),
        ],
    )
}

fn bearer(access: &str) -> String {
    format!("Bearer {access}")
}

/// `me.json` as the profile of `user` named `username`.
fn profile(user: &PelotonId, username: &str) -> Value {
    let mut me = fixture_json("me.json");
    me["id"] = json!(user.to_string());
    me["username"] = json!(username);
    me
}

/// Answers `GET /api/me` to the bearer of `access` with the profile of `user`.
async fn mount_me(server: &MockServer, access: &str, user: &PelotonId, username: &str) {
    Mock::given(method("GET"))
        .and(path("/api/me"))
        .and(header("authorization", bearer(access)))
        .respond_with(ResponseTemplate::new(200).set_body_json(profile(user, username)))
        .mount(server)
        .await;
}

/// Answers GraphQL operation `op` to the bearer of `access` with `body`,
/// after `delay`.
async fn mount_op_as(server: &MockServer, access: &str, op: &Op, body: &Value, delay: StdDuration) {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param(op.kind.as_str(), op.name))
        .and(header("authorization", bearer(access)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(body)
                .set_delay(delay),
        )
        .mount(server)
        .await;
}

/// The token endpoint, for refreshes with `refresh_token`.
fn refresh_mock(refresh_token: &str) -> wiremock::MockBuilder {
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains(format!(
            "refresh_token={refresh_token}"
        )))
}

/// Refuses every upstream call, expecting none.
async fn mount_no_upstream(server: &MockServer) {
    Mock::given(any())
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(server)
        .await;
}

/// The request of `V1_ROUTES` entry `method` `uri`, with its sample body.
fn v1_request(account: Option<&str>, method: &str, uri: &str) -> Request<Body> {
    request_as(account, method, uri, sample_body(method, uri).as_ref())
}

/// The path template of a `V1_ROUTES` entry.
fn template(uri: &str) -> String {
    let path = uri.split('?').next().unwrap();
    path.replace(SAMPLE_SCHEDULED, "{scheduled_id}")
        .replace(&format!("/v1/stack/{SAMPLE_CLASS}"), "/v1/stack/{class_id}")
        .replace(SAMPLE_ID, "{id}")
}

#[test]
fn v1_routes_cover_the_openapi_document() {
    assert_eq!(fixture_scheduled_id().1, SAMPLE_SCHEDULED);
    let listed: BTreeSet<(String, String)> = V1_ROUTES
        .iter()
        .map(|(method, uri)| (method.to_lowercase(), template(uri)))
        .collect();
    assert_eq!(listed.len(), V1_ROUTES.len(), "a route is listed twice");
    let document = serde_json::to_value(openapi()).unwrap();
    let verbs = [
        "get", "put", "post", "delete", "options", "head", "patch", "trace",
    ];
    let documented: BTreeSet<(String, String)> = document["paths"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(route, _)| route.starts_with("/v1/"))
        .flat_map(|(route, item)| {
            item.as_object()
                .unwrap()
                .keys()
                .filter(|verb| verbs.contains(&verb.as_str()))
                .map(|verb| (verb.clone(), route.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(listed, documented);
}

#[tokio::test]
async fn every_route_refuses_a_missing_or_bad_account() {
    let server = MockServer::start().await;
    mount_no_upstream(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let app = both(&dir, &server);
    let headers: [Option<HeaderValue>; 7] = [
        None,
        Some(HeaderValue::from_static("")),
        Some(HeaderValue::from_static("Alice")),
        Some(HeaderValue::from_static("al1ce")),
        Some(HeaderValue::from_static("abcdefghijklmnopq")),
        Some(HeaderValue::from_bytes(&[0xff]).unwrap()),
        Some(HeaderValue::from_static("carol")),
    ];
    for (method, uri) in V1_ROUTES {
        for value in &headers {
            let mut request = v1_request(None, method, uri);
            if let Some(value) = value {
                request.headers_mut().insert(ACCOUNT_HEADER, value.clone());
            }
            let (status, _, body) = send(&app, request).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{method} {uri} {value:?}: {body}"
            );
            assert_error(&body, "forbidden");
        }
    }
}

#[tokio::test]
async fn two_account_headers_are_refused() {
    let server = MockServer::start().await;
    mount_no_upstream(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let app = both(&dir, &server);
    for (first, second) in [("alice", "alice"), ("alice", "bob")] {
        let mut request = request_as(Some(first), "GET", "/v1/me", None);
        request
            .headers_mut()
            .append(ACCOUNT_HEADER, HeaderValue::from_static(second));
        let (status, _, body) = send(&app, request).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{first},{second}: {body}");
        assert_error(&body, "forbidden");
    }
}

#[tokio::test]
async fn account_is_checked_before_validation_and_routing() {
    let server = MockServer::start().await;
    mount_no_upstream(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let app = both(&dir, &server);
    for (method, uri) in [
        ("GET", "/v1/workouts?limit=nope"),
        ("GET", "/v1/nope"),
        ("PATCH", "/v1/me"),
    ] {
        let (status, _, body) = send(&app, request_as(None, method, uri, None)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {body}");
        assert_error(&body, "forbidden");
    }
    let (status, _, body) = send(&app, request_as(Some("alice"), "GET", "/v1/nope", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_error(&body, "not_found");
    let (status, _, body) = send(&app, request_as(Some("alice"), "PATCH", "/v1/me", None)).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
    assert_error(&body, "method_not_allowed");
}

#[tokio::test]
async fn probes_and_openapi_need_no_account() {
    let server = MockServer::start().await;
    mount_no_upstream(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let unseeded = relay_with(&dir, &server, NOW, &[("alice", None, &user_a())]);
    for app in [unseeded, router_of(Vec::new(), NOW)] {
        for uri in ["/healthz", "/readyz", "/openapi.json"] {
            let (status, _, body) = send(&app, request_as(None, "GET", uri, None)).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        }
    }
}

#[tokio::test]
async fn each_account_sees_only_its_own_data() {
    let server = MockServer::start().await;
    mount_me(&server, ALICE_ACCESS, &user_a(), "alice_rides").await;
    mount_me(&server, BOB_ACCESS, &user_b(), "bob_rides").await;
    let empty = json!({ "data": { "viewUserStack": {
        "__typename": "StackResponseSuccess",
        "numClasses": 0,
        "totalTime": 0,
        "userStack": { "stackedClassList": [] },
    } } });
    let none = StdDuration::ZERO;
    mount_op_as(
        &server,
        ALICE_ACCESS,
        &ops::VIEW_USER_STACK,
        &stack_with_one_class(),
        none,
    )
    .await;
    mount_op_as(&server, BOB_ACCESS, &ops::VIEW_USER_STACK, &empty, none).await;
    let dir = tempfile::tempdir().unwrap();
    let app = both(&dir, &server);

    for (account, user, username) in [
        ("alice", user_a(), "alice_rides"),
        ("bob", user_b(), "bob_rides"),
    ] {
        let (status, _, body) = send(&app, request_as(Some(account), "GET", "/v1/me", None)).await;
        assert_eq!(status, StatusCode::OK, "{account}: {body}");
        let me = parse(&body);
        assert_eq!(me["id"], user.to_string(), "{account}: {body}");
        assert_eq!(me["username"], username, "{account}: {body}");
    }

    let (status, _, body) = send(&app, request_as(Some("alice"), "GET", "/v1/stack", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = parse(&body)["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1, "{body}");
    assert_eq!(items[0]["class_id"], SAMPLE_CLASS, "{body}");
    let (status, _, body) = send(&app, request_as(Some("bob"), "GET", "/v1/stack", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(parse(&body)["items"], json!([]), "{body}");
}

#[tokio::test]
async fn one_accounts_dead_refresh_leaves_the_other_working() {
    let server = MockServer::start().await;
    mount_me(&server, BOB_ACCESS, &user_b(), "bob_rides").await;
    refresh_mock(ALICE_REFRESH)
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": "invalid_grant",
            "error_description": "Unknown or invalid refresh token.",
        })))
        .expect(1)
        .mount(&server)
        .await;
    refresh_mock(BOB_REFRESH)
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let alice = account_context(dir.path(), &server, "alice", None, &user_a(), 4);
    let mut expired = common::state(ALICE_ACCESS, ALICE_REFRESH, -Duration::minutes(5));
    expired.user_id = user_a();
    StateFile {
        path: dir.path().join("alice").join("state.json"),
    }
    .save(&expired, LOCK_WAIT)
    .unwrap();
    let bob = account_context(
        dir.path(),
        &server,
        "bob",
        Some((BOB_ACCESS, BOB_REFRESH)),
        &user_b(),
        4,
    );
    let app = router_of(vec![alice, bob], NOW);

    for _ in 0..2 {
        let (status, _, body) = send(&app, request_as(Some("alice"), "GET", "/v1/me", None)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_error(&body, "not_authenticated");
        let (status, _, body) = send(&app, request_as(Some("bob"), "GET", "/v1/me", None)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}

#[tokio::test]
async fn one_accounts_401_refreshes_only_that_account() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/me"))
        .and(header("authorization", bearer(ALICE_ACCESS)))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    mount_me(&server, "access-alice-2", &user_a(), "alice_rides").await;
    mount_me(&server, BOB_ACCESS, &user_b(), "bob_rides").await;
    refresh_mock(ALICE_REFRESH)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-alice-2",
            "refresh_token": "refresh-alice-2",
            "expires_in": 172_800,
            "token_type": "Bearer",
        })))
        .expect(1)
        .mount(&server)
        .await;
    refresh_mock(BOB_REFRESH)
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let app = both(&dir, &server);

    let (status, _, body) = send(&app, request_as(Some("alice"), "GET", "/v1/me", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(parse(&body)["id"], user_a().to_string());
    let (status, _, body) = send(&app, request_as(Some("bob"), "GET", "/v1/me", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(parse(&body)["id"], user_b().to_string());
    let token_calls = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/oauth/token")
        .count();
    assert_eq!(token_calls, 1);
}

#[tokio::test]
async fn unseeded_account_is_503_for_its_callers_only() {
    let server = MockServer::start().await;
    mount_me(&server, ALICE_ACCESS, &user_a(), "alice_rides").await;
    let dir = tempfile::tempdir().unwrap();
    let app = relay_with(
        &dir,
        &server,
        NOW,
        &[
            ("alice", Some((ALICE_ACCESS, ALICE_REFRESH)), &user_a()),
            ("bob", None, &user_b()),
        ],
    );
    let (status, _, body) = send(&app, request_as(Some("bob"), "GET", "/v1/me", None)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_error(&body, "not_authenticated");
    let (status, _, body) = send(&app, request_as(Some("alice"), "GET", "/v1/me", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Answers every Stack operation of the bearer of `access`, delaying its
/// `CheckClassInStack` (taken under the Stack's write lock) by `check_delay`.
async fn mount_stack_of(server: &MockServer, access: &str, check_delay: StdDuration) {
    let none = StdDuration::ZERO;
    mount_op_as(
        server,
        access,
        &ops::CHECK_CLASS_IN_STACK,
        &fixture_json("check_class_in_stack.json"),
        check_delay,
    )
    .await;
    mount_op_as(
        server,
        access,
        &ops::ADD_CLASS_TO_STACK,
        &fixture_json("stack_add.json"),
        none,
    )
    .await;
    mount_op_as(
        server,
        access,
        &ops::VIEW_USER_STACK,
        &stack_with_one_class(),
        none,
    )
    .await;
    mount_op_as(
        server,
        access,
        &ops::MODIFY_STACK,
        &fixture_json("stack_modify.json"),
        none,
    )
    .await;
}

/// Sends `request` from a task, answering its status and when it finished.
fn spawn_send(
    app: &Router,
    request: Request<Body>,
) -> tokio::task::JoinHandle<(StatusCode, Instant)> {
    let app = app.clone();
    tokio::spawn(async move {
        let (status, _, _) = send(&app, request).await;
        (status, Instant::now())
    })
}

#[tokio::test]
async fn stack_writes_of_two_accounts_do_not_wait_for_each_other() {
    const ALICE_DELAY: StdDuration = StdDuration::from_secs(2);
    let server = MockServer::start().await;
    mount_stack_of(&server, ALICE_ACCESS, ALICE_DELAY).await;
    mount_stack_of(&server, BOB_ACCESS, StdDuration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let app = both(&dir, &server);
    let add = json!({ "class_id": SAMPLE_CLASS });
    let replace = json!({ "class_ids": [SAMPLE_CLASS] });

    let start = Instant::now();
    let alice_add = spawn_send(
        &app,
        request_as(Some("alice"), "POST", "/v1/stack", Some(&add)),
    );
    tokio::time::sleep(StdDuration::from_millis(200)).await;
    // Waits for alice's add, which holds alice's write lock.
    let alice_replace = spawn_send(
        &app,
        request_as(Some("alice"), "PUT", "/v1/stack", Some(&replace)),
    );
    let bob_add = spawn_send(
        &app,
        request_as(Some("bob"), "POST", "/v1/stack", Some(&add)),
    );

    let (status, bob_done) = bob_add.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert!(
        bob_done - start < StdDuration::from_secs(1),
        "bob waited for alice"
    );
    assert!(!alice_add.is_finished(), "alice's add was not delayed");
    let (status, alice_added) = alice_add.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    let (status, alice_replaced) = alice_replace.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert!(bob_done < alice_added);
    assert!(
        alice_replaced - start >= ALICE_DELAY,
        "alice's second write did not wait for her first"
    );
}

#[tokio::test]
async fn slow_upstream_of_one_account_does_not_block_another() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/me"))
        .and(header("authorization", bearer(ALICE_ACCESS)))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(profile(&user_a(), "alice_rides"))
                .set_delay(StdDuration::from_secs(3)),
        )
        .mount(&server)
        .await;
    mount_me(&server, BOB_ACCESS, &user_b(), "bob_rides").await;
    let dir = tempfile::tempdir().unwrap();
    let contexts = vec![
        account_context(
            dir.path(),
            &server,
            "alice",
            Some((ALICE_ACCESS, ALICE_REFRESH)),
            &user_a(),
            1,
        ),
        account_context(
            dir.path(),
            &server,
            "bob",
            Some((BOB_ACCESS, BOB_REFRESH)),
            &user_b(),
            1,
        ),
    ];
    let app = router_of(contexts, NOW);

    let alice: Vec<_> = (0..2)
        .map(|_| spawn_send(&app, request_as(Some("alice"), "GET", "/v1/me", None)))
        .collect();
    tokio::time::sleep(StdDuration::from_millis(200)).await;
    let start = Instant::now();
    let (status, _, body) = send(&app, request_as(Some("bob"), "GET", "/v1/me", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        start.elapsed() < StdDuration::from_secs(1),
        "bob waited for alice's upstream"
    );
    for request in &alice {
        assert!(!request.is_finished(), "alice's upstream was not slow");
        request.abort();
    }
}

#[tokio::test]
async fn unusable_account_dir_affects_only_that_account() {
    let server = MockServer::start().await;
    mount_me(&server, ALICE_ACCESS, &user_a(), "alice_rides").await;
    mount_me(&server, BOB_ACCESS, &user_b(), "bob_rides").await;
    let dir = tempfile::tempdir().unwrap();
    let app = both(&dir, &server);
    let bob_dir = dir.path().join("bob");
    fs::set_permissions(&bob_dir, fs::Permissions::from_mode(0o000)).unwrap();
    // Root ignores directory permissions: nothing to test then.
    let enforced = fs::metadata(bob_dir.join("state.json")).is_err();
    if enforced {
        let (bob_status, _, bob_body) =
            send(&app, request_as(Some("bob"), "GET", "/v1/me", None)).await;
        let (alice_status, _, alice_body) =
            send(&app, request_as(Some("alice"), "GET", "/v1/me", None)).await;
        fs::set_permissions(&bob_dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(bob_status, StatusCode::SERVICE_UNAVAILABLE, "{bob_body}");
        assert_error(&bob_body, "not_authenticated");
        assert_eq!(alice_status, StatusCode::OK, "{alice_body}");
    } else {
        fs::set_permissions(&bob_dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

/// The JSON log lines logged inside a span named `case` with `event`.
fn events(case: &str, event: &str) -> Vec<Value> {
    captured_logs()
        .contents()
        .lines()
        .filter(|line| line.contains(case))
        .map(parse)
        .filter(|line| line["event"] == event)
        .collect()
}

#[tokio::test]
async fn writes_log_the_account() {
    captured_logs();
    let server = MockServer::start().await;
    mount_stack_of(&server, BOB_ACCESS, StdDuration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let app = both(&dir, &server);
    let request = request_as(
        Some("bob"),
        "POST",
        "/v1/stack",
        Some(&json!({ "class_id": SAMPLE_CLASS })),
    );
    let (status, _, body) = send(&app, request)
        .instrument(tracing::info_span!("case", name = "write_logs_account"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let writes = events("write_logs_account", "peloton_write");
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert_eq!(writes[0]["account"], "bob", "{}", writes[0]);
    assert_eq!(writes[0]["span"]["account"], "bob", "{}", writes[0]);
}

#[tokio::test]
async fn refusal_is_logged_without_echoing_bad_values() {
    captured_logs();
    let server = MockServer::start().await;
    mount_no_upstream(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let app = both(&dir, &server);
    let two = {
        let mut request = request_as(Some("alice"), "GET", "/v1/me", None);
        request
            .headers_mut()
            .append(ACCOUNT_HEADER, HeaderValue::from_static("bob"));
        request
    };
    let cases = [
        (
            "refused_missing",
            request_as(None, "GET", "/v1/me", None),
            "missing",
            None,
        ),
        ("refused_ambiguous", two, "ambiguous", None),
        (
            "refused_invalid",
            request_as(Some("<script>"), "GET", "/v1/me", None),
            "invalid",
            None,
        ),
        (
            "refused_unknown",
            request_as(Some("carol"), "GET", "/v1/me", None),
            "unknown",
            Some("carol"),
        ),
    ];
    for (case, request, reason, account) in cases {
        let (status, _, _) = send(&app, request)
            .instrument(tracing::info_span!("case", name = case))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{case}");
        let refusals = events(case, "account_refused");
        assert_eq!(refusals.len(), 1, "{case}: {refusals:?}");
        let refusal = &refusals[0];
        assert_eq!(refusal["level"], "WARN", "{case}: {refusal}");
        assert_eq!(refusal["reason"], reason, "{case}: {refusal}");
        match account {
            Some(account) => assert_eq!(refusal["account"], account, "{case}: {refusal}"),
            None => assert!(refusal.get("account").is_none(), "{case}: {refusal}"),
        }
        assert!(!refusal.to_string().contains("script"), "{case}: {refusal}");
    }
}
