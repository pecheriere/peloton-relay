//! Tests for the Peloton upstream client.

use std::fs;
use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use peloton::body::MAX_RESPONSE_BODY_BYTES;
use peloton::ids::PelotonId;
use peloton_relay::tokens::TokenManager;
use peloton_relay::upstream::{LibraryQuery, LibrarySort, Peloton, UpstreamError, WorkoutQuery};
use time::Duration;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::Instrument as _;
use url::Url;
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[allow(
    dead_code,
    reason = "shared by several test crates, each using a subset of the helpers"
)]
mod common;
mod gated;
mod raw_http;

use common::{ACCOUNT, LOCK_WAIT, account, captured_logs, http, state, state_file};
use gated::GatedServer;

const WORKOUT: &str = "fedcba9876543210fedcba9876543210";

fn fixture(name: &str) -> String {
    fs::read_to_string(format!(
        "{}/../peloton/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn json(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "application/json")
}

fn at(text: &str) -> OffsetDateTime {
    OffsetDateTime::parse(text, &Rfc3339).unwrap()
}

fn workout_id() -> PelotonId {
    WORKOUT.parse().unwrap()
}

/// A client whose token manager is seeded with `access` as the current token.
fn client(dir: &tempfile::TempDir, server: &MockServer, access: Option<&str>) -> Peloton {
    client_at(dir, &server.uri(), access)
}

/// As [`client`], with every endpoint on the server at `uri`.
fn client_at(dir: &tempfile::TempDir, uri: &str, access: Option<&str>) -> Peloton {
    if let Some(access) = access {
        state_file(dir.path())
            .save(&state(access, "refresh-1", Duration::hours(48)), LOCK_WAIT)
            .unwrap();
    }
    let auth = peloton::oauth::AuthClient::new(http(), Url::parse(uri).unwrap());
    let tokens = TokenManager::new(
        account(ACCOUNT),
        state_file(dir.path()),
        auth,
        StdDuration::from_secs(3600),
    );
    let api = Url::parse(&format!("{uri}/api")).unwrap();
    Peloton::new(http(), api, Arc::new(tokens), 4)
}

fn rotation() -> Mock {
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "tok-2",
            "refresh_token": "refresh-2",
            "expires_in": 172_800,
            "token_type": "Bearer",
        })))
}

fn query() -> WorkoutQuery {
    WorkoutQuery {
        page: 0,
        limit: 20,
        from: None,
        to: None,
    }
}

#[tokio::test]
async fn sends_bearer_and_platform_headers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/me"))
        .and(header("authorization", "Bearer tok-1"))
        .and(header("peloton-platform", "web"))
        .respond_with(json(fixture("me.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let me = client(&dir, &server, Some("tok-1")).me().await.unwrap();
    assert_eq!(me.id.to_string(), "000000000000000000000000000000f2");
}

#[tokio::test]
async fn workouts_query_params() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/user/{}/workouts", common::USER)))
        .and(query_param("sort_by", "-created"))
        .and(query_param("joins", "ride,ride.instructor"))
        .and(query_param("page", "2"))
        .and(query_param("limit", "50"))
        .and(query_param("from", "2026-01-02T03:04:05Z"))
        .and(query_param("to", "2026-03-04T05:06:07Z"))
        .respond_with(json(fixture("workouts.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let page = client(&dir, &server, Some("tok-1"))
        .workouts(&WorkoutQuery {
            page: 2,
            limit: 50,
            // A non-UTC offset must be normalised to UTC on the wire.
            from: Some(at("2026-01-02T04:04:05+01:00")),
            to: Some(at("2026-03-04T05:06:07Z")),
        })
        .await
        .unwrap();
    assert!(!page.data.is_empty());
}

#[tokio::test]
async fn workouts_omit_unset_range() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(json(fixture("workouts.json")))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    client(&dir, &server, Some("tok-1"))
        .workouts(&query())
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    let url = requests[0].url.query().unwrap();
    assert!(!url.contains("from="), "{url}");
    assert!(!url.contains("to="), "{url}");
}

#[tokio::test]
async fn workout_detail_graph_and_ride_paths() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/workout/{WORKOUT}")))
        .and(query_param("joins", "ride,ride.instructor"))
        .respond_with(json(fixture("workout-cycling-class.json")))
        .expect(1)
        .mount(&server)
        .await;
    // The graph's ownership check: the workout without joins.
    Mock::given(method("GET"))
        .and(path(format!("/api/workout/{WORKOUT}")))
        .and(query_param_is_missing("joins"))
        .respond_with(json(fixture("workout-cycling-class.json")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/workout/{WORKOUT}/performance_graph")))
        .and(query_param("every_n", "5"))
        .respond_with(json(fixture("performance_graph-cycling-class.json")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/ride/{WORKOUT}/details")))
        .respond_with(json(fixture("ride_details-cycling-class.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let peloton = client(&dir, &server, Some("tok-1"));
    peloton.workout(&workout_id()).await.unwrap();
    peloton.performance_graph(&workout_id(), 5).await.unwrap();
    peloton.ride_details(&workout_id()).await.unwrap();
}

#[tokio::test]
async fn retries_once_after_401() {
    let server = MockServer::start().await;
    rotation().expect(1).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/me"))
        .and(header("authorization", "Bearer tok-1"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/me"))
        .and(header("authorization", "Bearer tok-2"))
        .respond_with(json(fixture("me.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let me = client(&dir, &server, Some("tok-1")).me().await.unwrap();
    assert_eq!(me.id.to_string(), "000000000000000000000000000000f2");
}

#[tokio::test]
async fn second_401_is_not_authenticated() {
    let server = MockServer::start().await;
    rotation().expect(1).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/me"))
        .respond_with(ResponseTemplate::new(401))
        .expect(2)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let err = client(&dir, &server, Some("tok-1")).me().await.unwrap_err();
    assert!(matches!(err, UpstreamError::NotAuthenticated), "{err:?}");
}

#[tokio::test]
async fn no_credential_means_no_upstream_call() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let err = client(&dir, &server, None).me().await.unwrap_err();
    assert!(matches!(err, UpstreamError::NotAuthenticated), "{err:?}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn maps_404_429_5xx() {
    let cases = [
        (404, None, "NotFound"),
        (429, Some("30"), "RateLimited { retry_after: Some(30s) }"),
        (429, None, "RateLimited { retry_after: None }"),
        (429, Some("soon"), "RateLimited { retry_after: None }"),
        (500, None, "Upstream { status: 500 }"),
        (503, None, "Upstream { status: 503 }"),
        (400, None, "Upstream { status: 400 }"),
    ];
    for (status, retry_after, expected) in cases {
        let server = MockServer::start().await;
        let mut response = ResponseTemplate::new(status);
        if let Some(value) = retry_after {
            response = response.insert_header("Retry-After", value);
        }
        Mock::given(method("GET"))
            .respond_with(response)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let err = client(&dir, &server, Some("tok-1"))
            .workout(&workout_id())
            .await
            .unwrap_err();
        assert_eq!(format!("{err:?}"), expected, "status {status}");
    }
}

#[tokio::test]
async fn decode_error_reports_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(json(r#"{"id":"not-an-id","name":"secret-xyz"}"#.to_owned()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let logs = captured_logs();
    let err = client(&dir, &server, Some("tok-1"))
        .me()
        .instrument(tracing::info_span!("case", name = "decode_error"))
        .await
        .unwrap_err();
    let UpstreamError::Decode { path } = &err else {
        panic!("expected Decode, got {err:?}");
    };
    assert_eq!(path, "id");
    let logged = logs.contents();
    let ours: String = logged
        .lines()
        .filter(|l| l.contains("decode_error"))
        .collect();
    assert!(ours.contains("\"path\":\"id\""), "{ours}");
    for text in [format!("{err} {err:?}"), ours] {
        assert!(!text.contains("secret-xyz"), "{text}");
        assert!(!text.contains("tok-1"), "{text}");
    }
}

#[tokio::test]
async fn upstream_error_body_not_exposed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom secret-xyz"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let logs = captured_logs();
    let err = client(&dir, &server, Some("tok-1"))
        .me()
        .instrument(tracing::info_span!("case", name = "body_not_exposed"))
        .await
        .unwrap_err();
    assert!(matches!(err, UpstreamError::Upstream { status: 500 }));
    assert!(!format!("{err} {err:?}").contains("secret-xyz"));
    assert!(!logs.contents().contains("secret-xyz"));
}

#[tokio::test]
async fn empty_page_is_ok() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(json(
            r#"{"data":[],"page":0,"page_count":0,"total":0}"#.to_owned(),
        ))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let page = client(&dir, &server, Some("tok-1"))
        .workouts(&query())
        .await
        .unwrap();
    assert!(page.data.is_empty());
    assert_eq!(page.total, 0);
}

#[tokio::test]
async fn redirect_is_not_followed() {
    // The shared client must report the redirect, not carry the bearer token
    // to wherever it points.
    let target = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(json(fixture("me.json")))
        .expect(0)
        .mount(&target)
        .await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/me"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", format!("{}/api/me", target.uri()).as_str()),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let err = client(&dir, &server, Some("tok-1")).me().await.unwrap_err();
    assert_eq!(err, UpstreamError::Upstream { status: 302 });
    assert!(target.received_requests().await.unwrap().is_empty());
}

const MAX_BODY: usize = MAX_RESPONSE_BODY_BYTES;

/// An empty workout page padded with trailing whitespace to `len` bytes.
fn page_of_len(len: usize) -> String {
    let page = r#"{"data":[],"page":0,"page_count":0,"total":0}"#;
    format!("{page}{}", " ".repeat(len - page.len()))
}

#[tokio::test]
async fn body_at_the_size_limit_is_read() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(json(page_of_len(MAX_BODY)))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let page = client(&dir, &server, Some("tok-1"))
        .workouts(&query())
        .await
        .unwrap();
    assert!(page.data.is_empty());
}

#[tokio::test]
async fn body_over_the_size_limit_is_rejected() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(json(page_of_len(MAX_BODY + 1)))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let err = client(&dir, &server, Some("tok-1"))
        .workouts(&query())
        .await
        .unwrap_err();
    assert_eq!(err, UpstreamError::BodyTooLarge);
}

#[tokio::test]
async fn declared_length_over_the_size_limit_is_rejected_before_the_body() {
    // The server promises more than the limit and sends nothing else: only
    // an early refusal, not waiting for the body, ends in `BodyTooLarge`.
    let uri = raw_http::serve_once(|mut stream| {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        )
        .unwrap();
        // Hold the connection until the client gives up on it.
        let mut sink = [0_u8; 1];
        let _ = stream.read(&mut sink);
    });
    let dir = tempfile::tempdir().unwrap();
    let err = client_at(&dir, &uri, Some("tok-1")).me().await.unwrap_err();
    assert_eq!(err, UpstreamError::BodyTooLarge);
}

#[tokio::test]
async fn streamed_body_over_the_size_limit_is_cut_off_and_logged() {
    // No `Content-Length`: the limit can only be enforced while reading.
    let uri = raw_http::serve_once(|mut stream| {
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .unwrap();
        let chunk = format!("{:x}\r\n{}\r\n", 64 * 1024, " ".repeat(64 * 1024));
        // Twice the limit at most; the client stops reading well before.
        for _ in 0..(2 * MAX_BODY / (64 * 1024)) {
            if stream.write_all(chunk.as_bytes()).is_err() {
                return;
            }
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let logs = captured_logs();
    let err = client_at(&dir, &uri, Some("tok-1"))
        .me()
        .instrument(tracing::info_span!(
            "case",
            name = "streamed_body_too_large"
        ))
        .await
        .unwrap_err();
    assert_eq!(err, UpstreamError::BodyTooLarge);
    let ours: String = logs
        .contents()
        .lines()
        .filter(|l| l.contains("streamed_body_too_large"))
        .collect();
    assert!(ours.contains("exceeds the size limit"), "{ours}");
}

#[tokio::test]
async fn limits_in_flight_requests() {
    let server = GatedServer::start(fixture("me.json")).await;
    let dir = tempfile::tempdir().unwrap();
    let peloton = Arc::new(client_at(&dir, &server.uri(), Some("tok-1")));
    let calls: Vec<_> = (0..10)
        .map(|_| {
            let peloton = Arc::clone(&peloton);
            tokio::spawn(async move { peloton.me().await })
        })
        .collect();
    // The cap lets four requests reach Peloton and holds the other six back.
    server.wait_for_arrivals(4).await;
    assert!(server.stays_quiet().await, "a fifth request got through");
    server.release();
    for call in calls {
        call.await.unwrap().unwrap();
    }
    assert_eq!(server.peak(), 4);
}

#[tokio::test]
async fn rejected_refreshed_token_does_not_refresh_again() {
    let server = MockServer::start().await;
    rotation().expect(1).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/me"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let logs = captured_logs();
    let peloton = client(&dir, &server, Some("tok-1"));
    async {
        for _ in 0..2 {
            let err = peloton.me().await.unwrap_err();
            assert!(matches!(err, UpstreamError::NotAuthenticated), "{err:?}");
        }
    }
    .instrument(tracing::info_span!("case", name = "refresh_storm"))
    .await;
    let requests = server.received_requests().await.unwrap();
    let refreshes = requests
        .iter()
        .filter(|r| r.url.path() == "/oauth/token")
        .count();
    assert_eq!(refreshes, 1);
    // tok-1, tok-2, then tok-2 once more with no retry: no point resending it.
    assert_eq!(requests.len() - refreshes, 3);
    let warnings = logs
        .contents()
        .lines()
        .filter(|l| l.contains("refresh_storm") && l.contains("peloton_unauthorized_after_refresh"))
        .count();
    assert_eq!(warnings, 1, "{}", logs.contents());
}

/// `fixture` with its top-level `user_id` replaced (`None` removes it).
fn workout_owned_by(name: &str, owner: Option<&str>) -> String {
    let mut value: serde_json::Value = serde_json::from_str(&fixture(name)).unwrap();
    let object = value.as_object_mut().unwrap();
    match owner {
        Some(owner) => object.insert("user_id".to_owned(), owner.into()),
        None => object.remove("user_id"),
    };
    value.to_string()
}

#[tokio::test]
async fn foreign_or_unowned_workout_is_not_found() {
    let other = "0000000000000000000000000000abcd";
    for body in [
        workout_owned_by("workout-cycling-class.json", Some(other)),
        workout_owned_by("workout-cycling-class.json", None),
        workout_owned_by("workout-cycling-class.json", Some("not-an-id")),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/api/workout/{WORKOUT}")))
            .respond_with(json(body))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/workout/{WORKOUT}/performance_graph")))
            .respond_with(json(fixture("performance_graph-cycling-class.json")))
            .expect(0)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let peloton = client(&dir, &server, Some("tok-1"));
        let err = peloton.workout(&workout_id()).await.unwrap_err();
        assert_eq!(err, UpstreamError::NotFound);
        let err = peloton
            .performance_graph(&workout_id(), 1)
            .await
            .unwrap_err();
        assert_eq!(err, UpstreamError::NotFound);
    }
}

#[tokio::test]
async fn graph_of_own_workout_checks_ownership_first() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/workout/{WORKOUT}")))
        .respond_with(json(fixture("workout-cycling-class.json")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/workout/{WORKOUT}/performance_graph")))
        .respond_with(json(fixture("performance_graph-cycling-class.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    client(&dir, &server, Some("tok-1"))
        .performance_graph(&workout_id(), 1)
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[0].url.path(), format!("/api/workout/{WORKOUT}"));
}

#[tokio::test]
async fn library_query_params() {
    let class_type = "00000000000000000000000000000f2c";
    let instructor = "00000000000000000000000000000f15";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/ride/archived"))
        .and(header("authorization", "Bearer tok-1"))
        .and(header("peloton-platform", "web"))
        .and(query_param("browse_category", "cycling"))
        .and(query_param("class_type_id", class_type))
        .and(query_param("instructor_id", instructor))
        .and(query_param("duration", "1200"))
        // Peloton has no difficulty filter, only a difficulty sort.
        .and(query_param_is_missing("difficulty"))
        .and(query_param("sort_by", "original_air_time"))
        .and(query_param("desc", "true"))
        .and(query_param("page", "3"))
        .and(query_param("limit", "10"))
        .respond_with(json(fixture("library_archived.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let page = client(&dir, &server, Some("tok-1"))
        .library(&LibraryQuery {
            category: Some("cycling".to_owned()),
            class_type: Some(class_type.parse().unwrap()),
            instructor: Some(instructor.parse().unwrap()),
            duration_s: Some(1200),
            sort: Some(LibrarySort {
                by: "original_air_time".to_owned(),
                desc: true,
            }),
            page: 3,
            limit: 10,
        })
        .await
        .unwrap();
    assert_eq!(page.data.len(), 5);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[0].url.query_pairs().count(), 8);
}

#[tokio::test]
async fn library_omits_unset_filters() {
    let server = MockServer::start().await;
    let mut mock = Mock::given(method("GET"))
        .and(path("/api/v2/ride/archived"))
        .and(query_param("page", "0"))
        .and(query_param("limit", "20"));
    for param in [
        "browse_category",
        "class_type_id",
        "instructor_id",
        "duration",
        "difficulty",
        "sort_by",
        "desc",
    ] {
        mock = mock.and(query_param_is_missing(param));
    }
    mock.respond_with(json(fixture("library_archived.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    client(&dir, &server, Some("tok-1"))
        .library(&LibraryQuery {
            category: None,
            class_type: None,
            instructor: None,
            duration_s: None,
            sort: None,
            page: 0,
            limit: 20,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn library_sort_ascending_says_so() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/ride/archived"))
        .and(query_param("sort_by", "difficulty"))
        .and(query_param("desc", "false"))
        .respond_with(json(fixture("library_archived.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    client(&dir, &server, Some("tok-1"))
        .library(&LibraryQuery {
            category: None,
            class_type: None,
            instructor: None,
            duration_s: None,
            sort: Some(LibrarySort {
                by: "difficulty".to_owned(),
                desc: false,
            }),
            page: 0,
            limit: 20,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn ride_filters_sends_platform_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/ride/filters"))
        .and(header("authorization", "Bearer tok-1"))
        .and(header("peloton-platform", "web"))
        .and(query_param("library_type", "on_demand"))
        .and(query_param("browse_category", "cycling"))
        .respond_with(json(fixture("ride_filters.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let filters = client(&dir, &server, Some("tok-1"))
        .ride_filters("cycling")
        .await
        .unwrap();
    assert!(!filters.filters.is_empty());
    assert!(!filters.sorts.is_empty());
}

#[tokio::test]
async fn browse_categories_lists_on_demand_categories() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/browse_categories"))
        .and(header("peloton-platform", "web"))
        .and(query_param("library_type", "on_demand"))
        .respond_with(json(fixture("browse_categories.json")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let categories = client(&dir, &server, Some("tok-1"))
        .browse_categories()
        .await
        .unwrap();
    assert!(!categories.browse_categories.is_empty());
}

#[tokio::test]
async fn unrepresentable_utc_times_fail_without_a_request() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let peloton = client(&dir, &server, Some("tok-1"))
        .with_graphql_url(Url::parse(&format!("{}/graphql", server.uri())).unwrap());
    // Valid RFC 3339, but past the year 9999 once in UTC.
    let late = OffsetDateTime::parse("9999-12-31T20:00:00-05:00", &Rfc3339).unwrap();
    let early = OffsetDateTime::parse("9999-12-31T19:00:00-05:00", &Rfc3339).unwrap();
    let workouts = peloton
        .workouts(&WorkoutQuery {
            from: Some(late),
            ..query()
        })
        .await;
    assert_eq!(workouts.unwrap_err(), UpstreamError::Transport);
    assert_eq!(
        peloton.schedule(early, late).await.unwrap_err(),
        UpstreamError::Transport
    );
    assert_eq!(
        peloton
            .add_to_schedule(&workout_id(), late)
            .await
            .unwrap_err(),
        UpstreamError::Transport
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}
