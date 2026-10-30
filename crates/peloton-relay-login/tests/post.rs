//! Posting the token set to a relay, against a local test server.

use peloton::account::AccountName;
use peloton::oauth::TokenSet;
use peloton_relay_login::post::{Code, PostError, SeedUrl, client, post};
use peloton_relay_login::report;
use secrecy::SecretString;
use serde_json::json;
use time::macros::datetime;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CODE: &str = "0123456789abcdef0123456789abcdef";

fn tokens() -> TokenSet {
    TokenSet {
        access_token: SecretString::from("access-value"),
        refresh_token: SecretString::from("refresh-value"),
        expires_at: datetime!(2030-01-01 00:00:00 UTC),
    }
}

fn target(server: &MockServer) -> SeedUrl {
    format!("{}/admin/seed", server.uri()).parse().unwrap()
}

async fn post_to(server: &MockServer) -> Result<String, PostError> {
    let account: AccountName = "bob".parse().unwrap();
    let code: Code = CODE.parse().unwrap();
    post(
        &client().unwrap(),
        &target(server),
        &account,
        &code,
        &tokens(),
    )
    .await
}

async fn answer(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/admin/seed"))
        .respond_with(response)
        .mount(server)
        .await;
}

#[tokio::test]
async fn posts_the_token_set_with_the_code_and_shows_the_confirmation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/admin/seed"))
        .and(header("content-type", "application/json"))
        .and(body_json(json!({
            "account": "bob",
            "code": CODE,
            "tokens": {
                "access_token": "access-value",
                "refresh_token": "refresh-value",
                "expires_at": "2030-01-01T00:00:00Z",
            },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "account": "bob",
            "message": "account \"bob\": bound to Peloton user bob\u{1b}[2J\u{202e}rides (id \u{2026}00b2)",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let confirmation = post_to(&server).await.unwrap();

    assert_eq!(
        confirmation,
        "account \"bob\": bound to Peloton user bob\u{fffd}[2J\u{fffd}rides (id \u{2026}00b2)"
    );
}

#[tokio::test]
async fn refusals_are_explained_without_the_code_or_tokens() {
    let cases = [
        (
            ResponseTemplate::new(403)
                .set_body_json(json!({"error": "invalid_code", "message": "x"})),
            "the relay refused the code",
        ),
        (
            ResponseTemplate::new(403).set_body_json(json!({"error": "forbidden", "message": "x"})),
            "gateway that grants admin access",
        ),
        (ResponseTemplate::new(404), "no such account or route"),
        (
            ResponseTemplate::new(409).set_body_json(json!({
                "error": "user_bound_elsewhere",
                "message": "This Peloton user is already bound to another account.",
            })),
            "the relay refused the sign-in (409): This Peloton user is already bound to another account.",
        ),
        (
            ResponseTemplate::new(422).set_body_json(json!({
                "error": "token_rejected",
                "message": "Peloton rejected the access token; sign in again.",
            })),
            "(422): Peloton rejected the access token",
        ),
        (
            ResponseTemplate::new(429)
                .insert_header("retry-after", "540")
                .set_body_json(json!({"error": "too_many_attempts", "message": "x"})),
            "retry in 540 s",
        ),
        (
            ResponseTemplate::new(500),
            "the relay answered with status 500",
        ),
        (
            ResponseTemplate::new(302).insert_header("location", "https://elsewhere.example/"),
            "status 302",
        ),
    ];
    for (response, expected) in cases {
        let server = MockServer::start().await;
        answer(&server, response).await;
        let error = post_to(&server).await.unwrap_err();
        let shown = report(&error);
        assert!(shown.contains(expected), "{shown}");
        for secret in [CODE, "access-value", "refresh-value"] {
            assert!(!shown.contains(secret), "{shown}");
        }
        // A redirect is never followed.
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn an_unreachable_relay_is_reported_without_the_url() {
    // A port nothing listens on any more.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url: SeedUrl = format!("http://{}/admin/seed", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    drop(listener);
    let account: AccountName = "bob".parse().unwrap();
    let code: Code = CODE.parse().unwrap();

    let error = post(&client().unwrap(), &url, &account, &code, &tokens())
        .await
        .unwrap_err();

    assert!(matches!(error, PostError::Send(_)), "{error:?}");
    let shown = report(&error);
    assert!(shown.starts_with("could not reach the relay"), "{shown}");
    assert!(!shown.contains("/admin/seed"), "{shown}");
}
