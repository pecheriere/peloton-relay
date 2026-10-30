//! The admin page's HTML. Every value is escaped by `maud`; only the page's
//! own constant style, script and Pacer drawings are inserted as is.

use maud::{DOCTYPE, Markup, PreEscaped, html};
use peloton::ids::PelotonId;
use peloton::text::shown;
use secrecy::{ExposeSecret as _, SecretString};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use super::codes::CODE_TTL;
use super::{PAGE_CSS, PAGE_JS};
use crate::account::AccountName;
use crate::tokens::{CredentialStatus, Health};

const FACE: &str = include_str!("pacer/face.svg");
const READY: &str = include_str!("pacer/ready.svg");
const RESTING: &str = include_str!("pacer/resting.svg");
const OOPS: &str = include_str!("pacer/oops.svg");
const CHEERING: &str = include_str!("pacer/cheering.svg");

/// The Peloton user an account is bound to, as the page shows it.
pub(super) enum User {
    /// Never signed in: no binding yet.
    Unbound,
    /// The binding cannot be read.
    Unreadable,
    /// Bound to `id`.
    Bound {
        id: PelotonId,
        /// The id's last four hex digits.
        suffix: String,
        /// The username, when Peloton reported one.
        username: Option<String>,
    },
}

/// One account's card.
pub(super) struct AccountView {
    pub(super) name: AccountName,
    pub(super) status: CredentialStatus,
    pub(super) user: User,
    /// The CSRF token of its renew form.
    pub(super) code_token: String,
    /// The CSRF token of its revoke form.
    pub(super) revoke_token: String,
}

/// The steps of a renewal, shown once with the command holding the code.
pub(super) struct Renew {
    pub(super) account: AccountName,
    pub(super) command: SecretString,
}

/// What a form just did.
pub(super) enum Notice {
    /// A revoke signed `account` out.
    Revoked { account: AccountName },
}

/// Everything the page shows.
pub(super) struct View {
    pub(super) accounts: Vec<AccountView>,
    /// The relay's clock, for relative times.
    pub(super) now: OffsetDateTime,
    /// Whether the public URL is set, so codes can be issued.
    pub(super) renew_enabled: bool,
    pub(super) renew: Option<Renew>,
    pub(super) notice: Option<Notice>,
}

/// The status class and word of a health.
fn health_look(health: Health) -> (&'static str, &'static str) {
    match health {
        Health::Ok => ("ok", "Healthy"),
        Health::NeedsSignIn => ("warn", "Needs sign-in"),
        Health::Unreadable => ("bad", "Unreadable"),
    }
}

/// The page.
pub(super) fn render(view: &View) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Relay admin" }
                style { (PreEscaped(PAGE_CSS)) }
            }
            body {
                div.shell {
                    header.top {
                        (PreEscaped(FACE))
                        span.brand { "Relay" }
                        span.version { "relay " (env!("CARGO_PKG_VERSION")) }
                    }
                    main {
                        (hero(view))
                        @if let Some(Notice::Revoked { account }) = &view.notice {
                            p.notice role="status" {
                                strong { (account) } " is signed out. Its callers are not authenticated until it is signed in again."
                            }
                        }
                        @if !view.renew_enabled {
                            p.notice.quiet {
                                "Signing in from this page is off: set "
                                code.mono { "PELOTON_RELAY_PUBLIC_URL" }
                                " to the relay's https address. Until then, sign in with "
                                code.mono { "seed" }
                                "."
                            }
                        }
                        @if let Some(renew) = &view.renew {
                            (renew_panel(renew, view))
                        }
                        section.cards aria-label="Accounts" {
                            @for account in &view.accounts {
                                (card(account, view))
                            }
                        }
                    }
                }
                script { (PreEscaped(PAGE_JS)) }
            }
        }
    }
}

/// Pacer's mood and the summary line.
fn hero(view: &View) -> Markup {
    let count = |health| {
        view.accounts
            .iter()
            .filter(|account| account.status.health == health)
            .count()
    };
    let (needs, unreadable) = (count(Health::NeedsSignIn), count(Health::Unreadable));
    let mood = if unreadable > 0 {
        OOPS
    } else if needs > 0 {
        RESTING
    } else {
        READY
    };
    let total = view.accounts.len();
    html! {
        section.hero {
            // The renew panel has its own Pacer: one per screen.
            @if view.renew.is_none() {
                (PreEscaped(mood))
            }
            div {
                h1 { "Accounts" }
                p.summary {
                    (total) @if total == 1 { " account" } @else { " accounts" }
                    @if needs == 0 && unreadable == 0 { " · all healthy" }
                    @if needs > 0 { " · " (needs) @if needs == 1 { " needs" } @else { " need" } " sign-in" }
                    @if unreadable > 0 { " · " (unreadable) " unreadable" }
                }
            }
        }
    }
}

/// The account whose renew button is the page's one primary action: the
/// first that needs a sign-in or is unreadable, unless the renew panel is
/// shown, whose Copy button is then the primary action.
fn primary(view: &View) -> Option<&AccountName> {
    if view.renew.is_some() {
        return None;
    }
    view.accounts
        .iter()
        .find(|account| account.status.health != Health::Ok)
        .map(|account| &account.name)
}

/// One account's card.
fn card(account: &AccountView, view: &View) -> Markup {
    let (class, word) = health_look(account.status.health);
    let name = &account.name;
    let initial = name.as_str().chars().next().map(|c| c.to_ascii_uppercase());
    let first_sign_in = matches!(account.user, User::Unbound);
    // An unbound account can still hold an unusable state file to clear.
    let revocable = !first_sign_in || account.status.health == Health::Unreadable;
    html! {
        article class={ "card " (class) } aria-labelledby={ "account-" (name) } {
            div.card-head {
                span.avatar aria-hidden="true" { @if let Some(initial) = initial { (initial) } }
                h2 id={ "account-" (name) } { (name) }
                span class={ "pill " (class) } { span.dot {} (word) }
            }
            dl.facts {
                div {
                    dt { "Peloton user" }
                    dd {
                        @match &account.user {
                            User::Unbound => "Not signed in yet",
                            User::Unreadable => "Binding unreadable",
                            User::Bound { suffix, username, .. } => {
                                @if let Some(username) = username {
                                    (shown(username)) " · "
                                }
                                span.mono { (suffix) }
                            }
                        }
                    }
                }
                div {
                    dt { "Last refresh" }
                    dd.mono {
                        @match account.status.last_refresh {
                            Some(at) => time datetime=(rfc3339(at)) title=(stamp(at)) { (ago(at, view.now)) },
                            None => "None since start",
                        }
                    }
                }
                div {
                    dt { "Access token" }
                    dd.mono {
                        @match account.status.expires_at {
                            Some(at) => time datetime=(rfc3339(at)) title=(stamp(at)) { (expiry(at, view.now)) },
                            None => "None",
                        }
                    }
                }
            }
            @if !account.status.incidents.is_empty() {
                div.incidents {
                    h3 { "Recent problems" }
                    ul {
                        @for incident in &account.status.incidents {
                            li {
                                span.dot {}
                                span.what { (incident.kind.message()) }
                                time datetime=(rfc3339(incident.at)) title=(stamp(incident.at)) { (ago(incident.at, view.now)) }
                            }
                        }
                    }
                }
            }
            div.actions {
                @if view.renew_enabled {
                    form method="post" action={ "/admin/accounts/" (name) "/code" } {
                        input type="hidden" name="csrf" value=(account.code_token);
                        button type="submit" class={
                            "button " @if primary(view) == Some(name) { "primary" } @else { "secondary" }
                        } {
                            @if first_sign_in { "Sign in" } @else { "Renew sign-in" }
                        }
                    }
                }
                @if revocable {
                    details.revoke {
                        summary { "Revoke sign-in" }
                        div.confirm {
                            p {
                                "This deletes the stored sign-in of " strong { (name) } ". Its callers get “not authenticated” until it is signed in again."
                                @if !first_sign_in { " The Peloton user it is bound to stays bound." }
                            }
                            form method="post" action={ "/admin/accounts/" (name) "/revoke" } {
                                input type="hidden" name="csrf" value=(account.revoke_token);
                                button.button.danger type="submit" { "Really revoke " (name) }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The steps of a renewal, with the command holding the code.
fn renew_panel(renew: &Renew, view: &View) -> Markup {
    let name = &renew.account;
    let first_sign_in = view
        .accounts
        .iter()
        .any(|account| account.name == *name && matches!(account.user, User::Unbound));
    let minutes = CODE_TTL.as_secs() / 60;
    html! {
        section.panel aria-labelledby="renew-title" data-status={ "/admin/accounts/" (name) "/status" } {
            div.panel-head {
                (PreEscaped(RESTING))
                div {
                    h2 #renew-title { @if first_sign_in { "Sign in " } @else { "Renew " } (name) }
                    p { "Signing in happens on your Mac, in Peloton’s own page. The relay never sees the password." }
                }
            }
            ol.steps #renew-steps {
                li.step {
                    span.num.active { "1" }
                    div.step-body {
                        span.step-title { "Copy the command" }
                        div.command {
                            code #renew-command { (renew.command.expose_secret()) }
                            button.button.primary type="button" data-copy="renew-command" hidden {
                                span.label { "Copy command" }
                            }
                        }
                    }
                }
                li.step {
                    span.num { "2" }
                    div.step-body {
                        span.step-title { "Run it in Terminal on your Mac" }
                        span { "A sign-in window opens. The person who owns this Peloton account signs in." }
                    }
                }
                li.step {
                    span.num { "3" }
                    div.step-body {
                        span.step-title { "Waiting for the relay" }
                        span.without-script {
                            "When the helper says the account is bound, "
                            a href="/admin" { "reload the accounts" }
                            "."
                        }
                        span.with-script hidden { "This page updates by itself." }
                    }
                    span.spinner.with-script hidden aria-hidden="true" {}
                }
            }
            div.outcome #renew-done hidden role="status" {
                (PreEscaped(CHEERING))
                p.outcome-title { "Signed in" }
                p { strong { (name) } " is signed in." }
                a href="/admin" { "Back to accounts" }
            }
            div.outcome #renew-refused hidden role="status" {
                (PreEscaped(OOPS))
                p.outcome-title { "Not signed in" }
                p { "The relay refused that Peloton user for " strong { (name) } "; the helper says why." }
                a href="/admin" { "Back to accounts" }
            }
            div.outcome #renew-unusable hidden role="status" {
                (PreEscaped(OOPS))
                p.outcome-title { "Signed in, not in use" }
                p { "The relay stored the sign-in of " strong { (name) } " but cannot use it yet; the accounts page shows why." }
                a href="/admin" { "Back to accounts" }
            }
            div.outcome #renew-expired hidden role="status" {
                (PreEscaped(RESTING))
                p.outcome-title { "The code expired" }
                p { "Start again to get a new one." }
                a href="/admin" { "Back to accounts" }
            }
            div.outcome #renew-restarted hidden role="status" {
                (PreEscaped(RESTING))
                p.outcome-title { "The relay restarted" }
                p { "The code is gone with it. Start again to get a new one." }
                a href="/admin" { "Back to accounts" }
            }
            p.fine #renew-note {
                "The command holds a one-time code that expires in " (minutes) " minutes. It only works for “" (name) "”"
                @if first_sign_in {
                    ", and binds it to the Peloton user who signs in."
                } @else {
                    ", and only for the Peloton user bound to it."
                }
            }
        }
    }
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

/// `2026-10-28 21:04 UTC`.
fn stamp(at: OffsetDateTime) -> String {
    let at = at.to_offset(UtcOffset::UTC);
    format!(
        "{}-{:02}-{:02} {:02}:{:02} UTC",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute()
    )
}

/// How long ago `at` was: `just now`, `12 min ago`, `5 h ago`, `3 days ago`.
fn ago(at: OffsetDateTime, now: OffsetDateTime) -> String {
    let elapsed = now - at;
    if elapsed.whole_minutes() < 1 {
        "just now".to_owned()
    } else if elapsed.whole_hours() < 1 {
        format!("{} min ago", elapsed.whole_minutes())
    } else if elapsed.whole_hours() < 48 {
        format!("{} h ago", elapsed.whole_hours())
    } else {
        format!("{} days ago", elapsed.whole_days())
    }
}

/// When `at` comes: `expires in 12 min`, `expires in 47 h`, or `expired`.
fn expiry(at: OffsetDateTime, now: OffsetDateTime) -> String {
    let left = at - now;
    if left.is_negative() || left.is_zero() {
        "expired".to_owned()
    } else if left.whole_hours() < 1 {
        format!("expires in {} min", left.whole_minutes().max(1))
    } else if left.whole_hours() < 48 {
        format!("expires in {} h", left.whole_hours())
    } else {
        format!("expires in {} days", left.whole_days())
    }
}

#[cfg(test)]
mod tests {
    use time::Duration;
    use time::macros::datetime;

    use super::{ago, expiry, stamp};

    #[test]
    fn stamps_are_utc_to_the_minute() {
        assert_eq!(
            stamp(datetime!(2026-10-28 22:04:59 +01:00)),
            "2026-10-28 21:04 UTC"
        );
    }

    #[test]
    fn ago_rounds_down_to_a_readable_unit() {
        let now = datetime!(2026-10-28 12:00 UTC);
        assert_eq!(ago(now + Duration::minutes(3), now), "just now");
        assert_eq!(ago(now - Duration::seconds(59), now), "just now");
        assert_eq!(ago(now - Duration::minutes(12), now), "12 min ago");
        assert_eq!(ago(now - Duration::minutes(119), now), "1 h ago");
        assert_eq!(ago(now - Duration::hours(47), now), "47 h ago");
        assert_eq!(ago(now - Duration::hours(72), now), "3 days ago");
    }

    #[test]
    fn expiry_says_how_long_is_left() {
        let now = datetime!(2026-10-28 12:00 UTC);
        assert_eq!(expiry(now, now), "expired");
        assert_eq!(expiry(now - Duration::minutes(1), now), "expired");
        assert_eq!(expiry(now + Duration::seconds(20), now), "expires in 1 min");
        assert_eq!(
            expiry(now + Duration::minutes(30), now),
            "expires in 30 min"
        );
        assert_eq!(expiry(now + Duration::hours(47), now), "expires in 47 h");
        assert_eq!(expiry(now + Duration::hours(96), now), "expires in 4 days");
    }
}
