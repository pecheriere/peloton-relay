//! `check`: prove login, refresh and `GET /me` work, printing no secrets.

use peloton::endpoints;
use peloton::ids::PelotonId;
use peloton::oauth::TokenSet;
use secrecy::ExposeSecret as _;
use serde::Deserialize;
use time::OffsetDateTime;

use crate::session::{Session, SessionError};

/// The only `/me` fields this spike reads.
#[derive(Deserialize)]
pub struct Me {
    /// The user's id.
    pub id: PelotonId,
    /// The user's display name.
    pub username: String,
}

/// The `/me` response lacked a valid `id` and `username`. The parse error is
/// dropped on purpose: it could echo parts of the body.
#[derive(Debug, thiserror::Error)]
#[error("unexpected /me response shape")]
pub struct MeShapeError;

/// Why `check` failed. Never carries tokens or bodies.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CheckError {
    /// Login, refresh or a request failed.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// `/me` answered with something unexpected.
    #[error(transparent)]
    Me(#[from] MeShapeError),
}

/// Parses the `/me` response body.
///
/// # Errors
///
/// Returns [`MeShapeError`] when the body lacks a valid `id` and `username`.
pub fn parse_me(body: &[u8]) -> Result<Me, MeShapeError> {
    serde_json::from_slice(body).map_err(|_| MeShapeError)
}

/// Whether `set` carries a non-empty refresh token.
fn has_refresh_token(set: &TokenSet) -> bool {
    !set.refresh_token.expose_secret().is_empty()
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

/// Logs in, calls `/me`, refreshes once and calls `/me` with the new token.
///
/// # Errors
///
/// Returns a [`CheckError`] without secrets when any step fails.
pub fn run(session: &Session) -> Result<(), CheckError> {
    let first = session.login()?;
    let lifetime = (first.expires_at - OffsetDateTime::now_utc()).whole_seconds();
    let me = parse_me(&session.get(&first.access_token, &endpoints::me())?)?;

    let second = session.refresh(&first.refresh_token)?;
    let issued = has_refresh_token(&first);
    let rotated = second.refresh_token.expose_secret() != first.refresh_token.expose_secret();
    parse_me(&session.get(&second.access_token, &endpoints::me())?)?;

    eprintln!("user id:                 {}", me.id);
    eprintln!("username:                {}", me.username);
    eprintln!("refresh token issued:    {}", yes_no(issued));
    eprintln!("refresh rotated it:      {}", yes_no(rotated));
    eprintln!("access token lifetime:   about {lifetime} seconds");
    eprintln!("/me with refreshed token: ok");
    Ok(())
}

#[cfg(test)]
mod tests {
    use peloton::oauth::TokenSet;
    use secrecy::SecretString;
    use time::OffsetDateTime;

    use super::{has_refresh_token, parse_me, yes_no};

    fn token_set(refresh: &str) -> TokenSet {
        TokenSet {
            access_token: SecretString::from("access".to_owned()),
            refresh_token: SecretString::from(refresh.to_owned()),
            expires_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn refresh_token_issued_follows_the_token_set() {
        assert!(has_refresh_token(&token_set("refresh")));
        assert!(!has_refresh_token(&token_set("")));
        assert_eq!(yes_no(true), "yes");
        assert_eq!(yes_no(false), "no");
    }

    #[test]
    fn parses_only_needed_fields() {
        let body = br#"{"id":"0123456789abcdef0123456789abcdef","username":"rider","x":1}"#;
        let me = parse_me(body).expect("valid");
        assert_eq!(me.username, "rider");
    }

    #[test]
    fn rejects_invalid_id_without_echoing_the_body() {
        let error = parse_me(br#"{"id":"../etc","username":"rider"}"#)
            .map(|_| ())
            .expect_err("invalid id");
        assert_eq!(crate::report(&error), "unexpected /me response shape");
    }
}
