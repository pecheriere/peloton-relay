//! Tests of the admin routes: the admin header, the page, revoking, and
//! renewing a sign-in with a one-time code.

use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use base64::Engine as _;
use peloton::ids::PelotonId;
use peloton_relay::admin::{ADMIN_HEADER, AdminState, MAX_SEED_BODY_BYTES};
use peloton_relay::api::{ACCOUNT_HEADER, AccountContext};
use peloton_relay::tokens::MonotonicClock;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;
use time::macros::datetime;
use tracing::Instrument as _;
use url::Url;
use wiremock::matchers::{body_string_contains, header, method, path};
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
    V1_ROUTES, account_context, assert_error, request_as, router_with_admin, sample_body, send,
    unbound_context,
};
use common::{LOCK_WAIT, captured_logs};

const NOW: OffsetDateTime = datetime!(2030-01-01 00:00 UTC);
const PUBLIC: &str = "https://relay.example.net";
const ALICE_ACCESS: &str = "access-alice";
const ALICE_REFRESH: &str = "refresh-alice";
const BOB_ACCESS: &str = "access-bob";
const BOB_REFRESH: &str = "refresh-bob";
const NEW_ACCESS: &str = "access-new";
const NEW_REFRESH: &str = "refresh-new";
/// Every token any test hands the relay.
const SECRETS: [&str; 6] = [
    ALICE_ACCESS,
    ALICE_REFRESH,
    BOB_ACCESS,
    BOB_REFRESH,
    NEW_ACCESS,
    NEW_REFRESH,
];

/// Every admin route, with the account alice where it takes one, and paths
/// under `/admin` that match none.
const ADMIN_ROUTES: &[(&str, &str)] = &[
    ("GET", "/admin"),
    ("POST", "/admin/accounts/alice/code"),
    ("POST", "/admin/accounts/alice/revoke"),
    ("GET", "/admin/accounts/alice/status"),
    ("POST", "/admin/seed"),
    ("GET", "/admin/"),
    ("GET", "/admin/nowhere"),
];

fn user_a() -> PelotonId {
    common::user()
}

fn user_b() -> PelotonId {
    "000000000000000000000000000000b2".parse().unwrap()
}

fn user_c() -> PelotonId {
    "000000000000000000000000000000c3".parse().unwrap()
}

/// A monotonic clock the test moves by hand.
#[derive(Clone)]
struct TestClock(Arc<Mutex<Instant>>);

impl TestClock {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    fn advance(&self, by: StdDuration) {
        *self.0.lock().unwrap() += by;
    }

    fn monotonic(&self) -> MonotonicClock {
        let now = Arc::clone(&self.0);
        MonotonicClock::from_fn(move || *now.lock().unwrap())
    }
}

fn admin_state(dir: &tempfile::TempDir, clock: &TestClock, public: Option<&str>) -> AdminState {
    AdminState::with_timing(
        dir.path().to_owned(),
        public.map(|url| Url::parse(url).unwrap()),
        clock.monotonic(),
        LOCK_WAIT,
    )
}

fn seeded(
    dir: &tempfile::TempDir,
    server: &MockServer,
    name: &str,
    tokens: (&str, &str),
    user: &PelotonId,
) -> AccountContext {
    account_context(dir.path(), server, name, Some(tokens), user, 4)
}

/// alice (user A, seeded), bob (bound to user B, not seeded) and carol
/// (never signed in), with the public URL set.
fn three(dir: &tempfile::TempDir, server: &MockServer, clock: &TestClock) -> Router {
    router_with_admin(
        vec![
            seeded(
                dir,
                server,
                "alice",
                (ALICE_ACCESS, ALICE_REFRESH),
                &user_a(),
            ),
            account_context(dir.path(), server, "bob", None, &user_b(), 4),
            unbound_context(dir.path(), server, "carol"),
        ],
        NOW,
        admin_state(dir, clock, Some(PUBLIC)),
    )
}

/// An admin request without a body.
fn admin(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(ADMIN_HEADER, "1")
        .body(Body::empty())
        .unwrap()
}

/// A form post of the admin page, sent from its own origin.
fn form(uri: &str, csrf: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(ADMIN_HEADER, "1")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from(format!("csrf={csrf}")))
        .unwrap()
}

/// `POST /admin/seed` with `body` as JSON.
fn seed_post(body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/admin/seed")
        .header(ADMIN_HEADER, "1")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn seed_body(account: &str, code: &str) -> Value {
    json!({
        "account": account,
        "code": code,
        "tokens": {
            "access_token": NEW_ACCESS,
            "refresh_token": NEW_REFRESH,
            "expires_at": "2099-01-01T00:00:00Z",
        },
    })
}

async fn page(app: &Router) -> String {
    let (status, headers, body) = send(app, admin("GET", "/admin")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    body
}

/// The CSRF token of the form posting to `action` in `html`.
fn csrf(html: &str, action: &str) -> String {
    let form = html
        .find(&format!("action=\"{action}\""))
        .unwrap_or_else(|| panic!("no form for {action}"));
    let rest = &html[form..];
    let value = rest.find("value=\"").unwrap() + "value=\"".len();
    rest[value..value + 64].to_owned()
}

/// Issues a code for `account` from its page form; returns the code and the
/// page that shows it.
async fn issue(app: &Router, account: &str) -> (String, String) {
    let action = format!("/admin/accounts/{account}/code");
    let token = csrf(&page(app).await, &action);
    let (status, _, body) = send(app, form(&action, &token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let marker = " --code ";
    let start = body.find(marker).expect("the command") + marker.len();
    (body[start..start + 32].to_owned(), body)
}

async fn status_of(app: &Router, account: &str) -> Value {
    let (status, _, body) = send(
        app,
        admin("GET", &format!("/admin/accounts/{account}/status")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// Answers `GET /api/me` with the access token `access` as `user` named
/// `username`.
async fn me_as(server: &MockServer, access: &str, user: &PelotonId, username: &str) {
    Mock::given(method("GET"))
        .and(path("/api/me"))
        .and(header("authorization", format!("Bearer {access}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": user.to_string(),
            "username": username,
        })))
        .mount(server)
        .await;
}

/// Asserts that Peloton never saw the posted access token.
async fn assert_tokens_unused(server: &MockServer) {
    let posted = format!("Bearer {NEW_ACCESS}");
    for request in server.received_requests().await.unwrap() {
        assert!(
            request
                .headers
                .get("authorization")
                .is_none_or(|value| value != posted.as_str()),
            "{} {}",
            request.method,
            request.url
        );
    }
}

fn assert_no_secrets(text: &str) {
    for secret in SECRETS {
        assert!(!text.contains(secret), "leaks {secret}: {text}");
    }
}

fn assert_admin_headers(headers: &HeaderMap) {
    let policy = headers["content-security-policy"].to_str().unwrap();
    for directive in [
        "default-src 'none'",
        "frame-ancestors 'none'",
        "form-action 'self'",
        "connect-src 'self'",
        "base-uri 'none'",
        "script-src 'sha256-",
        "style-src 'sha256-",
    ] {
        assert!(policy.contains(directive), "{directive} in {policy}");
    }
    assert!(!policy.contains("unsafe"), "{policy}");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["referrer-policy"], "no-referrer");
}

/// The log lines of the case span `case`.
fn case_logs(case: &str) -> Vec<Value> {
    captured_logs()
        .contents()
        .lines()
        .filter(|line| line.contains(case))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn every_admin_route_refuses_requests_not_marked_as_admin() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let app = three(&dir, &server, &clock);
    let token = csrf(&page(&app).await, "/admin/accounts/alice/revoke");
    let marks: [&[&str]; 6] = [&[], &["0"], &["true"], &["1 "], &[""], &["1", "1"]];
    for (verb, uri) in ADMIN_ROUTES {
        for mark in marks {
            let mut request = Request::builder()
                .method(*verb)
                .uri(*uri)
                // An account never stands in for the admin mark.
                .header(ACCOUNT_HEADER, "alice")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("sec-fetch-site", "same-origin");
            for value in mark {
                request = request.header(ADMIN_HEADER, *value);
            }
            let request = request.body(Body::from(format!("csrf={token}"))).unwrap();
            let (status, headers, body) = send(&app, request).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{verb} {uri} {mark:?}");
            assert_error(&body, "forbidden");
            assert_admin_headers(&headers);
        }
    }
    // Nothing was revoked or issued.
    assert!(
        dir.path().join("alice/state.json").exists(),
        "a refused revoke deleted the state"
    );
    assert_eq!(status_of(&app, "alice").await["code"], "none");
}

#[tokio::test]
async fn refusals_are_logged_without_the_header_value() {
    // Installs the capture before anything logs.
    captured_logs();
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    let mut request = admin("GET", "/admin");
    request
        .headers_mut()
        .insert(ADMIN_HEADER, "zq7-admin-guess".parse().unwrap());
    send(&app, request)
        .instrument(tracing::info_span!("case", name = "admin_refused_logged"))
        .await;
    let logs = case_logs("admin_refused_logged");
    let refused = logs
        .iter()
        .find(|line| line["event"] == "admin_refused")
        .expect("an admin_refused event");
    assert_eq!(refused["reason"], "invalid");
    assert!(!refused.to_string().contains("zq7-admin-guess"));
}

#[tokio::test]
async fn the_admin_header_does_not_unlock_data_routes() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    for (verb, uri) in V1_ROUTES {
        let mut request = request_as(None, verb, uri, sample_body(verb, uri).as_ref());
        request
            .headers_mut()
            .insert(ADMIN_HEADER, "1".parse().unwrap());
        let (status, _, body) = send(&app, request).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{verb} {uri}");
        assert_error(&body, "forbidden");
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}

#[tokio::test]
async fn the_page_lists_every_account_and_never_a_token() {
    let server = MockServer::start().await;
    me_as(&server, ALICE_ACCESS, &user_a(), "alice.rides").await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());

    let body = page(&app).await;

    for expected in [
        "<title>Relay admin</title>",
        "3 accounts · 2 need sign-in",
        "alice.rides · <span class=\"mono\">\u{2026}00f2</span>",
        "\u{2026}00b2",
        "Not signed in yet",
        "Healthy",
        "Needs sign-in",
        "Renew sign-in",
        ">Sign in<",
        "Really revoke alice",
        "Pacer resting",
    ] {
        assert!(body.contains(expected), "{expected} missing: {body}");
    }
    for name in ["alice", "bob", "carol"] {
        assert!(body.contains(&format!("id=\"account-{name}\"")), "{name}");
    }
    // carol has nothing to revoke.
    assert!(!body.contains("Really revoke carol"));
    assert_no_secrets(&body);
}

#[tokio::test]
async fn the_page_shows_recent_problems() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("refresh_token=refresh-bob"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": "invalid_grant",
            "error_description": "Unknown or invalid refresh token.",
        })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let bob = account_context(dir.path(), &server, "bob", None, &user_b(), 4);
    let mut expired = common::state(BOB_ACCESS, BOB_REFRESH, -time::Duration::minutes(5));
    expired.user_id = user_b();
    peloton_relay::account::Accounts::new(dir.path().to_owned(), vec![common::account("bob")])
        .unwrap()
        .state_file(&common::account("bob"))
        .save(&expired, LOCK_WAIT)
        .unwrap();
    let app = router_with_admin(
        vec![bob],
        NOW,
        admin_state(&dir, &TestClock::new(), Some(PUBLIC)),
    );

    let body = page(&app).await;

    assert!(body.contains("Recent problems"), "{body}");
    assert!(
        body.contains("Peloton rejected the refresh token"),
        "{body}"
    );
    assert!(body.contains("Needs sign-in"), "{body}");
    assert_no_secrets(&body);
}

#[tokio::test]
async fn hostile_usernames_are_escaped() {
    let server = MockServer::start().await;
    me_as(
        &server,
        ALICE_ACCESS,
        &user_a(),
        "<script>alert(1)</script>\"&\u{202e}x",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());

    let body = page(&app).await;

    assert!(!body.contains("<script>alert"), "{body}");
    assert!(
        body.contains("&lt;script&gt;alert(1)&lt;/script&gt;&quot;&amp;\u{fffd}x"),
        "{body}"
    );
    assert!(!body.contains('\u{202e}'));
}

#[tokio::test]
async fn every_admin_answer_carries_a_strict_policy() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());

    let (_, headers, body) = send(&app, admin("GET", "/admin")).await;
    assert_admin_headers(&headers);
    // The inline script and style are exactly the ones the policy allows.
    let policy = headers["content-security-policy"].to_str().unwrap();
    for (open, close) in [("<script>", "</script>"), ("<style>", "</style>")] {
        let start = body.find(open).unwrap() + open.len();
        let end = start + body[start..].find(close).unwrap();
        let hash = base64::engine::general_purpose::STANDARD
            .encode(Sha256::digest(&body.as_bytes()[start..end]));
        assert!(policy.contains(&format!("'sha256-{hash}'")), "{open}");
        assert_eq!(body.matches(open).count(), 1, "{open}");
    }
    // No inline style attributes or handlers, which the policy would block.
    assert!(!body.contains(" style=\""));
    assert!(!body.contains(" onclick="));

    for request in [
        admin("GET", "/admin/accounts/alice/status"),
        admin("GET", "/admin/nowhere"),
        admin("GET", "/admin/seed"),
        admin("POST", "/admin/accounts/alice/revoke"),
        admin("POST", "/admin/accounts/dave/code"),
    ] {
        let (_, headers, _) = send(&app, request).await;
        assert_admin_headers(&headers);
    }
    // Data routes are not given the admin policy.
    let (_, headers, _) = send(&app, request_as(None, "GET", "/healthz", None)).await;
    assert!(headers.get("content-security-policy").is_none());
}

/// Requests to the form `action` of alice that must be refused, given the
/// page's tokens: no token, wrong ones, tokens of other forms, and browser
/// headers placing the post on another origin.
fn forged(action: &str, html: &str) -> Vec<Request<Body>> {
    let own = csrf(html, action);
    let mut refused = vec![
        admin("POST", action),
        form(action, ""),
        form(action, &"0".repeat(64)),
        form(action, &own[..63]),
    ];
    for other in [
        "/admin/accounts/alice/code",
        "/admin/accounts/alice/revoke",
        "/admin/accounts/bob/code",
        "/admin/accounts/bob/revoke",
    ] {
        if other != action {
            refused.push(form(action, &csrf(html, other)));
        }
    }
    for (name, value) in [
        ("sec-fetch-site", "cross-site"),
        ("sec-fetch-site", "same-site"),
        ("sec-fetch-site", "none"),
        ("origin", "https://evil.example"),
        // The relay's host, but not its public origin.
        ("origin", "http://relay.example.net"),
        ("origin", "https://relay.example.net:8443"),
    ] {
        let mut request = form(action, &own);
        request.headers_mut().insert(name, value.parse().unwrap());
        request
            .headers_mut()
            .insert("host", "relay.example.net".parse().unwrap());
        refused.push(request);
    }
    // `Origin: null` without `Sec-Fetch-Site` to vouch for it.
    let mut request = form(action, &own);
    request.headers_mut().remove("sec-fetch-site");
    request
        .headers_mut()
        .insert("origin", "null".parse().unwrap());
    refused.push(request);
    refused
}

#[tokio::test]
async fn both_forms_need_their_token_and_origin() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    let html = page(&app).await;

    for action in ["/admin/accounts/alice/code", "/admin/accounts/alice/revoke"] {
        for request in forged(action, &html) {
            let (status, _, body) = send(&app, request).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{action}: {body}");
            assert_error(&body, "csrf_rejected");
        }
    }
    // Nothing was issued or revoked.
    assert_eq!(status_of(&app, "alice").await["code"], "none");
    assert!(dir.path().join("alice/state.json").exists());

    // The public origin, or `null` (the page sends no referrer) with
    // `Sec-Fetch-Site: same-origin`; the request's host does not matter.
    for action in ["/admin/accounts/alice/code", "/admin/accounts/alice/revoke"] {
        for origin in [PUBLIC, "null"] {
            let mut request = form(action, &csrf(&html, action));
            request
                .headers_mut()
                .insert("origin", origin.parse().unwrap());
            request
                .headers_mut()
                .insert("host", "internal:8080".parse().unwrap());
            let (status, _, body) = send(&app, request).await;
            assert!(
                status.is_success() || status.is_redirection(),
                "{action} {origin}: {body}"
            );
        }
    }
    assert_eq!(status_of(&app, "alice").await["code"], "pending");
    assert!(!dir.path().join("alice/state.json").exists());
}

#[tokio::test]
async fn without_a_public_url_forms_must_come_from_the_requests_host() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = router_with_admin(
        vec![seeded(
            &dir,
            &server,
            "alice",
            (ALICE_ACCESS, ALICE_REFRESH),
            &user_a(),
        )],
        NOW,
        admin_state(&dir, &TestClock::new(), None),
    );
    let action = "/admin/accounts/alice/revoke";
    let token = csrf(&page(&app).await, action);
    for (origin, accepted) in [
        ("https://other.example", false),
        ("http://internal:8081", false),
        ("http://internal:8080", true),
    ] {
        let mut request = form(action, &token);
        request
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
        request
            .headers_mut()
            .insert("host", "internal:8080".parse().unwrap());
        let (status, _, body) = send(&app, request).await;
        if accepted {
            assert_eq!(status, StatusCode::SEE_OTHER, "{origin}: {body}");
        } else {
            assert_eq!(status, StatusCode::FORBIDDEN, "{origin}: {body}");
            assert_error(&body, "csrf_rejected");
        }
    }
}

#[tokio::test]
async fn revoke_deletes_the_credential_keeps_the_binding_and_is_audited() {
    // Installs the capture before anything logs.
    captured_logs();
    let server = MockServer::start().await;
    me_as(&server, ALICE_ACCESS, &user_a(), "alice.rides").await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    let (status, _, _) = send(&app, request_as(Some("alice"), "GET", "/v1/me", None)).await;
    assert_eq!(status, StatusCode::OK);
    let token = csrf(&page(&app).await, "/admin/accounts/alice/revoke");

    let (status, headers, body) = send(&app, form("/admin/accounts/alice/revoke", &token))
        .instrument(tracing::info_span!("case", name = "revoke_audited"))
        .await;

    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    assert_admin_headers(&headers);
    assert_eq!(headers["location"], "/admin?revoked=alice");
    let (status, _, body) = send(&app, admin("GET", "/admin?revoked=alice")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("<strong>alice</strong> is signed out."),
        "{body}"
    );
    assert!(!dir.path().join("alice/state.json").exists());
    assert!(dir.path().join("alice/binding.json").exists());
    let (status, _, body) = send(&app, request_as(Some("alice"), "GET", "/v1/me", None)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_error(&body, "not_authenticated");
    assert_eq!(status_of(&app, "alice").await["health"], "needs_sign_in");
    let logs = case_logs("revoke_audited");
    let audit = logs
        .iter()
        .find(|line| line["event"] == "account_revoked")
        .expect("an account_revoked event");
    assert_eq!(audit["account"], "alice");
    assert_eq!(audit["removed"], true);

    // Revoking again finds nothing to delete, and says so in the log.
    let (status, _, _) = send(&app, form("/admin/accounts/alice/revoke", &token))
        .instrument(tracing::info_span!("case", name = "revoke_again"))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let logs = case_logs("revoke_again");
    let audit = logs
        .iter()
        .find(|line| line["event"] == "account_revoked")
        .expect("an account_revoked event");
    assert_eq!(audit["removed"], false);
}

#[tokio::test]
async fn the_revoke_notice_names_configured_accounts_only() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    for query in ["revoked=dave", "revoked=%3Cb%3E", "revoked=", "other=1"] {
        let (status, _, body) = send(&app, admin("GET", &format!("/admin?{query}"))).await;
        assert_eq!(status, StatusCode::OK, "{query}");
        assert!(!body.contains("is signed out"), "{query}: {body}");
    }
}

#[tokio::test]
async fn issuing_a_code_shows_the_command_once() {
    // Installs the capture before anything logs.
    captured_logs();
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());

    let (code, body) = issue(&app, "bob")
        .instrument(tracing::info_span!("case", name = "code_issued"))
        .await;

    assert_eq!(code.len(), 32);
    assert!(code.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
    assert!(
        body.contains(&format!(
            "peloton-relay-login --account bob --post-to {PUBLIC}/admin/seed --code {code}"
        )),
        "{body}"
    );
    assert!(body.contains("data-status=\"/admin/accounts/bob/status\""));
    assert!(body.contains("Renew bob"));
    assert!(!page(&app).await.contains(&code));
    assert_eq!(status_of(&app, "bob").await["code"], "pending");
    let logs = case_logs("code_issued");
    assert!(logs.iter().any(|line| line["event"] == "admin_code_issued"));
    assert!(!captured_logs().contents().contains(&code));

    // carol has never signed in: her code signs her in for the first time.
    let (_, body) = issue(&app, "carol").await;
    assert!(body.contains("Sign in carol"), "{body}");
}

#[tokio::test]
async fn codes_need_the_public_url() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = router_with_admin(
        vec![seeded(
            &dir,
            &server,
            "alice",
            (ALICE_ACCESS, ALICE_REFRESH),
            &user_a(),
        )],
        NOW,
        admin_state(&dir, &TestClock::new(), None),
    );

    let body = page(&app).await;
    assert!(body.contains("PELOTON_RELAY_PUBLIC_URL"), "{body}");
    assert!(!body.contains("/admin/accounts/alice/code"), "{body}");

    let (status, _, body) = send(&app, form("/admin/accounts/alice/code", "x")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_error(&body, "not_configured");
}

#[tokio::test]
async fn codes_exist_only_for_configured_accounts() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    let token = csrf(&page(&app).await, "/admin/accounts/alice/code");
    for name in ["dave", "Alice", "al1ce", "abcdefghijklmnopq"] {
        for request in [
            form(&format!("/admin/accounts/{name}/code"), &token),
            form(&format!("/admin/accounts/{name}/revoke"), &token),
            admin("GET", &format!("/admin/accounts/{name}/status")),
        ] {
            let (status, _, body) = send(&app, request).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{name}");
            assert_error(&body, "not_found");
        }
    }
}

#[tokio::test]
async fn a_seed_with_the_code_signs_the_account_in_without_a_restart() {
    // Installs the capture before anything logs.
    captured_logs();
    let server = MockServer::start().await;
    me_as(&server, NEW_ACCESS, &user_b(), "bobrides").await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    let (status, _, _) = send(&app, request_as(Some("bob"), "GET", "/v1/me", None)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let (code, _) = issue(&app, "bob").await;

    let (status, headers, body) = send(&app, seed_post(&seed_body("bob", &code)))
        .instrument(tracing::info_span!("case", name = "admin_seed_ok"))
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_admin_headers(&headers);
    let answer: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(answer["account"], "bob");
    assert_eq!(
        answer["message"],
        "account \"bob\": bound to Peloton user bobrides (id \u{2026}00b2)"
    );
    assert_eq!(
        status_of(&app, "bob").await,
        json!({"account": "bob", "health": "ok", "code": "used"})
    );
    let (status, _, body) = send(&app, request_as(Some("bob"), "GET", "/v1/me", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(page(&app).await.contains("bobrides"));
    // The code is spent.
    let (status, _, body) = send(&app, seed_post(&seed_body("bob", &code))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_error(&body, "invalid_code");

    let logs = case_logs("admin_seed_ok");
    let audit = logs
        .iter()
        .find(|line| line["event"] == "admin_seed")
        .expect("an admin_seed event");
    assert_eq!(audit["account"], "bob");
    assert_eq!(audit["outcome"], "ok");
    assert_eq!(audit["user"], "\u{2026}00b2");
    let output = captured_logs().contents();
    assert_no_secrets(&output);
    assert!(!output.contains(&code));
}

#[tokio::test]
async fn a_first_seed_binds_an_account_that_never_signed_in() {
    let server = MockServer::start().await;
    me_as(&server, NEW_ACCESS, &user_c(), "carol_c").await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    let (code, _) = issue(&app, "carol").await;

    let (status, _, body) = send(&app, seed_post(&seed_body("carol", &code))).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(dir.path().join("carol/binding.json").exists());
    assert_eq!(status_of(&app, "carol").await["health"], "ok");
}

#[tokio::test]
async fn a_user_bound_to_another_account_is_refused_and_the_code_spent() {
    let server = MockServer::start().await;
    // The posted tokens belong to user A, whom alice is bound to.
    me_as(&server, NEW_ACCESS, &user_a(), "alice.rides").await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    let (code, _) = issue(&app, "carol").await;

    let (status, _, body) = send(&app, seed_post(&seed_body("carol", &code))).await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_error(&body, "user_bound_elsewhere");
    // The answer does not say which account the user is bound to.
    assert!(!body.contains("alice"), "{body}");
    assert!(!dir.path().join("carol/binding.json").exists());
    assert!(!dir.path().join("carol/state.json").exists());
    assert_eq!(status_of(&app, "carol").await["code"], "refused");
    let (status, _, body) = send(&app, seed_post(&seed_body("carol", &code))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_error(&body, "invalid_code");
}

#[tokio::test]
async fn an_unreadable_binding_of_another_account_refuses_the_seed_and_keeps_the_code() {
    let server = MockServer::start().await;
    me_as(&server, NEW_ACCESS, &user_c(), "carol_c").await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    let (code, _) = issue(&app, "carol").await;
    std::fs::write(dir.path().join("bob/binding.json"), "{").unwrap();

    let (status, _, body) = send(&app, seed_post(&seed_body("carol", &code))).await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_error(&body, "binding_unreadable");
    assert!(!dir.path().join("carol/binding.json").exists());
    assert!(!dir.path().join("carol/state.json").exists());
    assert_eq!(status_of(&app, "carol").await["code"], "pending");
}

#[tokio::test]
async fn another_user_is_refused_for_a_bound_account_without_rebind() {
    let server = MockServer::start().await;
    me_as(&server, NEW_ACCESS, &user_c(), "carol_c").await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    let (code, _) = issue(&app, "bob").await;

    let (status, _, body) = send(&app, seed_post(&seed_body("bob", &code))).await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_error(&body, "bound_to_another_user");
    let binding = std::fs::read_to_string(dir.path().join("bob/binding.json")).unwrap();
    assert!(binding.contains(&user_b().to_string()), "{binding}");
    assert_eq!(status_of(&app, "bob").await["code"], "refused");
}

#[tokio::test]
async fn a_failure_upstream_keeps_the_code() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/me"))
        .and(header("authorization", format!("Bearer {NEW_ACCESS}")))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    me_as(&server, NEW_ACCESS, &user_b(), "bobrides").await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    let (code, _) = issue(&app, "bob").await;

    let (status, _, body) = send(&app, seed_post(&seed_body("bob", &code))).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_error(&body, "upstream_error");
    assert_eq!(status_of(&app, "bob").await["code"], "pending");

    let (status, _, body) = send(&app, seed_post(&seed_body("bob", &code))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_token_peloton_rejects_keeps_the_code() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/me"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    let (code, _) = issue(&app, "bob").await;

    let (status, _, body) = send(&app, seed_post(&seed_body("bob", &code))).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_error(&body, "token_rejected");
    assert_eq!(status_of(&app, "bob").await["code"], "pending");
}

#[tokio::test]
async fn a_new_code_replaces_the_old_one() {
    let server = MockServer::start().await;
    me_as(&server, NEW_ACCESS, &user_b(), "bobrides").await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    let (old, _) = issue(&app, "bob").await;
    let (new, _) = issue(&app, "bob").await;
    assert_ne!(old, new);

    let (status, _, body) = send(&app, seed_post(&seed_body("bob", &old))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_error(&body, "invalid_code");
    let (status, _, body) = send(&app, seed_post(&seed_body("bob", &new))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_code_works_for_its_account_only() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    let (code, _) = issue(&app, "bob").await;

    for account in ["carol", "alice", "dave"] {
        let (status, _, body) = send(&app, seed_post(&seed_body(account, &code))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{account}");
        assert_error(&body, "invalid_code");
    }
    assert_tokens_unused(&server).await;
}

#[tokio::test]
async fn a_code_expires_after_ten_minutes() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let app = three(&dir, &server, &clock);
    let (code, _) = issue(&app, "bob").await;
    clock.advance(StdDuration::from_mins(10));

    assert_eq!(status_of(&app, "bob").await["code"], "expired");
    let (status, _, body) = send(&app, seed_post(&seed_body("bob", &code))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_error(&body, "invalid_code");
}

#[tokio::test]
async fn wrong_codes_lock_seeding_out_for_a_while() {
    // Installs the capture before anything logs.
    captured_logs();
    let server = MockServer::start().await;
    me_as(&server, NEW_ACCESS, &user_b(), "bobrides").await;
    let dir = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let app = three(&dir, &server, &clock);
    for _ in 0..5 {
        let (status, _, body) = send(&app, seed_post(&seed_body("bob", &"0".repeat(32)))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "invalid_code");
    }
    clock.advance(StdDuration::from_mins(1));
    let (code, _) = issue(&app, "bob").await;

    // Even the right code is refused now.
    let (status, headers, body) = send(&app, seed_post(&seed_body("bob", &code)))
        .instrument(tracing::info_span!("case", name = "seed_rate_limited"))
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_error(&body, "too_many_attempts");
    assert_eq!(headers["retry-after"], "540");
    let logs = case_logs("seed_rate_limited");
    assert!(
        logs.iter()
            .any(|line| line["event"] == "admin_code_refused" && line["reason"] == "limited")
    );

    clock.advance(StdDuration::from_mins(8));
    let (status, headers, _) = send(&app, seed_post(&seed_body("bob", &code))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers["retry-after"], "60");

    clock.advance(StdDuration::from_mins(1));
    let (status, _, body) = send(&app, seed_post(&seed_body("bob", &code))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_seed_needs_a_json_body_within_the_limit() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    let (code, _) = issue(&app, "bob").await;
    let valid = seed_body("bob", &code).to_string();

    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
        Some("multipart/form-data"),
    ] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/admin/seed")
            .header(ADMIN_HEADER, "1");
        if let Some(content_type) = content_type {
            request = request.header("content-type", content_type);
        }
        let (status, _, body) = send(&app, request.body(Body::from(valid.clone())).unwrap()).await;
        assert_eq!(
            status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{content_type:?}"
        );
        assert_error(&body, "unsupported_media_type");
    }

    let mut unknown = seed_body("bob", &code);
    unknown["rebind"] = json!(true);
    let mut no_tokens = seed_body("bob", &code);
    no_tokens.as_object_mut().unwrap().remove("tokens");
    for bad in [
        json!([]),
        json!({}),
        unknown,
        no_tokens,
        seed_body("Bob", &code),
        seed_body("", &code),
    ] {
        let (status, _, body) = send(&app, seed_post(&bad)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        assert_error(&body, "invalid_input");
    }

    let mut huge = seed_body("bob", &code);
    huge["tokens"]["access_token"] = json!("a".repeat(MAX_SEED_BODY_BYTES));
    let (status, _, body) = send(&app, seed_post(&huge)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_error(&body, "payload_too_large");

    // None of these touched the code.
    assert_eq!(status_of(&app, "bob").await["code"], "pending");
    assert_tokens_unused(&server).await;
}

#[tokio::test]
async fn the_status_reports_health_and_code() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    // The page checks every credential first.
    page(&app).await;

    assert_eq!(
        status_of(&app, "alice").await,
        json!({"account": "alice", "health": "ok", "code": "none"})
    );
    assert_eq!(
        status_of(&app, "carol").await,
        json!({"account": "carol", "health": "needs_sign_in", "code": "none"})
    );
}

#[test]
fn the_page_draws_pacer_from_the_design_files() {
    let root = env!("CARGO_MANIFEST_DIR");
    for name in ["face", "ready", "resting", "oops", "cheering"] {
        let design = std::fs::read(format!("{root}/../../docs/design/pacer/{name}.svg")).unwrap();
        let served = std::fs::read(format!("{root}/src/admin/pacer/{name}.svg")).unwrap();
        assert_eq!(served, design, "{name}.svg differs from docs/design");
    }
}

/// How many primary buttons `html` has.
fn primaries(html: &str) -> usize {
    html.matches("class=\"button primary\"").count()
}

#[tokio::test]
async fn each_screen_has_one_primary_action() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());

    // bob is the first account that needs a sign-in; healthy alice and
    // carol, who needs one too, get secondary buttons.
    let html = page(&app).await;
    assert_eq!(primaries(&html), 1, "{html}");
    let primary = html.find("class=\"button primary\"").unwrap();
    let bob = html.find("/admin/accounts/bob/code").unwrap();
    let carol = html.find("/admin/accounts/carol/code").unwrap();
    assert!(bob < primary && primary < carol, "{html}");

    // While renewing, Copy is the one primary action, and the panel's Pacer
    // is the only one shown.
    let (_, renew) = issue(&app, "bob").await;
    assert_eq!(primaries(&renew), 1, "{renew}");
    assert!(
        renew.contains("class=\"button primary\" type=\"button\" data-copy"),
        "{renew}"
    );
    assert!(renew.contains("<section class=\"hero\"><div>"), "{renew}");
}

#[tokio::test]
async fn a_page_of_healthy_accounts_has_no_primary_action() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = router_with_admin(
        vec![seeded(
            &dir,
            &server,
            "alice",
            (ALICE_ACCESS, ALICE_REFRESH),
            &user_a(),
        )],
        NOW,
        admin_state(&dir, &TestClock::new(), Some(PUBLIC)),
    );
    let html = page(&app).await;
    assert!(html.contains("all healthy"), "{html}");
    assert_eq!(primaries(&html), 0, "{html}");
    assert!(html.contains("class=\"button secondary\""), "{html}");
}

#[tokio::test]
async fn the_seed_route_refuses_browsers() {
    // Installs the capture before anything logs.
    captured_logs();
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    let (code, _) = issue(&app, "bob").await;

    for (name, value) in [
        ("origin", PUBLIC),
        ("origin", "null"),
        ("origin", "https://evil.example"),
        ("sec-fetch-site", "same-origin"),
        ("sec-fetch-site", "same-site"),
        ("sec-fetch-site", "cross-site"),
    ] {
        // Right and wrong codes alike.
        for presented in [code.as_str(), "00000000000000000000000000000000"] {
            let mut request = seed_post(&seed_body("bob", presented));
            request.headers_mut().insert(name, value.parse().unwrap());
            let (status, _, body) = send(&app, request)
                .instrument(tracing::info_span!("case", name = "seed_browser"))
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{name}: {value}");
            assert_error(&body, "browser_request");
        }
    }
    let logs = case_logs("seed_browser");
    assert!(
        logs.iter()
            .any(|line| line["event"] == "admin_seed" && line["outcome"] == "browser_request")
    );
    // None of them reached the code or the failure count: the right code
    // still works, and Peloton was never asked.
    assert_eq!(status_of(&app, "bob").await["code"], "pending");
    assert_tokens_unused(&server).await;
    me_as(&server, NEW_ACCESS, &user_b(), "bobrides").await;
    let mut request = seed_post(&seed_body("bob", &code));
    request
        .headers_mut()
        .insert("sec-fetch-site", "none".parse().unwrap());
    let (status, _, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn the_code_is_spent_only_once_the_credential_is_in_use() {
    let server = MockServer::start().await;
    me_as(&server, NEW_ACCESS, &user_b(), "bobrides").await;
    // The posted token expires within the refresh margin, so taking it into
    // use refreshes it first; the refresh is slow enough to poll during.
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains(format!("refresh_token={NEW_REFRESH}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "access_token": "access-rotated",
                    "refresh_token": "refresh-rotated",
                    "expires_in": 172_800,
                    "token_type": "Bearer",
                }))
                .set_delay(StdDuration::from_millis(800)),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let app = three(&dir, &server, &TestClock::new());
    let (code, _) = issue(&app, "bob").await;
    let mut body = seed_body("bob", &code);
    let soon = OffsetDateTime::now_utc() + time::Duration::minutes(30);
    body["tokens"]["expires_at"] = json!(
        soon.format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    );

    let seeding = tokio::spawn({
        let app = app.clone();
        async move { send(&app, seed_post(&body)).await }
    });
    let mut seen = Vec::new();
    while !seeding.is_finished() {
        let status = status_of(&app, "bob").await;
        seen.push((status["code"].clone(), status["health"].clone()));
        tokio::time::sleep(StdDuration::from_millis(20)).await;
    }
    let (status, _, answer) = seeding.await.unwrap();

    assert_eq!(status, StatusCode::OK, "{answer}");
    // While the credential was being taken into use, the code still read
    // pending: a poll never saw it spent on an account not yet healthy.
    assert!(seen.len() > 3, "{seen:?}");
    assert!(
        seen.iter()
            .all(|(code, health)| code != "used" || health == "ok"),
        "{seen:?}"
    );
    assert_eq!(
        status_of(&app, "bob").await,
        json!({"account": "bob", "health": "ok", "code": "used"})
    );
}
