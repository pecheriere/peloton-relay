//! Peloton object identifiers.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};
use time::OffsetDateTime;

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

/// Longest base64 class id accepted; real ids are well under 300 characters.
const MAX_CLASS_ID_LEN: usize = 512;

/// The string is not a valid Peloton class id of the expected kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid Peloton class id")]
pub struct ParseClassIdError;

/// The bytes of a non-empty, bounded class id in base64 under `engine`, which
/// only accepts its canonical form.
fn class_id_bytes(s: &str, engine: &impl base64::Engine) -> Result<Vec<u8>, ParseClassIdError> {
    if s.is_empty() || s.len() > MAX_CLASS_ID_LEN {
        return Err(ParseClassIdError);
    }
    engine.decode(s).map_err(|_| ParseClassIdError)
}

/// Decodes a standard, padded base64 string holding a JSON object of type `T`,
/// returning it with the JSON bytes.
fn decode_class_id<T: serde::de::DeserializeOwned>(
    s: &str,
) -> Result<(T, Vec<u8>), ParseClassIdError> {
    let json = class_id_bytes(s, &base64::engine::general_purpose::STANDARD)?;
    let value = serde_json::from_slice(&json).map_err(|_| ParseClassIdError)?;
    Ok((value, json))
}

/// Peloton's id for an on-demand class, as the GraphQL API expects it:
/// standard base64 of a JSON object naming the ride.
///
/// Encoding produces exactly the bytes Peloton's web app sends,
/// `{"home_peloton_id": null, "ride_id": "<hex>", "studio_peloton_id": null, "type": "on_demand"}`;
/// parsing accepts any JSON layout of that object and keeps the string verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OnDemandClassId {
    encoded: String,
    ride: PelotonId,
}

#[derive(Deserialize)]
enum OnDemandType {
    #[serde(rename = "on_demand")]
    OnDemand,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OnDemandPayload {
    #[expect(
        dead_code,
        reason = "validated on parse; the value carries no information"
    )]
    home_peloton_id: Option<PelotonId>,
    ride_id: PelotonId,
    #[expect(
        dead_code,
        reason = "validated on parse; the value carries no information"
    )]
    studio_peloton_id: Option<PelotonId>,
    #[serde(rename = "type")]
    #[expect(dead_code, reason = "validated on parse; only one value is accepted")]
    kind: OnDemandType,
}

impl OnDemandClassId {
    /// Encodes the on-demand class id of `ride`.
    #[must_use]
    pub fn from_ride(ride: &PelotonId) -> Self {
        use base64::Engine as _;

        let json = format!(
            r#"{{"home_peloton_id": null, "ride_id": "{ride}", "studio_peloton_id": null, "type": "on_demand"}}"#
        );
        Self {
            encoded: base64::engine::general_purpose::STANDARD.encode(json),
            ride: ride.clone(),
        }
    }

    /// The base64 string sent to Peloton.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.encoded
    }

    /// The ride this class id names.
    #[must_use]
    pub fn ride_id(&self) -> &PelotonId {
        &self.ride
    }
}

impl FromStr for OnDemandClassId {
    type Err = ParseClassIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (payload, _): (OnDemandPayload, _) = decode_class_id(s)?;
        Ok(Self {
            encoded: s.to_owned(),
            ride: payload.ride_id,
        })
    }
}

/// Peloton's id for a scheduled class (an "event"), issued by Peloton and
/// passed back verbatim once it decodes to the expected shape.
///
/// Peloton's form is standard, padded base64, whose `+`, `/` and `=` do not
/// fit a URL path segment; [`Self::url_safe`] and [`Self::from_url_safe`]
/// convert to and from the same bytes in URL-safe base64 without padding.
/// Both directions accept only canonical base64, so the conversion is exact.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScheduledClassId {
    encoded: String,
    url_safe: String,
    ride: PelotonId,
    start: OffsetDateTime,
}

#[derive(Deserialize)]
enum EventType {
    #[serde(rename = "event")]
    Event,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventPayload {
    #[expect(dead_code, reason = "validated on parse; personal and not exposed")]
    home_peloton_id: PelotonId,
    ride_id: PelotonId,
    scheduled_start_time: i64,
    #[expect(
        dead_code,
        reason = "validated on parse; the value carries no information"
    )]
    studio_peloton_id: Option<PelotonId>,
    #[serde(rename = "type")]
    #[expect(dead_code, reason = "validated on parse; only one value is accepted")]
    kind: EventType,
}

impl ScheduledClassId {
    /// The base64 string sent to Peloton.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.encoded
    }

    /// The same id as URL-safe base64 without padding.
    #[must_use]
    pub fn url_safe(&self) -> &str {
        &self.url_safe
    }

    /// Parses the URL-safe form produced by [`Self::url_safe`], validating
    /// it as [`FromStr`] validates Peloton's form.
    ///
    /// # Errors
    ///
    /// [`ParseClassIdError`] unless `s` is canonical URL-safe base64 without
    /// padding of a valid scheduled-class id.
    pub fn from_url_safe(s: &str) -> Result<Self, ParseClassIdError> {
        use base64::Engine as _;

        let json = class_id_bytes(s, &base64::engine::general_purpose::URL_SAFE_NO_PAD)?;
        base64::engine::general_purpose::STANDARD
            .encode(json)
            .parse()
    }

    /// The ride that is scheduled.
    #[must_use]
    pub fn ride_id(&self) -> &PelotonId {
        &self.ride
    }

    /// When the class is scheduled to start.
    #[must_use]
    pub fn scheduled_start_time(&self) -> OffsetDateTime {
        self.start
    }
}

impl FromStr for ScheduledClassId {
    type Err = ParseClassIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use base64::Engine as _;

        let (payload, json): (EventPayload, _) = decode_class_id(s)?;
        let start = OffsetDateTime::from_unix_timestamp(payload.scheduled_start_time)
            .map_err(|_| ParseClassIdError)?;
        Ok(Self {
            encoded: s.to_owned(),
            url_safe: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json),
            ride: payload.ride_id,
            start,
        })
    }
}
