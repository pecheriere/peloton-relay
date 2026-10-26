//! Tests for the relay's GraphQL request path, against synthetic answers.

use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

use peloton::graphql::{ADD_CLASS_TO_SCHEDULE, CHECK_CLASS_IN_STACK, Op, OpKind};
use peloton::ids::{OnDemandClassId, PelotonId};
use peloton_relay::tokens::TokenManager;
use peloton_relay::upstream::{Peloton, UpstreamError, schedule_result};
use serde::Deserialize;
use serde_json::{Value, json};
use time::Duration;
use tracing::Instrument as _;
use url::Url;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

#[allow(
    dead_code,
    reason = "shared by several test crates, each using a subset of the helpers"
)]
mod common;

use common::{captured_logs, http, state, state_file};

const RIDE: &str = "0123456789abcdef0123456789abcdef";

const ADD: Op = Op {
    name: "AddClassToSchedule",
    kind: OpKind::Mutation,
    document: ADD_CLASS_TO_SCHEDULE,
    field: "addClassToSchedule",
};

const CHECK: Op = Op {
    name: "CheckDoesClassExistInUserStack",
    kind: OpKind::Query,
    document: CHECK_CLASS_IN_STACK,
    field: "checkDoesClassExistInUserStack",
};

/// The members of the schedule union the tests decode on success.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Scheduled {
    id: String,
    scheduled_start_time: String,
}

fn ride() -> PelotonId {
    RIDE.parse().unwrap()
}

/// A client whose token manager holds `tok-1`, with every endpoint on `server`.
fn client(dir: &tempfile::TempDir, server: &MockServer) -> Peloton {
    state_file(dir.path())
        .save(&state("tok-1", "refresh-1", Duration::hours(48)))
        .unwrap();
    let auth = peloton::oauth::AuthClient::new(http(), Url::parse(&server.uri()).unwrap());
    let tokens = TokenManager::new(state_file(dir.path()), auth, StdDuration::from_secs(3600));
    let api = Url::parse(&format!("{}/api", server.uri())).unwrap();
    let graphql = Url::parse(&format!("{}/graphql", server.uri())).unwrap();
    Peloton::new(http(), api, Arc::new(tokens), 4).with_graphql_url(graphql)
}

fn answer(body: &Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

fn add_variables() -> Value {
    json!({
        "id": OnDemandClassId::from_ride(&ride()).as_str(),
        "startTime": "2026-11-01T18:00:00Z",
    })
}

/// Sends `AddClassToSchedule` against a gateway answering `root` as its
/// root field, and runs the answer through the schedule union.
async fn schedule_with_root(root: Value) -> Result<Scheduled, UpstreamError> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(answer(&json!({ "data": { "addClassToSchedule": root } })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let value: Value = client(&dir, &server)
        .gql(&ADD, &add_variables())
        .await
        .unwrap();
    schedule_result(value)
}

#[tokio::test]
async fn gql_posts_operation_and_headers() {
    let server = MockServer::start().await;
    let encoded = OnDemandClassId::from_ride(&ride());
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param("mutation", "AddClassToSchedule"))
        .and(header("authorization", "Bearer tok-1"))
        .and(header("peloton-platform", "web"))
        .and(header("content-type", "application/json"))
        .and(body_json(json!({
            "query": ADD_CLASS_TO_SCHEDULE,
            "variables": {
                "id": encoded.as_str(),
                "startTime": "2026-11-01T18:00:00Z",
            },
        })))
        .respond_with(answer(&json!({
            "data": {
                "addClassToSchedule": {
                    "__typename": "ScheduledClass",
                    "id": "event-1",
                    "scheduledStartTime": "2026-11-01T18:00:00Z",
                }
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let value: Value = client(&dir, &server)
        .gql(&ADD, &add_variables())
        .await
        .unwrap();
    let scheduled: Scheduled = schedule_result(value).unwrap();
    assert_eq!(
        scheduled,
        Scheduled {
            id: "event-1".to_owned(),
            scheduled_start_time: "2026-11-01T18:00:00Z".to_owned(),
        }
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[0].url.query(), Some("mutation=AddClassToSchedule"));
}

#[tokio::test]
async fn gql_query_kind_goes_in_the_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param("query", "CheckDoesClassExistInUserStack"))
        .respond_with(answer(
            &json!({ "data": { "checkDoesClassExistInUserStack": true } }),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let variables = json!({ "pelotonClassId": OnDemandClassId::from_ride(&ride()).as_str() });
    let found: bool = client(&dir, &server).gql(&CHECK, &variables).await.unwrap();
    assert!(found);
}

#[tokio::test]
async fn graphql_errors_are_upstream_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(answer(&json!({
            "data": null,
            "errors": [
                {
                    "message": "class secret-xyz is not allowed",
                    "extensions": { "code": "BAD_USER_INPUT" },
                },
                { "message": "secret-abc", "extensions": { "code": "drop me; secret-def" } },
                { "message": "no extensions" },
            ],
        })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let logs = captured_logs();
    let err = client(&dir, &server)
        .gql::<Value>(&ADD, &add_variables())
        .instrument(tracing::info_span!("case", name = "graphql_errors"))
        .await
        .unwrap_err();
    assert_eq!(err, UpstreamError::GraphQl);
    let ours: String = logs
        .contents()
        .lines()
        .filter(|l| l.contains("graphql_errors"))
        .collect();
    assert!(ours.contains("AddClassToSchedule"), "{ours}");
    assert!(ours.contains("BAD_USER_INPUT"), "{ours}");
    for text in [format!("{err} {err:?}"), ours] {
        assert!(!text.contains("secret-"), "{text}");
        assert!(!text.contains("tok-1"), "{text}");
    }
}

#[tokio::test]
async fn empty_errors_array_is_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(answer(&json!({
            "data": { "checkDoesClassExistInUserStack": false },
            "errors": [],
        })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let variables = json!({ "pelotonClassId": "x" });
    let found: bool = client(&dir, &server).gql(&CHECK, &variables).await.unwrap();
    assert!(!found);
}

#[tokio::test]
async fn missing_data_or_root_field_is_decode_error() {
    for (body, expected) in [
        (json!({}), "data"),
        (json!({ "data": null }), "data"),
        (json!({ "data": {} }), "data.checkDoesClassExistInUserStack"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(answer(&body))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let err = client(&dir, &server)
            .gql::<bool>(&CHECK, &json!({}))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            UpstreamError::Decode {
                path: expected.to_owned()
            },
            "{body}"
        );
    }
}

#[tokio::test]
async fn gql_decode_error_reports_path_only() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(answer(&json!({
            "data": { "addClassToSchedule": { "id": 7, "scheduledStartTime": "secret-xyz" } }
        })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let logs = captured_logs();
    let err = client(&dir, &server)
        .gql::<Scheduled>(&ADD, &add_variables())
        .instrument(tracing::info_span!("case", name = "gql_decode_error"))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        UpstreamError::Decode {
            path: "data.addClassToSchedule.id".to_owned()
        }
    );
    let ours: String = logs
        .contents()
        .lines()
        .filter(|l| l.contains("gql_decode_error"))
        .collect();
    assert!(ours.contains("data.addClassToSchedule.id"), "{ours}");
    assert!(!ours.contains("secret-xyz"), "{ours}");
}

#[tokio::test]
async fn unexpected_union_member_is_upstream_error() {
    let err = schedule_with_root(json!({
        "__typename": "SomethingNew",
        "message": "secret-xyz",
    }))
    .await
    .unwrap_err();
    assert_eq!(err, UpstreamError::GraphQl);
    assert!(!format!("{err} {err:?}").contains("secret-xyz"));
}

#[tokio::test]
async fn union_without_typename_is_decode_error() {
    for root in [
        json!({ "id": "x" }),
        json!(null),
        json!({ "__typename": 3 }),
    ] {
        let err = schedule_with_root(root.clone()).await.unwrap_err();
        assert_eq!(
            err,
            UpstreamError::Decode {
                path: "__typename".to_owned()
            },
            "{root}"
        );
    }
}

#[tokio::test]
async fn cannot_schedule_maps() {
    let err = schedule_with_root(json!({
        "__typename": "CannotScheduleClassError",
        "code": "CLASS_NOT_SCHEDULABLE",
        "message": "This class can't be scheduled at that time.",
    }))
    .await
    .unwrap_err();
    assert_eq!(
        err,
        UpstreamError::CannotSchedule {
            message: "This class can't be scheduled at that time.".to_owned()
        }
    );
}

#[tokio::test]
async fn cannot_schedule_message_is_capped() {
    let long = "é".repeat(300);
    let err = schedule_with_root(json!({
        "__typename": "CannotScheduleClassError",
        "code": "X",
        "message": long,
    }))
    .await
    .unwrap_err();
    let UpstreamError::CannotSchedule { message } = err else {
        panic!("expected CannotSchedule, got {err:?}");
    };
    assert_eq!(message, "é".repeat(200));
}

#[tokio::test]
async fn cannot_schedule_without_message_is_decode_error() {
    let err = schedule_with_root(json!({
        "__typename": "CannotScheduleClassError",
        "code": "X",
    }))
    .await
    .unwrap_err();
    assert_eq!(
        err,
        UpstreamError::Decode {
            path: "message".to_owned()
        }
    );
}

#[tokio::test]
async fn does_not_exist_maps_not_found() {
    let err = schedule_with_root(json!({
        "__typename": "ScheduledClassDoesNotExistError",
        "code": "NOT_FOUND",
        "message": "secret-xyz",
    }))
    .await
    .unwrap_err();
    assert_eq!(err, UpstreamError::NotFound);
}

#[tokio::test]
async fn gql_retries_once_after_401() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "tok-2",
            "refresh_token": "refresh-2",
            "expires_in": 172_800,
            "token_type": "Bearer",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("authorization", "Bearer tok-1"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("authorization", "Bearer tok-2"))
        .and(body_json(json!({
            "query": CHECK_CLASS_IN_STACK,
            "variables": { "pelotonClassId": "x" },
        })))
        .respond_with(answer(
            &json!({ "data": { "checkDoesClassExistInUserStack": true } }),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let found: bool = client(&dir, &server)
        .gql(&CHECK, &json!({ "pelotonClassId": "x" }))
        .await
        .unwrap();
    assert!(found);
}

#[tokio::test]
async fn gql_maps_http_statuses() {
    for (status, expected) in [
        (404, UpstreamError::NotFound),
        (500, UpstreamError::Upstream { status: 500 }),
        (400, UpstreamError::Upstream { status: 400 }),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_string("secret-xyz"))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let err = client(&dir, &server)
            .gql::<Value>(&CHECK, &json!({}))
            .await
            .unwrap_err();
        assert_eq!(err, expected, "status {status}");
    }
}

/// Records when each request arrives, then answers after a fixed delay.
struct Slow {
    delay: StdDuration,
    arrivals: Arc<Mutex<Vec<Instant>>>,
}

impl Respond for Slow {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.arrivals.lock().unwrap().push(Instant::now());
        answer(&json!({ "data": { "checkDoesClassExistInUserStack": true } })).set_delay(self.delay)
    }
}

#[tokio::test]
async fn gql_respects_in_flight_cap() {
    let delay = StdDuration::from_millis(400);
    let arrivals = Arc::new(Mutex::new(Vec::new()));
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(Slow {
            delay,
            arrivals: Arc::clone(&arrivals),
        })
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let peloton = Arc::new(client(&dir, &server));
    let calls: Vec<_> = (0..10)
        .map(|_| {
            let peloton = Arc::clone(&peloton);
            tokio::spawn(async move { peloton.gql::<bool>(&CHECK, &json!({})).await })
        })
        .collect();
    for call in calls {
        assert!(call.await.unwrap().unwrap());
    }
    let mut arrivals = arrivals.lock().unwrap().clone();
    arrivals.sort();
    assert_eq!(arrivals.len(), 10);
    // Requests are in flight for `delay` after they arrive, so the number
    // arriving within one delay of any arrival bounds the concurrency.
    let window = delay.mul_f32(0.9);
    let peak = arrivals
        .iter()
        .map(|start| {
            arrivals
                .iter()
                .filter(|t| *t >= start && t.duration_since(*start) < window)
                .count()
        })
        .max()
        .unwrap();
    assert_eq!(peak, 4);
}
