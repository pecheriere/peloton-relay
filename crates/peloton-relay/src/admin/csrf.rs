//! CSRF tokens for the admin page's forms.
//!
//! A token is the HMAC-SHA256 (aws-lc's, verified in constant time), under a
//! 256-bit key drawn once per process from the thread-local CSPRNG, of the
//! form's action and account, so it is valid only for that form and only
//! until the relay restarts.

use std::fmt;

use aws_lc_rs::hmac;

use crate::account::AccountName;

/// The length of a key, and of a tag, in bytes.
const TAG_LEN: usize = 32;

/// A state-changing form of the admin page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Issue a one-time sign-in code.
    IssueCode,
    /// Delete the stored credential.
    Revoke,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::IssueCode => "code",
            Self::Revoke => "revoke",
        }
    }
}

/// Issues and checks CSRF tokens.
pub struct Csrf {
    key: hmac::Key,
}

impl fmt::Debug for Csrf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Csrf").finish_non_exhaustive()
    }
}

impl Default for Csrf {
    fn default() -> Self {
        Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, &rand::random::<[u8; TAG_LEN]>()),
        }
    }
}

/// What a token authenticates: the action, a NUL and the account. Action
/// names hold no NUL, so the pair is unambiguous.
fn message(action: Action, account: &AccountName) -> Vec<u8> {
    [
        action.as_str().as_bytes(),
        b"\0",
        account.as_str().as_bytes(),
    ]
    .concat()
}

/// `text` as bytes, if it is lowercase hex of exactly one tag.
fn unhex(text: &str) -> Option<Vec<u8>> {
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    if text.len() != TAG_LEN * 2 {
        return None;
    }
    let (pairs, _) = text.as_bytes().as_chunks::<2>();
    pairs
        .iter()
        .map(|&[high, low]| Some(digit(high)? << 4 | digit(low)?))
        .collect()
}

impl Csrf {
    /// The token of form `action` for `account`: 64 lowercase hex characters.
    #[must_use]
    pub fn token(&self, action: Action, account: &AccountName) -> String {
        super::hex(hmac::sign(&self.key, &message(action, account)).as_ref())
    }

    /// Whether `token` is the token of form `action` for `account`; the tag
    /// is compared in constant time.
    #[must_use]
    pub fn verify(&self, action: Action, account: &AccountName, token: &str) -> bool {
        unhex(token)
            .is_some_and(|tag| hmac::verify(&self.key, &message(action, account), &tag).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use aws_lc_rs::hmac;

    use super::{Action, Csrf, unhex};
    use crate::account::AccountName;

    fn mac(key: &[u8], message: &[u8]) -> String {
        super::super::hex(hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), message).as_ref())
    }

    #[test]
    fn hmac_matches_the_rfc_4231_vectors() {
        // Test cases 1 to 3.
        assert_eq!(
            mac(&[0x0b; 20], b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            mac(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            mac(&[0xaa; 20], &[0xdd; 50]),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
    }

    #[test]
    fn unhex_takes_exactly_one_lowercase_tag() {
        let tag = "00ff".repeat(16);
        assert_eq!(unhex(&tag).unwrap(), [0x00, 0xff].repeat(16));
        for bad in [
            String::new(),
            "00ff".repeat(15),
            format!("{tag}00"),
            "00FF".repeat(16),
            "0g".repeat(32),
            "é".repeat(32),
        ] {
            assert_eq!(unhex(&bad), None, "{bad:?}");
        }
    }

    fn name(text: &str) -> AccountName {
        text.parse().unwrap()
    }

    #[test]
    fn a_token_is_valid_for_its_form_only() {
        let csrf = Csrf::default();
        let token = csrf.token(Action::Revoke, &name("bob"));
        assert_eq!(token.len(), 64);
        assert!(csrf.verify(Action::Revoke, &name("bob"), &token));
        assert!(!csrf.verify(Action::IssueCode, &name("bob"), &token));
        assert!(!csrf.verify(Action::Revoke, &name("alice"), &token));
        assert!(!csrf.verify(Action::Revoke, &name("bob"), &token[..63]));
        assert!(!csrf.verify(Action::Revoke, &name("bob"), ""));
        let mut flipped = token.clone().into_bytes();
        flipped[0] = if flipped[0] == b'0' { b'1' } else { b'0' };
        assert!(!csrf.verify(
            Action::Revoke,
            &name("bob"),
            &String::from_utf8(flipped).unwrap()
        ));
    }

    #[test]
    fn tokens_differ_between_processes() {
        let token = Csrf::default().token(Action::Revoke, &name("bob"));
        assert!(!Csrf::default().verify(Action::Revoke, &name("bob"), &token));
    }
}
