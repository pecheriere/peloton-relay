//! Shared login flow and Peloton API access for the subcommands.

use std::time::Duration;

use peloton::endpoints::{API_BASE, PLATFORM_HEADER};
use peloton::graphql::{OpKind, op_url};
use peloton::oauth::{
    AUTH_DOMAIN, AuthClient, AuthError, AuthRequest, CallbackError, TokenSet, parse_callback,
};
use secrecy::{ExposeSecret as _, SecretString};
use url::Url;

#[cfg(target_os = "macos")]
use crate::window;

const PAUSE: Duration = Duration::from_millis(400);

/// Why a session step failed. Never carries tokens, callback URLs or bodies.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SessionError {
    /// The auth base URL could not be built.
    #[error("auth url")]
    AuthUrl(#[source] url::ParseError),
    /// The HTTP client could not be built.
    #[error("could not build the HTTP client")]
    HttpClient(#[source] reqwest::Error),
    /// The async runtime could not be built.
    #[error("runtime")]
    Runtime(#[source] std::io::Error),
    /// The login window failed or was closed.
    #[error(transparent)]
    Login(#[from] crate::LoginError),
    /// The login redirect did not carry an authorization code.
    #[error(transparent)]
    Callback(#[from] CallbackError),
    /// Peloton's auth service refused the code exchange or the refresh.
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// A `GET` could not be sent.
    #[error("request to {path} failed")]
    Send {
        /// The path and query that was requested.
        path: String,
        /// The transport failure, without its URL.
        #[source]
        source: reqwest::Error,
    },
    /// A `GET` answered with a failure status.
    #[error("{path} answered with status {status}")]
    Status {
        /// The path and query that was requested.
        path: String,
        /// The status Peloton answered.
        status: reqwest::StatusCode,
    },
    /// A `GET` response body could not be read.
    #[error("reading the response of {path} failed")]
    Read {
        /// The path and query that was requested.
        path: String,
        /// The transport failure, without its URL.
        #[source]
        source: reqwest::Error,
    },
    /// A GraphQL operation could not be sent.
    #[error("GraphQL request {op} failed")]
    GraphqlSend {
        /// The operation name.
        op: String,
        /// The transport failure, without its URL.
        #[source]
        source: reqwest::Error,
    },
    /// A GraphQL response body could not be read.
    #[error("reading the response of GraphQL {op} failed")]
    GraphqlRead {
        /// The operation name.
        op: String,
        /// The transport failure, without its URL.
        #[source]
        source: reqwest::Error,
    },
}

/// A GraphQL gateway answer of any status, kept raw for saving and inspection.
#[derive(Debug)]
pub struct GraphqlReply {
    /// The HTTP status.
    pub status: reqwest::StatusCode,
    /// The raw response body; never printed.
    pub body: Vec<u8>,
}

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
    /// Returns [`SessionError`] when the auth URL, client or runtime cannot be built.
    pub fn new() -> Result<Self, SessionError> {
        let auth_base =
            Url::parse(&format!("https://{AUTH_DOMAIN}")).map_err(SessionError::AuthUrl)?;
        let http = peloton::tls::client_builder()
            .map_err(|e| SessionError::HttpClient(e.without_url()))?
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| SessionError::HttpClient(e.without_url()))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(SessionError::Runtime)?;
        let auth = AuthClient::new(http.clone(), auth_base.clone());
        Ok(Self {
            runtime,
            http,
            auth,
            auth_base,
        })
    }

    /// Runs the interactive login in the login window and exchanges the code.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] (without secrets) when any step fails.
    pub fn login(&self) -> Result<TokenSet, SessionError> {
        let request = AuthRequest::generate();
        let authorize_url = request.authorize_url(&self.auth_base);
        let callback = capture_in_window(&authorize_url)?;
        let code = parse_callback(&callback, &request.state)?;
        Ok(self
            .runtime
            .block_on(self.auth.exchange_code(&code, &request))?)
    }

    /// Trades a refresh token for a new token set.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Auth`] when the refresh fails.
    pub fn refresh(&self, refresh_token: &SecretString) -> Result<TokenSet, SessionError> {
        Ok(self.runtime.block_on(self.auth.refresh(refresh_token))?)
    }

    /// `GET`s `path_and_query` (relative to the API base) and returns the raw body.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] with the status only when the request fails or
    /// the status is not a success. Bodies are never included.
    pub fn get(
        &self,
        access: &SecretString,
        path_and_query: &str,
    ) -> Result<Vec<u8>, SessionError> {
        self.runtime.block_on(async {
            let response = self
                .http
                .get(format!("{API_BASE}{path_and_query}"))
                .bearer_auth(access.expose_secret())
                .header(PLATFORM_HEADER.0, PLATFORM_HEADER.1)
                .send()
                .await
                .map_err(|e| SessionError::Send {
                    path: path_and_query.to_owned(),
                    source: e.without_url(),
                })?;
            let status = response.status();
            if !status.is_success() {
                return Err(SessionError::Status {
                    path: path_and_query.to_owned(),
                    status,
                });
            }
            response
                .bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|e| SessionError::Read {
                    path: path_and_query.to_owned(),
                    source: e.without_url(),
                })
        })
    }

    /// `POST`s one GraphQL operation to the gateway and returns the raw answer.
    ///
    /// The status is returned rather than turned into an error, because the
    /// gateway reports validation and auth problems in error bodies worth saving.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] naming the operation only when the request cannot be
    /// sent or its body cannot be read. Bodies are never included.
    pub fn graphql(
        &self,
        access: &SecretString,
        op: &str,
        kind: OpKind,
        document: &str,
        variables: &serde_json::Value,
    ) -> Result<GraphqlReply, SessionError> {
        let body = serde_json::json!({ "query": document, "variables": variables });
        self.runtime.block_on(async {
            let response = self
                .http
                .post(op_url(op, kind))
                .bearer_auth(access.expose_secret())
                .header(PLATFORM_HEADER.0, PLATFORM_HEADER.1)
                .json(&body)
                .send()
                .await
                .map_err(|e| SessionError::GraphqlSend {
                    op: op.to_owned(),
                    source: e.without_url(),
                })?;
            let status = response.status();
            let body = response
                .bytes()
                .await
                .map_err(|e| SessionError::GraphqlRead {
                    op: op.to_owned(),
                    source: e.without_url(),
                })?
                .to_vec();
            Ok(GraphqlReply { status, body })
        })
    }

    /// Sleeps briefly so sequential requests stay gentle.
    pub fn pause(&self) {
        std::thread::sleep(PAUSE);
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
        "the login window needs macOS".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::Session;

    #[test]
    fn pause_works_outside_the_runtime_context() {
        let session = Session::new().expect("session");
        session.pause();
    }
}
