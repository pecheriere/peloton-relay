//! Peloton object identifiers.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};

const ID_LEN: usize = 32;

/// A Peloton object id: exactly 32 lowercase hexadecimal characters.
///
/// Validation keeps attacker-controlled strings out of upstream URL paths.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct PelotonId(String);

/// The string is not a valid Peloton id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid Peloton id: expected 32 lowercase hexadecimal characters")]
pub struct ParseIdError;

impl FromStr for PelotonId {
    type Err = ParseIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let valid = s.len() == ID_LEN && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if valid {
            Ok(Self(s.to_owned()))
        } else {
            Err(ParseIdError)
        }
    }
}

impl fmt::Display for PelotonId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PelotonId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}
