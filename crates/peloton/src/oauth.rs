//! OAuth 2.0 authorization code flow with PKCE against Peloton's Auth0 tenant.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore as _;
use reqwest::StatusCode;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};
use url::Url;

/// Auth0 tenant domain.
pub const AUTH_DOMAIN: &str = "auth.onepeloton.com";
/// Public OAuth client id of Peloton's web app.
pub const CLIENT_ID: &str = "WVoJxVDdPoFx4RNewvvg6ch2mZ7bwnsM";
/// API audience.
pub const AUDIENCE: &str = "https://api.onepeloton.com/";
/// Requested scopes.
pub const SCOPE: &str = "offline_access openid peloton-api.members:default";
/// Redirect URI registered for the client.
pub const REDIRECT_URI: &str = "https://members.onepeloton.com/callback";
/// Base64 JSON identifying the Auth0 client library.
pub const AUTH0_CLIENT: &str = "eyJuYW1lIjoiYXV0aDAuanMtdWxwIiwidmVyc2lvbiI6IjkuMTQuMyJ9";

const VERIFIER_BYTES: usize = 64;
const STATE_BYTES: usize = 32;

fn random_token(len: usize) -> String {
    let mut bytes = vec![0_u8; len];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// One authorization attempt: PKCE verifier and challenge plus state and nonce.
#[derive(Debug)]
pub struct AuthRequest {
    /// PKCE code verifier; only sent to the token endpoint.
    pub verifier: SecretString,
    /// PKCE S256 code challenge derived from the verifier.
    pub challenge: String,
    /// Anti-CSRF state echoed back on the redirect.
    pub state: String,
    /// `OpenID` Connect nonce.
    pub nonce: String,
}

impl AuthRequest {
    /// Generates a fresh verifier, challenge, state and nonce.
    #[must_use]
    pub fn generate() -> Self {
        let mut request = Self::from_verifier(SecretString::from(random_token(VERIFIER_BYTES)));
        request.state = random_token(STATE_BYTES);
        request.nonce = random_token(STATE_BYTES);
        request
    }

    /// Builds a request around a given verifier (S256 challenge derived from it).
    ///
    /// State and nonce are freshly random.
    #[must_use]
    pub fn from_verifier(verifier: SecretString) -> Self {
        let digest = Sha256::digest(verifier.expose_secret().as_bytes());
        Self {
            challenge: URL_SAFE_NO_PAD.encode(digest),
            verifier,
            state: random_token(STATE_BYTES),
            nonce: random_token(STATE_BYTES),
        }
    }

    /// The URL the user opens in a browser to sign in.
    #[must_use]
    pub fn authorize_url(&self, auth_base: &Url) -> Url {
        let mut url = auth_base.clone();
        url.set_path("/authorize");
        url.set_query(None);
        url.query_pairs_mut()
            .append_pair("client_id", CLIENT_ID)
            .append_pair("audience", AUDIENCE)
            .append_pair("scope", SCOPE)
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("auth0Client", AUTH0_CLIENT)
            .append_pair("response_type", "code")
            .append_pair("response_mode", "query")
            .append_pair("code_challenge", &self.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &self.state)
            .append_pair("nonce", &self.nonce);
        url
    }
}

/// Why a redirect URL could not be turned into an authorization code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallbackError {
    /// The URL is not the registered redirect URI.
    #[error("URL is not the expected redirect URI")]
    NotRedirectUri,
    /// The redirect carried no authorization code.
    #[error("redirect URL has no authorization code")]
    MissingCode,
    /// The state parameter is absent or differs from the one issued.
    #[error("state parameter does not match")]
    StateMismatch,
    /// The identity provider reported an error.
    #[error("identity provider returned error {error}")]
    ProviderError {
        /// The `error` query parameter.
        error: String,
    },
}

/// Extracts the authorization code from the redirect URL the browser landed on.
///
/// # Errors
///
/// Returns a [`CallbackError`] when the URL is not the redirect URI, the state
/// does not match, the provider reported an error, or no code is present.
pub fn parse_callback(url: &str, expected_state: &str) -> Result<SecretString, CallbackError> {
    let redirect = Url::parse(REDIRECT_URI).map_err(|_| CallbackError::NotRedirectUri)?;
    let parsed = Url::parse(url.trim()).map_err(|_| CallbackError::NotRedirectUri)?;
    if parsed.scheme() != redirect.scheme()
        || parsed.host_str() != redirect.host_str()
        || parsed.port_or_known_default() != redirect.port_or_known_default()
        || parsed.path() != redirect.path()
    {
        return Err(CallbackError::NotRedirectUri);
    }

    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }

    if state.as_deref() != Some(expected_state) {
        return Err(CallbackError::StateMismatch);
    }
    if let Some(error) = error {
        return Err(CallbackError::ProviderError { error });
    }
    match code {
        Some(code) if !code.is_empty() => Ok(SecretString::from(code)),
        _ => Err(CallbackError::MissingCode),
    }
}

fn serialize_secret<S: Serializer>(
    secret: &SecretString,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(secret.expose_secret())
}

/// An access and refresh token pair with the access token's expiry.
///
/// Serializing exposes the secrets on purpose: it is how tokens are handed from
/// the login tool to the relay. `Debug` stays redacted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSet {
    /// Bearer token for the Peloton API.
    #[serde(serialize_with = "serialize_secret")]
    pub access_token: SecretString,
    /// Long-lived token used to obtain new access tokens.
    #[serde(serialize_with = "serialize_secret")]
    pub refresh_token: SecretString,
    /// When the access token expires.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// A failed token request. Never carries upstream bodies, URLs or tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// The code or refresh token is invalid, expired or revoked.
    #[error("grant is invalid or expired")]
    InvalidGrant,
    /// The token endpoint answered with an unexpected error status.
    #[error("token endpoint rejected the request with status {status}")]
    Rejected {
        /// HTTP status code.
        status: u16,
    },
    /// The token endpoint could not be reached.
    #[error("could not reach the token endpoint")]
    Transport,
    /// The token endpoint's answer could not be understood.
    #[error("token endpoint response could not be decoded")]
    Decode,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: SecretString,
    refresh_token: Option<SecretString>,
    #[serde(deserialize_with = "deserialize_seconds")]
    expires_in: Duration,
}

fn deserialize_seconds<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    Ok(Duration::seconds(i64::from(u32::deserialize(
        deserializer,
    )?)))
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
}

/// Client for Auth0's token endpoint.
#[derive(Debug, Clone)]
pub struct AuthClient {
    http: reqwest::Client,
    token_url: Url,
}

impl AuthClient {
    /// Creates a client for the Auth0 origin `auth_base`.
    #[must_use]
    pub fn new(http: reqwest::Client, auth_base: Url) -> Self {
        let mut token_url = auth_base;
        token_url.set_path("/oauth/token");
        token_url.set_query(None);
        Self { http, token_url }
    }

    /// Exchanges an authorization code for tokens.
    ///
    /// # Errors
    ///
    /// Returns an [`AuthError`] when the request fails or the grant is rejected.
    pub async fn exchange_code(
        &self,
        code: &SecretString,
        req: &AuthRequest,
    ) -> Result<TokenSet, AuthError> {
        let body = serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code_verifier": req.verifier.expose_secret(),
            "code": code.expose_secret(),
            "redirect_uri": REDIRECT_URI,
        });
        let response = self
            .http
            .post(self.token_url.clone())
            .json(&body)
            .send()
            .await
            .map_err(|_| AuthError::Transport)?;
        Self::into_token_set(response, None).await
    }

    /// Obtains a new access token with a refresh token.
    ///
    /// If the response carries no new refresh token the given one is kept.
    ///
    /// # Errors
    ///
    /// Returns an [`AuthError`] when the request fails or the token is rejected.
    pub async fn refresh(&self, refresh_token: &SecretString) -> Result<TokenSet, AuthError> {
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token.expose_secret()),
        ];
        let response = self
            .http
            .post(self.token_url.clone())
            .form(&form)
            .send()
            .await
            .map_err(|_| AuthError::Transport)?;
        Self::into_token_set(response, Some(refresh_token)).await
    }

    async fn into_token_set(
        response: reqwest::Response,
        previous_refresh: Option<&SecretString>,
    ) -> Result<TokenSet, AuthError> {
        let status = response.status();
        if !status.is_success() {
            let invalid_grant = matches!(status, StatusCode::FORBIDDEN | StatusCode::BAD_REQUEST)
                && response
                    .json::<ErrorResponse>()
                    .await
                    .is_ok_and(|body| body.error == "invalid_grant");
            return Err(if invalid_grant {
                AuthError::InvalidGrant
            } else {
                AuthError::Rejected {
                    status: status.as_u16(),
                }
            });
        }

        let parsed: TokenResponse = response.json().await.map_err(|_| AuthError::Decode)?;
        let refresh_token = match (parsed.refresh_token, previous_refresh) {
            (Some(new), _) => new,
            (None, Some(old)) => SecretString::from(old.expose_secret().to_owned()),
            (None, None) => return Err(AuthError::Decode),
        };
        let expires_at = OffsetDateTime::now_utc()
            .checked_add(parsed.expires_in)
            .ok_or(AuthError::Decode)?;
        Ok(TokenSet {
            access_token: parsed.access_token,
            refresh_token,
            expires_at,
        })
    }
}
