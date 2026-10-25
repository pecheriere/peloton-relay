//! Shared login flow and Peloton API access for the subcommands.

use std::time::Duration;

use peloton::endpoints::{API_BASE, PLATFORM_HEADER};
use peloton::oauth::{AUTH_DOMAIN, AuthClient, AuthRequest, TokenSet, parse_callback};
use secrecy::{ExposeSecret as _, SecretString};
use url::Url;

use crate::paste;
#[cfg(target_os = "macos")]
use crate::window;

/// An HTTP client, a runtime and the auth endpoint shared by all subcommands.
pub struct Session {
    runtime: tokio::runtime::Runtime,
    http: reqwest::Client,
    auth: AuthClient,
    auth_base: Url,
}

impl Session {
    /// Builds the runtime and HTTP client (no redirects, bounded timeouts).
    ///
    /// # Errors
    ///
    /// Returns a message when the auth URL, client or runtime cannot be built.
    pub fn new() -> Result<Self, String> {
        let auth_base =
            Url::parse(&format!("https://{AUTH_DOMAIN}")).map_err(|e| format!("auth url: {e}"))?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| "could not build the HTTP client".to_owned())?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("runtime: {e}"))?;
        let auth = AuthClient::new(http.clone(), auth_base.clone());
        Ok(Self {
            runtime,
            http,
            auth,
            auth_base,
        })
    }

    /// Runs the interactive login (window or paste) and exchanges the code.
    ///
    /// # Errors
    ///
    /// Returns a message without secrets when any step fails.
    pub fn login(&self, paste: bool) -> Result<TokenSet, String> {
        let request = AuthRequest::generate();
        let authorize_url = request.authorize_url(&self.auth_base);
        let callback = if paste {
            paste::read_callback(&authorize_url)
        } else {
            capture_in_window(&authorize_url)
        }
        .map_err(|e| e.to_string())?;
        let code = parse_callback(&callback, &request.state).map_err(|e| e.to_string())?;
        self.runtime
            .block_on(self.auth.exchange_code(&code, &request))
            .map_err(|e| e.to_string())
    }

    /// Trades a refresh token for a new token set.
    ///
    /// # Errors
    ///
    /// Returns a message without secrets when the refresh fails.
    pub fn refresh(&self, refresh_token: &SecretString) -> Result<TokenSet, String> {
        self.runtime
            .block_on(self.auth.refresh(refresh_token))
            .map_err(|e| e.to_string())
    }

    /// `GET`s `path_and_query` (relative to the API base) and returns the raw body.
    ///
    /// # Errors
    ///
    /// Returns a message with the status only when the request fails or the
    /// status is not a success. Bodies are never included.
    pub fn get(&self, access: &SecretString, path_and_query: &str) -> Result<Vec<u8>, String> {
        self.runtime.block_on(async {
            let response = self
                .http
                .get(format!("{API_BASE}{path_and_query}"))
                .bearer_auth(access.expose_secret())
                .header(PLATFORM_HEADER.0, PLATFORM_HEADER.1)
                .send()
                .await
                .map_err(|_| format!("request to {path_and_query} failed"))?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("{path_and_query} answered with status {status}"));
            }
            response
                .bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|_| format!("reading the response of {path_and_query} failed"))
        })
    }

    /// Sleeps briefly so sequential requests stay gentle.
    pub fn pause(&self) {
        self.runtime
            .block_on(tokio::time::sleep(Duration::from_millis(400)));
    }
}

#[cfg(target_os = "macos")]
fn capture_in_window(authorize_url: &Url) -> Result<String, crate::LoginError> {
    eprintln!("Opening the Peloton login window...");
    window::capture_callback(authorize_url)
}

#[cfg(not(target_os = "macos"))]
fn capture_in_window(_authorize_url: &Url) -> Result<String, crate::LoginError> {
    Err(crate::LoginError::Window(
        "window mode needs macOS; use --paste".to_owned(),
    ))
}
