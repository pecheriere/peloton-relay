//! Tests for the HTTP API, driving the router against a mock Peloton.

use peloton_relay_test_support::{api, common};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use peloton::upstream as wire;
use peloton_relay::api::{AccountRegistry, openapi};
use peloton_relay::model;
use serde_json::Value;
use time::OffsetDateTime;
use tower::ServiceExt as _;
use tracing::Instrument as _;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use peloton_relay_test_support::api::{
    ACCESS, REFRESH, assert_error, bare, fixture, send, with_json,
};

const WORKOUT: &str = "fedcba9876543210fedcba9876543210";
/// The relay clock: these routes never read it.
const NOW: OffsetDateTime = time::macros::datetime!(2030-01-01 00:00 UTC);

fn json(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "application/json")
}

fn app(dir: &tempfile::TempDir, server: &MockServer, seeded: bool) -> Router {
    api::relay(dir, server, seeded, NOW)
}

async fn get(app: &Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    send(app, bare("GET", uri)).await
}

fn parse(body: &str) -> Value {
    serde_json::from_str(body).unwrap()
}

fn expected<T: serde::Serialize>(value: &T) -> Value {
    // Through text, exactly as a client sees it: `to_value` and the parser
    // round floats differently.
    parse(&serde_json::to_string(value).unwrap())
}

async fn upstream_hits(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

/// Answers `GET /workout/{WORKOUT}` with a workout of the credential owner,
/// which `/metrics` fetches to check ownership.
async fn mount_own_workout(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!("/api/workout/{WORKOUT}")))
        .respond_with(json(fixture("workout-cycling-class.json")))
        .mount(server)
        .await;
}

#[tokio::test]
async fn me_maps_profile() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/me"))
        .respond_with(json(fixture("me.json")))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, headers, body) = get(&app(&dir, &server, true), "/v1/me").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    let me: wire::Me = serde_json::from_str(&fixture("me.json")).unwrap();
    assert_eq!(parse(&body), expected(&model::Profile::from_upstream(&me)));
}

#[tokio::test]
async fn workouts_map_page_and_forward_query() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/user/{}/workouts", common::USER)))
        .and(wiremock::matchers::query_param("page", "1"))
        .and(wiremock::matchers::query_param("limit", "5"))
        .and(wiremock::matchers::query_param(
            "from",
            "2026-01-02T03:04:05Z",
        ))
        .and(wiremock::matchers::query_param(
            "to",
            "2026-03-04T05:06:07Z",
        ))
        .respond_with(json(fixture("workouts.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = get(
        &app(&dir, &server, true),
        "/v1/workouts?page=1&limit=5&from=2026-01-02T03:04:05Z&to=2026-03-04T05:06:07Z",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let page: wire::WorkoutPage = serde_json::from_str(&fixture("workouts.json")).unwrap();
    assert_eq!(
        parse(&body),
        expected(&model::WorkoutPage::from_upstream(&page))
    );
}

#[tokio::test]
async fn workouts_default_to_page_0_limit_20() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/user/{}/workouts", common::USER)))
        .and(wiremock::matchers::query_param("page", "0"))
        .and(wiremock::matchers::query_param("limit", "20"))
        .respond_with(json(fixture("workouts.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, _) = get(&app(&dir, &server, true), "/v1/workouts").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn workout_detail_includes_class() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/workout/{WORKOUT}")))
        .respond_with(json(fixture("workout-cycling-class.json")))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = get(
        &app(&dir, &server, true),
        &format!("/v1/workouts/{WORKOUT}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let workout: wire::Workout =
        serde_json::from_str(&fixture("workout-cycling-class.json")).unwrap();
    let want = expected(&model::WorkoutDetail::from_upstream(&workout));
    assert!(want["class"].is_object());
    assert_eq!(parse(&body), want);
}

#[tokio::test]
async fn metrics_map_and_forward_every_n() {
    let server = MockServer::start().await;
    mount_own_workout(&server).await;
    Mock::given(method("GET"))
        .and(path(format!("/api/workout/{WORKOUT}/performance_graph")))
        .and(wiremock::matchers::query_param("every_n", "10"))
        .respond_with(json(fixture("performance_graph-cycling-class.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = get(
        &app(&dir, &server, true),
        &format!("/v1/workouts/{WORKOUT}/metrics?every_n=10"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let graph: wire::PerformanceGraph =
        serde_json::from_str(&fixture("performance_graph-cycling-class.json")).unwrap();
    assert_eq!(
        parse(&body),
        expected(&model::Metrics::from_upstream(&graph))
    );
}

#[tokio::test]
async fn metrics_default_every_n_is_1() {
    let server = MockServer::start().await;
    mount_own_workout(&server).await;
    Mock::given(method("GET"))
        .and(path(format!("/api/workout/{WORKOUT}/performance_graph")))
        .and(wiremock::matchers::query_param("every_n", "1"))
        .respond_with(json(fixture("performance_graph-cycling-class.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, _) = get(
        &app(&dir, &server, true),
        &format!("/v1/workouts/{WORKOUT}/metrics"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn class_maps_details() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/ride/{WORKOUT}/details")))
        .respond_with(json(fixture("ride_details-cycling-class.json")))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = get(&app(&dir, &server, true), &format!("/v1/classes/{WORKOUT}")).await;
    assert_eq!(status, StatusCode::OK);
    let details: wire::RideDetails =
        serde_json::from_str(&fixture("ride_details-cycling-class.json")).unwrap();
    assert_eq!(
        parse(&body),
        expected(&model::ClassDetails::from_upstream(&details))
    );
}

#[tokio::test]
async fn rejects_bad_id() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, &server, true);
    for uri in [
        "/v1/workouts/..%2Fme".to_owned(),
        "/v1/workouts/FEDCBA9876543210FEDCBA9876543210".to_owned(),
        "/v1/workouts/short".to_owned(),
        "/v1/workouts/..%2Fme/metrics".to_owned(),
        "/v1/classes/..%2Fme".to_owned(),
        "/v1/classes/FEDCBA9876543210FEDCBA9876543210".to_owned(),
    ] {
        let (status, _, body) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert_error(&body, "invalid_input");
    }
    assert_eq!(upstream_hits(&server).await, 0);
}

#[tokio::test]
async fn rejects_invalid_query() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, &server, true);
    for uri in [
        "/v1/workouts?bogus=1",
        "/v1/workouts?limit=0",
        "/v1/workouts?limit=101",
        "/v1/workouts?limit=abc",
        "/v1/workouts?page=-1",
        "/v1/workouts?from=yesterday",
        "/v1/workouts?to=2026-01-01",
        "/v1/workouts?from=2026-03-01T00:00:00Z&to=2026-01-01T00:00:00Z",
        "/v1/workouts?from=2026-03-01T00:00:00Z&to=2026-03-01T00:00:00Z",
        // Valid RFC 3339 whose UTC instant is past the year 9999.
        "/v1/workouts?from=9999-12-31T20:00:00-05:00",
        "/v1/workouts?to=9999-12-31T20:00:00-05:00",
        // Valid RFC 3339 whose UTC instant is before the year 0.
        "/v1/workouts?from=0000-01-01T00:30:00+01:00",
        "/v1/me?x=1",
    ] {
        let (status, headers, body) = get(&app, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(headers[header::CONTENT_TYPE], "application/json", "{uri}");
        assert_error(&body, "invalid_input");
    }
    for uri in [
        format!("/v1/workouts/{WORKOUT}/metrics?every_n=0"),
        format!("/v1/workouts/{WORKOUT}/metrics?every_n=61"),
        format!("/v1/workouts/{WORKOUT}/metrics?bogus=1"),
        format!("/v1/workouts/{WORKOUT}?x=1"),
        format!("/v1/classes/{WORKOUT}?x=1"),
    ] {
        let (status, _, body) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert_error(&body, "invalid_input");
    }
    assert_eq!(upstream_hits(&server).await, 0);
}

#[tokio::test]
async fn accepts_range_boundaries() {
    let server = MockServer::start().await;
    mount_own_workout(&server).await;
    Mock::given(method("GET"))
        .respond_with(json(fixture("workouts.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/workout/{WORKOUT}/performance_graph")))
        .respond_with(json(fixture("performance_graph-cycling-class.json")))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, &server, true);
    for uri in [
        "/v1/workouts?limit=1".to_owned(),
        "/v1/workouts?limit=100".to_owned(),
        format!("/v1/workouts/{WORKOUT}/metrics?every_n=60"),
    ] {
        let (status, _, body) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    }
}

#[tokio::test]
async fn unknown_route_and_method_are_json() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, &server, true);
    let (status, headers, body) = get(&app, "/v2/nothing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert_error(&body, "not_found");

    let response = app.clone().oneshot(bare("POST", "/v1/me")).await.unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_error(std::str::from_utf8(&body).unwrap(), "method_not_allowed");
    assert_eq!(upstream_hits(&server).await, 0);
}

#[tokio::test]
async fn upstream_not_found_is_404() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404).set_body_string("upstream-body-secret"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = get(
        &app(&dir, &server, true),
        &format!("/v1/workouts/{WORKOUT}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_error(&body, "not_found");
    assert!(!body.contains("upstream-body-secret"));
}

#[tokio::test]
async fn upstream_failures_are_502() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/me"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream-body-secret"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/ride/{WORKOUT}/details")))
        .respond_with(json("{\"not\":\"a ride\"}".to_owned()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, &server, true);
    for uri in ["/v1/me".to_owned(), format!("/v1/classes/{WORKOUT}")] {
        let (status, _, body) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{uri}");
        assert_error(&body, "upstream_error");
        assert!(!body.contains("upstream-body-secret"));
    }
}

#[tokio::test]
async fn not_authenticated_is_503() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, body) = get(&app(&dir, &server, false), "/v1/me").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_error(&body, "not_authenticated");
    assert_eq!(upstream_hits(&server).await, 0);
}

#[tokio::test]
async fn rate_limited_passes_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/me"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "17"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, headers, body) = get(&app(&dir, &server, true), "/v1/me").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers[header::RETRY_AFTER], "17");
    assert_error(&body, "upstream_rate_limited");
}

#[tokio::test]
async fn rate_limited_without_hint_has_no_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/me"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, headers, body) = get(&app(&dir, &server, true), "/v1/me").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!headers.contains_key(header::RETRY_AFTER));
    assert_error(&body, "upstream_rate_limited");
}

#[tokio::test]
async fn healthz_never_calls_upstream() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (status, _, _) = get(&app(&dir, &server, false), "/healthz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(upstream_hits(&server).await, 0);
}

#[tokio::test]
async fn readyz_is_ready_without_credentials() {
    let server = MockServer::start().await;
    for seeded in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (status, _, body) = send(
            &app(&dir, &server, seeded),
            api::request_as(None, "GET", "/readyz", None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "seeded {seeded}: {body}");
        assert_eq!(parse(&body), serde_json::json!({ "status": "ready" }));
    }
    assert_eq!(upstream_hits(&server).await, 0);
}

#[test]
fn openapi_documents_the_account_requirement() {
    let document = serde_json::to_value(openapi()).unwrap();
    let mut checked = 0;
    for (route, item) in document["paths"].as_object().unwrap() {
        for (verb, operation) in item.as_object().unwrap() {
            let forbidden = &operation["responses"]["403"];
            if route.starts_with("/v1/") {
                let description = forbidden["description"].as_str().unwrap_or_default();
                assert!(
                    description.contains("`forbidden`"),
                    "{verb} {route}: {forbidden}"
                );
                assert_eq!(
                    forbidden["content"]["application/json"]["schema"]["$ref"],
                    "#/components/schemas/ErrorBody",
                    "{verb} {route}"
                );
                checked += 1;
            } else {
                assert!(forbidden.is_null(), "{verb} {route} has a 403");
            }
        }
    }
    assert_eq!(checked, api::V1_ROUTES.len());
    for probe in ["/healthz", "/readyz"] {
        assert!(document["paths"][probe]["get"].is_object(), "{probe}");
    }
    let description = document["info"]["description"].as_str().unwrap();
    assert!(description.contains("X-Peloton-Account"), "{description}");
}

#[tokio::test]
async fn openapi_lists_all_v1_routes() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (status, headers, body) = get(&app(&dir, &server, false), "/openapi.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    let document = parse(&body);
    assert_eq!(document, serde_json::to_value(openapi()).unwrap());
    assert!(document["openapi"].as_str().unwrap().starts_with("3.1"));
    let paths = document["paths"].as_object().unwrap();
    for route in [
        "/v1/me",
        "/v1/workouts",
        "/v1/workouts/{id}",
        "/v1/workouts/{id}/metrics",
        "/v1/classes/{id}",
        "/healthz",
        "/readyz",
    ] {
        assert!(paths.contains_key(route), "missing {route}");
        assert!(paths[route]["get"].is_object(), "{route} has no GET");
    }
    let schemas = document["components"]["schemas"].as_object().unwrap();
    for schema in [
        "Profile",
        "WorkoutPage",
        "WorkoutDetail",
        "Metrics",
        "ClassDetails",
        "ErrorBody",
    ] {
        assert!(schemas.contains_key(schema), "missing schema {schema}");
    }
}

#[tokio::test]
async fn responses_never_contain_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/me"))
        .respond_with(json(fixture("me.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/user/{}/workouts", common::USER)))
        .respond_with(json(fixture("workouts.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/workout/{WORKOUT}")))
        .respond_with(json(fixture("workout-cycling-class.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/workout/{WORKOUT}/performance_graph")))
        .respond_with(json(fixture("performance_graph-cycling-class.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/ride/{WORKOUT}/details")))
        .respond_with(json(fixture("ride_details-cycling-class.json")))
        .mount(&server)
        .await;
    api::mount_every_new_upstream(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, &server, true);
    for request in new_route_requests() {
        let uri = request.uri().to_string();
        let (status, headers, body) = send(&app, request).await;
        assert!(status.as_u16() < 500, "{uri}: {status} {body}");
        assert_no_tokens(&uri, &headers, &body);
    }
    for uri in [
        "/v1/me".to_owned(),
        "/v1/workouts".to_owned(),
        format!("/v1/workouts/{WORKOUT}"),
        format!("/v1/workouts/{WORKOUT}/metrics"),
        format!("/v1/classes/{WORKOUT}"),
        "/v1/workouts?bogus=1".to_owned(),
        "/healthz".to_owned(),
        "/readyz".to_owned(),
        "/openapi.json".to_owned(),
        "/nope".to_owned(),
    ] {
        let (_, headers, body) = get(&app, &uri).await;
        assert_no_tokens(&uri, &headers, &body);
    }
}

fn assert_no_tokens(uri: &str, headers: &axum::http::HeaderMap, body: &str) {
    for secret in [ACCESS, REFRESH] {
        assert!(!body.contains(secret), "{uri} leaked a token");
        assert!(
            headers
                .values()
                .all(|v| !v.to_str().unwrap_or_default().contains(secret)),
            "{uri} leaked a token in headers"
        );
    }
}

/// One request to each library, schedule and Stack route, reads and writes,
/// plus one rejected write.
fn new_route_requests() -> Vec<Request<Body>> {
    let (_, scheduled) = api::fixture_scheduled_id();
    let class = "00000000000000000000000000000f1d";
    let start = serde_json::json!("2030-01-07T12:00:00Z");
    let writes = [
        with_json(
            "POST",
            "/v1/schedule",
            &serde_json::json!({ "class_id": class, "start_time": start }),
        ),
        with_json(
            "PATCH",
            &format!("/v1/schedule/{scheduled}"),
            &serde_json::json!({ "start_time": start }),
        ),
        bare("DELETE", &format!("/v1/schedule/{scheduled}")),
        with_json(
            "POST",
            "/v1/stack",
            &serde_json::json!({ "class_id": class }),
        ),
        bare("DELETE", &format!("/v1/stack/{class}")),
        with_json(
            "PUT",
            "/v1/stack",
            &serde_json::json!({ "class_ids": [class] }),
        ),
        with_json(
            "POST",
            "/v1/stack",
            &serde_json::json!({ "class_id": "bad" }),
        ),
    ];
    let reads = [
        "/v1/library?category=cycling&duration=30".to_owned(),
        "/v1/library/filters?category=cycling".to_owned(),
        "/v1/schedule".to_owned(),
        format!("/v1/schedule/{scheduled}"),
        "/v1/stack".to_owned(),
    ];
    let mut requests: Vec<_> = reads.iter().map(|uri| bare("GET", uri)).collect();
    requests.extend(writes);
    requests
}

#[tokio::test]
async fn other_members_workouts_are_not_found() {
    let mut foreign: Value = parse(&fixture("workout-cycling-class.json"));
    foreign["user_id"] = "0000000000000000000000000000abcd".into();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/workout/{WORKOUT}")))
        .respond_with(json(foreign.to_string()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/workout/{WORKOUT}/performance_graph")))
        .respond_with(json(fixture("performance_graph-cycling-class.json")))
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, &server, true);
    for uri in [
        format!("/v1/workouts/{WORKOUT}"),
        format!("/v1/workouts/{WORKOUT}/metrics"),
    ] {
        let (status, _, body) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_error(&body, "not_found");
        assert!(!body.contains("abcd"), "{uri}: {body}");
    }
}

#[test]
fn openapi_describes_every_response() {
    let document = serde_json::to_value(openapi()).unwrap();
    let mut checked = 0;
    for (route, item) in document["paths"].as_object().unwrap() {
        for (verb, operation) in item.as_object().unwrap() {
            for (status, response) in operation["responses"].as_object().unwrap() {
                let description = response["description"].as_str().unwrap_or_default();
                assert!(
                    !description.trim().is_empty(),
                    "{verb} {route} {status} has no description"
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 0);
}

#[test]
fn openapi_documents_every_error_response() {
    let document = serde_json::to_value(openapi()).unwrap();
    let responses = |route: &str| document["paths"][route]["get"]["responses"].clone();
    let id_routes = [
        "/v1/workouts/{id}",
        "/v1/workouts/{id}/metrics",
        "/v1/classes/{id}",
    ];
    for route in ["/v1/me", "/v1/workouts"].into_iter().chain(id_routes) {
        let responses = responses(route);
        for status in ["200", "400", "502", "503"] {
            assert!(responses[status].is_object(), "{route} lacks {status}");
        }
        for status in ["400", "502", "503"] {
            assert_eq!(
                responses[status]["content"]["application/json"]["schema"]["$ref"],
                "#/components/schemas/ErrorBody",
                "{route} {status}"
            );
        }
        let unavailable = responses["503"]["description"].as_str().unwrap();
        for code in ["not_authenticated", "upstream_rate_limited"] {
            assert!(unavailable.contains(code), "{route}: {unavailable}");
        }
        assert!(
            responses["503"]["headers"]["Retry-After"].is_object(),
            "{route}"
        );
        let failed = responses["502"]["description"].as_str().unwrap();
        assert!(failed.contains("upstream_error"), "{route}: {failed}");
        assert!(
            responses["400"]["description"]
                .as_str()
                .unwrap()
                .contains("invalid_input"),
            "{route}"
        );
    }
    for route in id_routes {
        let not_found = responses(route)["404"]["description"].clone();
        assert!(
            not_found.as_str().unwrap().contains("not_found"),
            "{route}: {not_found}"
        );
    }
    assert!(responses("/readyz")["503"].is_null());
    // Unknown routes and wrong methods are documented once, for the whole API.
    let description = document["info"]["description"].as_str().unwrap();
    for text in [
        "404",
        "not_found",
        "405",
        "method_not_allowed",
        "500",
        "internal_error",
    ] {
        assert!(description.contains(text), "{description}");
    }
}

#[tokio::test]
async fn cannot_schedule_is_422_with_pelotons_message() {
    use axum::response::IntoResponse as _;
    use peloton_relay::error::ApiError;
    use peloton_relay::upstream::UpstreamError;

    let error = ApiError::from(UpstreamError::CannotSchedule {
        message: "This class can't be scheduled at that time.".to_owned(),
    });
    let response = error.into_response();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        serde_json::json!({
            "error": "cannot_schedule",
            "message": "This class can't be scheduled at that time.",
        })
    );
}

#[tokio::test]
async fn oversized_upstream_body_is_upstream_error() {
    use axum::response::IntoResponse as _;
    use peloton_relay::error::ApiError;
    use peloton_relay::upstream::UpstreamError;

    let response = ApiError::from(UpstreamError::BodyTooLarge).into_response();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["error"],
        "upstream_error"
    );
}

/// The id the relay gave the request that answered with `headers`.
fn request_id(headers: &axum::http::HeaderMap) -> String {
    headers["x-request-id"].to_str().unwrap().to_owned()
}

/// The JSON log lines of the request `id`.
fn request_logs(id: &str) -> Vec<Value> {
    common::captured_logs()
        .contents()
        .lines()
        .filter(|line| line.contains(id))
        .map(parse)
        .collect()
}

#[tokio::test]
async fn upstream_failure_log_carries_the_route_and_request_id() {
    let logs = common::captured_logs();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, &server, true);
    for (uri, route) in [
        (format!("/v1/workouts/{WORKOUT}"), "/v1/workouts/{id}"),
        ("/v1/workouts?limit=7".to_owned(), "/v1/workouts"),
    ] {
        let (status, headers, _) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{uri}");
        let id = request_id(&headers);
        let lines = request_logs(&id);
        let warning = lines
            .iter()
            .find(|line| line["message"] == "upstream request failed")
            .unwrap_or_else(|| panic!("no upstream warning for {id}: {}", logs.contents()));
        assert_eq!(warning["level"], "WARN");
        assert_eq!(warning["span"]["route"], route);
        assert_eq!(warning["span"]["request_id"], id);
        assert_eq!(warning["span"]["method"], "GET");
        // The span holds the route template and nothing else of the
        // request: no query string and no id from the path.
        let line = warning.to_string();
        assert!(!line.contains(WORKOUT), "{line}");
        assert!(!line.contains("limit=7"), "{line}");
    }
}

#[tokio::test]
async fn panic_log_and_500_carry_the_request_id_and_route() {
    use axum::routing::get;

    // Installs the capture before anything logs.
    common::captured_logs();
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let accounts = AccountRegistry::new(vec![api::alice(&dir, &server, true)]).unwrap();
    let app = peloton_relay::api::request_handling(
        Router::new().route("/boom/{id}", get(boom)),
        Arc::new(accounts),
    );
    let (status, headers, _) =
        get_with_span(&app, "/boom/zq7-sentinel?q=1", "panic_request_context").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let id = request_id(&headers);
    let panic_line = request_logs(&id)
        .into_iter()
        .find(|line| line["event"] == "handler_panicked")
        .unwrap_or_else(|| panic!("no panic log for {id}"));
    assert_eq!(panic_line["span"]["route"], "/boom/{id}");
    assert_eq!(panic_line["span"]["request_id"], id);
    let line = panic_line.to_string();
    assert!(
        !line.contains("zq7-sentinel") && !line.contains("q=1"),
        "{line}"
    );
}

#[tokio::test]
async fn every_request_gets_its_own_request_id() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, &server, true);
    let mut ids = Vec::new();
    for uri in ["/healthz", "/healthz", "/nowhere"] {
        let mut request = bare("GET", uri);
        request.headers_mut().insert(
            "x-request-id",
            axum::http::HeaderValue::from_static("client-chosen-id"),
        );
        let (_, headers, _) = send(&app, request).await;
        ids.push(request_id(&headers));
    }
    for id in &ids {
        assert_eq!(id.len(), 32, "{id}");
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3);
}

#[tokio::test]
async fn graphql_failure_is_upstream_error() {
    use axum::response::IntoResponse as _;
    use peloton_relay::error::ApiError;
    use peloton_relay::upstream::UpstreamError;

    let response = ApiError::from(UpstreamError::GraphQl).into_response();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["error"],
        "upstream_error"
    );
}

#[tokio::test]
async fn panicking_handler_answers_500_without_the_payload() {
    use axum::routing::get;

    let logs = common::captured_logs();
    let app = Router::new()
        .route("/boom", get(boom))
        .layer(peloton_relay::api::catch_panics());
    let (status, headers, body) = get_with_span(&app, "/boom", "handler_panic").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(
        parse(&body),
        serde_json::json!({ "error": "internal_error", "message": "The relay failed to handle the request." })
    );
    let ours: String = logs
        .contents()
        .lines()
        .filter(|line| line.contains("handler_panic"))
        .collect();
    assert!(ours.contains("handler_panicked"), "{ours}");
    assert!(!ours.contains("secret-payload"), "{ours}");
}

/// A handler that always panics, with a payload that must not leak.
async fn boom() -> StatusCode {
    panic!("secret-payload");
}

/// `GET uri` inside a span named `case`.
async fn get_with_span(
    app: &Router,
    uri: &str,
    case: &str,
) -> (StatusCode, axum::http::HeaderMap, String) {
    send(app, bare("GET", uri))
        .instrument(tracing::info_span!("case", name = case))
        .await
}
