//! `check`: prove login, refresh and `GET /me` work, printing no secrets.

use peloton::endpoints;
use peloton::ids::PelotonId;
use secrecy::ExposeSecret as _;
use serde::Deserialize;
use time::OffsetDateTime;

use crate::session::Session;

/// The only `/me` fields this spike reads.
#[derive(Deserialize)]
pub struct Me {
    /// The user's id.
    pub id: PelotonId,
    /// The user's display name.
    pub username: String,
}

/// Parses the `/me` response body.
///
/// # Errors
///
/// Returns a message when the body lacks a valid `id` and `username`.
pub fn parse_me(body: &[u8]) -> Result<Me, String> {
    serde_json::from_slice(body).map_err(|_| "unexpected /me response shape".to_owned())
}

/// Logs in, calls `/me`, refreshes once and calls `/me` with the new token.
///
/// # Errors
///
/// Returns a message without secrets when any step fails.
pub fn run(session: &Session) -> Result<(), String> {
    let first = session.login()?;
    let lifetime = (first.expires_at - OffsetDateTime::now_utc()).whole_seconds();
    let me = parse_me(&session.get(&first.access_token, &endpoints::me())?)?;

    let second = session.refresh(&first.refresh_token)?;
    let rotated = second.refresh_token.expose_secret() != first.refresh_token.expose_secret();
    parse_me(&session.get(&second.access_token, &endpoints::me())?)?;

    eprintln!("user id:                 {}", me.id);
    eprintln!("username:                {}", me.username);
    eprintln!("refresh token issued:    yes");
    eprintln!(
        "refresh rotated it:      {}",
        if rotated { "yes" } else { "no" }
    );
    eprintln!("access token lifetime:   about {lifetime} seconds");
    eprintln!("/me with refreshed token: ok");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_me;

    #[test]
    fn parses_only_needed_fields() {
        let body = br#"{"id":"0123456789abcdef0123456789abcdef","username":"rider","x":1}"#;
        let me = parse_me(body).expect("valid");
        assert_eq!(me.username, "rider");
    }

    #[test]
    fn rejects_invalid_id() {
        assert!(parse_me(br#"{"id":"../etc","username":"rider"}"#).is_err());
    }
}
