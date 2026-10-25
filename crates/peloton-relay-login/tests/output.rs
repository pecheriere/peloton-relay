use peloton::oauth::TokenSet;
use peloton_relay_login::output::{OutputError, write_tokens};
use secrecy::{ExposeSecret, SecretString};
use time::macros::datetime;

fn sample() -> TokenSet {
    TokenSet {
        access_token: SecretString::from("access-value"),
        refresh_token: SecretString::from("refresh-value"),
        expires_at: datetime!(2026-10-25 12:30:00 UTC),
    }
}

#[test]
fn refuses_to_write_to_terminal() {
    let mut buf = Vec::new();
    let err = write_tokens(&sample(), &mut buf, true).unwrap_err();
    assert!(matches!(err, OutputError::RefusingTerminal));
    assert!(buf.is_empty(), "nothing may be written to a terminal");
}

#[test]
fn writes_json_round_trip() {
    let set = sample();
    let mut buf = Vec::new();
    write_tokens(&set, &mut buf, false).unwrap();
    let parsed: TokenSet = serde_json::from_slice(&buf).unwrap();
    assert_eq!(
        parsed.access_token.expose_secret(),
        set.access_token.expose_secret()
    );
    assert_eq!(
        parsed.refresh_token.expose_secret(),
        set.refresh_token.expose_secret()
    );
    assert_eq!(parsed.expires_at, set.expires_at);
}
