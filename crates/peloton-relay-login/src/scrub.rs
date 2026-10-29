//! `scrub`: turn raw Peloton captures into fixtures that are safe to publish.
//!
//! Rules, applied to every file of a run with one shared state so that the
//! same input always maps to the same output across files:
//!
//! - every run of 32 or more hex characters (ids, hashes) and every UUID is
//!   replaced by a fake of the same shape; the all-zero id is kept;
//! - objects describing a person (they carry `username`, `first_name` or
//!   `last_name`: the member and instructors) have every free-text field
//!   that is not a bare id replaced by a `<key>-<n>` placeholder, and their names, usernames and
//!   locations are additionally erased (whole word, any case) from every
//!   other string in the run;
//! - URLs, opaque tokens, locations, e-mails, device serials, time zones,
//!   birthdays, heights and weights are replaced with fixed values; lists of
//!   devices, contracts, social links and Q&As are emptied;
//! - values derived from age or weight are neutralised: maximum heart rates
//!   and the heart-rate zone bounds computed from them use a fixed maximum,
//!   and every personal calorie figure is scaled by one factor, random per
//!   run (calories of timed classes are MET x weight x time);
//! - heart-rate zone durations (which encode the thresholds, hence the age,
//!   in force at the time) are recomputed from each workout's own HR series
//!   with the fixed zones, or set to 0 when the run has no HR series for that
//!   workout; scores derived from them (effort points) become `null`;
//! - FTP values (member setting, test result, estimate and per-workout) get
//!   one fixed value;
//! - leaderboard ranks and sizes get fixed values (rank, class and output
//!   together could single the member out on the public leaderboard);
//! - GPS points become a fixed synthetic track with nothing else kept;
//!   per-month activity buckets are emptied;
//! - every epoch timestamp of the member (workouts, the profile, anything
//!   outside a class) is shifted by one offset, random per run; a
//!   device-local `device_time_*` copy of a timestamp (which would reveal
//!   the member's UTC offset) is set to the shifted UTC value, or dropped;
//! - known catalogue times of a class (its air time and scheduled start) are
//!   public and kept as they are, so no public time can be matched against
//!   its shifted copy: a class is an object carrying `original_air_time` (a
//!   ride, a library item, a related ride) or the value of a `ride` key, and
//!   inside it only `original_air_time` and `scheduled_start_time` are left
//!   unshifted; any other timestamp in a class is shifted like a member time;
//! - Peloton's base64 class ids (on-demand class ids and scheduled-class
//!   ids, which embed the member's own event id) are decoded, their ids
//!   mapped like any other id and their scheduled start shifted as below,
//!   then re-encoded in Peloton's exact layout, so they stay valid; any other
//!   opaque token, or a class id of an unexpected shape, is replaced;
//! - schedule times (scheduled starts and ends, in ids and as RFC 3339
//!   strings) are moved by one offset that puts the run's earliest schedule
//!   time at a fixed synthetic instant, so neither the date nor the member's
//!   time of day survives; an unparseable one becomes that instant;
//! - the guests and invitees of a scheduled class are emptied; per-member
//!   library fields (bookmarked, times taken, taken by friends) are reset;
//! - everything else, notably all measured metric series, is kept as is.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use peloton::ids::{OnDemandClassId, ScheduledClassId};
use rand::Rng as _;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::BorrowedFormatItem;
use time::format_description::well_known::Rfc3339;

use crate::capture::{prepare_dir, save};

/// Timestamps between these bounds (2010-01-01 and 2033-05-18) are shifted.
const EPOCH_RANGE: std::ops::RangeInclusive<i64> = 1_262_304_000..=2_000_000_000;
/// Bounds of the random shift: between five and one years back.
const SHIFT_RANGE: std::ops::RangeInclusive<i64> = -5 * 365 * 86_400..=-365 * 86_400;
const MIN_HEX_RUN: usize = 32;
const MIN_TOKEN_RUN: usize = 40;
const MIN_SENSITIVE_LEN: usize = 3;
/// Characters from which a sensitive string is erased inside longer words too.
const MIN_SUBSTRING_LEN: usize = 4;

const REDACTED: &str = "redacted";
const REDACTED_URL: &str = "https://example.invalid/redacted";

/// Keys whose presence marks an object as describing a person.
const PERSON_MARKERS: &[&str] = &["username", "first_name", "last_name"];
/// Free-text fields of a person object that are kept (ids are still mapped).
const PERSON_KEEP: &[&str] = &[
    "id",
    "user_id",
    "instructor_id",
    "coach_type",
    "fitness_discipline",
    "locale",
    "height_unit",
    "weight_unit",
    "cycling_ftp_source",
    "account_creation_method",
    "feed_privacy",
];
/// Person fields whose words are erased from every other string of the run.
const PERSON_WORDS: &[&str] = &["name", "first_name", "last_name", "location"];
/// Person fields erased from every other string of the run as a whole.
const PERSON_WHOLE: &[&str] = &["username", "email"];
/// String fields replaced wherever they appear.
const REDACT_KEYS: &[&str] = &[
    "location",
    "email",
    "obfuscated_email",
    "phone_number",
    "serial_number",
    "facebook_id",
    "facebook_access_token",
    "bio",
];
/// Containers emptied wherever they appear.
const EMPTY_KEYS: &[&str] = &[
    "paired_devices",
    "contract_agreements",
    "external_music_auth_list",
    "social_connections",
    "member_groups",
    "ordered_q_and_as",
    "scheduledUsers",
    "getInvitedUsers",
];
/// Per-member fields set to a fixed value wherever they appear, whatever
/// their value.
const RESET_KEYS: &[(&str, Fixed)] = &[
    ("is_favorite", Fixed::Bool(false)),
    ("total_user_workouts", Fixed::Int(0)),
    ("total_following_workouts", Fixed::Int(0)),
];
/// RFC 3339 schedule times, moved with the schedule offset.
const SCHEDULE_TIME_KEYS: &[&str] = &["scheduledStartTime", "scheduledEndTime"];
/// The scheduled start inside a scheduled-class id.
const ID_START_KEY: &str = "scheduled_start_time";
/// Where the earliest schedule time of a run lands: 2030-01-07T12:00:00Z.
const SYNTHETIC_SCHEDULE_START: i64 = 1_894_017_600;
/// Peloton's schedule time layout, e.g. `2030-01-07T12:00:00.000Z`.
const SCHEDULE_TIME_FORMAT: &[BorrowedFormatItem<'static>] = time::macros::format_description!(
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
);
const TIMEZONE: &str = "Etc/UTC";
/// Bounds of the random calorie factor's distance from 1.
const CALORIE_DEVIATION: std::ops::RangeInclusive<f64> = 0.05..=0.2;
/// Fixed maximum heart rate (220 - the fixed age of 35).
const FIXED_MAX_HEART_RATE: u32 = 185;
/// Fixed functional threshold power in watts, for every FTP field.
const FIXED_FTP: u32 = 200;
/// Peloton's heart-rate zone lower bounds in percent of the maximum.
const HEART_RATE_ZONE_PERCENTS: [u32; 5] = [0, 65, 75, 85, 95];
const DEVICE_TIME_PREFIX: &str = "device_time_";
/// A key only class objects carry.
const CLASS_MARKER: &str = "original_air_time";
/// A key whose value is always a class.
const CLASS_KEY: &str = "ride";
/// Public catalogue times left unshifted inside a class.
const CLASS_TIME_KEYS: &[&str] = &["original_air_time", "scheduled_start_time"];
const GRAPH_PREFIX: &str = "performance_graph-";
/// Scores computed from zone durations; nulled because they encode the old zones.
const ZONE_SCORES: &[&str] = &[
    "total_effort_points",
    "hr_total_points",
    "average_effort_score",
];

/// Seconds per heart-rate zone (zone 1 first).
type ZoneDurations = [u64; 5];

/// A fixed replacement for a personal number.
#[derive(Debug, Clone, Copy)]
enum Fixed {
    Int(u32),
    Float(f64),
    Bool(bool),
}

impl Fixed {
    /// The replacement for `value`, when `value` is of the same JSON type.
    fn replace(self, value: &Value) -> Option<Value> {
        match self {
            Self::Int(n) if value.is_number() => Some(json!(n)),
            Self::Float(f) if value.is_number() => Some(json!(f)),
            Self::Bool(b) if value.is_boolean() => Some(json!(b)),
            _ => None,
        }
    }
}

/// Personal numbers replaced by fixed values (0, meaning "not set", is kept).
const FIXED_NUMBERS: &[(&str, Fixed)] = &[
    ("birthday", Fixed::Int(631_152_000)),
    ("height", Fixed::Float(70.0)),
    ("weight", Fixed::Float(170.0)),
    ("age", Fixed::Int(35)),
    ("default_max_heart_rate", Fixed::Int(FIXED_MAX_HEART_RATE)),
    (
        "customized_max_heart_rate",
        Fixed::Int(FIXED_MAX_HEART_RATE),
    ),
    ("cycling_ftp", Fixed::Int(FIXED_FTP)),
    ("cycling_workout_ftp", Fixed::Int(FIXED_FTP)),
    ("estimated_cycling_ftp", Fixed::Int(FIXED_FTP)),
    ("ftp", Fixed::Int(FIXED_FTP)),
    ("leaderboard_rank", Fixed::Int(1_000)),
    ("total_leaderboard_users", Fixed::Int(10_000)),
    ("leaderboard_distance_rank", Fixed::Int(1_000)),
    ("total_leaderboard_distance_users", Fixed::Int(10_000)),
];
/// Heart-rate zone bound lists recomputed from the fixed maximum.
const HEART_RATE_ZONE_LISTS: &[&str] = &["default_heart_rate_zones", "customized_heart_rate_zones"];
/// Personal calorie fields scaled by the run's calorie factor.
const CALORIE_KEYS: &[&str] = &["calories", "active_calories", "total_calories"];
/// Fields of a calorie summary or series (slug containing "calories") that are scaled.
const CALORIE_VALUE_KEYS: &[&str] = &["value", "values", "average_value", "max_value"];
const TRACK_ORIGIN: (f64, f64) = (0.0, 0.0);
const TRACK_STEP: f64 = 0.000_1;

/// The secrets of one scrub run. They are never written anywhere, so the
/// output cannot be mapped back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrubParams {
    /// Seconds added to every epoch timestamp.
    pub time_shift_s: i64,
    /// Factor applied to every personal calorie figure.
    pub calorie_factor: f64,
}

impl ScrubParams {
    /// Draws fresh random parameters.
    #[must_use]
    pub fn random() -> Self {
        let mut rng = rand::rng();
        let deviation = rng.random_range(CALORIE_DEVIATION);
        Self {
            time_shift_s: rng.random_range(SHIFT_RANGE),
            calorie_factor: if rng.random_bool(0.5) {
                1.0 + deviation
            } else {
                1.0 - deviation
            },
        }
    }
}

/// Reads every `*.json` file of `in_dir`, scrubs them together and writes
/// the results (with scrubbed file names) to `out_dir`.
///
/// # Errors
///
/// Returns a message when a file cannot be read, parsed or written, or when
/// `out_dir` is `in_dir`.
pub fn run(in_dir: &Path, out_dir: &Path) -> Result<(), String> {
    let input = in_dir
        .canonicalize()
        .map_err(|e| format!("bad input directory {}: {e}", in_dir.display()))?;
    prepare_dir(out_dir)?;
    let output = out_dir
        .canonicalize()
        .map_err(|e| format!("bad output directory {}: {e}", out_dir.display()))?;
    if input == output {
        return Err("output directory must differ from the input directory".to_owned());
    }

    let mut names = Vec::new();
    for entry in
        fs::read_dir(&input).map_err(|e| format!("could not list {}: {e}", input.display()))?
    {
        let entry = entry.map_err(|e| format!("could not list {}: {e}", input.display()))?;
        let is_file = entry.file_type().is_ok_and(|t| t.is_file());
        let path = entry.path();
        let is_json = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
        if let Some(name) = entry.file_name().to_str()
            && is_file
            && is_json
        {
            names.push(name.to_owned());
        }
    }
    names.sort();

    let mut files = Vec::with_capacity(names.len());
    for name in names {
        let path = input.join(&name);
        let body = fs::read(&path).map_err(|e| format!("could not read {name}: {e}"))?;
        let value: Value =
            serde_json::from_slice(&body).map_err(|e| format!("could not parse {name}: {e}"))?;
        files.push((name, value));
    }

    for (name, value) in scrub_files(files, ScrubParams::random()) {
        let mut body = serde_json::to_vec_pretty(&value)
            .map_err(|e| format!("could not encode {name}: {e}"))?;
        body.push(b'\n');
        save(&output, &name, &body)?;
    }
    Ok(())
}

/// Scrubs `(file name, content)` pairs together with `params`. File names
/// are scrubbed like string values.
#[must_use]
pub fn scrub_files(files: Vec<(String, Value)>, params: ScrubParams) -> Vec<(String, Value)> {
    let mut scrubber = Scrubber::new(params);
    for (name, value) in &files {
        scrubber.collect(value);
        if let Some(id) = graph_workout_id(name)
            && let Some(durations) = recompute_zone_durations(value)
        {
            scrubber.zone_durations.insert(id, durations);
        }
    }
    scrubber.finish_collecting();
    files
        .into_iter()
        .map(|(name, value)| {
            scrubber.zone_context = graph_workout_id(&name)
                .and_then(|id| scrubber.zone_durations.get(&id).copied())
                .unwrap_or_default();
            (scrubber.ids_in(&name), scrubber.value(value))
        })
        .collect()
}

#[derive(Debug)]
struct Scrubber {
    params: ScrubParams,
    hex: HashMap<String, String>,
    uuids: HashMap<String, String>,
    placeholders: HashMap<(String, String), String>,
    counters: HashMap<String, u32>,
    /// Case-folded strings to erase, longest first.
    sensitive: Vec<String>,
    track_points: u32,
    /// Recomputed zone durations by original workout id.
    zone_durations: HashMap<String, ZoneDurations>,
    /// Zone durations for the workout being scrubbed (zeros when unknown).
    zone_context: ZoneDurations,
    /// Whether the object being scrubbed is inside the `heart_rate` metric;
    /// only its `zones` are heart-rate zones.
    in_heart_rate: bool,
    /// Whether the value being scrubbed is inside a class, whose
    /// [`CLASS_TIME_KEYS`] are public and not shifted.
    in_class: bool,
    /// Earliest schedule time (start or end) seen while collecting, epoch
    /// seconds.
    earliest_schedule: Option<i64>,
    /// Seconds added to every schedule time.
    schedule_shift_s: i64,
}

impl Scrubber {
    fn new(params: ScrubParams) -> Self {
        Self {
            params,
            hex: HashMap::new(),
            uuids: HashMap::new(),
            placeholders: HashMap::new(),
            counters: HashMap::new(),
            sensitive: Vec::new(),
            track_points: 0,
            zone_durations: HashMap::new(),
            zone_context: ZoneDurations::default(),
            in_heart_rate: false,
            in_class: false,
            earliest_schedule: None,
            schedule_shift_s: 0,
        }
    }

    fn collect(&mut self, value: &Value) {
        match value {
            Value::Object(map) => {
                if is_person(map) {
                    self.collect_person(map);
                }
                for key in SCHEDULE_TIME_KEYS {
                    if let Some(time) = map
                        .get(*key)
                        .and_then(Value::as_str)
                        .and_then(|text| OffsetDateTime::parse(text, &Rfc3339).ok())
                    {
                        self.note_schedule(time.unix_timestamp());
                    }
                }
                for child in map.values() {
                    self.collect(child);
                }
            }
            Value::Array(items) => {
                for child in items {
                    self.collect(child);
                }
            }
            Value::String(text) => {
                if let Ok(id) = text.parse::<ScheduledClassId>() {
                    self.note_schedule(id.scheduled_start_time().unix_timestamp());
                }
            }
            _ => {}
        }
    }

    fn note_schedule(&mut self, start: i64) {
        self.earliest_schedule = Some(self.earliest_schedule.map_or(start, |e| e.min(start)));
    }

    fn collect_person(&mut self, map: &Map<String, Value>) {
        for (key, child) in map {
            let Value::String(text) = child else { continue };
            if PERSON_WORDS.contains(&key.as_str()) {
                self.sensitive.push(fold_case(text));
                self.sensitive
                    .extend(text.split(|c: char| !c.is_alphanumeric()).map(fold_case));
            } else if PERSON_WHOLE.contains(&key.as_str()) {
                self.sensitive.push(fold_case(text));
            }
        }
    }

    fn finish_collecting(&mut self) {
        self.sensitive
            .retain(|s| s.chars().count() >= MIN_SENSITIVE_LEN);
        self.sensitive
            .sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        self.sensitive.dedup();
        self.schedule_shift_s = self
            .earliest_schedule
            .map_or(0, |earliest| SYNTHETIC_SCHEDULE_START - earliest);
    }

    fn value(&mut self, value: Value) -> Value {
        match value {
            Value::Object(map) => self.object(map),
            Value::Array(items) => Value::Array(items.into_iter().map(|v| self.value(v)).collect()),
            Value::String(text) => Value::String(self.string(&text)),
            Value::Number(number) => match number.as_i64() {
                Some(n) if EPOCH_RANGE.contains(&n) => json!(n + self.params.time_shift_s),
                _ => Value::Number(number),
            },
            other => other,
        }
    }

    fn object(&mut self, map: Map<String, Value>) -> Value {
        if is_month_buckets(&map) {
            return Value::Object(Map::new());
        }
        if let Some(point) = self.gps_point(&map) {
            return point;
        }
        let outer_context = self.zone_context;
        let in_heart_rate = self.in_heart_rate;
        if let Some(slug) = map.get("slug").and_then(Value::as_str)
            && map.contains_key("zones")
        {
            self.in_heart_rate = slug == "heart_rate";
        }
        if let Some(durations) = map
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| self.zone_durations.get(&id.to_ascii_lowercase()))
        {
            self.zone_context = *durations;
        }
        let in_class = self.in_class;
        self.in_class |= map.contains_key(CLASS_MARKER);
        let person = is_person(&map);
        let mut out = Map::new();
        for (key, child) in map {
            let outer = self.in_class;
            self.in_class |= key == CLASS_KEY;
            let scrubbed = self.key_rule(&key, child, person);
            self.in_class = outer;
            out.insert(self.string(&key), scrubbed);
        }
        let slug = out.get("slug").and_then(Value::as_str).map(str::to_owned);
        if let Some(slug) = slug {
            if let Some(zone) = heart_rate_zone(&slug)
                && in_heart_rate
                && out.contains_key("range")
            {
                set_zone_bounds(&mut out, zone);
                if out.get("duration").is_some_and(Value::is_number) {
                    out.insert("duration".to_owned(), json!(self.zone_context[zone - 1]));
                }
            }
            if slug.contains("calories") {
                for key in CALORIE_VALUE_KEYS {
                    if let Some(found) = out.get_mut(*key) {
                        *found = scale(found, self.params.calorie_factor);
                    }
                }
            }
        }
        align_device_times(&mut out);
        self.zone_context = outer_context;
        self.in_heart_rate = in_heart_rate;
        self.in_class = in_class;
        Value::Object(out)
    }

    fn key_rule(&mut self, key: &str, child: Value, person: bool) -> Value {
        if EMPTY_KEYS.contains(&key) {
            return match child {
                Value::Array(_) => Value::Array(Vec::new()),
                Value::Object(_) => Value::Object(Map::new()),
                other => other,
            };
        }
        if let Some(&(_, fixed)) = RESET_KEYS.iter().find(|(k, _)| *k == key)
            && let Some(replaced) = fixed.replace(&child)
        {
            return replaced;
        }
        if SCHEDULE_TIME_KEYS.contains(&key)
            && let Value::String(text) = &child
        {
            return Value::String(self.schedule_time(text));
        }
        if let Some(&(_, fixed)) = FIXED_NUMBERS.iter().find(|(k, _)| *k == key)
            && child.is_number()
            && child.as_f64() != Some(0.0)
            && let Some(replaced) = fixed.replace(&child)
        {
            return replaced;
        }
        if HEART_RATE_ZONE_LISTS.contains(&key)
            && let Value::Array(items) = &child
            && !items.is_empty()
        {
            return fixed_zone_list();
        }
        if let Some(zone) = zone_duration_key(key)
            && child.is_number()
        {
            return json!(self.zone_context[zone - 1]);
        }
        if ZONE_SCORES.contains(&key) && child.is_number() {
            return Value::Null;
        }
        if self.in_class && CLASS_TIME_KEYS.contains(&key) && child.is_number() {
            return child;
        }
        if CALORIE_KEYS.contains(&key) && child.is_number() {
            return scale(&child, self.params.calorie_factor);
        }
        match child {
            Value::String(text) if key == "timezone" && !text.is_empty() => {
                Value::String(TIMEZONE.to_owned())
            }
            Value::String(text) if REDACT_KEYS.contains(&key) && !text.is_empty() => {
                Value::String(REDACTED.to_owned())
            }
            Value::String(text)
                if person && !PERSON_KEEP.contains(&key) && !text.is_empty() && !is_id(&text) =>
            {
                Value::String(self.placeholder(key, text))
            }
            other => self.value(other),
        }
    }

    fn placeholder(&mut self, key: &str, text: String) -> String {
        match self.placeholders.entry((key.to_owned(), text)) {
            Entry::Occupied(found) => found.get().clone(),
            Entry::Vacant(slot) => {
                let counter = self.counters.entry(key.to_owned()).or_default();
                *counter += 1;
                slot.insert(format!("{key}-{counter}")).clone()
            }
        }
    }

    /// Replaces a GPS point by the next point of the synthetic track. Every
    /// other field (altitude, accuracy, speed, time, …) is dropped.
    fn gps_point(&mut self, map: &Map<String, Value>) -> Option<Value> {
        let lat_key = ["latitude", "lat"]
            .into_iter()
            .find(|k| map.contains_key(*k))?;
        let lon_key = ["longitude", "lon", "lng"]
            .into_iter()
            .find(|k| map.contains_key(*k))?;
        let step = f64::from(self.track_points) * TRACK_STEP;
        self.track_points += 1;
        let mut point = Map::new();
        point.insert(lat_key.to_owned(), json!(TRACK_ORIGIN.0 + step));
        point.insert(lon_key.to_owned(), json!(TRACK_ORIGIN.1 + step));
        Some(Value::Object(point))
    }

    fn string(&mut self, text: &str) -> String {
        if text.contains("://") {
            return REDACTED_URL.to_owned();
        }
        if let Some(id) = self.class_id(text) {
            return id;
        }
        if has_opaque_token(text) {
            return REDACTED.to_owned();
        }
        let mapped = self.ids_in(text);
        erase_words(&mapped, &self.sensitive)
    }

    /// Re-encodes a Peloton class id with its ids mapped and its scheduled
    /// start moved; `None` when `text` is not a class id of a known shape.
    fn class_id(&mut self, text: &str) -> Option<String> {
        let scheduled = text.parse::<ScheduledClassId>().is_ok();
        if !scheduled && text.parse::<OnDemandClassId>().is_err() {
            return None;
        }
        let payload: Map<String, Value> =
            serde_json::from_slice(&BASE64.decode(text).ok()?).ok()?;
        let mut fields: Vec<(String, Value)> = payload
            .into_iter()
            .map(|(key, value)| {
                let value = match value {
                    Value::String(id) => Value::String(self.ids_in(&id)),
                    Value::Number(start) if key == ID_START_KEY => {
                        json!(start.as_i64()?.checked_add(self.schedule_shift_s)?)
                    }
                    other => other,
                };
                Some((key, value))
            })
            .collect::<Option<_>>()?;
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        let encoded = BASE64.encode(python_json(&fields));
        let valid = if scheduled {
            encoded.parse::<ScheduledClassId>().is_ok()
        } else {
            encoded.parse::<OnDemandClassId>().is_ok()
        };
        valid.then_some(encoded)
    }

    /// `text` moved by the schedule offset, or the synthetic start when it
    /// does not parse.
    fn schedule_time(&self, text: &str) -> String {
        let synthetic = OffsetDateTime::from_unix_timestamp(SYNTHETIC_SCHEDULE_START);
        OffsetDateTime::parse(text, &Rfc3339)
            .ok()
            .and_then(|t| t.checked_add(time::Duration::seconds(self.schedule_shift_s)))
            .or_else(|| synthetic.ok())
            .and_then(|t| {
                t.to_offset(time::UtcOffset::UTC)
                    .format(SCHEDULE_TIME_FORMAT)
                    .ok()
            })
            .unwrap_or_else(|| REDACTED.to_owned())
    }

    /// Replaces UUIDs and long hex runs in `text` with consistent fakes.
    fn ids_in(&mut self, text: &str) -> String {
        let with_uuids = self.map_uuids(text);
        self.map_hex_runs(&with_uuids)
    }

    fn map_uuids(&mut self, text: &str) -> String {
        const UUID_LEN: usize = 36;
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while !rest.is_empty() {
            let found = rest
                .char_indices()
                .map(|(i, _)| i)
                .find(|&i| rest.get(i..i + UUID_LEN).is_some_and(is_uuid));
            let Some(start) = found else {
                out.push_str(rest);
                break;
            };
            out.push_str(&rest[..start]);
            let original = rest[start..start + UUID_LEN].to_ascii_lowercase();
            let next = self.uuids.len() + 1;
            let fake = self
                .uuids
                .entry(original)
                .or_insert_with(|| format!("00000000-0000-4000-8000-{next:012x}"));
            out.push_str(fake);
            rest = &rest[start + UUID_LEN..];
        }
        out
    }

    fn map_hex_runs(&mut self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut run_start = None;
        for (i, c) in text
            .char_indices()
            .chain(std::iter::once((text.len(), ' ')))
        {
            if c.is_ascii_hexdigit() && i < text.len() {
                run_start.get_or_insert(i);
                continue;
            }
            if let Some(start) = run_start.take() {
                let run = &text[start..i];
                if run.len() >= MIN_HEX_RUN && run.bytes().any(|b| b != b'0') {
                    out.push_str(&self.fake_hex(run));
                } else {
                    out.push_str(run);
                }
            }
            if i < text.len() {
                out.push(c);
            }
        }
        out
    }

    fn fake_hex(&mut self, run: &str) -> String {
        let next = self.hex.len() + 1;
        let width = run.len();
        self.hex
            .entry(run.to_ascii_lowercase())
            .or_insert_with(|| format!("{:0>width$}", format!("f{next:x}")))
            .clone()
    }
}

/// `fields` as Python's `json.dumps` writes an object, the layout of
/// Peloton's class ids: `{"key": value, "key": value}`.
fn python_json(fields: &[(String, Value)]) -> String {
    let entries: Vec<String> = fields
        .iter()
        .map(|(key, value)| format!("{}: {value}", Value::String(key.clone())))
        .collect();
    format!("{{{}}}", entries.join(", "))
}

/// Multiplies every number in `value` (or in the list `value`) by `factor`,
/// rounded to two decimals.
fn scale(value: &Value, factor: f64) -> Value {
    match value {
        Value::Number(n) => n.as_f64().map_or_else(
            || value.clone(),
            |f| json!((f * factor * 100.0).round() / 100.0),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|v| scale(v, factor)).collect()),
        other => other.clone(),
    }
}

/// Lower bound in bpm of heart-rate zone `index` (0-based) for the fixed maximum.
fn zone_floor(index: usize) -> u32 {
    HEART_RATE_ZONE_PERCENTS[index] * FIXED_MAX_HEART_RATE / 100
}

/// `default_heart_rate_zones`-style list for the fixed maximum.
fn fixed_zone_list() -> Value {
    Value::Array(
        HEART_RATE_ZONE_PERCENTS
            .iter()
            .map(|p| json!(f64::from(p * FIXED_MAX_HEART_RATE) / 100.0))
            .collect(),
    )
}

/// The original workout id of a `performance_graph-<id>.json` file name.
fn graph_workout_id(name: &str) -> Option<String> {
    let id = name.strip_prefix(GRAPH_PREFIX)?.strip_suffix(".json")?;
    Some(id.to_ascii_lowercase())
}

/// The 1-based zone of a `heart_rate_z<n>_duration` key.
fn zone_duration_key(key: &str) -> Option<usize> {
    let n = key
        .strip_prefix("heart_rate_z")?
        .strip_suffix("_duration")?;
    heart_rate_zone(&format!("zone{n}"))
}

/// Seconds per fixed zone from a performance graph's heart-rate series: each
/// sample with a reading counts until the next sample (the last one counts 1 s).
/// `None` when the graph has no heart-rate series.
fn recompute_zone_durations(graph: &Value) -> Option<ZoneDurations> {
    let offsets: Vec<u64> = graph
        .get("seconds_since_pedaling_start")?
        .as_array()?
        .iter()
        .filter_map(Value::as_u64)
        .collect();
    let series = graph
        .get("metrics")?
        .as_array()?
        .iter()
        .find(|m| m.get("slug").and_then(Value::as_str) == Some("heart_rate"))?;
    let mut durations = ZoneDurations::default();
    for (i, reading) in series.get("values")?.as_array()?.iter().enumerate() {
        let Some(bpm) = reading.as_f64().filter(|b| *b > 0.0) else {
            continue;
        };
        let seconds = match (offsets.get(i), offsets.get(i + 1)) {
            (Some(at), Some(next)) if next > at => next - at,
            _ => 1,
        };
        let zone = (0..durations.len())
            .rev()
            .find(|&z| bpm >= f64::from(zone_floor(z)))
            .unwrap_or(0);
        durations[zone] += seconds;
    }
    Some(durations)
}

/// The 1-based zone number of a heart-rate zone slug ("zone1" … "zone5").
fn heart_rate_zone(slug: &str) -> Option<usize> {
    let n: usize = slug.strip_prefix("zone")?.parse().ok()?;
    (1..=HEART_RATE_ZONE_PERCENTS.len())
        .contains(&n)
        .then_some(n)
}

/// Rewrites a heart-rate zone's `min_value`, `max_value` and `range` for the fixed maximum.
fn set_zone_bounds(zone: &mut Map<String, Value>, number: usize) {
    let last = HEART_RATE_ZONE_PERCENTS.len();
    let min = zone_floor(number - 1);
    let (max, range) = if number == last {
        (FIXED_MAX_HEART_RATE, format!(">={min} bpm"))
    } else {
        let next = zone_floor(number);
        if number == 1 {
            (next - 1, format!("<{next} bpm"))
        } else {
            (next - 1, format!("{min}-{} bpm", next - 1))
        }
    };
    zone.insert("min_value".to_owned(), json!(min));
    zone.insert("max_value".to_owned(), json!(max));
    zone.insert("range".to_owned(), json!(range));
}

/// Sets each `device_time_<key>` to the (already shifted) `<key>` of the same
/// object, or drops it when there is none: the difference between the two is
/// the member's UTC offset.
fn align_device_times(map: &mut Map<String, Value>) {
    let device_keys: Vec<String> = map
        .keys()
        .filter(|k| k.starts_with(DEVICE_TIME_PREFIX))
        .cloned()
        .collect();
    for device_key in device_keys {
        let utc = map
            .get(&device_key[DEVICE_TIME_PREFIX.len()..])
            .filter(|v| v.is_number())
            .cloned();
        match utc {
            Some(utc) => {
                map.insert(device_key, utc);
            }
            None => {
                map.remove(&device_key);
            }
        }
    }
}

fn is_person(map: &Map<String, Value>) -> bool {
    PERSON_MARKERS.iter().any(|k| map.contains_key(*k))
}

fn is_month_buckets(map: &Map<String, Value>) -> bool {
    !map.is_empty()
        && map.keys().all(|k| {
            let b = k.as_bytes();
            b.len() == 7 && b[4] == b'-' && b[..4].iter().chain(&b[5..]).all(u8::is_ascii_digit)
        })
}

/// True for a bare id: a long hex run or a UUID.
fn is_id(s: &str) -> bool {
    is_uuid(s) || (s.len() >= MIN_HEX_RUN && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.split('-').map(str::len).eq([8, 4, 4, 4, 12])
        && s.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit())
}

/// True when `text` holds a long base64-like run that is not plain hex
/// (encoded tokens may embed ids).
fn has_opaque_token(text: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')))
        .any(|run| run.len() >= MIN_TOKEN_RUN && !run.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Folds `text` for case-insensitive comparison: each character is
/// lower-cased, upper-cased and lower-cased again, so "ß", "ẞ" and "SS", "ς"
/// and "σ", "É" and "é" all agree.
///
/// This matches Unicode full case folding (`CaseFolding.txt`, statuses C and
/// F) for the scripts that have case. It does not normalise, so a name
/// written with combining accents (NFD) is not equal to its precomposed
/// (NFC) form.
fn fold_case(text: &str) -> String {
    text.chars().flat_map(fold_char).collect()
}

fn fold_char(c: char) -> impl Iterator<Item = char> {
    c.to_lowercase()
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
}

/// Replaces occurrences of each folded needle, ignoring case.
///
/// A needle of at least [`MIN_SUBSTRING_LEN`] characters matches anywhere, so
/// compound forms (`MarieClaire`, `Jean-Luc`) are erased; a shorter one only
/// matches as a whole word, so "ben" does not erase half of "benefits".
fn erase_words(text: &str, needles: &[String]) -> String {
    let mut current = text.to_owned();
    for needle in needles {
        let whole_word = needle.chars().count() < MIN_SUBSTRING_LEN;
        let (folded, origins) = fold_with_origins(&current);
        let mut out = String::with_capacity(current.len());
        let mut last = 0;
        for (start, matched) in folded.match_indices(needle.as_str()) {
            let (from, _) = origins[start];
            let (_, to) = origins[start + matched.len() - 1];
            if from < last {
                continue;
            }
            let bounded = !whole_word || {
                let before = current[..from].chars().next_back();
                let after = current[to..].chars().next();
                !before.is_some_and(char::is_alphanumeric)
                    && !after.is_some_and(char::is_alphanumeric)
            };
            if bounded {
                out.push_str(&current[last..from]);
                out.push_str(REDACTED);
                last = to;
            }
        }
        out.push_str(&current[last..]);
        current = out;
    }
    current
}

/// Folds `text` like [`fold_case`] and records, for every byte of the result,
/// the byte range of the character of `text` it came from.
fn fold_with_origins(text: &str) -> (String, Vec<(usize, usize)>) {
    let mut folded = String::with_capacity(text.len());
    let mut origins = Vec::with_capacity(text.len());
    for (start, c) in text.char_indices() {
        let range = (start, start + c.len_utf8());
        for f in fold_char(c) {
            folded.push(f);
            origins.extend(std::iter::repeat_n(range, f.len_utf8()));
        }
    }
    (folded, origins)
}

#[cfg(test)]
mod tests {
    use super::{erase_words, fold_case, has_opaque_token, is_month_buckets, is_uuid};
    use proptest::prelude::*;
    use serde_json::json;

    #[test]
    fn erases_whole_words_only() {
        let needles = vec!["ben".to_owned()];
        assert_eq!(
            erase_words("Ride with Ben: benefits", &needles),
            "Ride with redacted: benefits"
        );
    }

    #[test]
    fn erases_non_ascii_names_in_any_case() {
        let needles = vec![fold_case("élodie")];
        assert_eq!(
            erase_words("ÉLODIE, Élodie and élodie", &needles),
            "redacted, redacted and redacted"
        );
    }

    #[test]
    fn erases_names_across_special_case_folding() {
        let needles = vec![fold_case("Groß"), fold_case("ΟΔΥΣΣΕΥΣ")];
        assert_eq!(
            erase_words("GROSS, groß, Gross and Οδυσσευς, ΟΔΥΣΣΕΥΣ", &needles),
            "redacted, redacted, redacted and redacted, redacted"
        );
    }

    #[test]
    fn erases_names_inside_compound_forms() {
        let needles = vec![fold_case("marie"), fold_case("jean-luc")];
        assert_eq!(
            erase_words("MarieClaire, Jean-Luc and the Marie-Louise ride", &needles),
            "redactedClaire, redacted and the redacted-Louise ride"
        );
    }

    #[test]
    fn short_names_are_erased_as_whole_words_only() {
        let needles = vec![fold_case("Léa")];
        assert_eq!(
            erase_words("Ride with LÉA: Léandre", &needles),
            "Ride with redacted: Léandre"
        );
    }

    proptest! {
        #[test]
        fn scrubbed_text_never_contains_the_name_in_any_case(
            name in "[a-zA-Zàâçéèêëîïôöûüñßẞœ]{4,12}",
            before in "[ .,:;!-]{0,3}",
            after in "[ .,:;!-]{0,3}",
        ) {
            let folded = fold_case(&name);
            prop_assume!(!"redacted".contains(&folded));
            let needles = vec![folded.clone()];
            for variant in [name.to_lowercase(), name.to_uppercase(), name.clone()] {
                let text = format!("{before}{variant}{after}x{variant}");
                let scrubbed = erase_words(&text, &needles);
                prop_assert!(
                    !fold_case(&scrubbed).contains(&folded),
                    "{scrubbed:?} still contains {name:?}"
                );
            }
        }
    }

    #[test]
    fn detects_opaque_tokens_but_not_ids() {
        assert!(has_opaque_token(
            "eyJob21lX3BlbG90b25faWQiOiJjY2NjY2NjY2NjY2Nj"
        ));
        assert!(!has_opaque_token(
            "0123456789abcdef0123456789abcdef0123456789abcdef"
        ));
        assert!(!has_opaque_token("stretching_yoga_flexibility"));
    }

    #[test]
    fn recognises_uuids() {
        assert!(is_uuid("3d96f8b6-1c2d-4e5f-8a9b-0c1d2e3f4a5b"));
        assert!(!is_uuid("3d96f8b6-1c2d-4e5f-8a9b-0c1d2e3f4a5"));
        assert!(!is_uuid("3d96f8b6x1c2d-4e5f-8a9b-0c1d2e3f4a5b"));
    }

    #[test]
    fn recognises_month_buckets() {
        let buckets = json!({"2021-01": 3, "2022-12": 1});
        let other = json!({"2021-01": 3, "total": 1});
        assert!(is_month_buckets(buckets.as_object().expect("object")));
        assert!(!is_month_buckets(other.as_object().expect("object")));
        assert!(!is_month_buckets(json!({}).as_object().expect("object")));
    }
}
