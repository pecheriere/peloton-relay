//! The admin page: every account's credential health, and revoking or
//! renewing an account's sign-in.
//!
//! Every path under `/admin` is served only to a request the deployment's
//! gateway marks with exactly one [`ADMIN_HEADER`] whose value is
//! [`ADMIN_HEADER_VALUE`]; anything else answers 403 `forbidden` before
//! routing. These paths need no `X-Peloton-Account`: they act on the account
//! named in the path or body. Every answer under `/admin`, refusals included,
//! carries [`security_headers`].
//!
//! - `GET /admin`: the page, one card per account.
//! - `POST /admin/accounts/{name}/code`: issues a one-time code (see
//!   [`codes`]) and answers the page with the command that uses it.
//! - `POST /admin/accounts/{name}/revoke`: deletes the account's stored
//!   credential, keeping its binding.
//! - `GET /admin/accounts/{name}/status`: the account's health and code
//!   status as JSON, which the page polls while a renewal runs.
//! - `POST /admin/seed`: a token set from the login helper, with a code; the
//!   same checks as `seed`, without `--rebind`.
//!
//! The two form routes need the page's CSRF token (see [`csrf`]) and refuse a
//! request that a browser reports as coming from another site.

pub mod codes;
pub mod csrf;
mod page;

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, FailedToBufferBody, FormRejection};
use axum::extract::{DefaultBodyLimit, FromRequest as _, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::Engine as _;
use peloton::ids::PelotonId;
use peloton::oauth::TokenSet;
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::task::JoinSet;
use url::Url;

use crate::account::{AccountName, Accounts, user_suffix};
use crate::api::{AccountContext, AppState};
use crate::error::ErrorBody;
use crate::seed::{SeedError, seed_tokens};
use crate::store::StoreError;
use crate::tokens::{Health, MonotonicClock};
use codes::{CodeRefusal, Codes};
use csrf::{Action, Csrf};

/// Header with which the deployment's gateway marks a request as coming from
/// the operator. The gateway sets it from an app capability only the operator
/// holds, and drops any copy the client sent.
pub const ADMIN_HEADER: HeaderName = HeaderName::from_static("x-peloton-admin");

/// The only value of [`ADMIN_HEADER`] that grants admin access.
pub const ADMIN_HEADER_VALUE: &str = "1";

/// Upper bound for the body of `POST /admin/seed`, as for `seed`'s input.
pub const MAX_SEED_BODY_BYTES: usize = 64 * 1024;

/// How long `POST /admin/seed` waits for the state and bindings locks: longer
/// than a refresh in flight may hold the state lock (a 30 s request plus the
/// save).
pub const DEFAULT_SEED_LOCK_WAIT: Duration = Duration::from_secs(45);

/// How long the page waits for an account's credential check, and for
/// Peloton to name the user of an account.
const PAGE_WAIT: Duration = Duration::from_secs(5);

/// The page's one script and one style sheet, allowed by their hashes.
const PAGE_JS: &str = include_str!("page.js");
const PAGE_CSS: &str = include_str!("page.css");

/// Whether `path` is served by the admin routes.
pub(crate) fn is_admin_path(path: &str) -> bool {
    path == "/admin" || path.starts_with("/admin/")
}

/// The admin routes' own state, next to [`AppState`].
pub struct AdminState {
    /// The state directory, holding one directory per account.
    state_dir: PathBuf,
    /// The origin the renew command posts to; renewing is off without it.
    public_url: Option<Url>,
    clock: MonotonicClock,
    /// How long a seed waits for each lock.
    lock_wait: Duration,
    csrf: Csrf,
    codes: Mutex<Codes>,
    /// The Peloton user each account was last seen bound to, and their
    /// username, so that the page asks Peloton once per user.
    usernames: Mutex<HashMap<AccountName, KnownUser>>,
}

#[derive(Debug, Clone)]
struct KnownUser {
    id: PelotonId,
    username: Option<String>,
}

impl std::fmt::Debug for AdminState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminState")
            .field("state_dir", &self.state_dir)
            .field("public_url", &self.public_url)
            .finish_non_exhaustive()
    }
}

impl AdminState {
    /// The admin state of a relay whose accounts live under `state_dir`,
    /// reachable at `public_url`.
    #[must_use]
    pub fn new(state_dir: PathBuf, public_url: Option<Url>) -> Self {
        Self::with_timing(
            state_dir,
            public_url,
            MonotonicClock::system(),
            DEFAULT_SEED_LOCK_WAIT,
        )
    }

    /// [`Self::new`], with code expiry and the failure limit measured on
    /// `clock`, and seeds waiting at most `lock_wait` for each lock.
    #[must_use]
    pub fn with_timing(
        state_dir: PathBuf,
        public_url: Option<Url>,
        clock: MonotonicClock,
        lock_wait: Duration,
    ) -> Self {
        Self {
            state_dir,
            public_url,
            clock,
            lock_wait,
            csrf: Csrf::default(),
            codes: Mutex::new(Codes::default()),
            usernames: Mutex::new(HashMap::new()),
        }
    }

    fn codes(&self) -> MutexGuard<'_, Codes> {
        self.codes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn usernames(&self) -> MutexGuard<'_, HashMap<AccountName, KnownUser>> {
        self.usernames
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// The admin routes, each served behind [`gate`].
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin", get(show))
        .route("/admin/accounts/{name}/code", post(issue_code))
        .route("/admin/accounts/{name}/revoke", post(revoke))
        .route("/admin/accounts/{name}/status", get(status))
        .route(
            "/admin/seed",
            post(seed).layer(DefaultBodyLimit::max(MAX_SEED_BODY_BYTES)),
        )
}

/// Why a request under `/admin` is refused; logged as `reason`.
#[derive(Debug, Clone, Copy)]
enum Refusal {
    Missing,
    Ambiguous,
    Invalid,
}

impl Refusal {
    fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Ambiguous => "ambiguous",
            Self::Invalid => "invalid",
        }
    }
}

/// Why `headers` do not mark an admin request, if they do not.
fn admin_refusal(headers: &HeaderMap) -> Option<Refusal> {
    let mut values = headers.get_all(ADMIN_HEADER).iter();
    let Some(value) = values.next() else {
        return Some(Refusal::Missing);
    };
    if values.next().is_some() {
        return Some(Refusal::Ambiguous);
    }
    (value.as_bytes() != ADMIN_HEADER_VALUE.as_bytes()).then_some(Refusal::Invalid)
}

/// Lets an admin request through, answers anything else with 403
/// `forbidden` (logged as `event="admin_refused"` with the reason, never the
/// header's value), and adds [`security_headers`] to every answer.
pub(crate) async fn gate(request: Request, next: Next) -> Response {
    let mut response = match admin_refusal(request.headers()) {
        None => next.run(request).await,
        Some(refusal) => {
            tracing::warn!(
                event = "admin_refused",
                reason = refusal.as_str(),
                "admin request refused: it is not marked as admin"
            );
            AdminError::Forbidden.into_response()
        }
    };
    response.headers_mut().extend(security_headers().clone());
    response
}

/// `'sha256-<base64>'`: a CSP source allowing the inline `text`.
fn csp_hash(text: &str) -> String {
    format!(
        "'sha256-{}'",
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(text.as_bytes()))
    )
}

/// The headers of every answer under `/admin`: a strict content security
/// policy allowing only the page's own script and style (by hash), requests
/// back to the relay and forms posting to it, and no framing; no caching, no
/// MIME sniffing and no referrer.
#[must_use]
pub fn security_headers() -> &'static HeaderMap {
    static HEADERS: LazyLock<HeaderMap> = LazyLock::new(|| {
        let policy = format!(
            "default-src 'none'; script-src {}; style-src {}; connect-src 'self'; \
             form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
            csp_hash(PAGE_JS),
            csp_hash(PAGE_CSS),
        );
        let mut headers = HeaderMap::new();
        for (name, value) in [
            (header::CONTENT_SECURITY_POLICY, policy.as_str()),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_FRAME_OPTIONS, "DENY"),
        ] {
            if let Ok(value) = HeaderValue::from_str(value) {
                headers.insert(name, value);
            }
        }
        headers
    });
    &HEADERS
}

/// An error the admin routes report, in the API's error format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdminError {
    /// The request is not marked as admin.
    Forbidden,
    /// A form's CSRF token or origin is wrong.
    CsrfRejected,
    /// No such account.
    NotFound,
    /// The body is malformed.
    InvalidInput,
    /// A browser sent a request meant for the login helper.
    BrowserRequest,
    /// The body is not JSON.
    UnsupportedMediaType,
    /// The body exceeds [`MAX_SEED_BODY_BYTES`].
    PayloadTooLarge,
    /// The code is wrong, expired, spent or for another account.
    InvalidCode,
    /// Too many wrong codes lately.
    TooManyAttempts { retry_after: Duration },
    /// Renewing needs the public URL.
    NotConfigured,
    /// The account is bound to another Peloton user.
    BoundToAnotherUser,
    /// The Peloton user is bound to another account.
    UserBoundElsewhere,
    /// A binding the seed must check cannot be read.
    BindingUnreadable,
    /// Peloton rejected the posted access token.
    TokenRejected,
    /// Peloton failed or answered something unusable.
    Upstream,
    /// A lock stayed held.
    Busy,
    /// The relay failed.
    Internal,
}

impl AdminError {
    fn status(self) -> StatusCode {
        match self {
            Self::Forbidden | Self::CsrfRejected | Self::BrowserRequest | Self::InvalidCode => {
                StatusCode::FORBIDDEN
            }
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::InvalidInput => StatusCode::BAD_REQUEST,
            Self::UnsupportedMediaType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::TooManyAttempts { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::NotConfigured
            | Self::BoundToAnotherUser
            | Self::UserBoundElsewhere
            | Self::BindingUnreadable => StatusCode::CONFLICT,
            Self::TokenRejected => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Upstream => StatusCode::BAD_GATEWAY,
            Self::Busy => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn code(self) -> &'static str {
        match self {
            Self::Forbidden => "forbidden",
            Self::CsrfRejected => "csrf_rejected",
            Self::NotFound => "not_found",
            Self::InvalidInput => "invalid_input",
            Self::BrowserRequest => "browser_request",
            Self::UnsupportedMediaType => "unsupported_media_type",
            Self::PayloadTooLarge => "payload_too_large",
            Self::InvalidCode => "invalid_code",
            Self::TooManyAttempts { .. } => "too_many_attempts",
            Self::NotConfigured => "not_configured",
            Self::BoundToAnotherUser => "bound_to_another_user",
            Self::UserBoundElsewhere => "user_bound_elsewhere",
            Self::BindingUnreadable => "binding_unreadable",
            Self::TokenRejected => "token_rejected",
            Self::Upstream => "upstream_error",
            Self::Busy => "busy",
            Self::Internal => "internal_error",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::Forbidden => "This request is not marked as an admin request.",
            Self::CsrfRejected => {
                "The form is stale or did not come from the admin page; reload the page."
            }
            Self::NotFound => "No such account.",
            Self::InvalidInput => "The request is invalid.",
            Self::BrowserRequest => "This route takes the login helper's request, not a browser's.",
            Self::UnsupportedMediaType => "The body must be application/json.",
            Self::PayloadTooLarge => "The body is too large.",
            Self::InvalidCode => {
                "The code is wrong, expired, already used or for another account; issue a new one on the admin page."
            }
            Self::TooManyAttempts { .. } => "Too many wrong codes; retry later.",
            Self::NotConfigured => {
                "Renewing from the admin page needs PELOTON_RELAY_PUBLIC_URL to be set."
            }
            Self::BoundToAnotherUser => {
                "The account is bound to another Peloton user; sign in as that user."
            }
            Self::UserBoundElsewhere => "This Peloton user is already bound to another account.",
            Self::BindingUnreadable => {
                "A binding the relay must check cannot be read; see the relay's log."
            }
            Self::TokenRejected => "Peloton rejected the access token; sign in again.",
            Self::Upstream => "Peloton failed to answer the request.",
            Self::Busy => "Another seed or a token refresh holds the lock; try again.",
            Self::Internal => "The relay failed to handle the request.",
        }
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: self.code(),
            message: Cow::Borrowed(self.message()),
        };
        let mut response = (self.status(), Json(body)).into_response();
        if let Self::TooManyAttempts { retry_after } = self {
            let seconds = retry_after.as_secs() + u64::from(retry_after.subsec_nanos() > 0);
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        response
    }
}

/// The context of the account a path `{name}` names, recorded on the
/// request span; 404 for any other name.
fn named(app: &AppState, name: &str) -> Result<Arc<AccountContext>, AdminError> {
    let context = name
        .parse::<AccountName>()
        .ok()
        .and_then(|name| app.accounts.get(&name))
        .ok_or(AdminError::NotFound)?;
    tracing::Span::current().record("account", context.name.as_str());
    Ok(context)
}

/// A form of the page: its CSRF token.
#[derive(Deserialize)]
struct FormToken {
    #[serde(default)]
    csrf: String,
}

/// Accepts a form post only with the CSRF token of `action` for `account`,
/// and only when the browser, if it says, sent it from the relay's own origin:
/// `Sec-Fetch-Site: same-origin`, and an `Origin` that is exactly the public
/// URL's origin when it is set, or else the request's own host.
fn check_form(
    admin: &AdminState,
    headers: &HeaderMap,
    form: Result<Form<FormToken>, FormRejection>,
    action: Action,
    account: &AccountName,
) -> Result<(), AdminError> {
    let token = form.map(|Form(form)| form.csrf).unwrap_or_default();
    if same_origin(headers, admin.public_url.as_ref()) && admin.csrf.verify(action, account, &token)
    {
        Ok(())
    } else {
        tracing::warn!(
            event = "admin_form_refused",
            account = %account,
            "admin form refused: wrong CSRF token or another origin"
        );
        Err(AdminError::CsrfRejected)
    }
}

/// Whether the browser headers, when present, place the request on the
/// relay's own origin.
///
/// `Origin: null` is what a browser sends for a form of a page served with
/// `Referrer-Policy: no-referrer`, as the admin page is; it passes only when
/// `Sec-Fetch-Site: same-origin` vouches for the request.
fn same_origin(headers: &HeaderMap, public_url: Option<&Url>) -> bool {
    let site = headers.get("sec-fetch-site");
    if site.is_some_and(|site| site.as_bytes() != b"same-origin") {
        return false;
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    if origin.as_bytes() == b"null" {
        return site.is_some();
    }
    let Some(origin) = origin.to_str().ok().and_then(|text| Url::parse(text).ok()) else {
        return false;
    };
    if let Some(public_url) = public_url {
        return public_url.origin() == origin.origin();
    }
    // Without a public URL, the only reference is the request's own host;
    // the relay itself cannot tell which scheme the browser used.
    let host = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok());
    let authority = origin.host_str().map(|name| match origin.port() {
        Some(port) => format!("{name}:{port}"),
        None => name.to_owned(),
    });
    host.is_some_and(|host| authority.as_deref() == Some(host))
}

/// Query of `GET /admin`.
#[derive(Deserialize)]
struct ShowParams {
    /// The account a revoke just signed out, for the notice.
    revoked: Option<String>,
}

/// `GET /admin`; `?revoked=<name>` adds the notice of a revoke of a
/// configured account (anything else is ignored).
async fn show(State(app): State<AppState>, Query(params): Query<ShowParams>) -> Response {
    let notice = params
        .revoked
        .and_then(|name| name.parse::<AccountName>().ok())
        .filter(|name| app.accounts.get(name).is_some())
        .map(|account| page::Notice::Revoked { account });
    render(&app, None, notice).await
}

/// `POST /admin/accounts/{name}/code`.
async fn issue_code(
    State(app): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    form: Result<Form<FormToken>, FormRejection>,
) -> Result<Response, AdminError> {
    let context = named(&app, &name)?;
    let Some(public_url) = &app.admin.public_url else {
        return Err(AdminError::NotConfigured);
    };
    check_form(&app.admin, &headers, form, Action::IssueCode, &context.name)?;
    let post_to = public_url
        .join("admin/seed")
        .map_err(|_| AdminError::Internal)?;
    let code = app
        .admin
        .codes()
        .issue(&context.name, app.admin.clock.now());
    tracing::info!(
        event = "admin_code_issued",
        account = %context.name,
        "issued a one-time sign-in code"
    );
    let command = SecretString::from(format!(
        "peloton-relay-login --account {} --post-to {post_to} --code {}",
        context.name,
        code.expose_secret()
    ));
    let renew = page::Renew {
        account: context.name.clone(),
        command,
    };
    Ok(render(&app, Some(renew), None).await)
}

/// `POST /admin/accounts/{name}/revoke`.
async fn revoke(
    State(app): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    form: Result<Form<FormToken>, FormRejection>,
) -> Result<Response, AdminError> {
    let context = named(&app, &name)?;
    check_form(&app.admin, &headers, form, Action::Revoke, &context.name)?;
    let removed = match context.peloton.tokens().revoke().await {
        Ok(removed) => removed,
        Err(StoreError::Locked) => return Err(AdminError::Busy),
        Err(err) => {
            tracing::error!(
                event = "account_revoke_failed",
                account = %context.name,
                cause = ?err,
                "cannot delete the account's state file"
            );
            return Err(AdminError::Internal);
        }
    };
    tracing::info!(
        event = "account_revoked",
        account = %context.name,
        removed,
        "revoked the account's Peloton credential"
    );
    // See other: reloading the page that follows does not post again.
    Ok(Redirect::to(&format!("/admin?revoked={}", context.name)).into_response())
}

/// Body of `GET /admin/accounts/{name}/status`.
#[derive(Serialize)]
struct StatusBody {
    account: String,
    /// `ok`, `needs_sign_in` or `unreadable`.
    health: &'static str,
    /// `none`, `pending`, `used`, `refused` or `expired`.
    code: &'static str,
}

/// `GET /admin/accounts/{name}/status`.
async fn status(
    State(app): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<StatusBody>, AdminError> {
    let context = named(&app, &name)?;
    let health = context.peloton.tokens().status().health;
    let code = app
        .admin
        .codes()
        .status(&context.name, app.admin.clock.now());
    Ok(Json(StatusBody {
        account: context.name.to_string(),
        health: health_str(health),
        code: code.as_str(),
    }))
}

fn health_str(health: Health) -> &'static str {
    match health {
        Health::Ok => "ok",
        Health::NeedsSignIn => "needs_sign_in",
        Health::Unreadable => "unreadable",
    }
}

/// Body of `POST /admin/seed`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SeedRequest {
    /// The account the code was issued for.
    account: String,
    /// The one-time code.
    code: SecretString,
    /// The token set, as the login helper prints it for `seed`.
    tokens: TokenSet,
}

/// Body of a successful `POST /admin/seed`.
#[derive(Serialize)]
struct SeedAnswer {
    account: String,
    /// The line `seed` prints.
    message: String,
}

/// `POST /admin/seed`: checks the code, then seeds the account exactly as
/// `seed` does, without `--rebind`, and takes the credential into use.
async fn seed(
    State(app): State<AppState>,
    request: Request,
) -> Result<Json<SeedAnswer>, AdminError> {
    let (account, request) = seed_request(request).await.inspect_err(|error| {
        tracing::warn!(
            event = "admin_seed",
            outcome = error.code(),
            "the admin API refused a seed request"
        );
    })?;
    tracing::Span::current().record("account", account.as_str());
    let claim = claim_code(&app.admin, &account, &request.code)?;
    let Some(context) = app.accounts.get(&account) else {
        app.admin.codes().release(claim);
        return Err(AdminError::NotFound);
    };
    let names = app
        .accounts
        .iter()
        .map(|context| context.name.clone())
        .collect();
    let Ok(accounts) = Accounts::new(app.admin.state_dir.clone(), names) else {
        app.admin.codes().release(claim);
        return Err(AdminError::Internal);
    };
    let seeded = seed_tokens(
        request.tokens,
        &accounts,
        &account,
        false,
        context.peloton.http(),
        context.peloton.api_base(),
        app.admin.lock_wait,
    )
    .await;
    match seeded {
        Ok(seeded) => {
            // Takes the new credential into use now, before the code reads as
            // used, so a poll that sees it used also sees the account's new
            // health; how that went shows in the manager's status.
            let _ = context.peloton.tokens().bearer().await;
            app.admin.codes().spend(claim);
            app.admin.usernames().insert(
                account.clone(),
                KnownUser {
                    id: seeded.user_id.clone(),
                    username: seeded.username.clone(),
                },
            );
            tracing::info!(
                event = "admin_seed",
                account = %account,
                user = %user_suffix(&seeded.user_id),
                outcome = "ok",
                "seeded the account from the admin API"
            );
            Ok(Json(SeedAnswer {
                account: account.to_string(),
                message: seeded.to_string(),
            }))
        }
        Err(err) => Err(seed_refused(&app.admin, claim, &account, &err)),
    }
}

/// The account and body of a `POST /admin/seed`: a JSON body of at most
/// [`MAX_SEED_BODY_BYTES`] naming a valid account, sent by the login helper.
///
/// The headers are checked before the body is read. A request a browser
/// sent (`Origin`, or `Sec-Fetch-Site` other than `none`) is refused: no web
/// page may post codes, right or wrong, even if a gateway let it through.
async fn seed_request(request: Request) -> Result<(AccountName, SeedRequest), AdminError> {
    let headers = request.headers();
    let from_browser = headers.contains_key(header::ORIGIN)
        || headers
            .get("sec-fetch-site")
            .is_some_and(|site| site.as_bytes() != b"none");
    if from_browser {
        return Err(AdminError::BrowserRequest);
    }
    if !is_json(headers) {
        return Err(AdminError::UnsupportedMediaType);
    }
    let body = Bytes::from_request(request, &()).await;
    let body = body.map_err(|rejection| match rejection {
        BytesRejection::FailedToBufferBody(FailedToBufferBody::LengthLimitError(_)) => {
            AdminError::PayloadTooLarge
        }
        _ => AdminError::InvalidInput,
    })?;
    // The parse error is dropped on purpose: it can quote the body.
    let request: SeedRequest =
        serde_json::from_slice(&body).map_err(|_| AdminError::InvalidInput)?;
    let account = request
        .account
        .parse()
        .map_err(|_| AdminError::InvalidInput)?;
    Ok((account, request))
}

/// Claims `code` for `account`, logging `event="admin_code_refused"` when it
/// is refused.
fn claim_code(
    admin: &AdminState,
    account: &AccountName,
    code: &SecretString,
) -> Result<codes::Claim, AdminError> {
    let claimed = admin
        .codes()
        .claim(account, code.expose_secret(), admin.clock.now());
    claimed.map_err(|refusal| {
        let (reason, error) = match refusal {
            CodeRefusal::Invalid => ("invalid", AdminError::InvalidCode),
            CodeRefusal::Limited { retry_after } => {
                ("limited", AdminError::TooManyAttempts { retry_after })
            }
        };
        tracing::warn!(
            event = "admin_code_refused",
            account = %account,
            reason,
            "sign-in code refused"
        );
        error
    })
}

/// Settles `claim` after the seed of `account` failed with `err`, logs it,
/// and returns its answer. A binding refusal spends the code: it is final
/// for this account. Anything else (Peloton unreachable or rejecting the
/// token, a lock held, a binding that cannot be read) leaves the code for a
/// retry.
fn seed_refused(
    admin: &AdminState,
    claim: codes::Claim,
    account: &AccountName,
    err: &SeedError,
) -> AdminError {
    let error = match err {
        SeedError::UnknownAccount { .. } => AdminError::NotFound,
        SeedError::BoundToAnotherUser { .. } => AdminError::BoundToAnotherUser,
        SeedError::UserBoundElsewhere { .. } => AdminError::UserBoundElsewhere,
        SeedError::BindingUnreadable { .. } => AdminError::BindingUnreadable,
        SeedError::InvalidInput => AdminError::InvalidInput,
        SeedError::Rejected { .. } => AdminError::TokenRejected,
        SeedError::Upstream | SeedError::Unexpected { .. } => AdminError::Upstream,
        SeedError::Busy => AdminError::Busy,
        // `NoState` is `bind`'s; a store failure is the relay's own.
        SeedError::NoState { .. } | SeedError::Store(_) => AdminError::Internal,
    };
    if matches!(
        error,
        AdminError::BoundToAnotherUser | AdminError::UserBoundElsewhere
    ) {
        admin.codes().refuse(claim);
    } else {
        admin.codes().release(claim);
    }
    tracing::warn!(
        event = "admin_seed",
        account = %account,
        outcome = error.code(),
        cause = %err,
        "the admin API refused a seed"
    );
    error
}

/// Whether the body is declared as `application/json` (parameters allowed).
fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
}

/// The page, with `renew`'s steps or `notice` above the accounts.
async fn render(
    app: &AppState,
    renew: Option<page::Renew>,
    notice: Option<page::Notice>,
) -> Response {
    let accounts = account_views(app).await;
    let view = page::View {
        accounts,
        now: app.clock.now(),
        renew_enabled: app.admin.public_url.is_some(),
        renew,
        notice,
    };
    Html(page::render(&view).into_string()).into_response()
}

/// What the page shows of each account, by name.
///
/// First checks every account's credential the way the background check
/// does (reading a new state file, refreshing when due), waiting at most
/// [`PAGE_WAIT`] for it; a check still running then goes on detached.
async fn account_views(app: &AppState) -> Vec<page::AccountView> {
    let mut checks = JoinSet::new();
    for context in app.accounts.iter() {
        let tokens = Arc::clone(context.peloton.tokens());
        checks.spawn(async move {
            // The outcome shows in the manager's status.
            let _ = tokio::time::timeout(PAGE_WAIT, tokens.bearer()).await;
        });
    }
    while checks.join_next().await.is_some() {}
    let mut views = Vec::new();
    for context in app.accounts.iter() {
        let tokens = context.peloton.tokens();
        let user = match tokens.bound_user().await {
            Ok(None) => page::User::Unbound,
            Ok(Some(id)) => page::User::Bound {
                suffix: user_suffix(&id),
                username: None,
                id,
            },
            Err(_) => page::User::Unreadable,
        };
        views.push(page::AccountView {
            name: context.name.clone(),
            status: tokens.status(),
            user,
            code_token: app.admin.csrf.token(Action::IssueCode, &context.name),
            revoke_token: app.admin.csrf.token(Action::Revoke, &context.name),
        });
    }
    fill_usernames(app, &mut views).await;
    views
}

/// Fills in the username of each bound user: from what the relay already
/// knows, or else from `GET /me` with the account's credential when it is
/// healthy, waiting at most [`PAGE_WAIT`].
async fn fill_usernames(app: &AppState, views: &mut [page::AccountView]) {
    let mut lookups = JoinSet::new();
    {
        let known = app.admin.usernames();
        for view in views.iter_mut() {
            let page::User::Bound { id, username, .. } = &mut view.user else {
                continue;
            };
            match known.get(&view.name) {
                Some(user) if user.id == *id => username.clone_from(&user.username),
                _ if view.status.health == Health::Ok => {
                    if let Some(context) = app.accounts.get(&view.name) {
                        lookups.spawn(async move {
                            let me = tokio::time::timeout(PAGE_WAIT, context.peloton.me()).await;
                            (context.name.clone(), me)
                        });
                    }
                }
                _ => {}
            }
        }
    }
    while let Some(joined) = lookups.join_next().await {
        let Ok((name, Ok(Ok(me)))) = joined else {
            continue;
        };
        let Some(view) = views.iter_mut().find(|view| view.name == name) else {
            continue;
        };
        if let page::User::Bound { id, username, .. } = &mut view.user
            && *id == me.id
        {
            username.clone_from(&me.username);
            app.admin.usernames().insert(
                name,
                KnownUser {
                    id: me.id,
                    username: me.username,
                },
            );
        }
    }
}

/// `bytes` as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};
    use url::Url;

    use super::{admin_refusal, hex, is_admin_path, is_json, same_origin};

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, HeaderValue::from_static(value));
        }
        headers
    }

    #[test]
    fn admin_paths_are_admin_and_its_subpaths() {
        for path in ["/admin", "/admin/", "/admin/seed", "/admin/x/y"] {
            assert!(is_admin_path(path), "{path}");
        }
        for path in [
            "/",
            "/administrator",
            "/v1/admin",
            "/adminx/seed",
            "/healthz",
        ] {
            assert!(!is_admin_path(path), "{path}");
        }
    }

    #[test]
    fn only_one_header_of_value_1_is_admin() {
        assert!(admin_refusal(&headers(&[("x-peloton-admin", "1")])).is_none());
        for refused in [
            headers(&[]),
            headers(&[("x-peloton-admin", "0")]),
            headers(&[("x-peloton-admin", "true")]),
            headers(&[("x-peloton-admin", "1 ")]),
            headers(&[("x-peloton-admin", "")]),
            headers(&[("x-peloton-admin", "1"), ("x-peloton-admin", "1")]),
        ] {
            assert!(admin_refusal(&refused).is_some(), "{refused:?}");
        }
    }

    #[test]
    fn json_content_types() {
        assert!(is_json(&headers(&[("content-type", "application/json")])));
        assert!(is_json(&headers(&[(
            "content-type",
            "application/json; charset=utf-8"
        )])));
        assert!(is_json(&headers(&[("content-type", "Application/JSON")])));
        for refused in [
            headers(&[]),
            headers(&[("content-type", "text/plain")]),
            headers(&[("content-type", "application/x-www-form-urlencoded")]),
            headers(&[("content-type", "application/jsonx")]),
        ] {
            assert!(!is_json(&refused), "{refused:?}");
        }
    }

    #[test]
    fn same_origin_accepts_the_public_url_or_else_the_own_host() {
        let public = Url::parse("https://relay.example.net").unwrap();
        assert!(same_origin(&headers(&[]), None));
        assert!(same_origin(
            &headers(&[("sec-fetch-site", "same-origin")]),
            None
        ));
        assert!(same_origin(
            &headers(&[
                ("origin", "http://127.0.0.1:8080"),
                ("host", "127.0.0.1:8080")
            ]),
            None
        ));
        assert!(same_origin(
            &headers(&[
                ("origin", "https://relay.example.net"),
                ("host", "internal")
            ]),
            Some(&public)
        ));
        assert!(same_origin(
            &headers(&[("origin", "null"), ("sec-fetch-site", "same-origin")]),
            None
        ));
        for refused in [
            headers(&[("sec-fetch-site", "cross-site")]),
            headers(&[("sec-fetch-site", "same-site")]),
            headers(&[("sec-fetch-site", "none")]),
            headers(&[("origin", "null"), ("host", "relay.example.net")]),
            headers(&[("origin", "null"), ("sec-fetch-site", "cross-site")]),
            headers(&[
                ("origin", "https://evil.example"),
                ("host", "relay.example.net"),
            ]),
            headers(&[("origin", "https://relay.example.net:444")]),
            headers(&[("origin", "http://relay.example.net")]),
            // With a public URL, the request's own host no longer counts.
            headers(&[
                ("origin", "http://127.0.0.1:8080"),
                ("host", "127.0.0.1:8080"),
            ]),
        ] {
            assert!(!same_origin(&refused, Some(&public)), "{refused:?}");
        }
    }

    #[test]
    fn hex_is_lowercase_two_digits_per_byte() {
        assert_eq!(hex(&[0x00, 0x0f, 0xab, 0xff]), "000fabff");
        assert_eq!(hex(&[]), "");
    }
}
