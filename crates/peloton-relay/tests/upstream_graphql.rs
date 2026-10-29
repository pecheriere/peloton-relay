//! Tests for the relay's GraphQL operations (schedule and Stack), against
//! scrubbed fixtures and synthetic answers.

use std::fs;
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

use peloton::graphql::{
    ADD_CLASS_TO_SCHEDULE, ADD_CLASS_TO_STACK, CHECK_CLASS_IN_STACK, MODIFY_STACK,
    REMOVE_CLASS_FROM_SCHEDULE, RESCHEDULE_CLASS, SCHEDULED_CLASS, VIEW_USER_STACK,
    YOUR_SCHEDULE_ITEMS,
};
use peloton::ids::{OnDemandClassId, PelotonId, ScheduledClassId};
use peloton::upstream::ScheduleItem;
use peloton_relay::error::ApiError;
use peloton_relay::stack::{StackError, StackService, Undecodable};
use peloton_relay::tokens::TokenManager;
use peloton_relay::upstream::{Peloton, UpstreamError};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
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

/// The ride of every scrubbed schedule and Stack fixture.
const FIXTURE_RIDE: &str = "00000000000000000000000000000f1d";

fn fixture(name: &str) -> Value {
    let text = fs::read_to_string(format!(
        "{}/../peloton/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    serde_json::from_str(&text).unwrap()
}

fn ride(n: u16) -> PelotonId {
    format!("{n:032x}").parse().unwrap()
}

fn fixture_ride() -> PelotonId {
    FIXTURE_RIDE.parse().unwrap()
}

fn encoded(ride: &PelotonId) -> String {
    OnDemandClassId::from_ride(ride).as_str().to_owned()
}

fn at(text: &str) -> OffsetDateTime {
    OffsetDateTime::parse(text, &Rfc3339).unwrap()
}

/// The scheduled-class id of `schedule_add.json`.
fn scheduled_id() -> ScheduledClassId {
    fixture("schedule_add.json")["data"]["addClassToSchedule"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

/// A client using `http`, whose token manager holds `tok-1`, with every
/// endpoint on `server`.
fn client_with(dir: &tempfile::TempDir, server: &MockServer, http: reqwest::Client) -> Peloton {
    state_file(dir.path())
        .save(&state("tok-1", "refresh-1", Duration::hours(48)))
        .unwrap();
    let auth = peloton::oauth::AuthClient::new(common::http(), Url::parse(&server.uri()).unwrap());
    let tokens = TokenManager::new(state_file(dir.path()), auth, StdDuration::from_secs(3600));
    let api = Url::parse(&format!("{}/api", server.uri())).unwrap();
    let graphql = Url::parse(&format!("{}/graphql", server.uri())).unwrap();
    Peloton::new(http, api, Arc::new(tokens), 4).with_graphql_url(graphql)
}

fn client(dir: &tempfile::TempDir, server: &MockServer) -> Peloton {
    client_with(dir, server, http())
}

fn answer(body: &Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

fn rotation() -> Mock {
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "tok-2",
            "refresh_token": "refresh-2",
            "expires_in": 172_800,
            "token_type": "Bearer",
        })))
}

/// Answers every GraphQL request with `body`.
async fn gateway(body: &Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(answer(body))
        .mount(&server)
        .await;
    server
}

/// `AddClassToSchedule` against a gateway answering `root` as its root field.
async fn schedule_with_root(root: Value) -> Result<(), UpstreamError> {
    let server = gateway(&json!({ "data": { "addClassToSchedule": root } })).await;
    let dir = tempfile::tempdir().unwrap();
    client(&dir, &server)
        .add_to_schedule(&ride(1), at("2026-11-01T18:00:00Z"))
        .await
        .map(|_| ())
}

/// The log lines of events emitted inside a span named `case`.
fn logged(logs: &common::LogBuffer, case: &str) -> String {
    logs.contents()
        .lines()
        .filter(|line| line.contains(case))
        .collect()
}

#[tokio::test]
async fn gql_posts_operation_and_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param("mutation", "AddClassToSchedule"))
        .and(header("authorization", "Bearer tok-1"))
        .and(header("peloton-platform", "web"))
        .and(header("content-type", "application/json"))
        .and(body_json(json!({
            "query": ADD_CLASS_TO_SCHEDULE,
            "variables": {
                "id": encoded(&fixture_ride()),
                "startTime": "2030-01-07T12:00:00Z",
            },
        })))
        .respond_with(answer(&fixture("schedule_add.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let scheduled = client(&dir, &server)
        .add_to_schedule(&fixture_ride(), at("2030-01-07T04:00:00-08:00"))
        .await
        .unwrap();
    assert_eq!(scheduled.id, scheduled_id());
    assert_eq!(scheduled.id.ride_id(), &fixture_ride());
    assert_eq!(
        scheduled.scheduled_start_time,
        Some(at("2030-01-07T12:00:00Z"))
    );
    let class = scheduled.peloton_class.unwrap();
    assert_eq!(class.class_id, Some(fixture_ride()));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[0].url.query(), Some("mutation=AddClassToSchedule"));
}

#[tokio::test]
async fn schedule_lists_items_in_the_window() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param("query", "YourScheduleItems"))
        .and(body_json(json!({
            "query": YOUR_SCHEDULE_ITEMS,
            "variables": {
                "startTime": "2030-01-01T00:00:00Z",
                "endTime": "2030-01-15T00:00:00Z",
            },
        })))
        .respond_with(answer(&fixture("schedule_during.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let items = client(&dir, &server)
        .schedule(at("2030-01-01T01:00:00+01:00"), at("2030-01-15T00:00:00Z"))
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    let ScheduleItem::ScheduledClass(scheduled) = &items[0] else {
        panic!("expected a scheduled class, got {:?}", items[0]);
    };
    assert_eq!(scheduled.id.ride_id(), &fixture_ride());
}

#[tokio::test]
async fn schedule_keeps_other_items_and_empty_lists() {
    let server = gateway(&json!({ "data": { "userScheduledItemsList": {
        "__typename": "YourScheduleItemList",
        "yourScheduleItems": [{ "__typename": "InviteDetails", "id": "x" }],
    } } }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let from = at("2030-01-01T00:00:00Z");
    let items = client(&dir, &server).schedule(from, from).await.unwrap();
    assert!(
        matches!(&items[..], [ScheduleItem::Other { typename }] if typename == "InviteDetails"),
        "{items:?}"
    );

    let server = gateway(&fixture("schedule_before.json")).await;
    let items = client(&dir, &server).schedule(from, from).await.unwrap();
    assert!(items.is_empty());
}

#[tokio::test]
async fn schedule_list_of_another_member_is_upstream_error() {
    let server = gateway(&json!({ "data": { "userScheduledItemsList": {
        "__typename": "InviteFriendsError",
        "message": "secret-xyz",
    } } }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let from = at("2030-01-01T00:00:00Z");
    let err = client(&dir, &server)
        .schedule(from, from)
        .await
        .unwrap_err();
    assert!(matches!(err, UpstreamError::Decode { .. }), "{err:?}");
    assert!(!format!("{err} {err:?}").contains("secret-xyz"));
}

/// Answers `ScheduledClass` for `scheduled_id()` with `class`.
async fn mount_scheduled_class(server: &MockServer, class: &Value) {
    Mock::given(method("POST"))
        .and(query_param("query", "ScheduledClass"))
        .and(body_json(json!({
            "query": SCHEDULED_CLASS,
            "variables": { "id": scheduled_id().as_str() },
        })))
        .respond_with(answer(class))
        .expect(1)
        .mount(server)
        .await;
}

/// Answers `YourScheduleItems` with `list`, expecting `calls` requests.
async fn mount_schedule_list(server: &MockServer, list: &Value, calls: u64) {
    Mock::given(method("POST"))
        .and(query_param("query", "YourScheduleItems"))
        .respond_with(answer(list))
        .expect(calls)
        .mount(server)
        .await;
}

#[tokio::test]
async fn scheduled_class_reads_by_pelotons_id_and_confirms_it_in_its_window() {
    let server = MockServer::start().await;
    mount_scheduled_class(&server, &fixture("scheduled_class.json")).await;
    // The class runs 12:00 to 12:30; the window is a minute wider each side.
    Mock::given(method("POST"))
        .and(query_param("query", "YourScheduleItems"))
        .and(body_json(json!({
            "query": YOUR_SCHEDULE_ITEMS,
            "variables": {
                "startTime": "2030-01-07T11:59:00Z",
                "endTime": "2030-01-07T12:31:00Z",
            },
        })))
        .respond_with(answer(&fixture("schedule_during.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let scheduled = client(&dir, &server)
        .scheduled_class(&scheduled_id())
        .await
        .unwrap();
    assert_eq!(scheduled.id, scheduled_id());
}

#[tokio::test]
async fn scheduled_class_missing_from_the_schedule_is_not_found() {
    // Peloton rebuilds a removed class from its id, but no longer lists it.
    let server = MockServer::start().await;
    mount_scheduled_class(&server, &fixture("scheduled_class.json")).await;
    mount_schedule_list(&server, &fixture("schedule_after.json"), 1).await;
    let dir = tempfile::tempdir().unwrap();
    let err = client(&dir, &server)
        .scheduled_class(&scheduled_id())
        .await
        .unwrap_err();
    assert_eq!(err, UpstreamError::NotFound);
}

#[tokio::test]
async fn scheduled_class_listed_only_under_another_id_is_not_found() {
    let server = MockServer::start().await;
    mount_scheduled_class(&server, &fixture("scheduled_class.json")).await;
    let other = fixture("schedule_reschedule.json")["data"]["rescheduleClass"]["id"].clone();
    let mut list = fixture("schedule_during.json");
    list["data"]["userScheduledItemsList"]["yourScheduleItems"][0]["id"] = other;
    let invite = json!({ "__typename": "InviteDetails", "id": scheduled_id().as_str() });
    list["data"]["userScheduledItemsList"]["yourScheduleItems"]
        .as_array_mut()
        .unwrap()
        .push(invite);
    mount_schedule_list(&server, &list, 1).await;
    let dir = tempfile::tempdir().unwrap();
    let err = client(&dir, &server)
        .scheduled_class(&scheduled_id())
        .await
        .unwrap_err();
    assert_eq!(err, UpstreamError::NotFound);
}

#[tokio::test]
async fn scheduled_class_without_its_times_does_not_decode() {
    for field in ["scheduledStartTime", "scheduledEndTime"] {
        let server = MockServer::start().await;
        let mut class = fixture("scheduled_class.json");
        class["data"]["scheduledClass"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        mount_scheduled_class(&server, &class).await;
        mount_schedule_list(&server, &fixture("schedule_during.json"), 0).await;
        let dir = tempfile::tempdir().unwrap();
        let err = client(&dir, &server)
            .scheduled_class(&scheduled_id())
            .await
            .unwrap_err();
        assert_eq!(
            err,
            UpstreamError::Decode {
                path: format!("data.scheduledClass.{field}")
            }
        );
    }
}

#[tokio::test]
async fn reschedule_sends_id_and_utc_time_and_returns_the_new_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(query_param("mutation", "RescheduleClass"))
        .and(body_json(json!({
            "query": RESCHEDULE_CLASS,
            "variables": {
                "id": scheduled_id().as_str(),
                "startTime": "2030-01-07T13:00:00Z",
            },
        })))
        .respond_with(answer(&fixture("schedule_reschedule.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let moved = client(&dir, &server)
        .reschedule(&scheduled_id(), at("2030-01-07T14:00:00+01:00"))
        .await
        .unwrap();
    assert_ne!(moved.id, scheduled_id());
    assert_eq!(moved.id.scheduled_start_time(), at("2030-01-07T13:00:00Z"));
}

#[tokio::test]
async fn remove_from_schedule_sends_the_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(query_param("mutation", "RemoveClassFromSchedule"))
        .and(body_json(json!({
            "query": REMOVE_CLASS_FROM_SCHEDULE,
            "variables": { "id": scheduled_id().as_str() },
        })))
        .respond_with(answer(&fixture("schedule_remove.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    client(&dir, &server)
        .remove_from_schedule(&scheduled_id())
        .await
        .unwrap();
}

#[tokio::test]
async fn graphql_errors_are_upstream_error() {
    let server = gateway(&json!({
        "data": null,
        "errors": [
            {
                "message": "class secret-xyz is not allowed",
                "extensions": { "code": "BAD_USER_INPUT" },
            },
            { "message": "secret-abc", "extensions": { "code": "drop me; secret-def" } },
            { "message": "no extensions" },
        ],
    }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let logs = captured_logs();
    let err = client(&dir, &server)
        .add_to_schedule(&ride(1), at("2030-01-07T12:00:00Z"))
        .instrument(tracing::info_span!("case", name = "graphql_errors"))
        .await
        .unwrap_err();
    assert_eq!(err, UpstreamError::GraphQl);
    let ours = logged(logs, "graphql_errors");
    assert!(ours.contains("AddClassToSchedule"), "{ours}");
    assert!(ours.contains("BAD_USER_INPUT"), "{ours}");
    for text in [format!("{err} {err:?}"), ours] {
        assert!(!text.contains("secret-"), "{text}");
        assert!(!text.contains("tok-1"), "{text}");
    }
}

#[tokio::test]
async fn empty_errors_array_is_success() {
    let server = gateway(&json!({
        "data": { "checkDoesClassExistInUserStack": false },
        "errors": [],
    }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    assert!(
        !client(&dir, &server)
            .class_in_stack(&ride(1))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn missing_data_or_root_field_is_decode_error() {
    for (body, expected) in [
        (json!({}), "data"),
        (json!({ "data": null }), "data"),
        (json!({ "data": {} }), "data.checkDoesClassExistInUserStack"),
        (
            json!({ "data": { "checkDoesClassExistInUserStack": "yes" } }),
            "data.checkDoesClassExistInUserStack",
        ),
    ] {
        let server = gateway(&body).await;
        let dir = tempfile::tempdir().unwrap();
        let err = client(&dir, &server)
            .class_in_stack(&ride(1))
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
    let server = gateway(&json!({ "data": { "userScheduledItemsList": {
        "__typename": "YourScheduleItemList",
        "yourScheduleItems": [
            { "__typename": "ScheduledClass", "id": "secret-xyz" },
        ],
    } } }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let logs = captured_logs();
    let from = at("2030-01-01T00:00:00Z");
    let err = client(&dir, &server)
        .schedule(from, from)
        .instrument(tracing::info_span!("case", name = "gql_decode_error"))
        .await
        .unwrap_err();
    // Peloton's unions are buffered before their member decodes, so the path
    // stops at the union.
    let path = "data.userScheduledItemsList";
    assert_eq!(
        err,
        UpstreamError::Decode {
            path: path.to_owned()
        }
    );
    let ours = logged(logs, "gql_decode_error");
    assert!(ours.contains(path), "{ours}");
    assert!(!ours.contains("secret-xyz"), "{ours}");
}

#[tokio::test]
async fn unexpected_union_member_is_upstream_error() {
    let logs = captured_logs();
    let err = schedule_with_root(json!({
        "__typename": "SomethingNew",
        "message": "secret-xyz",
    }))
    .instrument(tracing::info_span!("case", name = "unexpected_member"))
    .await
    .unwrap_err();
    assert_eq!(
        err,
        UpstreamError::Decode {
            path: "data.addClassToSchedule.__typename".to_owned()
        }
    );
    assert_eq!(ApiError::from(err.clone()), ApiError::Upstream);
    for text in [format!("{err} {err:?}"), logged(logs, "unexpected_member")] {
        assert!(!text.contains("secret-xyz"), "{text}");
        assert!(!text.contains("SomethingNew"), "{text}");
    }
}

#[tokio::test]
async fn union_without_typename_is_decode_error() {
    for (root, path) in [
        (json!({ "id": "x" }), "data.addClassToSchedule"),
        (json!(null), "data.addClassToSchedule"),
        (
            json!({ "__typename": 3 }),
            "data.addClassToSchedule.__typename",
        ),
    ] {
        let err = schedule_with_root(root.clone()).await.unwrap_err();
        assert_eq!(
            err,
            UpstreamError::Decode {
                path: path.to_owned()
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
            path: "data.addClassToSchedule.message".to_owned()
        }
    );
}

#[tokio::test]
async fn cannot_schedule_message_never_reaches_logs() {
    let logs = captured_logs();
    let err = schedule_with_root(json!({
        "__typename": "CannotScheduleClassError",
        "code": "TOO_FAR_AHEAD",
        "message": "secret-message for the member",
    }))
    .instrument(tracing::info_span!("case", name = "cannot_schedule_log"))
    .await
    .unwrap_err();
    // Whatever a caller logs, the message never shows: not in the error's
    // Display or Debug, not in the API error's, not in the client's own logs.
    let api = ApiError::from(err.clone());
    tracing::info_span!("case", name = "cannot_schedule_log").in_scope(|| {
        tracing::warn!(error = %err, ?err, ?api, "caller logs the failure");
    });
    let ours = logged(logs, "cannot_schedule_log");
    assert!(ours.contains("TOO_FAR_AHEAD"), "{ours}");
    assert!(ours.contains("CannotSchedule"), "{ours}");
    for text in [format!("{err} {err:?} {api} {api:?}"), ours] {
        assert!(!text.contains("secret-message"), "{text}");
    }
}

#[tokio::test]
async fn does_not_exist_maps_not_found() {
    let missing = json!({
        "__typename": "ScheduledClassDoesNotExistError",
        "code": "NOT_FOUND",
        "message": "secret-xyz",
    });
    let dir = tempfile::tempdir().unwrap();
    let server = gateway(&json!({ "data": { "scheduledClass": missing } })).await;
    let err = client(&dir, &server)
        .scheduled_class(&scheduled_id())
        .await
        .unwrap_err();
    assert_eq!(err, UpstreamError::NotFound);
    let server = gateway(&json!({ "data": { "removeClassFromSchedule": missing } })).await;
    let err = client(&dir, &server)
        .remove_from_schedule(&scheduled_id())
        .await
        .unwrap_err();
    assert_eq!(err, UpstreamError::NotFound);
    let server = gateway(&json!({ "data": { "rescheduleClass": missing } })).await;
    let err = client(&dir, &server)
        .reschedule(&scheduled_id(), at("2030-01-07T13:00:00Z"))
        .await
        .unwrap_err();
    assert_eq!(err, UpstreamError::NotFound);
}

#[tokio::test]
async fn gql_retries_once_after_401() {
    let server = MockServer::start().await;
    rotation().expect(1).mount(&server).await;
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
            "variables": { "pelotonClassId": encoded(&ride(1)) },
        })))
        .respond_with(answer(&fixture("check_class_in_stack.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let found = client(&dir, &server)
        .class_in_stack(&ride(1))
        .await
        .unwrap();
    assert!(!found);
}

fn unauthenticated() -> Value {
    json!({
        "data": null,
        "errors": [{
            "message": "secret-xyz",
            "extensions": { "code": "UNAUTHENTICATED" },
        }],
    })
}

#[tokio::test]
async fn in_band_unauthenticated_refreshes_and_retries_once() {
    let server = MockServer::start().await;
    rotation().expect(1).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("authorization", "Bearer tok-1"))
        .respond_with(answer(&unauthenticated()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("authorization", "Bearer tok-2"))
        .respond_with(answer(&fixture("schedule_add.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let scheduled = client(&dir, &server)
        .add_to_schedule(&fixture_ride(), at("2030-01-07T12:00:00Z"))
        .await
        .unwrap();
    assert_eq!(scheduled.id, scheduled_id());
}

#[tokio::test]
async fn in_band_unauthenticated_twice_is_not_authenticated() {
    let server = MockServer::start().await;
    rotation().expect(1).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(answer(&unauthenticated()))
        .expect(2)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let err = client(&dir, &server)
        .add_to_schedule(&ride(1), at("2030-01-07T12:00:00Z"))
        .await
        .unwrap_err();
    assert_eq!(err, UpstreamError::NotAuthenticated);
}

#[tokio::test]
async fn other_in_band_errors_do_not_refresh() {
    let server = MockServer::start().await;
    rotation().expect(0).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(answer(&json!({
            "errors": [{
                "message": "User is not authorized",
                "extensions": { "code": "FORBIDDEN" },
            }],
        })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let err = client(&dir, &server).stack().await.unwrap_err();
    assert_eq!(err, UpstreamError::GraphQl);
}

#[tokio::test]
async fn gql_maps_http_statuses() {
    for (status, retry_after, expected) in [
        (404, None, UpstreamError::NotFound),
        (500, None, UpstreamError::Upstream { status: 500 }),
        (400, None, UpstreamError::Upstream { status: 400 }),
        (
            429,
            Some("30"),
            UpstreamError::RateLimited {
                retry_after: Some(StdDuration::from_secs(30)),
            },
        ),
        (429, None, UpstreamError::RateLimited { retry_after: None }),
        (
            429,
            Some("soon"),
            UpstreamError::RateLimited { retry_after: None },
        ),
    ] {
        let server = MockServer::start().await;
        let mut response = ResponseTemplate::new(status).set_body_string("secret-xyz");
        if let Some(value) = retry_after {
            response = response.insert_header("Retry-After", value);
        }
        Mock::given(method("POST"))
            .respond_with(response)
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let err = client(&dir, &server).stack().await.unwrap_err();
        assert_eq!(err, expected, "status {status} {retry_after:?}");
    }
}

/// Every write, run once against `server`.
async fn every_write(peloton: &Peloton) -> Vec<Result<(), UpstreamError>> {
    let start = at("2030-01-07T12:00:00Z");
    vec![
        peloton.add_to_schedule(&ride(1), start).await.map(|_| ()),
        peloton.reschedule(&scheduled_id(), start).await.map(|_| ()),
        peloton.remove_from_schedule(&scheduled_id()).await,
        peloton.add_to_stack(&ride(1)).await,
        peloton
            .modify_stack(&[OnDemandClassId::from_ride(&ride(1))])
            .await,
    ]
}

#[tokio::test]
async fn writes_are_sent_once_on_429() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "30"))
        .expect(5)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    for result in every_write(&client(&dir, &server)).await {
        assert_eq!(
            result,
            Err(UpstreamError::RateLimited {
                retry_after: Some(StdDuration::from_secs(30))
            })
        );
    }
}

#[tokio::test]
async fn writes_are_sent_once_on_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(503))
        .expect(5)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    for result in every_write(&client(&dir, &server)).await {
        assert_eq!(result, Err(UpstreamError::Upstream { status: 503 }));
    }
}

#[tokio::test]
async fn writes_are_sent_once_on_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(answer(&fixture("stack_modify.json")).set_delay(StdDuration::from_secs(2)))
        .expect(5)
        .mount(&server)
        .await;
    let impatient = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(StdDuration::from_secs(5))
        .timeout(StdDuration::from_millis(100))
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    for result in every_write(&client_with(&dir, &server, impatient)).await {
        assert_eq!(result, Err(UpstreamError::Transport));
    }
}

#[tokio::test]
async fn stack_reads_the_whole_stack() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(query_param("query", "ViewUserStack"))
        .and(body_json(
            json!({ "query": VIEW_USER_STACK, "variables": {} }),
        ))
        .respond_with(answer(&json!({ "data": {
            "viewUserStack": fixture("stack_add.json")["data"]["addClassToStack"],
        } })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let stack = client(&dir, &server).stack().await.unwrap();
    assert_eq!(stack.num_classes, Some(1));
    assert_eq!(stack.total_time, Some(1800));
    let class = stack.user_stack.stacked_class_list[0]
        .peloton_class
        .as_ref()
        .unwrap();
    assert_eq!(class.class_id, Some(fixture_ride()));
}

/// A `viewUserStack` root holding `items` and claiming `num_classes`.
fn stack_root(num_classes: Option<u32>, items: &[Value]) -> Value {
    json!({ "data": { "viewUserStack": {
        "__typename": "StackResponseSuccess",
        "numClasses": num_classes,
        "totalTime": 0,
        "userStack": { "stackedClassList": items },
    } } })
}

/// A Stack item for `ride` whose `joinToken` is `token`.
fn stacked(play_order: Option<u32>, ride: &PelotonId, token: &Value) -> Value {
    json!({
        "playOrder": play_order,
        "pelotonClass": {
            "__typename": "OnDemandInstructorClass",
            "classId": ride.to_string(),
            "joinToken": token,
            "title": "secret-title",
        },
    })
}

#[tokio::test]
async fn stack_refuses_a_partial_list() {
    for claimed in [Some(3), None, Some(1)] {
        let items = [
            stacked(Some(0), &ride(1), &json!(encoded(&ride(1)))),
            stacked(Some(1), &ride(2), &json!(encoded(&ride(2)))),
        ];
        let server = gateway(&stack_root(claimed, &items)).await;
        let dir = tempfile::tempdir().unwrap();
        let err = client(&dir, &server).stack().await.unwrap_err();
        assert_eq!(
            err,
            UpstreamError::Decode {
                path: "data.viewUserStack.numClasses".to_owned()
            },
            "{claimed:?}"
        );
    }
}

#[tokio::test]
async fn stack_ops_send_pelotons_class_ids() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(query_param("mutation", "AddClassToStack"))
        .and(body_json(json!({
            "query": ADD_CLASS_TO_STACK,
            "variables": { "input": { "pelotonClassId": encoded(&fixture_ride()) } },
        })))
        .respond_with(answer(&fixture("stack_add.json")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(query_param("mutation", "ModifyStack"))
        .and(body_json(json!({
            "query": MODIFY_STACK,
            "variables": { "input": { "pelotonClassIdList": [
                encoded(&ride(2)),
                encoded(&ride(1)),
            ] } },
        })))
        .respond_with(answer(&fixture("stack_modify.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let peloton = client(&dir, &server);
    peloton.add_to_stack(&fixture_ride()).await.unwrap();
    let ids = [
        OnDemandClassId::from_ride(&ride(2)),
        OnDemandClassId::from_ride(&ride(1)),
    ];
    peloton.modify_stack(&ids).await.unwrap();
}

#[tokio::test]
async fn stack_writes_succeed_whatever_stack_peloton_lists() {
    // Peloton applied the write; an odd Stack listed with the answer must
    // not report it as failed.
    let mut odd = stack_root(Some(3), &[]);
    let root = odd["data"]["viewUserStack"].take();
    let server = gateway(&json!({ "data": {
        "addClassToStack": root.clone(),
        "modifyStack": root,
    } }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let peloton = client(&dir, &server);
    peloton.add_to_stack(&ride(1)).await.unwrap();
    peloton.modify_stack(&[]).await.unwrap();
    for success in [
        json!({ "__typename": "StackResponseSuccess" }),
        json!({ "__typename": "StackResponseSuccess", "userStack": null }),
    ] {
        let server = gateway(&json!({ "data": { "modifyStack": success } })).await;
        client(&dir, &server).modify_stack(&[]).await.unwrap();
    }
}

#[tokio::test]
async fn stack_write_without_success_is_upstream_error() {
    for root in [
        json!({ "__typename": "StackResponseError", "message": "secret-xyz" }),
        json!({ "numClasses": 0 }),
    ] {
        let server = gateway(&json!({ "data": { "addClassToStack": root } })).await;
        let dir = tempfile::tempdir().unwrap();
        let err = client(&dir, &server)
            .add_to_stack(&ride(1))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, UpstreamError::Decode { path } if path.starts_with("data.addClassToStack")),
            "{err:?}"
        );
        assert!(!format!("{err} {err:?}").contains("secret-xyz"));
    }
}

/// Peloton's own layout of an on-demand class id, which differs from the
/// relay's encoding, so a re-encoded id would not match.
fn issued(ride: &PelotonId) -> String {
    use base64::Engine as _;

    let json = format!(
        r#"{{"type":"on_demand","ride_id":"{ride}","studio_peloton_id":null,"home_peloton_id":null}}"#
    );
    let token = base64::engine::general_purpose::STANDARD.encode(json);
    assert_ne!(token, encoded(ride));
    token
}

#[tokio::test]
async fn stack_service_over_peloton_writes_back_join_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(query_param("query", "ViewUserStack"))
        .respond_with(answer(&stack_root(
            Some(3),
            &[
                stacked(Some(1), &ride(2), &json!(issued(&ride(2)))),
                stacked(Some(0), &ride(1), &json!(issued(&ride(1)))),
                stacked(Some(2), &ride(3), &json!(issued(&ride(3)))),
            ],
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(query_param("mutation", "ModifyStack"))
        .and(body_json(json!({
            "query": MODIFY_STACK,
            "variables": { "input": { "pelotonClassIdList": [
                issued(&ride(1)),
                issued(&ride(3)),
            ] } },
        })))
        .respond_with(answer(&fixture("stack_modify.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let stack = StackService::new(client(&dir, &server));
    stack.remove(&ride(2)).await.unwrap();
}

#[tokio::test]
async fn stack_service_view_marks_odd_items_undecodable() {
    let items = [
        stacked(Some(0), &ride(1), &json!(issued(&ride(1)))),
        // A join token for another ride than the item's class.
        stacked(Some(1), &ride(2), &json!(issued(&ride(9)))),
        stacked(Some(2), &ride(3), &json!(null)),
        stacked(Some(3), &ride(4), &json!("secret-not-base64")),
        json!({ "playOrder": 4, "pelotonClass": null }),
        json!({ "playOrder": 5, "pelotonClass": {
            "classId": "secret-odd",
            "joinToken": issued(&ride(6)),
        } }),
    ];
    let server = gateway(&stack_root(Some(6), &items)).await;
    let dir = tempfile::tempdir().unwrap();
    let entries = StackService::new(client(&dir, &server))
        .view()
        .await
        .unwrap();
    let orders: Vec<u32> = entries.iter().map(|entry| entry.play_order).collect();
    assert_eq!(orders, [0, 1, 2, 3, 4, 5]);
    assert_eq!(
        entries[0].class.as_ref().map(OnDemandClassId::as_str),
        Ok(issued(&ride(1)).as_str())
    );
    for entry in &entries[1..] {
        assert_eq!(entry.class, Err(Undecodable), "{entry:?}");
    }
}

#[tokio::test]
async fn stack_service_view_requires_play_orders() {
    let items = [
        stacked(Some(0), &ride(1), &json!(issued(&ride(1)))),
        stacked(None, &ride(2), &json!(issued(&ride(2)))),
    ];
    let server = gateway(&stack_root(Some(2), &items)).await;
    let dir = tempfile::tempdir().unwrap();
    let err = StackService::new(client(&dir, &server))
        .view()
        .await
        .unwrap_err();
    assert_eq!(
        err,
        StackError::Upstream(UpstreamError::Decode {
            path: "data.viewUserStack.userStack.stackedClassList[1].playOrder".to_owned()
        })
    );
}

#[tokio::test]
async fn stack_service_add_checks_then_appends() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(query_param("query", "CheckDoesClassExistInUserStack"))
        .respond_with(answer(
            &json!({ "data": { "checkDoesClassExistInUserStack": true } }),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(query_param("mutation", "AddClassToStack"))
        .respond_with(answer(&fixture("stack_add.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let stack = StackService::new(client(&dir, &server));
    assert_eq!(
        stack.add(&fixture_ride()).await,
        Err(StackError::AlreadyInStack)
    );
    server.reset().await;
    Mock::given(method("POST"))
        .and(query_param("query", "CheckDoesClassExistInUserStack"))
        .respond_with(answer(&fixture("check_class_in_stack.json")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(query_param("mutation", "AddClassToStack"))
        .and(body_json(json!({
            "query": ADD_CLASS_TO_STACK,
            "variables": { "input": { "pelotonClassId": encoded(&fixture_ride()) } },
        })))
        .respond_with(answer(&fixture("stack_add.json")))
        .expect(1)
        .mount(&server)
        .await;
    stack.add(&fixture_ride()).await.unwrap();
}

#[tokio::test]
async fn stack_service_replace_reports_a_failed_modify_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(query_param("query", "ViewUserStack"))
        .respond_with(answer(&stack_root(
            Some(1),
            &[stacked(Some(0), &ride(1), &json!(issued(&ride(1))))],
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(query_param("mutation", "ModifyStack"))
        .respond_with(ResponseTemplate::new(500).set_body_string("secret-xyz"))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let stack = StackService::new(client(&dir, &server));
    let err = stack.replace(&[ride(2), ride(1)]).await.unwrap_err();
    assert_eq!(
        err,
        StackError::Upstream(UpstreamError::Upstream { status: 500 })
    );
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
            tokio::spawn(async move { peloton.class_in_stack(&ride(1)).await })
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
