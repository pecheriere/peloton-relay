//! Tests for the schedule and Stack routes: reads, validated writes and their
//! audit events, driving the router against a mock Peloton gateway.

use axum::Router;
use axum::http::{StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use peloton::graphql::ops;
use peloton::ids::{OnDemandClassId, PelotonId};
use peloton::upstream as wire;
use peloton_relay::model;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::macros::datetime;
use tracing::Instrument as _;
use wiremock::matchers::{body_partial_json, method, path, query_param};
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
    ACCESS, REFRESH, assert_error, bare, fixture_json, fixture_scheduled_id, mount_op, send,
    stack_with_one_class, with_json,
};
use common::captured_logs;

/// The relay clock: a week before the fixtures' scheduled class.
const NOW: OffsetDateTime = datetime!(2030-01-01 00:00 UTC);
/// The ride of every scrubbed schedule and Stack fixture.
const RIDE: &str = "00000000000000000000000000000f1d";

fn ride(n: u16) -> PelotonId {
    format!("{n:032x}").parse().unwrap()
}

fn encoded(ride: &PelotonId) -> String {
    OnDemandClassId::from_ride(ride).as_str().to_owned()
}

fn parse(body: &str) -> Value {
    serde_json::from_str(body).unwrap()
}

fn expected<T: serde::Serialize>(value: &T) -> Value {
    parse(&serde_json::to_string(value).unwrap())
}

fn app(dir: &tempfile::TempDir, server: &MockServer) -> Router {
    api_common::relay(dir, server, true, NOW)
}

async fn upstream_hits(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

/// The root field of a GraphQL fixture's `data`.
fn root(name: &str, field: &str) -> Value {
    fixture_json(name)["data"][field].clone()
}

fn scheduled_class(name: &str, field: &str) -> wire::ScheduledClass {
    serde_json::from_value(root(name, field)).unwrap()
}

/// A `ViewUserStack` answer listing `rides` in order, with Peloton's tokens.
fn stack_of(rides: &[PelotonId]) -> Value {
    let template =
        stack_with_one_class()["data"]["viewUserStack"]["userStack"]["stackedClassList"][0].clone();
    let items: Vec<Value> = rides
        .iter()
        .enumerate()
        .map(|(order, ride)| {
            let mut item = template.clone();
            item["playOrder"] = json!(order);
            item["pelotonClass"]["classId"] = json!(ride.to_string());
            item["pelotonClass"]["joinToken"] = json!(encoded(ride));
            item
        })
        .collect();
    json!({ "data": { "viewUserStack": {
        "__typename": "StackResponseSuccess",
        "numClasses": items.len(),
        "totalTime": 1800 * items.len(),
        "userStack": { "stackedClassList": items },
    } } })
}

#[tokio::test]
async fn schedule_lists_the_next_14_days_by_default() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param("query", ops::YOUR_SCHEDULE_ITEMS.name))
        .and(body_partial_json(json!({ "variables": {
            "startTime": "2030-01-01T00:00:00Z",
            "endTime": "2030-01-15T00:00:00Z",
        } })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture_json("schedule_during.json")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = send(&app(&dir, &server), bare("GET", "/v1/schedule")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items: Vec<wire::ScheduleItem> = serde_json::from_value(
        root("schedule_during.json", "userScheduledItemsList")["yourScheduleItems"].clone(),
    )
    .unwrap();
    assert_eq!(
        parse(&body),
        expected(&model::ScheduleList::from_upstream(&items))
    );
    let (_, url_safe) = fixture_scheduled_id();
    let item = &parse(&body)["items"][0];
    assert_eq!(item["scheduled_id"], url_safe.as_str());
    assert_eq!(item["kind"], "on_demand");
    assert_eq!(item["start_time"], "2030-01-07T12:00:00Z");
    assert_eq!(item["end_time"], "2030-01-07T12:30:00Z");
    assert_eq!(item["class"]["class_id"], RIDE);
    assert_eq!(item["class"]["title"], "30 min Groove Ride");
    assert_eq!(item["class"]["discipline"], "cycling");
    assert_eq!(item["class"]["duration_s"], 1800);
}

#[tokio::test]
async fn schedule_forwards_the_window_and_lists_other_kinds() {
    let server = MockServer::start().await;
    let mut list = fixture_json("schedule_during.json");
    list["data"]["userScheduledItemsList"]["yourScheduleItems"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "__typename": "InviteDetails", "inviteId": "x" }));
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(json!({ "variables": {
            "startTime": "2030-01-06T16:00:00Z",
            "endTime": "2030-01-08T00:00:00Z",
        } })))
        .respond_with(ResponseTemplate::new(200).set_body_json(list))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = send(
        &app(&dir, &server),
        bare(
            "GET",
            "/v1/schedule?from=2030-01-06T08:00:00-08:00&to=2030-01-08T00:00:00Z",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = parse(&body)["items"].clone();
    assert_eq!(items.as_array().unwrap().len(), 2);
    assert_eq!(
        items[1],
        json!({
            "scheduled_id": null,
            "kind": "InviteDetails",
            "start_time": null,
            "end_time": null,
            "class": null,
        })
    );
}

#[tokio::test]
async fn schedule_window_defaults_and_bounds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(json!({ "variables": {
            "startTime": "2030-01-03T00:00:00Z",
            "endTime": "2030-01-17T00:00:00Z",
        } })))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_json("schedule_after.json")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(json!({ "variables": {
            "startTime": "2030-01-01T00:00:00Z",
            "endTime": "2030-01-05T00:00:00Z",
        } })))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_json("schedule_after.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, &server);
    for uri in [
        "/v1/schedule?from=2030-01-03T00:00:00Z",
        "/v1/schedule?to=2030-01-05T00:00:00Z",
    ] {
        let (status, _, body) = send(&app, bare("GET", uri)).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert_eq!(parse(&body), json!({ "items": [] }));
    }
}

#[tokio::test]
async fn schedule_rejects_bad_windows() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, &server);
    for uri in [
        "/v1/schedule?from=2030-01-02T00:00:00",
        "/v1/schedule?from=tomorrow",
        "/v1/schedule?from=2030-01-05T00:00:00Z&to=2030-01-05T00:00:00Z",
        "/v1/schedule?from=2030-01-05T00:00:00Z&to=2030-01-04T00:00:00Z",
        "/v1/schedule?from=2030-01-01T00:00:00Z&to=2030-02-01T00:00:01Z",
        // The default window would end past the year 9999.
        "/v1/schedule?from=9999-12-31T23:59:59Z",
        // Valid RFC 3339 whose UTC instants are past the year 9999.
        "/v1/schedule?from=9999-12-31T20:00:00-05:00&to=9999-12-31T22:00:00-05:00",
        "/v1/schedule?to=9999-12-31T20:00:00-05:00",
        // Valid RFC 3339 whose UTC instant is before the year 0.
        "/v1/schedule?from=0000-01-01T00:30:00+01:00",
        "/v1/schedule?bogus=1",
    ] {
        let (status, _, body) = send(&app, bare("GET", uri)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
        assert_error(&body, "invalid_input");
    }
    assert_eq!(upstream_hits(&server).await, 0);
}

#[tokio::test]
async fn scheduled_class_reads_by_relay_id() {
    let server = MockServer::start().await;
    let (id, url_safe) = fixture_scheduled_id();
    assert_ne!(url_safe, id.as_str());
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param("query", ops::SCHEDULED_CLASS.name))
        .and(body_partial_json(
            json!({ "variables": { "id": id.as_str() } }),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture_json("scheduled_class.json")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = send(
        &app(&dir, &server),
        bare("GET", &format!("/v1/schedule/{url_safe}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        parse(&body),
        expected(&model::ScheduledClassView::from_upstream(&scheduled_class(
            "scheduled_class.json",
            "scheduledClass"
        )))
    );
    assert_eq!(parse(&body)["scheduled_id"], url_safe.as_str());
}

#[tokio::test]
async fn missing_scheduled_class_is_404() {
    let server = MockServer::start().await;
    let missing = json!({ "data": { "scheduledClass": {
        "__typename": "ScheduledClassDoesNotExistError", "code": "NOT_FOUND", "message": "gone",
    } } });
    mount_op(&server, &ops::SCHEDULED_CLASS, &missing, Some(1)).await;
    let (_, url_safe) = fixture_scheduled_id();
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = send(
        &app(&dir, &server),
        bare("GET", &format!("/v1/schedule/{url_safe}")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_error(&body, "not_found");
}

#[tokio::test]
async fn rejects_bad_scheduled_id() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, &server);
    let (id, url_safe) = fixture_scheduled_id();
    let on_demand = URL_SAFE_NO_PAD.encode(STANDARD.decode(encoded(&ride(1))).unwrap());
    let padded = format!("{url_safe}=");
    let other_type = URL_SAFE_NO_PAD.encode(
        String::from_utf8(URL_SAFE_NO_PAD.decode(&url_safe).unwrap())
            .unwrap()
            .replace("\"event\"", "\"on_demand\""),
    );
    for bad in [
        // Peloton's own form is not the relay's.
        url::form_urlencoded::byte_serialize(id.as_str().as_bytes()).collect::<String>(),
        padded,
        on_demand,
        other_type,
        "not-an-id".to_owned(),
        "A".repeat(600),
    ] {
        let uri = format!("/v1/schedule/{bad}");
        for request in [
            bare("GET", &uri),
            with_json(
                "PATCH",
                &uri,
                &json!({ "start_time": "2030-01-07T12:00:00Z" }),
            ),
            bare("DELETE", &uri),
        ] {
            let what = format!("{} {uri}", request.method());
            let (status, _, body) = send(&app, request).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {body}");
            assert_error(&body, "invalid_input");
        }
    }
    assert_eq!(upstream_hits(&server).await, 0);
}

#[tokio::test]
async fn schedule_add_sends_whole_utc_seconds_and_returns_the_class() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param("mutation", ops::ADD_CLASS_TO_SCHEDULE.name))
        .and(body_partial_json(json!({ "variables": {
            "id": encoded(&RIDE.parse().unwrap()),
            "startTime": "2030-01-07T12:00:00Z",
        } })))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_json("schedule_add.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, headers, body) = send(
        &app(&dir, &server),
        with_json(
            "POST",
            "/v1/schedule",
            &json!({ "class_id": RIDE, "start_time": "2030-01-07T04:00:00.789-08:00" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let view = model::ScheduledClassView::from_upstream(&scheduled_class(
        "schedule_add.json",
        "addClassToSchedule",
    ));
    assert_eq!(parse(&body), expected(&view));
    let (_, url_safe) = fixture_scheduled_id();
    assert_eq!(
        headers[header::LOCATION],
        format!("/v1/schedule/{url_safe}").as_str()
    );
}

#[tokio::test]
async fn rejects_bad_start_time() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, &server);
    let (_, url_safe) = fixture_scheduled_id();
    let starts = [
        json!("2030-01-07T12:00:00"),
        json!("2030-01-07T12:00Z"),
        json!("2030-01-07"),
        json!("next monday"),
        json!(1_894_017_600),
        json!(null),
        // In the past, or now: the clock reads 2030-01-01T00:00:00Z.
        json!("2029-12-31T23:59:59Z"),
        json!("2030-01-01T00:00:00Z"),
        json!("2030-01-01T00:00:00.999Z"),
        json!("2029-12-31T16:00:00-08:00"),
        // More than 14 days ahead.
        json!("2030-01-15T00:00:01Z"),
        json!("2030-01-14T16:00:01-08:00"),
        json!("9999-12-31T20:00:00-05:00"),
    ];
    for start in starts {
        for request in [
            with_json(
                "POST",
                "/v1/schedule",
                &json!({ "class_id": RIDE, "start_time": start }),
            ),
            with_json(
                "PATCH",
                &format!("/v1/schedule/{url_safe}"),
                &json!({ "start_time": start }),
            ),
        ] {
            let what = format!("{} {start}", request.method());
            let (status, _, body) = send(&app, request).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {body}");
            assert_error(&body, "invalid_input");
        }
    }
    assert_eq!(upstream_hits(&server).await, 0);
}

#[tokio::test]
async fn accepts_start_times_up_to_14_days_ahead() {
    let server = MockServer::start().await;
    mount_op(
        &server,
        &ops::ADD_CLASS_TO_SCHEDULE,
        &fixture_json("schedule_add.json"),
        Some(2),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, &server);
    for start in ["2030-01-01T00:00:01Z", "2030-01-14T16:00:00-08:00"] {
        let (status, _, body) = send(
            &app,
            with_json(
                "POST",
                "/v1/schedule",
                &json!({ "class_id": RIDE, "start_time": start }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{start}: {body}");
    }
}

#[tokio::test]
async fn write_bodies_are_strict() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, &server);
    let (_, url_safe) = fixture_scheduled_id();
    let start = "2030-01-07T12:00:00Z";
    let scheduled = format!("/v1/schedule/{url_safe}");
    let requests = [
        with_json("POST", "/v1/schedule", &json!({ "start_time": start })),
        with_json("POST", "/v1/schedule", &json!({ "class_id": RIDE })),
        with_json(
            "POST",
            "/v1/schedule",
            &json!({ "class_id": RIDE, "start_time": start, "notify": true }),
        ),
        with_json(
            "POST",
            "/v1/schedule",
            &json!({ "class_id": "XYZ", "start_time": start }),
        ),
        with_json("POST", "/v1/schedule", &json!([RIDE, start])),
        bare("POST", "/v1/schedule"),
        with_json(
            "PATCH",
            &scheduled,
            &json!({ "start_time": start, "class_id": RIDE }),
        ),
        with_json(
            "POST",
            "/v1/stack",
            &json!({ "class_id": RIDE, "position": 0 }),
        ),
        with_json(
            "POST",
            "/v1/stack",
            &json!({ "class_id": RIDE.to_uppercase() }),
        ),
        with_json("PUT", "/v1/stack", &json!({ "class_ids": RIDE })),
        with_json("PUT", "/v1/stack", &json!({ "class_ids": ["nope"] })),
        with_json("PUT", "/v1/stack", &json!({})),
        with_json(
            "PUT",
            "/v1/stack",
            &json!({ "class_ids": (0..101).map(|n| ride(n).to_string()).collect::<Vec<_>>() }),
        ),
        bare("DELETE", "/v1/stack/not-an-id"),
        bare("DELETE", &format!("/v1/stack/{RIDE}?force=1")),
        with_json(
            "POST",
            "/v1/schedule?dry_run=1",
            &json!({ "class_id": RIDE, "start_time": start }),
        ),
    ];
    for request in requests {
        let what = format!("{} {}", request.method(), request.uri());
        let (status, _, body) = send(&app, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {body}");
        assert_error(&body, "invalid_input");
    }
    assert_eq!(upstream_hits(&server).await, 0);
}

#[tokio::test]
async fn reschedule_returns_new_id() {
    let server = MockServer::start().await;
    let (id, url_safe) = fixture_scheduled_id();
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param("mutation", ops::RESCHEDULE_CLASS.name))
        .and(body_partial_json(json!({ "variables": {
            "id": id.as_str(),
            "startTime": "2030-01-07T13:00:00Z",
        } })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture_json("schedule_reschedule.json")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = send(
        &app(&dir, &server),
        with_json(
            "PATCH",
            &format!("/v1/schedule/{url_safe}"),
            &json!({ "start_time": "2030-01-07T14:00:00+01:00" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let moved = scheduled_class("schedule_reschedule.json", "rescheduleClass");
    assert_eq!(
        parse(&body),
        expected(&model::ScheduledClassView::from_upstream(&moved))
    );
    let new_id = parse(&body)["scheduled_id"].as_str().unwrap().to_owned();
    assert_ne!(new_id, url_safe);
    assert_eq!(new_id, moved.id.url_safe());
    assert_eq!(parse(&body)["start_time"], "2030-01-07T13:00:00Z");
}

#[tokio::test]
async fn remove_from_schedule_is_204() {
    let server = MockServer::start().await;
    let (id, url_safe) = fixture_scheduled_id();
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param(
            "mutation",
            ops::REMOVE_CLASS_FROM_SCHEDULE.name,
        ))
        .and(body_partial_json(
            json!({ "variables": { "id": id.as_str() } }),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture_json("schedule_remove.json")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = send(
        &app(&dir, &server),
        bare("DELETE", &format!("/v1/schedule/{url_safe}")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_empty(), "{body}");
}

#[tokio::test]
async fn cannot_schedule_is_422() {
    let server = MockServer::start().await;
    let refused = |field: &str| {
        json!({ "data": { field: {
            "__typename": "CannotScheduleClassError",
            "code": "CANNOT_SCHEDULE",
            "message": "This class can't be scheduled at that time.",
        } } })
    };
    mount_op(
        &server,
        &ops::ADD_CLASS_TO_SCHEDULE,
        &refused("addClassToSchedule"),
        Some(1),
    )
    .await;
    mount_op(
        &server,
        &ops::RESCHEDULE_CLASS,
        &refused("rescheduleClass"),
        Some(1),
    )
    .await;
    let (_, url_safe) = fixture_scheduled_id();
    let start = "2030-01-07T12:00:00Z";
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, &server);
    for request in [
        with_json(
            "POST",
            "/v1/schedule",
            &json!({ "class_id": RIDE, "start_time": start }),
        ),
        with_json(
            "PATCH",
            &format!("/v1/schedule/{url_safe}"),
            &json!({ "start_time": start }),
        ),
    ] {
        let (status, _, body) = send(&app, request).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(
            parse(&body),
            json!({
                "error": "cannot_schedule",
                "message": "This class can't be scheduled at that time.",
            })
        );
    }
}

#[tokio::test]
async fn stack_lists_classes_in_play_order_with_total_time() {
    let server = MockServer::start().await;
    let mut answer = stack_of(&[ride(1), ride(2)]);
    answer["data"]["viewUserStack"]["userStack"]["stackedClassList"]
        .as_array_mut()
        .unwrap()
        .reverse();
    mount_op(&server, &ops::VIEW_USER_STACK, &answer, Some(1)).await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = send(&app(&dir, &server), bare("GET", "/v1/stack")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let stack: wire::UserStack =
        serde_json::from_value(answer["data"]["viewUserStack"].clone()).unwrap();
    assert_eq!(
        parse(&body),
        expected(&model::StackView::from_upstream(&stack))
    );
    let value = parse(&body);
    assert_eq!(value["total_s"], 3600);
    let ids: Vec<&str> = value["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["class_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [ride(1).to_string(), ride(2).to_string()]);
    assert_eq!(value["items"][0]["title"], "30 min Groove Ride");
}

#[tokio::test]
async fn stack_add_checks_appends_and_answers_the_stack() {
    let server = MockServer::start().await;
    let class: PelotonId = RIDE.parse().unwrap();
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param("query", ops::CHECK_CLASS_IN_STACK.name))
        .and(body_partial_json(
            json!({ "variables": { "pelotonClassId": encoded(&class) } }),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture_json("check_class_in_stack.json")),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param("mutation", ops::ADD_CLASS_TO_STACK.name))
        .and(body_partial_json(
            json!({ "variables": { "input": { "pelotonClassId": encoded(&class) } } }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_json("stack_add.json")))
        .expect(1)
        .mount(&server)
        .await;
    mount_op(
        &server,
        &ops::VIEW_USER_STACK,
        &stack_with_one_class(),
        Some(1),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = send(
        &app(&dir, &server),
        with_json("POST", "/v1/stack", &json!({ "class_id": RIDE })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let stack: wire::UserStack =
        serde_json::from_value(stack_with_one_class()["data"]["viewUserStack"].clone()).unwrap();
    assert_eq!(
        parse(&body),
        expected(&model::StackView::from_upstream(&stack))
    );
}

#[tokio::test]
async fn already_in_stack_is_409() {
    let server = MockServer::start().await;
    mount_op(
        &server,
        &ops::CHECK_CLASS_IN_STACK,
        &json!({ "data": { "checkDoesClassExistInUserStack": true } }),
        Some(1),
    )
    .await;
    mount_op(
        &server,
        &ops::ADD_CLASS_TO_STACK,
        &fixture_json("stack_add.json"),
        Some(0),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = send(
        &app(&dir, &server),
        with_json("POST", "/v1/stack", &json!({ "class_id": RIDE })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_error(&body, "already_in_stack");
}

#[tokio::test]
async fn delete_stack_item_writes_back_the_rest() {
    let server = MockServer::start().await;
    let before = stack_of(&[ride(1), ride(2), ride(3)]);
    let after = stack_of(&[ride(1), ride(3)]);
    // Read for the write, then read back after it.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param("query", ops::VIEW_USER_STACK.name))
        .respond_with(ResponseTemplate::new(200).set_body_json(&before))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param("query", ops::VIEW_USER_STACK.name))
        .respond_with(ResponseTemplate::new(200).set_body_json(&after))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param("mutation", ops::MODIFY_STACK.name))
        .and(body_partial_json(json!({ "variables": { "input": {
            "pelotonClassIdList": [encoded(&ride(1)), encoded(&ride(3))],
        } } })))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_json("stack_modify.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = send(
        &app(&dir, &server),
        bare("DELETE", &format!("/v1/stack/{}", ride(2))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(parse(&body)["items"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn delete_missing_stack_item_is_404() {
    let server = MockServer::start().await;
    mount_op(
        &server,
        &ops::VIEW_USER_STACK,
        &stack_with_one_class(),
        Some(1),
    )
    .await;
    mount_op(
        &server,
        &ops::MODIFY_STACK,
        &fixture_json("stack_modify.json"),
        Some(0),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = send(
        &app(&dir, &server),
        bare("DELETE", &format!("/v1/stack/{}", ride(9))),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_error(&body, "not_found");
}

#[tokio::test]
async fn stack_replace_sends_the_order_and_refuses_duplicates() {
    let server = MockServer::start().await;
    let rides = [ride(3), ride(1)];
    mount_op(
        &server,
        &ops::VIEW_USER_STACK,
        &stack_of(&[ride(1), ride(2)]),
        None,
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param("mutation", ops::MODIFY_STACK.name))
        .and(body_partial_json(json!({ "variables": { "input": {
            "pelotonClassIdList": [encoded(&rides[0]), encoded(&rides[1])],
        } } })))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_json("stack_modify.json")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param("mutation", ops::MODIFY_STACK.name))
        .and(body_partial_json(
            json!({ "variables": { "input": { "pelotonClassIdList": [] } } }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_json("stack_modify.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, &server);
    let ids: Vec<String> = rides.iter().map(ToString::to_string).collect();
    let (status, _, body) = send(
        &app,
        with_json("PUT", "/v1/stack", &json!({ "class_ids": ids })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, body) = send(
        &app,
        with_json("PUT", "/v1/stack", &json!({ "class_ids": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let twice = [ride(1).to_string(), ride(1).to_string()];
    let (status, _, body) = send(
        &app,
        with_json("PUT", "/v1/stack", &json!({ "class_ids": twice })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_error(&body, "invalid_input");
}

#[tokio::test]
async fn stack_write_answers_204_when_the_read_back_fails() {
    let server = MockServer::start().await;
    mount_op(
        &server,
        &ops::CHECK_CLASS_IN_STACK,
        &fixture_json("check_class_in_stack.json"),
        Some(1),
    )
    .await;
    mount_op(
        &server,
        &ops::ADD_CLASS_TO_STACK,
        &fixture_json("stack_add.json"),
        Some(1),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(query_param("query", ops::VIEW_USER_STACK.name))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = send(
        &app(&dir, &server),
        with_json("POST", "/v1/stack", &json!({ "class_id": RIDE })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(body.is_empty(), "{body}");
}

#[tokio::test]
async fn undecodable_stack_item_refuses_the_write() {
    let server = MockServer::start().await;
    let mut answer = stack_of(&[ride(1), ride(2)]);
    answer["data"]["viewUserStack"]["userStack"]["stackedClassList"][1]["pelotonClass"]["joinToken"] =
        json!("garbage");
    mount_op(&server, &ops::VIEW_USER_STACK, &answer, None).await;
    mount_op(
        &server,
        &ops::MODIFY_STACK,
        &fixture_json("stack_modify.json"),
        Some(0),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = send(
        &app(&dir, &server),
        bare("DELETE", &format!("/v1/stack/{}", ride(1))),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_error(&body, "upstream_error");
}

/// The audit events logged inside a span named `case`.
fn audit_events(case: &str) -> Vec<String> {
    captured_logs()
        .contents()
        .lines()
        .filter(|line| line.contains(case) && line.contains("peloton_write"))
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn writes_log_peloton_write_event() {
    let logs = captured_logs();
    let server = MockServer::start().await;
    api_common::mount_every_new_upstream(&server).await;
    let (_, url_safe) = fixture_scheduled_id();
    let moved = scheduled_class("schedule_reschedule.json", "rescheduleClass")
        .id
        .url_safe()
        .to_owned();
    let start = "2030-01-07T12:00:00Z";
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, &server);
    let cases = [
        (
            "write_schedule_add",
            with_json(
                "POST",
                "/v1/schedule",
                &json!({ "class_id": RIDE, "start_time": "2030-01-07T12:00:00.5Z" }),
            ),
            vec![
                "op=\"schedule_add\"".to_owned(),
                format!("class_id={RIDE}"),
                format!("start_time={start}"),
                "outcome=\"ok\"".to_owned(),
                format!("new_scheduled_id={url_safe}"),
            ],
        ),
        (
            "write_schedule_move",
            with_json(
                "PATCH",
                &format!("/v1/schedule/{url_safe}"),
                &json!({ "start_time": start }),
            ),
            vec![
                "op=\"schedule_move\"".to_owned(),
                format!("scheduled_id={url_safe}"),
                format!("start_time={start}"),
                format!("new_scheduled_id={moved}"),
                "outcome=\"ok\"".to_owned(),
            ],
        ),
        (
            "write_schedule_remove",
            bare("DELETE", &format!("/v1/schedule/{url_safe}")),
            vec![
                "op=\"schedule_remove\"".to_owned(),
                format!("scheduled_id={url_safe}"),
                "outcome=\"ok\"".to_owned(),
            ],
        ),
        (
            "write_stack_add",
            with_json("POST", "/v1/stack", &json!({ "class_id": RIDE })),
            vec![
                "op=\"stack_add\"".to_owned(),
                format!("class_id={RIDE}"),
                "outcome=\"ok\"".to_owned(),
            ],
        ),
        (
            "write_stack_remove_missing",
            bare("DELETE", &format!("/v1/stack/{}", ride(9))),
            vec![
                "op=\"stack_remove\"".to_owned(),
                format!("class_id={}", ride(9)),
                "outcome=\"not_found\"".to_owned(),
            ],
        ),
        (
            "write_stack_replace",
            with_json(
                "PUT",
                "/v1/stack",
                &json!({ "class_ids": [RIDE, ride(2).to_string()] }),
            ),
            vec![
                "op=\"stack_replace\"".to_owned(),
                format!("class_ids={RIDE},{}", ride(2)),
                "outcome=\"ok\"".to_owned(),
            ],
        ),
    ];
    for (case, request, fields) in cases {
        let (status, _, body) = send(&app, request)
            .instrument(tracing::info_span!("case", name = case))
            .await;
        assert!(status.as_u16() < 500, "{case}: {status} {body}");
        let events = audit_events(case);
        assert_eq!(events.len(), 1, "{case}: {events:?}");
        let event = &events[0];
        assert!(event.contains(" INFO "), "{case}: {event}");
        for field in fields {
            assert!(event.contains(&field), "{case} lacks {field}: {event}");
        }
    }
    let all = logs.contents();
    for secret in [ACCESS, REFRESH] {
        assert!(!all.contains(secret), "a token reached the logs");
    }
}

#[tokio::test]
async fn refused_writes_log_their_code_only() {
    // Installs the capture before anything logs.
    captured_logs();
    let server = MockServer::start().await;
    mount_op(
        &server,
        &ops::ADD_CLASS_TO_SCHEDULE,
        &json!({ "data": { "addClassToSchedule": {
            "__typename": "CannotScheduleClassError",
            "code": "CANNOT_SCHEDULE",
            "message": "secret-reason for the member",
        } } }),
        Some(1),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let case = "write_cannot_schedule";
    let (status, _, _) = send(
        &app(&dir, &server),
        with_json(
            "POST",
            "/v1/schedule",
            &json!({ "class_id": RIDE, "start_time": "2030-01-07T12:00:00Z" }),
        ),
    )
    .instrument(tracing::info_span!("case", name = case))
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let events = audit_events(case);
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(
        events[0].contains("outcome=\"cannot_schedule\""),
        "{events:?}"
    );
    let ours: String = captured_logs()
        .contents()
        .lines()
        .filter(|line| line.contains(case))
        .collect();
    assert!(!ours.contains("secret-reason"), "{ours}");
}

#[tokio::test]
async fn rejected_inputs_are_not_writes() {
    // Installs the capture before anything logs.
    captured_logs();
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let case = "write_rejected_input";
    let (status, _, _) = send(
        &app(&dir, &server),
        with_json(
            "POST",
            "/v1/schedule",
            &json!({ "class_id": RIDE, "start_time": "2029-01-01T00:00:00Z" }),
        ),
    )
    .instrument(tracing::info_span!("case", name = case))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(audit_events(case).is_empty(), "no audit event");
}

#[tokio::test]
async fn duplicate_stack_replace_is_rejected_before_any_write() {
    // Installs the capture before anything logs.
    captured_logs();
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let case = "write_duplicate_replace";
    let twice = [
        ride(1).to_string(),
        ride(2).to_string(),
        ride(1).to_string(),
    ];
    let (status, _, body) = send(
        &app(&dir, &server),
        with_json("PUT", "/v1/stack", &json!({ "class_ids": twice })),
    )
    .instrument(tracing::info_span!("case", name = case))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_error(&body, "invalid_input");
    assert!(audit_events(case).is_empty(), "no audit event");
    assert_eq!(upstream_hits(&server).await, 0);
}
