//! Posting the token set to a relay's `POST /admin/seed` with a one-time
//! code, instead of printing it.

use std::io::{self, BufRead, Write};
use std::net::IpAddr;
use std::str::FromStr;
use std::time::Duration;

use peloton::account::AccountName;
use peloton::body::read_limited;
use peloton::hostname::is_dns_name;
use peloton::oauth::TokenSet;
use peloton::text::shown;
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use url::{Host, Url};

/// How long the relay may take: it waits up to 45 s for its locks and calls
/// Peloton's `/me` before it answers.
const POST_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest relay message shown.
const MAX_MESSAGE_CHARS: usize = 200;

/// Where to post: an `https` URL of an IP address or a DNS name of letters,
/// digits and hyphens, without credentials or fragment, or an `http` URL of a
/// loopback address, whose traffic never leaves the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedUrl(Url);

impl FromStr for SeedUrl {
    type Err = &'static str;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        const EXPECTED: &str = "expected the https URL the admin page shows, like https://relay.example.net/admin/seed";
        let url = Url::parse(text).map_err(|_| EXPECTED)?;
        let loopback = match url.host() {
            Some(Host::Ipv4(ip)) => IpAddr::V4(ip).is_loopback(),
            Some(Host::Ipv6(ip)) => IpAddr::V6(ip).is_loopback(),
            Some(Host::Domain(_)) | None => false,
        };
        let named = match url.host() {
            Some(Host::Ipv4(_) | Host::Ipv6(_)) => true,
            Some(Host::Domain(name)) => is_dns_name(name),
            None => false,
        };
        let secure = match url.scheme() {
            "https" => named,
            "http" => loopback,
            _ => false,
        };
        if secure
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
        {
            Ok(Self(url))
        } else {
            Err(EXPECTED)
        }
    }
}

impl SeedUrl {
    /// The host and port the token set goes to, as the user should check it.
    #[must_use]
    pub fn destination(&self) -> String {
        let host = self.0.host_str().unwrap_or_default();
        match self.0.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        }
    }
}

/// Asks on `output` whether to send the sign-in of `account` to `to`, and
/// reads the answer from `input`: only `y` or `yes` (any case) is a yes.
///
/// # Errors
///
/// [`PostError::Prompt`] when the question cannot be written or the answer
/// read.
pub fn confirm(
    to: &SeedUrl,
    account: &AccountName,
    mut input: impl BufRead,
    mut output: impl Write,
) -> Result<bool, PostError> {
    write!(
        output,
        "The Peloton sign-in for relay account \"{account}\" will be sent to {}. Continue? [y/N] ",
        to.destination()
    )
    .and_then(|()| output.flush())
    .map_err(PostError::Prompt)?;
    let mut answer = String::new();
    input.read_line(&mut answer).map_err(PostError::Prompt)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// The one-time code: 32 lowercase hex characters.
#[derive(Debug, Clone)]
pub struct Code(SecretString);

impl FromStr for Code {
    type Err = &'static str;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.len() == 32 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(Self(SecretString::from(text)))
        } else {
            Err("the code is the 32 hex characters the admin page shows")
        }
    }
}

/// Why posting the token set failed. Never carries the code, the tokens or
/// the URL.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PostError {
    /// Stdin is not a terminal, so the destination cannot be confirmed.
    #[error("stdin is not a terminal to confirm the relay on; pass --yes to post without asking")]
    NoTerminal,
    /// The user did not confirm the destination.
    #[error("nothing was sent: the relay was not confirmed")]
    Declined,
    /// The confirmation could not be asked or read.
    #[error("could not ask for confirmation")]
    Prompt(#[source] io::Error),
    /// The HTTP client could not be built.
    #[error("could not build the HTTP client")]
    Client(#[source] reqwest::Error),
    /// The relay could not be reached.
    #[error("could not reach the relay")]
    Send(#[source] reqwest::Error),
    /// The relay's answer could not be read.
    #[error("could not read the relay's answer")]
    Read,
    /// The code is wrong, expired, spent or for another account.
    #[error(
        "the relay refused the code: it is wrong, expired, already used or for another account; start again on the admin page"
    )]
    InvalidCode,
    /// The request was not marked as admin, or a form check failed.
    #[error(
        "the relay refused the request (403); --post-to must go through the gateway that grants admin access"
    )]
    Forbidden,
    /// No such account or route.
    #[error("the relay has no such account or route (404); check --account and --post-to")]
    NotFound,
    /// The relay refused the sign-in for a reason it explains.
    #[error("the relay refused the sign-in ({status}): {message}")]
    Refused {
        /// The HTTP status.
        status: u16,
        /// The relay's explanation, made safe to show.
        message: String,
    },
    /// Too many wrong codes lately.
    #[error("the relay refuses codes for now after too many wrong ones; retry in {retry_after} s")]
    TooManyAttempts {
        /// Seconds to wait, as the relay said.
        retry_after: u64,
    },
    /// Any other answer.
    #[error("the relay answered with status {status}")]
    Status {
        /// The HTTP status.
        status: u16,
    },
}

/// What the relay answers, success or error.
#[derive(Deserialize)]
struct Answer {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// The HTTP client for the post: no redirects (the token set must never be
/// sent elsewhere), bounded connect and total time.
///
/// # Errors
///
/// [`PostError::Client`] when the TLS backend cannot be initialised.
pub fn client() -> Result<reqwest::Client, PostError> {
    peloton::tls::client_builder()
        .map_err(|err| PostError::Client(err.without_url()))?
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(POST_TIMEOUT)
        .build()
        .map_err(|err| PostError::Client(err.without_url()))
}

/// Posts `tokens` for `account` with `code` to `to`, and returns the relay's
/// confirmation, the line `seed` prints, made safe to show.
///
/// # Errors
///
/// See [`PostError`].
pub async fn post(
    http: &reqwest::Client,
    to: &SeedUrl,
    account: &AccountName,
    code: &Code,
    tokens: &TokenSet,
) -> Result<String, PostError> {
    let body = serde_json::json!({
        "account": account.as_str(),
        "code": code.0.expose_secret(),
        "tokens": tokens,
    });
    let response = http
        .post(to.0.clone())
        .json(&body)
        .send()
        .await
        .map_err(|err| PostError::Send(err.without_url()))?;
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    let bytes = read_limited(response).await.map_err(|_| PostError::Read)?;
    let answer: Option<Answer> = serde_json::from_slice(&bytes).ok();
    let message = answer
        .as_ref()
        .and_then(|answer| answer.message.as_deref())
        .map(|message| shown(&message.chars().take(MAX_MESSAGE_CHARS).collect::<String>()));
    let error = answer.as_ref().and_then(|answer| answer.error.as_deref());
    match (status, error) {
        (200, _) => Ok(message.unwrap_or_else(|| format!("account \"{account}\": seeded"))),
        (403, Some("invalid_code")) => Err(PostError::InvalidCode),
        (403, _) => Err(PostError::Forbidden),
        (404, _) => Err(PostError::NotFound),
        (429, _) => Err(PostError::TooManyAttempts {
            retry_after: retry_after.unwrap_or(600),
        }),
        (400 | 409 | 413 | 415 | 422 | 502 | 503, _) => match message {
            Some(message) => Err(PostError::Refused { status, message }),
            None => Err(PostError::Status { status }),
        },
        _ => Err(PostError::Status { status }),
    }
}

#[cfg(test)]
mod tests {
    use peloton::account::AccountName;

    use super::{Code, SeedUrl, confirm};

    #[test]
    fn the_question_names_the_account_and_the_destination() {
        let to: SeedUrl = "https://relay.example.net:8443/admin/seed".parse().unwrap();
        let bob: AccountName = "bob".parse().unwrap();
        for (answer, yes) in [
            ("y\n", true),
            ("YES\n", true),
            (" yes \n", true),
            ("\n", false),
            ("n\n", false),
            ("sure\n", false),
            ("", false),
        ] {
            let mut shown = Vec::new();
            assert_eq!(
                confirm(&to, &bob, answer.as_bytes(), &mut shown).unwrap(),
                yes,
                "{answer:?}"
            );
            assert_eq!(
                String::from_utf8(shown).unwrap(),
                "The Peloton sign-in for relay account \"bob\" will be sent to relay.example.net:8443. Continue? [y/N] "
            );
        }
    }

    #[test]
    fn urls_must_be_https_or_loopback_http() {
        for good in [
            "https://relay.example.net/admin/seed",
            "https://relay.example.net:8443/admin/seed",
            "http://127.0.0.1:8080/admin/seed",
            "http://[::1]:8080/admin/seed",
        ] {
            assert!(good.parse::<SeedUrl>().is_ok(), "{good}");
        }
        for bad in [
            "",
            "relay.example.net/admin/seed",
            "http://relay.example.net/admin/seed",
            "http://localhost:8080/admin/seed",
            "http://192.0.2.7/admin/seed",
            "ftp://relay.example.net/admin/seed",
            "https://user:pass@relay.example.net/admin/seed",
            "https://relay.example.net/admin/seed#x",
            "file:///admin/seed",
            "https://relay.example.net&x/admin/seed",
            "https://a$b.example.net/admin/seed",
        ] {
            assert!(bad.parse::<SeedUrl>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn codes_are_32_lowercase_hex() {
        assert!("0123456789abcdef0123456789abcdef".parse::<Code>().is_ok());
        for bad in [
            "",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcdeg",
        ] {
            assert!(bad.parse::<Code>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_rejected_code_is_not_echoed() {
        let error = "0123456789abcdef-secret-code-xyz"
            .parse::<Code>()
            .unwrap_err();
        assert!(!error.contains("secret"));
    }
}
