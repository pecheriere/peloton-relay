//! Tests for the Peloton upstream client.

use std::fs;
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

use peloton::ids::PelotonId;
use peloton_relay::tokens::TokenManager;
use peloton_relay::upstream::{Peloton, UpstreamError, WorkoutQuery};
use time::Duration;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::Instrument as _;
use url::Url;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

mod common;

use common::{captured_logs, http, state, state_file};

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
    if let Some(access) = access {
        state_file(dir.path())
            .save(&state(access, "refresh-1", Duration::hours(48)))
            .unwrap();
    }
    let auth = peloton::oauth::AuthClient::new(http(), Url::parse(&server.uri()).unwrap());
    let tokens = TokenManager::new(state_file(dir.path()), auth, StdDuration::from_secs(3600));
    let api = Url::parse(&format!("{}/api", server.uri())).unwrap();
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
    assert!(ours.contains("path=id"), "{ours}");
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

/// Records when each request arrives, then answers after a fixed delay.
struct Slow {
    delay: StdDuration,
    arrivals: Arc<Mutex<Vec<Instant>>>,
}

impl Respond for Slow {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.arrivals.lock().unwrap().push(Instant::now());
        ResponseTemplate::new(200)
            .set_delay(self.delay)
            .set_body_raw(fixture("me.json"), "application/json")
    }
}

#[tokio::test]
async fn limits_in_flight_requests() {
    let delay = StdDuration::from_millis(400);
    let arrivals = Arc::new(Mutex::new(Vec::new()));
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(Slow {
            delay,
            arrivals: Arc::clone(&arrivals),
        })
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let peloton = Arc::new(client(&dir, &server, Some("tok-1")));
    let calls: Vec<_> = (0..10)
        .map(|_| {
            let peloton = Arc::clone(&peloton);
            tokio::spawn(async move { peloton.me().await })
        })
        .collect();
    for call in calls {
        call.await.unwrap().unwrap();
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
