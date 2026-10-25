//! Tests for the OAuth PKCE client and Peloton identifiers.

use peloton::ids::PelotonId;
use peloton::oauth::{AuthClient, AuthError, AuthRequest, CallbackError, TokenSet, parse_callback};
use secrecy::{ExposeSecret, SecretString};
use time::{Duration, OffsetDateTime};
use url::Url;
use wiremock::matchers::{body_json, body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn request_with_verifier(verifier: &str) -> AuthRequest {
    AuthRequest::from_verifier(SecretString::from(verifier.to_owned()))
}

#[test]
fn challenge_is_s256_of_verifier() {
    let req = request_with_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
    assert_eq!(req.challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
}

#[test]
fn generate_produces_distinct_well_formed_values() {
    let a = AuthRequest::generate();
    let b = AuthRequest::generate();
    // 64 bytes -> 86 chars, 32 bytes -> 43 chars, base64url without padding.
    assert_eq!(a.verifier.expose_secret().len(), 86);
    assert_eq!(a.state.len(), 43);
    assert_eq!(a.nonce.len(), 43);
    assert_eq!(a.challenge.len(), 43);
    assert_ne!(a.verifier.expose_secret(), b.verifier.expose_secret());
    assert_ne!(a.state, b.state);
    assert_ne!(a.nonce, b.nonce);
}

#[test]
fn authorize_url_has_all_params() {
    let req = AuthRequest::generate();
    let base = Url::parse("https://auth.onepeloton.com").unwrap();
    let url = req.authorize_url(&base);
    assert_eq!(url.path(), "/authorize");
    assert_eq!(url.host_str(), Some("auth.onepeloton.com"));
    let get = |k: &str| {
        url.query_pairs()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.into_owned())
    };
    assert_eq!(get("client_id").as_deref(), Some(peloton::oauth::CLIENT_ID));
    assert_eq!(get("audience").as_deref(), Some(peloton::oauth::AUDIENCE));
    assert_eq!(get("scope").as_deref(), Some(peloton::oauth::SCOPE));
    assert_eq!(
        get("redirect_uri").as_deref(),
        Some(peloton::oauth::REDIRECT_URI)
    );
    assert_eq!(
        get("auth0Client").as_deref(),
        Some(peloton::oauth::AUTH0_CLIENT)
    );
    assert_eq!(get("response_type").as_deref(), Some("code"));
    assert_eq!(get("response_mode").as_deref(), Some("query"));
    assert_eq!(get("code_challenge_method").as_deref(), Some("S256"));
    assert_eq!(
        get("code_challenge").as_deref(),
        Some(req.challenge.as_str())
    );
    assert_eq!(get("state").as_deref(), Some(req.state.as_str()));
    assert_eq!(get("nonce").as_deref(), Some(req.nonce.as_str()));
    assert!(!url.as_str().contains(req.verifier.expose_secret()));
}

#[test]
fn parse_callback_ok() {
    let code = parse_callback(
        "https://members.onepeloton.com/callback?code=abc123&state=xyz",
        "xyz",
    )
    .unwrap();
    assert_eq!(code.expose_secret(), "abc123");
}

#[test]
fn parse_callback_wrong_host() {
    let err =
        parse_callback("https://evil.example/callback?code=abc&state=xyz", "xyz").unwrap_err();
    assert!(matches!(err, CallbackError::NotRedirectUri));
    let err = parse_callback(
        "https://members.onepeloton.com/other?code=abc&state=xyz",
        "xyz",
    )
    .unwrap_err();
    assert!(matches!(err, CallbackError::NotRedirectUri));
    let err = parse_callback("not a url", "xyz").unwrap_err();
    assert!(matches!(err, CallbackError::NotRedirectUri));
}

#[test]
fn parse_callback_missing_code() {
    let err =
        parse_callback("https://members.onepeloton.com/callback?state=xyz", "xyz").unwrap_err();
    assert!(matches!(err, CallbackError::MissingCode));
}

#[test]
fn parse_callback_state_mismatch() {
    let err = parse_callback(
        "https://members.onepeloton.com/callback?code=abc&state=other",
        "xyz",
    )
    .unwrap_err();
    assert!(matches!(err, CallbackError::StateMismatch));
    let err =
        parse_callback("https://members.onepeloton.com/callback?code=abc", "xyz").unwrap_err();
    assert!(matches!(err, CallbackError::StateMismatch));
}

#[test]
fn parse_callback_provider_error() {
    let err = parse_callback(
        "https://members.onepeloton.com/callback?error=access_denied&error_description=nope&state=xyz",
        "xyz",
    )
    .unwrap_err();
    match err {
        CallbackError::ProviderError { error } => assert_eq!(error, "access_denied"),
        other => panic!("unexpected error: {other:?}"),
    }
}

fn client_for(server: &MockServer) -> AuthClient {
    AuthClient::new(reqwest::Client::new(), Url::parse(&server.uri()).unwrap())
}

#[tokio::test]
async fn exchange_and_refresh_against_mock() {
    let server = MockServer::start().await;
    let req = request_with_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");

    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_json(serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": peloton::oauth::CLIENT_ID,
            "code_verifier": "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
            "code": "the-code",
            "redirect_uri": peloton::oauth::REDIRECT_URI,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "access-1",
            "refresh_token": "refresh-1",
            "expires_in": 172_800,
            "token_type": "Bearer",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(header("content-type", "application/x-www-form-urlencoded"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains(format!(
            "client_id={}",
            peloton::oauth::CLIENT_ID
        )))
        .and(body_string_contains("refresh_token=refresh-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "access-2",
            "refresh_token": "refresh-2",
            "expires_in": 172_800,
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = client_for(&server);
    let before = OffsetDateTime::now_utc();
    let set = client
        .exchange_code(&SecretString::from("the-code".to_owned()), &req)
        .await
        .unwrap();
    assert_eq!(set.access_token.expose_secret(), "access-1");
    assert_eq!(set.refresh_token.expose_secret(), "refresh-1");
    let ttl = set.expires_at - before;
    assert!(
        ttl > Duration::hours(47) && ttl < Duration::hours(49),
        "{ttl}"
    );

    let refreshed = client.refresh(&set.refresh_token).await.unwrap();
    assert_eq!(refreshed.access_token.expose_secret(), "access-2");
    assert_eq!(refreshed.refresh_token.expose_secret(), "refresh-2");
}

#[tokio::test]
async fn refresh_keeps_old_refresh_token_when_absent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "access-new",
            "expires_in": 60,
        })))
        .mount(&server)
        .await;
    let set = client_for(&server)
        .refresh(&SecretString::from("keep-me".to_owned()))
        .await
        .unwrap();
    assert_eq!(set.access_token.expose_secret(), "access-new");
    assert_eq!(set.refresh_token.expose_secret(), "keep-me");
}

#[tokio::test]
async fn invalid_grant_maps_to_invalid_grant() {
    for status in [403, 400] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(
                ResponseTemplate::new(status).set_body_json(serde_json::json!({
                    "error": "invalid_grant",
                    "error_description": "Unknown or invalid refresh token."
                })),
            )
            .mount(&server)
            .await;
        let err = client_for(&server)
            .refresh(&SecretString::from("stale".to_owned()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, AuthError::InvalidGrant),
            "status {status}: {err:?}"
        );
    }
}

#[tokio::test]
async fn other_failures_map_to_rejected_decode_transport() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(500).set_body_string("secret-upstream-detail"))
        .mount(&server)
        .await;
    let err = client_for(&server)
        .refresh(&SecretString::from("r".to_owned()))
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::Rejected { status: 500 }));
    let rendered = format!("{err} {err:?}");
    assert!(!rendered.contains("secret-upstream-detail"));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;
    let err = client_for(&server)
        .refresh(&SecretString::from("r".to_owned()))
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::Decode));

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let client = AuthClient::new(
        reqwest::Client::new(),
        Url::parse(&format!("http://{addr}")).unwrap(),
    );
    let err = client
        .refresh(&SecretString::from("r".to_owned()))
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::Transport));
}

fn sample_set() -> TokenSet {
    TokenSet {
        access_token: SecretString::from("access-secret-value".to_owned()),
        refresh_token: SecretString::from("refresh-secret-value".to_owned()),
        expires_at: OffsetDateTime::UNIX_EPOCH + Duration::days(20_000),
    }
}

#[test]
fn token_set_debug_is_redacted() {
    let rendered = format!("{:?}", sample_set());
    assert!(!rendered.contains("access-secret-value"));
    assert!(!rendered.contains("refresh-secret-value"));
}

#[test]
fn token_set_serde_round_trips_with_rfc3339() {
    let set = sample_set();
    let json = serde_json::to_string(&set).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["access_token"], "access-secret-value");
    assert_eq!(value["refresh_token"], "refresh-secret-value");
    assert_eq!(value["expires_at"], "2024-10-04T00:00:00Z");
    let back: TokenSet = serde_json::from_str(&json).unwrap();
    assert_eq!(back.access_token.expose_secret(), "access-secret-value");
    assert_eq!(back.expires_at, set.expires_at);
}

#[test]
fn peloton_id_accepts_valid() {
    let raw = "0123456789abcdef0123456789abcdef";
    let id: PelotonId = raw.parse().unwrap();
    assert_eq!(id.to_string(), raw);
    assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{raw}\""));
    let back: PelotonId = serde_json::from_str(&format!("\"{raw}\"")).unwrap();
    assert_eq!(back, id);
}

#[test]
fn peloton_id_rejects_non_hex_and_wrong_length() {
    for bad in [
        "../me",
        "",
        "0123456789ABCDEF0123456789ABCDEF",
        "0123456789abcdef0123456789abcde",
        "0123456789abcdef0123456789abcdef0",
        "0123456789abcdef0123456789abcdeg",
        "0123456789abcdef/123456789abcdef",
    ] {
        assert!(bad.parse::<PelotonId>().is_err(), "{bad:?}");
        assert!(serde_json::from_str::<PelotonId>(&format!("\"{bad}\"")).is_err());
    }
}

#[test]
fn endpoint_paths() {
    use peloton::endpoints;
    let id: PelotonId = "0123456789abcdef0123456789abcdef".parse().unwrap();
    assert_eq!(endpoints::API_BASE, "https://api.onepeloton.com/api");
    assert_eq!(endpoints::PLATFORM_HEADER, ("Peloton-Platform", "web"));
    assert_eq!(endpoints::me(), "/me");
    assert_eq!(
        endpoints::user_workouts(&id),
        format!("/user/{id}/workouts")
    );
    assert_eq!(endpoints::workout(&id), format!("/workout/{id}"));
    assert_eq!(
        endpoints::performance_graph(&id),
        format!("/workout/{id}/performance_graph")
    );
    assert_eq!(endpoints::ride_details(&id), format!("/ride/{id}/details"));
}
