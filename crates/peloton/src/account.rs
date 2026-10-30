//! Account names, as a relay's deployment chooses them.

use std::fmt;
use std::str::FromStr;

const MAX_NAME_LEN: usize = 16;

/// The name of an account: 1 to 16 lowercase ASCII letters, chosen by the
/// deployment. Being a bare path component, it is safe to join onto a path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountName(String);

/// The text is not a valid [`AccountName`].
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("an account name is 1 to 16 lowercase ASCII letters")]
pub struct InvalidAccountName;

impl FromStr for AccountName {
    type Err = InvalidAccountName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if (1..=MAX_NAME_LEN).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase()) {
            Ok(Self(s.to_owned()))
        } else {
            Err(InvalidAccountName)
        }
    }
}

impl fmt::Display for AccountName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AccountName {
    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{AccountName, InvalidAccountName};

    #[test]
    fn account_names_follow_the_pattern() {
        for good in ["a", "alice", "abcdefghijklmnop"] {
            assert_eq!(good.parse::<AccountName>().unwrap().as_str(), good);
        }
        for bad in [
            "",
            "Alice",
            "al1ce",
            "al-ice",
            "abcdefghijklmnopq",
            " alice",
            "\u{e9}",
        ] {
            assert_eq!(
                bad.parse::<AccountName>(),
                Err(InvalidAccountName),
                "{bad:?}"
            );
        }
    }
}
