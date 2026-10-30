//! `scrub`: turn raw Peloton captures into fixtures that are safe to publish.
//!
//! Rules, applied to every file of a run with one shared state so that the
//! same input always maps to the same output across files:
//!
//! - every run of 32 or more hex characters (ids, hashes) and every UUID is
//!   replaced by a fake of the same shape; the all-zero id is kept;
//! - objects describing a person (they carry `username`, `first_name` or
//!   `last_name`: the member and instructors) have every free-text field
//!   that is not a bare id replaced by a `<key>-<n>` placeholder, and their
//!   names (and each word of them; accents and vowel signs belong to their
//!   word), usernames, e-mails and locations are additionally erased from
//!   every other string of the run:
//!   - matching ignores case, accents and normalization form: both sides are
//!     decomposed with compatibility (NFKD, so full-width, circled and
//!     ligature letters count as their plain letters), stripped of combining
//!     marks and case-folded, and the letters NFKD does not split are spelled
//!     out ("ł", "ø", "đ", "ð", "ħ", "ŧ" as "l", "o", "d", "d", "h", "t";
//!     "þ", "œ", "æ" as "th", "oe", "ae"), so "Élodie" also erases "ELODIE"
//!     and "ｅｌｏｄｉｅ", and "Łukasz" erases "Lukasz";
//!   - a string of fewer than 3 characters (counted as written, composed,
//!     so a spacing vowel sign counts, and without invisible format
//!     characters) is never erased, unless it is written
//!     in a script without spaces between words (see below); one that folds
//!     to 3 characters or fewer only as a whole word or glued to digits
//!     ("Ben123", "`ben_2024`"; "ben" leaves "benefits" and "Bento" alone,
//!     and hex ids alone); one that folds to 4 or more also
//!     inside a longer word when both its ends are boundaries: the edge of
//!     the text or a character that is neither a letter nor a digit
//!     ("Jean-Luc", "`coach_elodie`"); a digit on either side ("Elodie123",
//!     "rides4elodie"), except inside a word that looks like a hex id (only
//!     hex digits, at least one decimal digit, 8 or more long), which is
//!     never erased into; a lower to upper case change ("`MarieClaire`",
//!     "`elodieMartin`"), and after the match also an upper to lower one
//!     ("`ELODIEmartin`"); a change between a cased and an uncased letter
//!     ("Elodieさん", "日本Elodie"). Where the stretch between boundaries has
//!     no lowercase letter, case marks no boundary, so one boundary at
//!     either end is enough ("JEANLUC", "MARIECLAIRE", and "KINGDOM" for a
//!     name "King"). Ordinary words are kept ("walking", "glove", "Kingdom",
//!     "XKINGX" for "King" or "Love"); "iPhone" for a name "Phone" is erased;
//!   - a string made only of letters of a script written without spaces
//!     between words (Han, kana, Bopomofo, Yi, Tibetan, the scripts of
//!     mainland and island Southeast Asia, and Hangul, whose particles and
//!     honorifics are glued to the name) is erased from 2 characters on, and
//!     anywhere, even inside a run of the same script ("ゆらのさん"), as long
//!     as it matches whole characters ("成功" leaves "㍻功" alone), except
//!     that a Hangul name may carry one more final consonant ("유날" for
//!     "유나를"); hiragana and katakana match each other ("ユラ" for "ゆら");
//!     a single character is never erased. Such text has no word boundary to
//!     look for, and publishing a name costs more than erasing an ordinary
//!     word that holds it, so a name "ゆら" also erases "ゆらゆら"
//!     ("swaying"). A name that folds to one letter (a Thai or Burmese
//!     syllable whose vowel is a mark: "ปู" folds to "ป") is matched with its
//!     marks instead, anywhere, on whole syllables ("ปูดี" is erased, "ป",
//!     "ปา" and "ปู่" are not): that one letter alone would erase most text of
//!     its script, and these scripts glue words together, so a name kept to
//!     whole words would leak. The unspaced part of a mixed word is noted on
//!     its own ("梓潮" of "梓潮Rivo"), and the words of a name, or a first
//!     and a last name, in such a script are also noted joined, in either
//!     order, as each part is often one character ("柏 沅" as "柏沅");
//!   - an object key is erased only when it is the whole string, as keys are
//!     schema: `max_value` and `power_zone` stay for names "Max" and "Zone";
//!     keys of one object that would end up equal ("Max" and "MAX") get
//!     distinct suffixes ("redacted", "redacted-2"), so no value is lost;
//!   - a string with percent-encoded UTF-8 or JSON escapes (`\uXXXX` with
//!     surrogate pairs, `\\`, `\n`, …) is also matched decoded (up to three
//!     times over, for "%25C3%2589lodie", "%5Cu3086" or "\\u3086"), keys
//!     included, and a match erases the whole encoded span ("%C3%89lodie",
//!     "elodie%40example.com", "\u3086\u3089");
//!   - invisible format characters (soft hyphens, zero-width joiners and
//!     spaces, bidi controls) are skipped when matching, but still mark the
//!     word boundary they did ("Elo\u{ad}die", "elodie\u{200b}martin");
//!   - text outside the erased spans is left byte for byte as it was;
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

use std::cell::OnceCell;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use peloton::ids::{OnDemandClassId, ScheduledClassId};
use rand::RngExt as _;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::BorrowedFormatItem;
use time::format_description::well_known::Rfc3339;
use unicode_normalization::UnicodeNormalization as _;
use unicode_normalization::char::is_combining_mark;
use unicode_script::{Script, UnicodeScript as _};

use crate::capture::{StoreError, prepare_dir, save};

/// Timestamps between these bounds (2010-01-01 and 2033-05-18) are shifted.
const EPOCH_RANGE: std::ops::RangeInclusive<i64> = 1_262_304_000..=2_000_000_000;
/// Bounds of the random shift: between five and one years back.
const SHIFT_RANGE: std::ops::RangeInclusive<i64> = -5 * 365 * 86_400..=-365 * 86_400;
const MIN_HEX_RUN: usize = 32;
const MIN_TOKEN_RUN: usize = 40;
const MIN_SENSITIVE_LEN: usize = 3;
/// Characters from which a string in [`UNSPACED_SCRIPTS`] is erased.
const MIN_UNSPACED_LEN: usize = 2;
/// Scripts written without spaces between words, or (Hangul) with particles
/// and honorifics glued to the word: a name in them is erased anywhere.
const UNSPACED_SCRIPTS: &[Script] = &[
    Script::Han,
    Script::Hiragana,
    Script::Katakana,
    Script::Hangul,
    Script::Thai,
    Script::Lao,
    Script::Khmer,
    Script::Myanmar,
    Script::Tibetan,
    Script::Yi,
    Script::Javanese,
    Script::Balinese,
    Script::Sundanese,
    Script::Buginese,
    Script::Tai_Tham,
    Script::Tai_Le,
    Script::New_Tai_Lue,
    Script::Tai_Viet,
    Script::Bopomofo,
];
/// Hangul vowel letters (jungseong), which NFKD splits out of a syllable.
const HANGUL_VOWELS: std::ops::RangeInclusive<char> = '\u{1161}'..='\u{1175}';
/// Hangul final consonant letters (jongseong).
const HANGUL_FINALS: std::ops::RangeInclusive<char> = '\u{11a8}'..='\u{11c2}';
/// Katakana letters that have a hiragana twin [`KANA_OFFSET`] below them.
const KATAKANA: std::ops::RangeInclusive<char> = '\u{30a1}'..='\u{30f6}';
/// Katakana iteration marks ("ヽ", "ヾ"), twins of "ゝ" and "ゞ".
const KATAKANA_ITERATION: std::ops::RangeInclusive<char> = '\u{30fd}'..='\u{30fe}';
const KANA_OFFSET: u32 = 0x60;
/// Length of a JSON `\uXXXX` escape.
const ESCAPE_LEN: usize = 6;
/// Characters from which a sensitive string is erased inside longer words too.
const MIN_SUBSTRING_LEN: usize = 4;
/// How many times escapes (percent-encoding, `\uXXXX`) are decoded in a row
/// at most.
const MAX_DECODINGS: usize = 3;
/// Length from which a word of hex digits (with a decimal digit) is an id.
const MIN_HEX_WORD: usize = 8;

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

/// Why `scrub` failed. Never carries file contents.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScrubError {
    /// The input directory does not exist or cannot be resolved.
    #[error("bad input directory {}", .path.display())]
    InputDir {
        /// The directory as given.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// The output directory cannot be resolved.
    #[error("bad output directory {}", .path.display())]
    OutputDir {
        /// The directory as given.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// Input and output are the same directory, so the raw files would be
    /// overwritten.
    #[error("output directory must differ from the input directory")]
    SameDirectory,
    /// The output directory could not be prepared or a result not written.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The input directory could not be listed.
    #[error("could not list {}", .path.display())]
    List {
        /// The input directory.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// An input file could not be read.
    #[error("could not read {name}")]
    Read {
        /// The file name.
        name: String,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// An input file is not valid JSON.
    #[error("could not parse {name}")]
    Parse {
        /// The file name.
        name: String,
        /// The underlying failure.
        #[source]
        source: serde_json::Error,
    },
    /// A scrubbed file could not be encoded.
    #[error("could not encode {name}")]
    Encode {
        /// The file name.
        name: String,
        /// The underlying failure.
        #[source]
        source: serde_json::Error,
    },
}

/// Reads every `*.json` file of `in_dir`, scrubs them together and writes
/// the results (with scrubbed file names) to `out_dir`.
///
/// # Errors
///
/// Returns a [`ScrubError`] when a file cannot be read, parsed or written, or
/// when `out_dir` is `in_dir`.
pub fn run(in_dir: &Path, out_dir: &Path) -> Result<(), ScrubError> {
    let input = in_dir
        .canonicalize()
        .map_err(|source| ScrubError::InputDir {
            path: in_dir.to_path_buf(),
            source,
        })?;
    prepare_dir(out_dir)?;
    let output = out_dir
        .canonicalize()
        .map_err(|source| ScrubError::OutputDir {
            path: out_dir.to_path_buf(),
            source,
        })?;
    if input == output {
        return Err(ScrubError::SameDirectory);
    }

    let list_error = |source| ScrubError::List {
        path: input.clone(),
        source,
    };
    let mut names = Vec::new();
    for entry in fs::read_dir(&input).map_err(list_error)? {
        let entry = entry.map_err(list_error)?;
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
        let body = fs::read(&path).map_err(|source| ScrubError::Read {
            name: name.clone(),
            source,
        })?;
        let value: Value = serde_json::from_slice(&body).map_err(|source| ScrubError::Parse {
            name: name.clone(),
            source,
        })?;
        files.push((name, value));
    }

    for (name, value) in scrub_files(files, ScrubParams::random()) {
        let mut body = serde_json::to_vec_pretty(&value).map_err(|source| ScrubError::Encode {
            name: name.clone(),
            source,
        })?;
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
                self.note_sensitive(text);
                let words: Vec<&str> = text
                    .split(|c: char| !is_word_char(c))
                    .filter(|word| !word.is_empty())
                    .collect();
                for word in &words {
                    self.note_sensitive(word);
                    for run in script_runs(word) {
                        self.note_sensitive(run);
                    }
                }
                if key == "name" {
                    self.note_joined(&words);
                }
            } else if PERSON_WHOLE.contains(&key.as_str()) {
                self.note_sensitive(text);
            }
        }
        if let (Some(Value::String(first)), Some(Value::String(last))) =
            (map.get("first_name"), map.get("last_name"))
        {
            self.note_joined(&[last.trim(), first.trim()]);
        }
    }

    /// Notes the parts of a name joined, in their order and reversed, when
    /// they are in an unspaced script: such a name is written without a space
    /// between its parts, which often have one character each (a family name
    /// and a given name, "柏" and "沅" written "柏沅").
    fn note_joined(&mut self, parts: &[&str]) {
        if parts.len() < 2 {
            return;
        }
        let forward = parts.concat();
        let backward: String = parts.iter().rev().copied().collect();
        for joined in [forward, backward] {
            if is_unspaced(&fold_case(&joined)) {
                self.note_sensitive(&joined);
            }
        }
    }

    /// Adds `text` to the strings to erase, unless it is too short (see
    /// [`sensitive_form`]).
    fn note_sensitive(&mut self, text: &str) {
        if let Some(folded) = sensitive_form(text) {
            self.sensitive.push(folded);
        }
    }

    fn finish_collecting(&mut self) {
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
        let mut renamed = Vec::new();
        for (key, child) in map {
            let outer = self.in_class;
            self.in_class |= key == CLASS_KEY;
            let scrubbed = self.key_rule(&key, child, person);
            self.in_class = outer;
            let new_key = self.key(&key);
            if new_key == key {
                out.insert(key, scrubbed);
            } else {
                renamed.push((new_key, scrubbed));
            }
        }
        // Renamed keys go in last, each under a name still free, so that two
        // keys scrubbed to the same name ("Max" and "MAX") keep both values.
        for (key, scrubbed) in renamed {
            out.insert(free_key(&out, key), scrubbed);
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
        match self.opaque(text) {
            Some(replaced) => replaced,
            None => erase_words(&self.ids_in(text), &self.sensitive),
        }
    }

    /// An object key, scrubbed like a string except that a sensitive string
    /// is erased only when it is the whole key: other keys are schema
    /// (`max_value`, `power_zone`), not personal data.
    fn key(&mut self, key: &str) -> String {
        if let Some(replaced) = self.opaque(key) {
            return replaced;
        }
        let mapped = self.ids_in(key);
        let is_name = |text: &str| {
            self.sensitive.contains(&fold_case(text))
                || self.sensitive.contains(&fold_keeping_marks(text).collect())
        };
        if is_name(&mapped) || decodings(&mapped).iter().any(|layer| is_name(&layer.text)) {
            REDACTED.to_owned()
        } else {
            mapped
        }
    }

    /// The replacement of a URL, a class id or an opaque token; `None` for
    /// any other text.
    fn opaque(&mut self, text: &str) -> Option<String> {
        if text.contains("://") {
            return Some(REDACTED_URL.to_owned());
        }
        if let Some(id) = self.class_id(text) {
            return Some(id);
        }
        has_opaque_token(text).then(|| REDACTED.to_owned())
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

/// `key`, or `key` with the first suffix `-2`, `-3`, … that `map` does not
/// use yet.
fn free_key(map: &Map<String, Value>, key: String) -> String {
    if !map.contains_key(&key) {
        return key;
    }
    let mut n = 2_u32;
    loop {
        let candidate = format!("{key}-{n}");
        if !map.contains_key(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Whether `c` belongs to a word of a name: a letter, a digit, or a
/// combining mark (an accent of a decomposed letter, a vowel sign).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || is_combining_mark(c)
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

/// The folded form of `text` (see [`fold_case`]) to erase, or `None` when it
/// is shorter than [`MIN_SENSITIVE_LEN`], or than [`MIN_UNSPACED_LEN`] when
/// it folds to letters of [`UNSPACED_SCRIPTS`] only. The length is counted on
/// the text as written (composed, so a decomposed "É" is one character and a
/// spacing vowel sign counts), not on its folded form, which drops marks: a
/// three letter Devanagari name folds to two characters but is still a name.
/// Invisible format characters (see [`is_default_ignorable`]) do not count.
/// An unspaced string that folds to a single letter (a Thai or Burmese
/// syllable whose vowel is a mark) keeps its marks (see
/// [`fold_keeping_marks`]) and is matched with them ([`NeedleKind::Marked`]).
fn sensitive_form(text: &str) -> Option<String> {
    let folded = fold_case(text);
    let unspaced = is_unspaced(&folded);
    let min = if unspaced {
        MIN_UNSPACED_LEN
    } else {
        MIN_SENSITIVE_LEN
    };
    if text.nfc().filter(|c| !is_default_ignorable(*c)).count() < min {
        return None;
    }
    if unspaced && folded.chars().count() < MIN_UNSPACED_LEN {
        return Some(fold_keeping_marks(text).collect());
    }
    Some(folded)
}

/// Whether `folded` is made only of letters of [`UNSPACED_SCRIPTS`] (or
/// shared by them, as the kana length mark "ー").
fn is_unspaced(folded: &str) -> bool {
    !folded.is_empty()
        && folded.chars().all(|c| {
            c.is_alphabetic()
                && c.script_extension()
                    .iter()
                    .any(|script| UNSPACED_SCRIPTS.contains(&script))
        })
}

/// The maximal runs of `word` that fold to letters of [`UNSPACED_SCRIPTS`]
/// only, and those that fold to none of them ("梓潮" and "Rivo" in
/// "梓潮Rivo"), so that the unspaced part of a mixed word is erased anywhere.
/// A character that folds to nothing (a combining mark, an invisible format
/// character) belongs to the run before it.
fn script_runs(word: &str) -> Vec<&str> {
    let mut runs = Vec::new();
    let mut start = 0;
    let mut current = None;
    for (offset, c) in word.char_indices() {
        let folded: String = fold_char(c).collect();
        if folded.is_empty() {
            continue;
        }
        let unspaced = is_unspaced(&folded);
        if current.is_some_and(|before| before != unspaced) {
            runs.push(&word[start..offset]);
            start = offset;
        }
        current = Some(unspaced);
    }
    runs.push(&word[start..]);
    runs
}

/// Whether folding drops `c` (see [`fold_case`]): a combining mark, or an
/// invisible format character.
fn is_folded_away(c: char) -> bool {
    is_combining_mark(c) || is_default_ignorable(c)
}

/// Whether `c` has the Unicode `Default_Ignorable_Code_Point` property
/// (`DerivedCoreProperties.txt`): invisible characters such as the soft
/// hyphen, zero-width spaces and joiners, bidi controls, Hangul fillers,
/// variation selectors and tags.
fn is_default_ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{ad}'
            | '\u{34f}'
            | '\u{61c}'
            | '\u{115f}'..='\u{1160}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{feff}'
            | '\u{ffa0}'
            | '\u{fff0}'..='\u{fff8}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0000}'..='\u{e0fff}'
    )
}

/// Folds `text` for comparison: combining marks and invisible format characters
/// (see [`is_default_ignorable`]: "Elo\u{ad}die" is "Elodie") are dropped, and
/// every other character is decomposed with compatibility (NFKD, so "ｅ", "ⓔ"
/// and "e" agree, and "ǅ" is "Dž") and stripped of its combining marks
/// (accents, so "É", "é", "E" and "e" plus an accent all agree, and Turkish "İ"
/// becomes "i"), then lower-cased, upper-cased and lower-cased again, so "ß",
/// "ẞ" and "SS", "ς" and "σ", "ı" and "i" agree, and finally the letters NFKD
/// does not split are spelled out (see [`spell_out`]) and katakana letters
/// become their hiragana twins ("ユラ" is "ゆら"), which spell the same sounds.
///
/// The case part matches Unicode full case folding (`CaseFolding.txt`,
/// statuses C and F) for the scripts that have case. Dropping every
/// `General_Category=Mark` character also drops the spacing marks of some
/// scripts (Devanagari vowel signs), which makes matching looser: words that
/// differ only in those marks fold alike, and erasing one erases the other.
/// Privacy wins that trade.
fn fold_case(text: &str) -> String {
    text.chars()
        .filter(|c| !is_folded_away(*c))
        .flat_map(fold_char)
        .collect()
}

/// `text` folded like [`fold_case`], except that its combining marks are
/// kept, in canonical order after the letter they belong to: "ปู" stays
/// "ปู" where [`fold_case`] gives "ป".
fn fold_keeping_marks(text: &str) -> impl Iterator<Item = char> + '_ {
    text.nfd()
        .filter(|c| !is_default_ignorable(*c))
        .flat_map(|c| {
            is_combining_mark(c)
                .then_some(c)
                .into_iter()
                .chain(fold_char(c))
        })
}

fn fold_char(c: char) -> impl Iterator<Item = char> {
    c.nfkd()
        .filter(|d| !is_folded_away(*d))
        .flat_map(char::to_lowercase)
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .flat_map(|f| spell_out(f).into_iter().flatten())
        .map(hiragana)
}

/// The hiragana twin of a katakana letter or iteration mark ("ア" is "あ",
/// "ヽ" is "ゝ"), or `c` itself.
fn hiragana(c: char) -> char {
    if KATAKANA.contains(&c) || KATAKANA_ITERATION.contains(&c) {
        char::from_u32(u32::from(c) - KANA_OFFSET).unwrap_or(c)
    } else {
        c
    }
}

/// The plain spelling of a lower-case letter that NFKD does not split into a
/// base letter and a mark ("ł" is "l", "þ" is "th", "œ" is "oe"), or the
/// letter itself.
fn spell_out(c: char) -> [Option<char>; 2] {
    match c {
        'ł' => [Some('l'), None],
        'ø' => [Some('o'), None],
        'đ' | 'ð' => [Some('d'), None],
        'ħ' => [Some('h'), None],
        'ŧ' => [Some('t'), None],
        'þ' => [Some('t'), Some('h')],
        'œ' => [Some('o'), Some('e')],
        'æ' => [Some('a'), Some('e')],
        other => [Some(other), None],
    }
}

/// Erases every occurrence of each folded needle (see [`fold_case`]) from
/// `text`, ignoring case, accents and normalization form. Text outside the
/// erased spans is left byte for byte as it was, and the combining marks
/// following a match are erased with it. Invisible format characters (see
/// [`is_default_ignorable`]) are skipped when matching.
///
/// A needle of fewer than [`MIN_SUBSTRING_LEN`] characters only matches as a
/// word of its own or glued to digits, so "ben" does not erase half of
/// "benefits". A longer one also matches inside longer words where
/// [`FoldedText::counts`] accepts it. A needle made only of letters of
/// [`UNSPACED_SCRIPTS`] matches anywhere, on whole characters. A text with
/// percent-encoded UTF-8 (`%C3%89lodie`) or JSON escapes (`\u3086`) is also
/// matched in its decoded form, decoded again while that changes it, up to
/// [`MAX_DECODINGS`] times (`%25C3%2589lodie`, `%5Cu3086`), and a match there
/// erases the whole encoded span. Each form is folded once.
fn erase_words(text: &str, needles: &[String]) -> String {
    let mut spans = match_spans(text, needles);
    let layers = decodings(text);
    for (depth, layer) in layers.iter().enumerate() {
        spans.extend(
            match_spans(&layer.text, needles)
                .into_iter()
                .filter_map(|span| {
                    layers[..=depth]
                        .iter()
                        .rev()
                        .try_fold(span, |(from, to), outer| outer.original(from, to))
                }),
        );
    }
    spans.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (from, to) in spans {
        if from < last {
            last = last.max(to);
            continue;
        }
        out.push_str(&text[last..from]);
        out.push_str(REDACTED);
        last = to;
    }
    out.push_str(&text[last..]);
    out
}

/// `text` decoded again and again while that changes it (see [`Unescaped`]),
/// up to [`MAX_DECODINGS`] times, each layer decoded from the one before.
fn decodings(text: &str) -> Vec<Unescaped> {
    let mut layers: Vec<Unescaped> = Vec::new();
    while layers.len() < MAX_DECODINGS {
        let source = layers.last().map_or(text, |layer| layer.text.as_str());
        let Some(decoded) = Unescaped::new(source) else {
            break;
        };
        layers.push(decoded);
    }
    layers
}

/// The byte ranges of `text` matched by `needles` (see [`erase_words`]),
/// unsorted and possibly overlapping.
fn match_spans(text: &str, needles: &[String]) -> Vec<(usize, usize)> {
    let folded = FoldedText::new(text);
    // Folded with its marks only when a needle needs it.
    let marked = OnceCell::new();
    let mut spans = Vec::new();
    for needle in needles.iter().filter(|n| !n.is_empty()) {
        let kind = NeedleKind::of(needle);
        if kind == NeedleKind::Marked {
            // A match with the marks is also one without them.
            let letters: String = needle.chars().filter(|c| !is_combining_mark(*c)).collect();
            if !folded.folded.contains(&letters) {
                continue;
            }
        }
        let marks =
            (kind == NeedleKind::Marked).then(|| marked.get_or_init(|| MarkedText::new(text)));
        let haystack = marks.map_or(folded.folded.as_str(), |m| m.folded.as_str());
        let mut at = 0;
        while let Some(found) = haystack[at..].find(needle.as_str()) {
            let start = at + found;
            at = start + haystack[start..].chars().next().map_or(1, char::len_utf8);
            let span = match marks {
                Some(marks) => marks.span(&folded, start, needle.len()),
                None => folded.span(start, needle.len(), kind),
            };
            spans.extend(span);
        }
    }
    spans
}

/// A text folded with its combining marks (see [`fold_keeping_marks`]), for
/// [`NeedleKind::Marked`] needles.
#[derive(Debug)]
struct MarkedText {
    folded: String,
    /// For every byte of `folded`, the base character it came from, counted
    /// as in [`FoldedText`].
    origins: Vec<usize>,
}

impl MarkedText {
    fn new(text: &str) -> Self {
        let mut folded = String::with_capacity(text.len());
        let mut origins = Vec::with_capacity(text.len());
        let mut starts = text
            .char_indices()
            .filter(|(_, c)| !is_combining_mark(*c))
            .map(|(offset, _)| offset)
            .peekable();
        let mut base = 0;
        while let Some(start) = starts.next() {
            let end = starts.peek().copied().unwrap_or(text.len());
            for f in fold_keeping_marks(&text[start..end]) {
                folded.push(f);
                origins.extend(std::iter::repeat_n(base, f.len_utf8()));
            }
            base += 1;
        }
        Self { folded, origins }
    }

    /// The byte range of `text` to erase for the needle of `len` bytes found
    /// at byte `start` of the folded text, or `None` unless the match covers
    /// whole base characters with all their marks ("ปู" is not in "ปู่").
    fn span(&self, text: &FoldedText, start: usize, len: usize) -> Option<(usize, usize)> {
        let first = *self.origins.get(start)?;
        let last = *self.origins.get(start + len.checked_sub(1)?)?;
        let whole = start
            .checked_sub(1)
            .and_then(|before| self.origins.get(before))
            != Some(&first)
            && self.origins.get(start + len) != Some(&last);
        whole.then(|| (text.offset(first), text.offset(last + 1)))
    }
}

/// A text with its percent-encoded UTF-8 and its JSON escapes (`\uXXXX`,
/// `\\`, `\n`, …) decoded, and the way back to the original. Percent
/// sequences that are not valid UTF-8, and escapes of a lone surrogate, are
/// kept as they were. A doubly escaped `\\u3086` decodes to `\u3086`, which
/// the next layer decodes.
#[derive(Debug)]
struct Unescaped {
    text: String,
    original_len: usize,
    /// For every character of `text`: its byte offset there, and the byte
    /// range in the original it came from.
    chars: Vec<(usize, usize, usize)>,
}

impl Unescaped {
    /// `None` when `text` has neither a `%XX` sequence that decodes to UTF-8
    /// nor a JSON escape.
    fn new(text: &str) -> Option<Self> {
        let mut decoded = String::with_capacity(text.len());
        let mut chars = Vec::new();
        let mut changed = false;
        let mut at = 0;
        while at < text.len() {
            if let Some((c, len)) = escaped_char(text, at) {
                chars.push((decoded.len(), at, at + len));
                decoded.push(c);
                at += len;
                changed = true;
                continue;
            }
            let mut run = Vec::new();
            while let Some(byte) = encoded_byte(text, at + 3 * run.len()) {
                run.push(byte);
            }
            if run.is_empty() {
                let c = text[at..].chars().next()?;
                chars.push((decoded.len(), at, at + c.len_utf8()));
                decoded.push(c);
                at += c.len_utf8();
                continue;
            }
            for chunk in run.utf8_chunks() {
                for c in chunk.valid().chars() {
                    let end = at + 3 * c.len_utf8();
                    chars.push((decoded.len(), at, end));
                    decoded.push(c);
                    at = end;
                    changed = true;
                }
                let kept = at + 3 * chunk.invalid().len();
                for (offset, c) in text[at..kept].char_indices() {
                    chars.push((decoded.len(), at + offset, at + offset + 1));
                    decoded.push(c);
                }
                at = kept;
            }
        }
        changed.then_some(Self {
            text: decoded,
            original_len: text.len(),
            chars,
        })
    }

    /// The byte range of the original that the decoded range `from..to`
    /// came from, both ends on character boundaries.
    fn original(&self, from: usize, to: usize) -> Option<(usize, usize)> {
        Some((self.original_offset(from)?, self.original_offset(to)?))
    }

    fn original_offset(&self, offset: usize) -> Option<usize> {
        if offset == self.text.len() {
            return Some(self.original_len);
        }
        let index = self
            .chars
            .binary_search_by_key(&offset, |(decoded, _, _)| *decoded)
            .ok()?;
        self.chars.get(index).map(|(_, start, _)| *start)
    }
}

/// The character a JSON escape at byte `at` of `text` stands for, and the
/// length of the escape: `\uXXXX`, two of them for a surrogate pair, or a
/// backslash and one character (`\\`, `\"`, `\/`, `\b`, `\f`, `\n`, `\r`,
/// `\t`).
fn escaped_char(text: &str, at: usize) -> Option<(char, usize)> {
    let simple = match text.get(at..)?.strip_prefix('\\')?.chars().next()? {
        c @ ('"' | '\\' | '/') => Some(c),
        'b' => Some('\u{8}'),
        'f' => Some('\u{c}'),
        'n' => Some('\n'),
        'r' => Some('\r'),
        't' => Some('\t'),
        _ => None,
    };
    if let Some(c) = simple {
        return Some((c, 2));
    }
    let unit = escaped_unit(text, at)?;
    if let Some(Ok(c)) = char::decode_utf16([unit]).next() {
        return Some((c, ESCAPE_LEN));
    }
    let low = escaped_unit(text, at + ESCAPE_LEN)?;
    let c = char::decode_utf16([unit, low]).next()?.ok()?;
    Some((c, 2 * ESCAPE_LEN))
}

/// The UTF-16 code unit of a `\uXXXX` escape at byte `at` of `text`, if any.
fn escaped_unit(text: &str, at: usize) -> Option<u16> {
    let digits = text.get(at..at + ESCAPE_LEN)?.strip_prefix("\\u")?;
    if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(digits, 16).ok()
}

/// The byte encoded by a `%XX` sequence at byte `at` of `text`, if any.
fn encoded_byte(text: &str, at: usize) -> Option<u8> {
    let digits = text.get(at..at + 3)?.strip_prefix('%')?;
    if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u8::from_str_radix(digits, 16).ok()
}

/// A text folded once for matching (see [`fold_case`]), with the way back
/// from the folded form to the original and everything [`Self::counts`]
/// needs, computed once so that judging a match takes constant time.
///
/// Positions are counted in base characters: every character of the text
/// but combining marks, which belong to the base character before them. A
/// cut `at` lies between base characters `at - 1` and `at`. An invisible
/// format character (see [`is_default_ignorable`]) is a base character that
/// folds to nothing: a match runs across it, and it still marks a boundary.
#[derive(Debug)]
struct FoldedText<'a> {
    text: &'a str,
    folded: String,
    /// Byte offset in `text` and value of every base character.
    bases: Vec<(usize, char)>,
    /// For every byte of `folded`, the base character it came from.
    origins: Vec<usize>,
    /// For every cut, whether it is a word boundary (see [`is_boundary`]).
    cuts: Vec<bool>,
    /// For every cut, whether it is a hard boundary (see
    /// [`is_hard_boundary`]).
    hard_cuts: Vec<bool>,
    /// Runs of letters and digits.
    words: Stretches,
    /// Stretches between word boundaries.
    segments: Stretches,
    /// For every cut, how many base characters before it are lowercase.
    lowercase_before: Vec<usize>,
    /// For every cut, how many base characters before it are not a bare
    /// ASCII hex digit (a hex digit carrying a combining mark is not).
    non_hex_before: Vec<usize>,
    /// For every cut, how many base characters before it are ASCII digits.
    digits_before: Vec<usize>,
}

impl<'a> FoldedText<'a> {
    fn new(text: &'a str) -> Self {
        let mut folded = String::with_capacity(text.len());
        let mut bases = Vec::with_capacity(text.len());
        let mut origins = Vec::with_capacity(text.len());
        for (offset, c) in text.char_indices().filter(|(_, c)| !is_combining_mark(*c)) {
            let base = bases.len();
            bases.push((offset, c));
            for f in fold_char(c) {
                folded.push(f);
                origins.extend(std::iter::repeat_n(base, f.len_utf8()));
            }
        }
        let char_at = |at: usize| bases.get(at).map(|(_, c)| *c);
        let cut_count = bases.len() + 1;
        let cuts: Vec<bool> = (0..cut_count)
            .map(|at| is_boundary(at.checked_sub(1).and_then(char_at), char_at(at)))
            .collect();
        let hard_cuts: Vec<bool> = (0..cut_count)
            .map(|at| is_hard_boundary(at.checked_sub(1).and_then(char_at), char_at(at)))
            .collect();
        let alphanumeric = |at: usize| char_at(at).is_some_and(char::is_alphanumeric);
        let in_word: Vec<bool> = (0..cut_count)
            .map(|at| at > 0 && alphanumeric(at - 1) && alphanumeric(at))
            .collect();
        let in_segment: Vec<bool> = cuts.iter().map(|cut| !cut).collect();
        let byte_end = |at: usize| bases.get(at + 1).map_or(text.len(), |(offset, _)| *offset);
        let bare_hex: Vec<bool> = bases
            .iter()
            .enumerate()
            .map(|(at, (offset, c))| c.is_ascii_hexdigit() && byte_end(at) == offset + 1)
            .collect();
        let lowercase_before = counts_before(bases.iter().map(|(_, c)| c.is_lowercase()));
        let non_hex_before = counts_before(bare_hex.iter().map(|hex| !hex));
        let digits_before = counts_before(bases.iter().map(|(_, c)| c.is_ascii_digit()));
        Self {
            text,
            folded,
            bases,
            origins,
            cuts,
            hard_cuts,
            words: Stretches::new(&in_word),
            segments: Stretches::new(&in_segment),
            lowercase_before,
            non_hex_before,
            digits_before,
        }
    }

    /// The byte range of `text` to erase for the needle of `len` bytes found
    /// at byte `start` of the folded text, or `None` when that match does not
    /// count. The range ends past the combining marks of its last character.
    ///
    /// A [`NeedleKind::Unspaced`] match must also cover the whole folded
    /// form of its first and last characters ("成功" does not match "㍻功",
    /// which folds to "平成功"), except that its last Hangul syllable may
    /// carry one more final consonant: a colloquial contraction glues a
    /// particle to a name that way ("유날" for "유나를").
    fn span(&self, start: usize, len: usize, kind: NeedleKind) -> Option<(usize, usize)> {
        let first = *self.origins.get(start)?;
        let last = *self.origins.get(start + len.checked_sub(1)?)?;
        let whole_start = start
            .checked_sub(1)
            .and_then(|before| self.origins.get(before))
            != Some(&first);
        let whole_end = self.origins.get(start + len) != Some(&last)
            || self.only_final_consonant_at(start + len, last);
        if kind == NeedleKind::Unspaced && !(whole_start && whole_end) {
            return None;
        }
        let end = last + 1;
        self.counts(first, end, kind)
            .then(|| (self.offset(first), self.offset(end)))
    }

    /// Whether the folded text at byte `at`, after a Hangul vowel, holds
    /// nothing more of base character `last` than one final consonant.
    fn only_final_consonant_at(&self, at: usize, last: usize) -> bool {
        let after_vowel = self.folded[..at]
            .chars()
            .next_back()
            .is_some_and(|c| HANGUL_VOWELS.contains(&c));
        after_vowel
            && self.folded[at..].chars().next().is_some_and(|c| {
                HANGUL_FINALS.contains(&c) && self.origins.get(at + c.len_utf8()) != Some(&last)
            })
    }

    /// Whether a match of the base characters `start..end` is erased.
    ///
    /// No match may lie inside a word (a run of letters and digits) that
    /// looks like a hex id (see [`Self::is_hex_id`]). An unspaced needle
    /// matches anywhere else. A short needle must start and end at a hard
    /// boundary (see [`is_hard_boundary`]: "Ben", "Ben123", not "benefits").
    /// A long one must start and end at a boundary (see [`is_boundary`];
    /// after the match an upper to lower case change also counts, as in
    /// "`ELODIEmartin`"). Inside a stretch between
    /// boundaries that has no lowercase letter (`JEANLUC`, `KINGDOM`), case
    /// marks no boundary, so a boundary at either end is enough.
    fn counts(&self, start: usize, end: usize, kind: NeedleKind) -> bool {
        let word = self.words.around(start, end);
        if word != (start, end) && self.is_hex_id(word) {
            return false;
        }
        match kind {
            NeedleKind::Unspaced | NeedleKind::Marked => return true,
            NeedleKind::Short => return self.hard_cuts[start] && self.hard_cuts[end],
            NeedleKind::Long => {}
        }
        let opens = self.cuts[start];
        let closes = self.cuts[end]
            || matches!(
                (self.kind(end - 1), self.kind(end)),
                (Some(CharKind::Upper), Some(CharKind::Lower))
            );
        if opens && closes {
            return true;
        }
        let (from, to) = self.segments.around(start, end);
        (opens || closes) && self.lowercase_before[to] == self.lowercase_before[from]
    }

    /// Whether the base characters `from..to` look like a hex id: only bare
    /// hex digits, at least one of them a decimal digit, and at least
    /// [`MIN_HEX_WORD`] bytes long. Names are never erased from inside one.
    fn is_hex_id(&self, (from, to): (usize, usize)) -> bool {
        self.offset(to) - self.offset(from) >= MIN_HEX_WORD
            && self.non_hex_before[to] == self.non_hex_before[from]
            && self.digits_before[to] > self.digits_before[from]
    }

    fn kind(&self, at: usize) -> Option<CharKind> {
        self.bases.get(at).map(|(_, c)| CharKind::of(*c))
    }

    /// Byte offset of base character `at`, or the end of the text.
    fn offset(&self, at: usize) -> usize {
        self.bases
            .get(at)
            .map_or(self.text.len(), |(offset, _)| *offset)
    }
}

/// For every cut, how many of the flags before it are set.
fn counts_before(flags: impl Iterator<Item = bool>) -> Vec<usize> {
    std::iter::once(0)
        .chain(flags.scan(0, |count, flag| {
            *count += usize::from(flag);
            Some(*count)
        }))
        .collect()
}

/// Maximal stretches of base characters whose inner cuts are all joined,
/// stored as the furthest cut reachable to the left and to the right of
/// every cut.
#[derive(Debug)]
struct Stretches {
    left: Vec<usize>,
    right: Vec<usize>,
}

impl Stretches {
    /// `joined[at]` tells whether cut `at` lies inside a stretch; the first
    /// and last cuts (the text's edges) never do.
    fn new(joined: &[bool]) -> Self {
        let mut left = Vec::with_capacity(joined.len());
        for (at, inside) in joined.iter().enumerate() {
            let reach = match left.last() {
                Some(&before) if *inside => before,
                _ => at,
            };
            left.push(reach);
        }
        let mut right = vec![0; joined.len()];
        let mut reach = joined.len().saturating_sub(1);
        for (at, inside) in joined.iter().enumerate().rev() {
            if !inside {
                reach = at;
            }
            right[at] = reach;
        }
        Self { left, right }
    }

    /// `start..end` widened to the stretch around it.
    fn around(&self, start: usize, end: usize) -> (usize, usize) {
        (self.left[start], self.right[end])
    }
}

/// Where a needle may match (see [`FoldedText::counts`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NeedleKind {
    /// Fewer than [`MIN_SUBSTRING_LEN`] characters: a word of its own, or
    /// glued to digits.
    Short,
    /// Also inside longer words, between boundaries.
    Long,
    /// Only letters of [`UNSPACED_SCRIPTS`], at least [`MIN_UNSPACED_LEN`]
    /// of them: anywhere.
    Unspaced,
    /// With combining marks, which folding never leaves: an unspaced name
    /// that folds to one letter (see [`sensitive_form`]), matched with its
    /// marks anywhere, on whole base characters.
    Marked,
}

impl NeedleKind {
    /// An unspaced needle of one letter without its marks is short: matched
    /// anywhere, it would erase that letter from all text of its script.
    fn of(needle: &str) -> Self {
        if needle.chars().any(is_combining_mark) {
            Self::Marked
        } else if is_unspaced(needle) && needle.chars().count() >= MIN_UNSPACED_LEN {
            Self::Unspaced
        } else if needle.chars().count() < MIN_SUBSTRING_LEN {
            Self::Short
        } else {
            Self::Long
        }
    }
}

/// What a character is for word boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharKind {
    /// Neither a letter nor a digit: space, punctuation, `_`, `@`, …
    Separator,
    Digit,
    Lower,
    /// An upper-case or title-case letter.
    Upper,
    /// A letter without case, such as CJK or kana.
    Uncased,
}

impl CharKind {
    fn of(c: char) -> Self {
        if c.is_numeric() {
            Self::Digit
        } else if !c.is_alphabetic() {
            Self::Separator
        } else if c.is_lowercase() {
            Self::Lower
        } else if c.is_uppercase() || !c.to_lowercase().eq([c]) {
            Self::Upper
        } else {
            Self::Uncased
        }
    }
}

/// Whether a hard boundary lies between `before` and `after` (`None` is
/// the edge of the text): next to a separator, or between a digit and a
/// letter. These also end a short name.
fn is_hard_boundary(before: Option<char>, after: Option<char>) -> bool {
    let (Some(before), Some(after)) = (before, after) else {
        return true;
    };
    match (CharKind::of(before), CharKind::of(after)) {
        (CharKind::Digit, other) | (other, CharKind::Digit) => other != CharKind::Digit,
        (CharKind::Separator, _) | (_, CharKind::Separator) => true,
        _ => false,
    }
}

/// Whether a word boundary lies between `before` and `after` (`None` is the
/// edge of the text): a hard boundary (see [`is_hard_boundary`]), a lower to
/// upper case change, or a change between a cased and an uncased letter.
fn is_boundary(before: Option<char>, after: Option<char>) -> bool {
    is_hard_boundary(before, after)
        || before.zip(after).is_some_and(|(before, after)| {
            matches!(
                (CharKind::of(before), CharKind::of(after)),
                (CharKind::Lower, CharKind::Upper)
                    | (CharKind::Uncased, CharKind::Lower | CharKind::Upper)
                    | (CharKind::Lower | CharKind::Upper, CharKind::Uncased)
            )
        })
}

#[cfg(test)]
mod tests {
    use super::{
        FoldedText, NeedleKind, ScrubError, ScrubParams, erase_words, fold_case, has_opaque_token,
        is_boundary, is_month_buckets, is_uuid, run, scrub_files, sensitive_form,
    };
    use crate::report;
    use proptest::prelude::*;
    use serde_json::json;
    use std::fmt::Write as _;
    use unicode_normalization::UnicodeNormalization as _;
    use unicode_normalization::char::is_combining_mark;

    const PARAMS: ScrubParams = ScrubParams {
        time_shift_s: 0,
        calorie_factor: 1.0,
    };

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

    #[test]
    fn erases_decomposed_names_from_precomposed_text() {
        let needles = vec![fold_case("e\u{301}lodie")];
        assert_eq!(
            erase_words("Ride with \u{c9}lodie, \u{e9}lodie", &needles),
            "Ride with redacted, redacted"
        );
    }

    #[test]
    fn erases_precomposed_names_from_decomposed_text() {
        let needles = vec![fold_case("\u{e9}lodie")];
        assert_eq!(
            erase_words("Ride with E\u{301}lodie, e\u{301}lodie", &needles),
            "Ride with redacted, redacted"
        );
    }

    #[test]
    fn text_outside_erased_spans_keeps_its_bytes() {
        let needles = vec![fold_case("zed")];
        assert_eq!(
            erase_words("cafe\u{301} zed", &needles),
            "cafe\u{301} redacted"
        );
    }

    #[test]
    fn erases_accent_less_spellings() {
        let needles = vec![fold_case("\u{c9}lodie"), fold_case("\u{130}rem")];
        assert_eq!(
            erase_words(
                "ELODIE, elodie, IREM, irem, \u{130}REM, e\u{301}lodie",
                &needles
            ),
            "redacted, redacted, redacted, redacted, redacted, redacted"
        );
    }

    #[test]
    fn erases_ligatures_spelled_out() {
        let needles = vec![fold_case("C\u{153}ur"), fold_case("Coeur")];
        assert_eq!(
            erase_words("Coeur, C\u{153}ur, COEUR", &needles),
            "redacted, redacted, redacted"
        );
    }

    #[test]
    fn a_mark_left_behind_by_an_erasure_is_erased_with_it() {
        let needles = vec![fold_case("max"), fold_case("rides")];
        assert_eq!(
            erase_words("Max\u{331} rides", &needles),
            "redacted redacted"
        );
    }

    #[test]
    fn names_are_not_erased_inside_ordinary_words() {
        let needles = vec![fold_case("King"), fold_case("Love")];
        assert_eq!(
            erase_words("walking, glove, KING, LoveKing, Kingdom", &needles),
            "walking, glove, redacted, redactedredacted, Kingdom"
        );
    }

    #[test]
    fn names_are_erased_at_the_edges_of_all_caps_words() {
        let needles = vec![fold_case("Jean"), fold_case("Claire"), fold_case("King")];
        assert_eq!(
            erase_words("JEANLUC, MARIECLAIRE, KINGDOM, Kingdom, XKINGX", &needles),
            "redactedLUC, MARIEredacted, redactedDOM, Kingdom, XKINGX"
        );
    }

    #[test]
    fn names_are_not_erased_inside_hex_ids() {
        let needles = vec![fold_case("Face"), fold_case("Dade")];
        assert_eq!(
            erase_words("0123facedade89 and face89", &needles),
            "0123facedade89 and redacted89"
        );
    }

    #[test]
    fn mixed_case_compounds_are_erased() {
        let needles = vec![fold_case("\u{c9}lodie"), fold_case("Gro\u{df}")];
        assert_eq!(
            erase_words("\u{c9}lodieGRO\u{1e9e}, MarieGross", &needles),
            "redactedredacted, Marieredacted"
        );
    }

    #[test]
    fn boundaries_lie_at_separators_digits_case_and_script_changes() {
        let cases = [
            (None, Some('a'), true),
            (Some('a'), None, true),
            (Some('a'), Some(' '), true),
            (Some('_'), Some('a'), true),
            (Some('\u{2019}'), Some('s'), true),
            (Some('4'), Some('e'), true),
            (Some('e'), Some('1'), true),
            (Some('1'), Some('2'), false),
            (Some('e'), Some('M'), true),
            (Some('\u{1c5}'), Some('a'), false),
            (Some('a'), Some('\u{1c5}'), true),
            (Some('E'), Some('m'), false),
            (Some('E'), Some('M'), false),
            (Some('e'), Some('m'), false),
            (Some('e'), Some('\u{3055}'), true),
            (Some('\u{65e5}'), Some('E'), true),
            (Some('\u{65e5}'), Some('\u{672c}'), false),
            (Some('\u{65e5}'), Some('1'), true),
        ];
        for (before, after, expected) in cases {
            assert_eq!(is_boundary(before, after), expected, "{before:?} {after:?}");
        }
    }

    #[test]
    fn hex_id_words_need_a_digit_and_eight_hex_characters() {
        let is_hex_id = |word: &str| {
            let folded = FoldedText::new(word);
            folded.is_hex_id((0, folded.bases.len()))
        };
        assert!(is_hex_id("0123facedade89"));
        assert!(is_hex_id("0123FACE"));
        assert!(!is_hex_id("face89"));
        assert!(!is_hex_id("facedade"));
        assert!(!is_hex_id("0123facx"));
        assert!(!is_hex_id("0123fa\u{301}ce"));
    }

    #[test]
    fn turkish_dotted_and_dotless_i_match_plain_i() {
        let needles = vec![fold_case("T\u{131}lvor"), fold_case("\u{130}rem")];
        assert_eq!(
            erase_words("TILVOR, tilvor, T\u{131}lvor, irem, \u{130}REM", &needles),
            "redacted, redacted, redacted, redacted, redacted"
        );
    }

    #[test]
    fn letters_nfd_does_not_split_match_their_plain_spelling() {
        let needles = vec![
            fold_case("\u{141}ukasz"),
            fold_case("Bj\u{f8}rn"),
            fold_case("\u{110}or\u{111}e"),
            fold_case("I\u{f0}unn"),
            fold_case("\u{126}adrel"),
            fold_case("\u{166}arvo"),
            fold_case("\u{de}orvin"),
            fold_case("\u{c6}gir"),
            fold_case("Coeurval"),
        ];
        assert_eq!(
            erase_words(
                "Lukasz, BJORN, Dorde, IDUNN, hadrel, Tarvo, THORVIN, Aegir, C\u{152}URVAL",
                &needles
            ),
            "redacted, redacted, redacted, redacted, redacted, redacted, redacted, redacted, \
             redacted"
        );
    }

    #[test]
    fn digits_next_to_a_name_are_boundaries() {
        let needles = vec![fold_case("\u{c9}lodie")];
        assert_eq!(
            erase_words("Elodie123, 123Elodie, rides4elodie, 2024elodie", &needles),
            "redacted123, 123redacted, rides4redacted, 2024redacted"
        );
    }

    #[test]
    fn names_are_not_erased_inside_words_that_look_like_hex_ids() {
        let needles = vec![fold_case("Face"), fold_case("Dade")];
        assert_eq!(
            erase_words("0123FACE, face0123, 12dade34", &needles),
            "0123FACE, face0123, 12dade34"
        );
    }

    #[test]
    fn lower_to_upper_case_changes_are_boundaries() {
        let needles = vec![fold_case("\u{c9}lodie"), fold_case("Claire")];
        assert_eq!(
            erase_words("elodieMartin, MarieClaire", &needles),
            "redactedMartin, Marieredacted"
        );
    }

    #[test]
    fn an_upper_to_lower_change_after_a_name_ends_it() {
        let needles = vec![fold_case("\u{c9}lodie")];
        assert_eq!(
            erase_words("ELODIEmartin, XELODIEmartin", &needles),
            "redactedmartin, XELODIEmartin"
        );
    }

    #[test]
    fn a_change_between_cased_and_uncased_letters_is_a_boundary() {
        let needles = vec![fold_case("\u{c9}lodie")];
        assert_eq!(
            erase_words(
                "Elodie\u{3055}\u{3093}, \u{65e5}\u{672c}Elodie, ELODIE\u{65e5}\u{672c}",
                &needles
            ),
            "redacted\u{3055}\u{3093}, \u{65e5}\u{672c}redacted, redacted\u{65e5}\u{672c}"
        );
    }

    #[test]
    fn names_in_unspaced_scripts_are_erased_inside_longer_words() {
        let needles = vec![
            fold_case("\u{3086}\u{3089}\u{306e}"),
            fold_case("\u{30e6}\u{30fc}\u{30ea}\u{30ab}"),
            fold_case("\u{9673}\u{8a60}\u{5d50}"),
            fold_case("\u{c724}\u{b2e4}\u{c194}"),
            fold_case("\u{e28}\u{e23}\u{e35}\u{e27}\u{e23}\u{e32}"),
        ];
        assert_eq!(
            erase_words(
                "\u{306f}\u{3044}\u{3086}\u{3089}\u{306e}\u{3055}\u{3093}, \
                 \u{3088}\u{3046}\u{3053}\u{305d}\u{30e6}\u{30fc}\u{30ea}\u{30ab}\u{69d8}, \
                 \u{4eca}\u{5929}\u{9673}\u{8a60}\u{5d50}\u{8001}\u{5e2b}, \
                 \u{c624}\u{b298}\u{c724}\u{b2e4}\u{c194}\u{b2d8}\u{c774}, \
                 \u{e04}\u{e38}\u{e13}\u{e28}\u{e23}\u{e35}\u{e27}\u{e23}\u{e32}\u{e04}\u{e23}\u{e31}\u{e1a}",
                &needles
            ),
            "\u{306f}\u{3044}redacted\u{3055}\u{3093}, \u{3088}\u{3046}\u{3053}\u{305d}redacted\u{69d8}, \
             \u{4eca}\u{5929}redacted\u{8001}\u{5e2b}, \u{c624}\u{b298}redacted\u{b2d8}\u{c774}, \
             \u{e04}\u{e38}\u{e13}redacted\u{e04}\u{e23}\u{e31}\u{e1a}"
        );
    }

    #[test]
    fn names_in_unspaced_scripts_match_their_compatibility_forms() {
        let needles = vec![fold_case("\u{30e6}\u{30e9}\u{30ce}")];
        assert_eq!(
            erase_words("\u{3088}\u{3046}\u{ff95}\u{ff97}\u{ff89}\u{69d8}", &needles),
            "\u{3088}\u{3046}redacted\u{69d8}"
        );
    }

    #[test]
    fn names_in_unspaced_scripts_match_whole_characters() {
        let needles = vec![fold_case("\u{6210}\u{529f}"), fold_case("\u{5927}\u{5e73}")];
        assert_eq!(
            erase_words(
                "\u{337b}\u{529f}, \u{5927}\u{337b}, \u{6210}\u{529f}",
                &needles
            ),
            "\u{337b}\u{529f}, \u{5927}\u{337b}, redacted"
        );
    }

    #[test]
    fn korean_names_contracted_with_a_final_consonant_are_erased() {
        let needles = vec![fold_case("\u{c720}\u{b098}"), fold_case("\u{b2e4}\u{c18c}")];
        assert_eq!(
            erase_words(
                "\u{c720}\u{b0a0}, \u{c720}\u{b098}\u{b97c}, \u{b2e4}\u{c194}\u{c528}",
                &needles
            ),
            "redacted, redacted\u{b97c}, redacted\u{c528}"
        );
    }

    #[test]
    fn hiragana_and_katakana_spellings_match_each_other() {
        let needles = vec![fold_case("\u{3086}\u{3089}"), fold_case("\u{30ea}\u{30ce}")];
        assert_eq!(
            erase_words(
                "\u{30e6}\u{30e9}\u{3055}\u{3093}, \u{ff95}\u{ff97}, \u{306f}\u{308a}\u{306e}\u{3061}\u{3083}\u{3093}",
                &needles
            ),
            "redacted\u{3055}\u{3093}, redacted, \u{306f}redacted\u{3061}\u{3083}\u{3093}"
        );
    }

    #[test]
    fn names_in_every_unspaced_script_are_erased_from_two_characters() {
        let cases = [
            ("\u{f40}\u{f41}", "\u{f42}\u{f40}\u{f41}\u{f42}\u{f0b}"),
            ("\u{a000}\u{a001}", "\u{a002}\u{a000}\u{a001}\u{a002}"),
            ("\u{a98f}\u{a990}", "\u{a991}\u{a98f}\u{a990}\u{a991}"),
            ("\u{1b13}\u{1b14}", "\u{1b15}\u{1b13}\u{1b14}\u{1b15}"),
            ("\u{1b8a}\u{1b8b}", "\u{1b8c}\u{1b8a}\u{1b8b}\u{1b8c}"),
            ("\u{1a00}\u{1a01}", "\u{1a02}\u{1a00}\u{1a01}\u{1a02}"),
            ("\u{1a20}\u{1a21}", "\u{1a22}\u{1a20}\u{1a21}\u{1a22}"),
            ("\u{1950}\u{1951}", "\u{1952}\u{1950}\u{1951}\u{1952}"),
            ("\u{1980}\u{1981}", "\u{1982}\u{1980}\u{1981}\u{1982}"),
            ("\u{aa80}\u{aa81}", "\u{aa82}\u{aa80}\u{aa81}\u{aa82}"),
            ("\u{3105}\u{3106}", "\u{3107}\u{3105}\u{3106}\u{3107}"),
            ("\u{e81}\u{e82}", "\u{e84}\u{e81}\u{e82}\u{e84}"),
            ("\u{1780}\u{1781}", "\u{1782}\u{1780}\u{1781}\u{1782}"),
            ("\u{1000}\u{1001}", "\u{1002}\u{1000}\u{1001}\u{1002}"),
        ];
        for (name, text) in cases {
            let needles: Vec<String> = sensitive_form(name).into_iter().collect();
            let first: String = text.chars().take(1).collect();
            let tail: String = text.chars().skip(3).collect();
            assert_eq!(
                erase_words(text, &needles),
                format!("{first}redacted{tail}"),
                "{name:?} in {text:?}"
            );
        }
    }

    #[test]
    fn invisible_format_characters_do_not_hide_a_name() {
        let needles = vec![
            fold_case("\u{3086}\u{3089}"),
            fold_case("\u{c9}lodie"),
            fold_case("Ri\u{ad}vo"),
            fold_case("Dead"),
        ];
        assert_eq!(
            erase_words(
                "\u{306f}\u{3086}\u{200d}\u{3089}\u{3055}\u{3093}, \u{3086}\u{200c}\u{3089}, \
                 Elo\u{ad}die, E\u{feff}lo\u{2060}die, Rivo, elodie\u{200b}martin, x\u{200e}elodie, \
                 dead\u{200b}beef12",
                &needles
            ),
            "\u{306f}redacted\u{3055}\u{3093}, redacted, redacted, redacted, redacted, \
             redacted\u{200b}martin, x\u{200e}redacted, redacted\u{200b}beef12"
        );
    }

    #[test]
    fn json_escaped_names_are_erased() {
        let needles = vec![
            fold_case("\u{3086}\u{3089}"),
            fold_case("\u{20000}\u{4e00}"),
            fold_case("\u{c9}lodie"),
        ];
        assert_eq!(
            erase_words(
                "\\u3086\\u3089\\u3055\\u3093, \\uD840\\uDC00\\u4e00x, \\u0045lodie, \
                 %5Cu3086%5Cu3089, \\uD840 \\u12G4 \\u30",
                &needles
            ),
            "redacted\\u3055\\u3093, redactedx, redacted, redacted, \\uD840 \\u12G4 \\u30"
        );
    }

    #[test]
    fn single_character_name_fields_are_erased_together() {
        let files = vec![
            (
                "me.json".to_owned(),
                json!({
                    "user": {"username": "u_x1", "first_name": "\u{6c85}", "last_name": "\u{67cf}"},
                    "feed": ["\u{67cf}\u{6c85}\u{8001}\u{5e2b}", "\u{4eca}\u{6c85}\u{67cf}"]
                }),
            ),
            (
                "ride.json".to_owned(),
                json!({
                    "instructor": {"first_name": "\u{b2e4}\u{c194}", "last_name": "\u{c724}"},
                    "description": "\u{c624}\u{b298}\u{c724}\u{b2e4}\u{c194}\u{b2d8}"
                }),
            ),
        ];
        let output = scrub_files(files, PARAMS);
        assert_eq!(
            output[0].1["feed"],
            json!(["redacted\u{8001}\u{5e2b}", "\u{4eca}redacted"])
        );
        assert_eq!(
            output[1].1["description"],
            "\u{c624}\u{b298}redacted\u{b2d8}"
        );
    }

    #[test]
    fn names_folding_to_one_letter_are_erased_with_their_marks() {
        let files = vec![(
            "me.json".to_owned(),
            json!({
                "user": {"username": "u_x1", "name": "\u{e1b}\u{e39}", "last_name": "\u{1019}\u{1031}"},
                "feed": [
                    "\u{e1b}\u{e39} \u{e04}",
                    "\u{e1b}\u{e39}1",
                    "\u{e1b}\u{e39}\u{e14}\u{e35}",
                    "\u{e04}\u{e38}\u{e13}\u{e1b}\u{e39}\u{e04}\u{e23}\u{e31}\u{e1a}",
                    "\u{1019}\u{1031}\u{101e}\u{100a}\u{103a}",
                    "\u{e1b}\u{e32} \u{e1b} \u{e1b}\u{e1b}\u{e1b} \u{e1b}\u{e39}\u{e48}"
                ],
                "data": {"\u{e1b}\u{e39}": 1}
            }),
        )];
        let output = scrub_files(files, PARAMS);
        assert_eq!(
            output[0].1["feed"],
            json!([
                "redacted \u{e04}",
                "redacted1",
                "redacted\u{e14}\u{e35}",
                "\u{e04}\u{e38}\u{e13}redacted\u{e04}\u{e23}\u{e31}\u{e1a}",
                "redacted\u{101e}\u{100a}\u{103a}",
                "\u{e1b}\u{e32} \u{e1b} \u{e1b}\u{e1b}\u{e1b} \u{e1b}\u{e39}\u{e48}"
            ])
        );
        assert_eq!(output[0].1["data"], json!({"redacted": 1}));
    }

    #[test]
    fn words_of_an_unspaced_name_are_erased_joined() {
        let files = vec![
            (
                "ride.json".to_owned(),
                json!({
                    "instructor": {"username": "coach_x", "name": "\u{67cf} \u{6c85}"},
                    "description": "\u{4eca}\u{5929}\u{67cf}\u{6c85}\u{8001}\u{5e2b}, \u{6c85}\u{67cf}"
                }),
            ),
            (
                "me.json".to_owned(),
                json!({
                    "user": {"username": "u_x1", "first_name": "\u{6c50} ", "last_name": "\u{3000}\u{82d3}"},
                    "feed": ["\u{82d3}\u{6c50}\u{5148}\u{751f}"]
                }),
            ),
        ];
        let output = scrub_files(files, PARAMS);
        assert_eq!(
            output[0].1["description"],
            "\u{4eca}\u{5929}redacted\u{8001}\u{5e2b}, redacted"
        );
        assert_eq!(output[1].1["feed"], json!(["redacted\u{5148}\u{751f}"]));
    }

    #[test]
    fn nested_json_escapes_are_decoded() {
        let needles = vec![fold_case("\u{3086}\u{3089}"), fold_case("\u{c9}lodie")];
        assert_eq!(
            erase_words(r"\\u3086\\u3089さん, x\nelodie", &needles),
            r"redactedさん, x\nredacted"
        );
    }

    #[test]
    fn object_keys_are_decoded_as_often_as_values() {
        let files = vec![(
            "me.json".to_owned(),
            json!({
                "user": {"username": "u_x1", "name": "\u{3086}\u{3089}"},
                "data": {"%5Cu3086%5Cu3089": 1, "%255Cu3086%255Cu3089": 2}
            }),
        )];
        let output = scrub_files(files, PARAMS);
        let data = output[0].1["data"].as_object().expect("object");
        let mut keys: Vec<&str> = data.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["redacted", "redacted-2"]);
    }

    #[test]
    fn invisible_characters_do_not_count_toward_a_name_length() {
        let files = vec![(
            "ride.json".to_owned(),
            json!({
                "instructor": {"first_name": "A\u{ad}l", "last_name": "Zorvic"},
                "description": "Al rides with Zorvic"
            }),
        )];
        let output = scrub_files(files, PARAMS);
        assert_eq!(output[0].1["description"], "Al rides with redacted");
    }

    #[test]
    fn katakana_iteration_marks_match_hiragana_ones() {
        let needles = vec![fold_case("\u{3086}\u{3089}\u{309d}")];
        assert_eq!(
            erase_words("\u{30e6}\u{30e9}\u{30fd}\u{3055}\u{3093}", &needles),
            "redacted\u{3055}\u{3093}"
        );
    }

    #[test]
    fn unspaced_runs_of_mixed_script_names_are_erased_on_their_own() {
        let files = vec![(
            "me.json".to_owned(),
            json!({
                "user": {"username": "u_x1", "name": "\u{6893}\u{6f6e}Rivo"},
                "feed": ["\u{5c71}\u{6893}\u{6f6e}Rivo\u{3055}\u{3093}", "\u{6893}\u{6f6e}\u{5148}\u{751f}"]
            }),
        )];
        let output = scrub_files(files, PARAMS);
        assert_eq!(
            output[0].1["feed"],
            json!([
                "\u{5c71}redactedredacted\u{3055}\u{3093}",
                "redacted\u{5148}\u{751f}"
            ])
        );
    }

    #[test]
    fn names_in_spaced_uncased_scripts_keep_word_boundaries() {
        let needles = vec![fold_case("\u{930}\u{93e}\u{92e}\u{928}")];
        assert_eq!(
            erase_words(
                "\u{930}\u{93e}\u{92e}\u{928}\u{93e}\u{925}, \u{930}\u{93e}\u{92e}\u{928} \u{91c}\u{940}",
                &needles
            ),
            "\u{930}\u{93e}\u{92e}\u{928}\u{93e}\u{925}, redacted \u{91c}\u{940}"
        );
    }

    #[test]
    fn two_character_names_in_unspaced_scripts_are_erased() {
        let files = vec![(
            "me.json".to_owned(),
            json!({
                "user": {
                    "username": "u_x1",
                    "name": "\u{6893}\u{6f6e} \u{3086}\u{3089}",
                    "first_name": "\u{b2e4}\u{c194}",
                    "last_name": "\u{82b3}"
                },
                "feed": [
                    "\u{6893}\u{6f6e}\u{5148}\u{751f}",
                    "\u{3086}\u{3089}\u{3061}\u{3083}\u{3093}",
                    "\u{b2e4}\u{c194}\u{c528} \u{c548}\u{b155}",
                    "\u{82b3}\u{9999}"
                ]
            }),
        )];
        let output = scrub_files(files, PARAMS);
        assert_eq!(
            output[0].1["feed"],
            json!([
                "redacted\u{5148}\u{751f}",
                "redacted\u{3061}\u{3083}\u{3093}",
                "redacted\u{c528} \u{c548}\u{b155}",
                "\u{82b3}\u{9999}"
            ])
        );
    }

    #[test]
    fn apostrophes_and_underscores_are_boundaries() {
        let needles = vec![fold_case("\u{c9}lodie")];
        assert_eq!(
            erase_words(
                "Elodie's, Elodie\u{2019}s, coach_elodie, elodie.x@y",
                &needles
            ),
            "redacted's, redacted\u{2019}s, coach_redacted, redacted.x@y"
        );
    }

    #[test]
    fn case_changes_inside_product_names_are_accepted_collateral() {
        let needles = vec![fold_case("Phone"), fold_case("Book")];
        assert_eq!(
            erase_words("iPhone, MacBook", &needles),
            "iredacted, Macredacted"
        );
    }

    #[test]
    fn overlapping_matches_are_erased_as_one_span() {
        let needles = vec![
            fold_case("Marie-Claire"),
            fold_case("Marie"),
            fold_case("Claire"),
        ];
        assert_eq!(
            erase_words("Ride with Marie-Claire.", &needles),
            "Ride with redacted."
        );
    }

    #[test]
    fn a_leading_combining_mark_does_not_panic() {
        let needles = vec![fold_case("\u{c9}lodie")];
        assert_eq!(
            erase_words("\u{301}Elodie\u{301}\u{302}", &needles),
            "\u{301}redacted"
        );
    }

    #[test]
    fn short_names_glued_to_digits_are_erased() {
        let needles = vec![fold_case("Ben")];
        assert_eq!(
            erase_words(
                "Ben123, 123ben, ben_2024, benefits, Bento, abc1ben2",
                &needles
            ),
            "redacted123, 123redacted, redacted_2024, benefits, Bento, abc1redacted2"
        );
    }

    #[test]
    fn short_names_are_not_erased_inside_hex_ids() {
        let needles = vec![fold_case("Bed"), fold_case("Ace")];
        assert_eq!(
            erase_words("0123bed456, ace12345, bed1", &needles),
            "0123bed456, ace12345, redacted1"
        );
    }

    #[test]
    fn percent_encoded_names_are_erased() {
        let needles = vec![fold_case("\u{c9}lodie")];
        assert_eq!(
            erase_words(
                "%C3%89lodie, elodie%40example.com, E%CC%81lodie, %c3%a9LODIE x, \
                 %45%4c%4f%44%49%45, u/%C3%89lodie?x=1",
                &needles
            ),
            "redacted, redacted%40example.com, redacted, redacted x, redacted, u/redacted?x=1"
        );
    }

    #[test]
    fn invalid_percent_sequences_are_left_alone() {
        let needles = vec![fold_case("\u{c9}lodie")];
        assert_eq!(
            erase_words("%C3lodie %ZZ %C3%89 100%", &needles),
            "%C3lodie %ZZ %C3%89 100%"
        );
    }

    #[test]
    fn name_length_counts_vowel_signs_and_composed_letters() {
        let files = vec![(
            "me.json".to_owned(),
            json!({
                "user": {
                    "username": "u_x1",
                    "name": "\u{930}\u{93e}\u{92e} \u{938}\u{940}\u{924}\u{93e}",
                    "first_name": "Le\u{301}a"
                },
                "feed": [
                    "\u{930}\u{93e}\u{92e} \u{91c}\u{940}",
                    "Varal%20\u{930}\u{93e}\u{92e}",
                    "%E0%A4%B0%E0%A4%BE%E0%A4%AE",
                    "\u{938}\u{940}\u{924}\u{93e}",
                    "Ride with L\u{e9}a"
                ]
            }),
        )];
        let output = scrub_files(files, PARAMS);
        assert_eq!(
            output[0].1["feed"],
            json!([
                "redacted \u{91c}\u{940}",
                "Varal%20redacted",
                "redacted",
                "redacted",
                "Ride with redacted"
            ])
        );
    }

    #[test]
    fn repeatedly_percent_encoded_names_are_erased() {
        let needles = vec![fold_case("\u{c9}lodie")];
        assert_eq!(
            erase_words("%25C3%2589lodie, %2525C3%252589lodie, x%2540y", &needles),
            "redacted, redacted, x%2540y"
        );
    }

    #[test]
    fn decomposed_names_are_split_into_whole_words() {
        let files = vec![(
            "me.json".to_owned(),
            json!({
                "user": {
                    "username": "u_x1",
                    "name": "E\u{301}lodie Martel",
                    "location": "Saint-E\u{301}tienne, France"
                },
                "feed": ["\u{c9}lodie rode", "Ride in \u{c9}tienne", "Martel"]
            }),
        )];
        let output = scrub_files(files, PARAMS);
        assert_eq!(
            output[0].1["feed"],
            json!(["redacted rode", "Ride in redacted", "redacted"])
        );
    }

    #[test]
    fn devanagari_names_keep_their_vowel_signs_when_split() {
        let files = vec![(
            "me.json".to_owned(),
            json!({
                "user": {"username": "u_x1", "name": "\u{936}\u{94d}\u{930}\u{947}\u{935}\u{928}\u{940} \u{935}\u{930}\u{93e}\u{932}"},
                "feed": [
                    "\u{936}\u{94d}\u{930}\u{947}\u{935}\u{928}\u{940} \u{91c}\u{940}",
                    "\u{935}\u{930}\u{93e}\u{932} \u{91c}\u{940}"
                ]
            }),
        )];
        let output = scrub_files(files, PARAMS);
        assert_eq!(
            output[0].1["feed"],
            json!(["redacted \u{91c}\u{940}", "redacted \u{91c}\u{940}"])
        );
    }

    #[test]
    fn compatibility_forms_match_their_plain_letters() {
        let needles = vec![
            fold_case("\u{c9}lodie"),
            fold_case("D\u{17e}or\u{111}e"),
            fold_case("Fiorel"),
        ];
        assert_eq!(
            erase_words(
                "\u{ff45}\u{ff4c}\u{ff4f}\u{ff44}\u{ff49}\u{ff45}, \u{ff25}\u{ff2c}\u{ff2f}\u{ff24}\u{ff29}\u{ff25}, \
                 \u{24ba}lodie, \u{1c5}or\u{111}e, \u{1c4}OR\u{110}E, \u{fb01}orel",
                &needles
            ),
            "redacted, redacted, redacted, redacted, redacted, redacted"
        );
    }

    #[test]
    fn long_glued_text_is_scrubbed_in_linear_time() {
        let mut needles: Vec<String> = (0..48).map(|i| fold_case(&format!("Zorvane{i}"))).collect();
        needles.push(fold_case("\u{c9}lodie"));
        needles.push(fold_case("\u{65e5}\u{672c}\u{65e5}\u{672c}"));
        needles.push(fold_case("\u{65e5}\u{672c}"));
        let latin = "\u{c9}lodie".repeat(400_000 / "\u{c9}lodie".len());
        let caps = "\u{c9}LODIE".repeat(400_000 / "\u{c9}LODIE".len());
        let cjk = "\u{65e5}\u{672c}".repeat(400_000 / "\u{65e5}\u{672c}".len());
        let escaped = r"\u65e5\u672c".repeat(400_000 / r"\u65e5\u672c".len());
        let joined = "\u{65e5}\u{200d}\u{672c}".repeat(400_000 / "\u{65e5}\u{200d}\u{672c}".len());
        needles.extend(sensitive_form("\u{e1b}\u{e39}"));
        let nested = r"\\u65e5\\u672c".repeat(400_000 / r"\\u65e5\\u672c".len());
        let thai =
            "\u{e1b}\u{e39}\u{e14}\u{e35}".repeat(400_000 / "\u{e1b}\u{e39}\u{e14}\u{e35}".len());
        for text in [latin, caps, cjk, escaped, joined, nested, thai] {
            let started = std::time::Instant::now();
            erase_words(&text, &needles);
            let elapsed = started.elapsed();
            assert!(
                elapsed < std::time::Duration::from_secs(5),
                "took {elapsed:?} for {} bytes",
                text.len()
            );
        }
    }

    /// Text, names and expected output of probes from the scrubber review.
    const REVIEW_PROBES: &[(&str, &[&str], &str)] = &[
        (
            "STRASSE strasse Stra\u{df}e",
            &["Stra\u{df}e"],
            "redacted redacted redacted",
        ),
        (
            "\u{39f}\u{394}\u{3a5}\u{3a3}\u{3a3}\u{395}\u{391}\u{3a3} \u{3bf}\u{3b4}\u{3c5}\u{3c3}\u{3c3}\u{3b5}\u{3b1}\u{3c3}",
            &["\u{39f}\u{3b4}\u{3c5}\u{3c3}\u{3c3}\u{3b5}\u{3b1}\u{3c2}"],
            "redacted redacted",
        ),
        (
            "istanbul ISTANBUL",
            &["\u{130}stanbul"],
            "redacted redacted",
        ),
        (
            "Ben123 BEN\u{65e5}\u{672c} Ben\u{3055}\u{3093}",
            &["Ben"],
            "redacted123 BEN\u{65e5}\u{672c} Ben\u{3055}\u{3093}",
        ),
        ("Jo jo", &["Jo"], "Jo jo"),
        (
            "0123facedade89 face89 deadbeef12 Elodie2024 2024ELODIE",
            &["face", "dead", "beef", "Elodie"],
            "0123facedade89 redacted89 deadbeef12 redacted2024 2024redacted",
        ),
        (
            "MarieClaire elodieMartin LoveKing",
            &["Marie", "Claire", "Martin", "Elodie", "Love", "King"],
            "redactedredacted redactedredacted redactedredacted",
        ),
        (
            "JEANLUC MARIECLAIRE KINGDOM",
            &["Jean", "Luc", "Marie", "King"],
            "redactedLUC redactedCLAIRE redactedDOM",
        ),
        (
            "ELODIEmartin XELODIEmartin Elodiemartin",
            &["Elodie"],
            "redactedmartin XELODIEmartin Elodiemartin",
        ),
        (
            "O'Brien O\u{2019}Brien d'Elodie l\u{2019}Elodie",
            &["O'Brien", "Brien", "Elodie"],
            "redacted O\u{2019}redacted d'redacted l\u{2019}redacted",
        ),
        (
            "mail elodie.martin@example.com x",
            &["elodie.martin@example.com"],
            "mail redacted x",
        ),
        (
            "ELODIE.MARTIN@EXAMPLE.COM",
            &["elodie.martin@example.com"],
            "redacted",
        ),
        (
            "www.example.com/u/elodie?name=Elodie&x=1",
            &["Elodie"],
            "www.example.com/u/redacted?name=redacted&x=1",
        ),
        (
            "elodieelodie ElodieElodie ELODIEELODIE",
            &["Elodie"],
            "elodieelodie redactedredacted redactedredacted",
        ),
        (
            "Elodie\u{301}\u{302}x Elodie\u{301} y",
            &["Elodie"],
            "Elodie\u{301}\u{302}x redacted y",
        ),
        ("e\u{301}", &["\u{c9}"], "e\u{301}"),
        ("  ", &["", "   ", " "], "  "),
        (
            "Saint \u{c9}tienne Saint-E\u{301}tienne",
            &["Saint-E\u{301}tienne"],
            "Saint \u{c9}tienne redacted",
        ),
        (
            "Annabelle Anna",
            &["Anna", "Annabelle"],
            "redacted redacted",
        ),
        ("cafe1234Elodie", &["Elodie"], "cafe1234redacted"),
        (
            "Elodie\u{b2} Elodie\u{bd} \u{216b}Elodie",
            &["Elodie"],
            "redacted\u{b2} redacted\u{bd} \u{216b}redacted",
        ),
        (
            "ELODIE_MARTIN coach_elodie",
            &["Elodie"],
            "redacted_MARTIN coach_redacted",
        ),
    ];

    #[test]
    fn review_probes_scrub_as_specified() {
        for (text, names, expected) in REVIEW_PROBES {
            let mut needles: Vec<String> = names
                .iter()
                .filter_map(|name| sensitive_form(name))
                .collect();
            needles.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
            needles.dedup();
            assert_eq!(erase_words(text, &needles), *expected, "{text:?} {names:?}");
        }
    }

    #[test]
    fn keys_that_fold_to_the_same_name_stay_distinct() {
        let files = vec![(
            "me.json".to_owned(),
            json!({
                "user": {"username": "maxpower", "first_name": "Max", "last_name": "Zone"},
                "data": {"Max": 3, "MAX": 4, "max_value": 1, "redacted": 5}
            }),
        )];
        let output = scrub_files(files, PARAMS);
        let data = output[0].1["data"].as_object().expect("object");
        let mut values: Vec<i64> = data
            .values()
            .filter_map(serde_json::Value::as_i64)
            .collect();
        values.sort_unstable();
        assert_eq!(values, [1, 3, 4, 5]);
        assert_eq!(data["max_value"], 1);
        assert_eq!(data["redacted"], 5);
    }

    #[test]
    fn names_of_spaced_scripts_shorter_than_three_characters_are_never_erased() {
        let files = vec![(
            "ride.json".to_owned(),
            json!({
                "instructor": {"first_name": "Al", "last_name": "Zorvic"},
                "description": "Al rides with Zorvic"
            }),
        )];
        let output = scrub_files(files, PARAMS);
        assert_eq!(output[0].1["description"], "Al rides with redacted");
    }

    #[test]
    fn object_keys_are_erased_only_when_they_are_the_whole_name() {
        let files = vec![(
            "ride.json".to_owned(),
            json!({
                "instructor": {"first_name": "Max", "last_name": "Zone"},
                "metrics": {
                    "max_value": 1,
                    "power_zone": 2,
                    "default_max_heart_rate": 0,
                    "MAX": 3,
                    "note": "Max_power"
                }
            }),
        )];
        let output = scrub_files(files, PARAMS);
        let metrics = output[0].1["metrics"].as_object().expect("object");
        let mut keys: Vec<&str> = metrics.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "default_max_heart_rate",
                "max_value",
                "note",
                "power_zone",
                "redacted"
            ]
        );
        assert_eq!(metrics["note"], "redacted_power");
    }

    /// `text` with the letters NFD does not split replaced by their plain
    /// spelling, keeping case.
    fn spelled_out(text: &str) -> String {
        text.chars()
            .map(|c| match c {
                '\u{141}' => "L".to_owned(),
                '\u{142}' => "l".to_owned(),
                '\u{d8}' => "O".to_owned(),
                '\u{f8}' => "o".to_owned(),
                '\u{110}' | '\u{d0}' => "D".to_owned(),
                '\u{111}' | '\u{f0}' => "d".to_owned(),
                '\u{126}' => "H".to_owned(),
                '\u{127}' => "h".to_owned(),
                '\u{166}' => "T".to_owned(),
                '\u{167}' => "t".to_owned(),
                '\u{de}' => "TH".to_owned(),
                '\u{fe}' => "th".to_owned(),
                '\u{152}' => "OE".to_owned(),
                '\u{153}' => "oe".to_owned(),
                '\u{c6}' => "AE".to_owned(),
                '\u{e6}' => "ae".to_owned(),
                other => other.to_string(),
            })
            .collect()
    }

    /// `text` percent-encoded: every byte when `all`, else only the bytes of
    /// non-ASCII characters.
    fn percent_encode(text: &str, all: bool) -> String {
        let mut out = String::new();
        for c in text.chars() {
            if all || !c.is_ascii() {
                let mut buffer = [0; 4];
                for byte in c.encode_utf8(&mut buffer).bytes() {
                    write!(out, "%{byte:02X}").expect("write to a String");
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// `text` with every non-ASCII character as a JSON `\uXXXX` escape (a
    /// surrogate pair outside the Basic Multilingual Plane).
    fn json_escape(text: &str) -> String {
        let mut out = String::new();
        for c in text.chars() {
            if c.is_ascii() {
                out.push(c);
            } else {
                for unit in c.encode_utf16(&mut [0; 2]) {
                    write!(out, "\\u{unit:04x}").expect("write to a String");
                }
            }
        }
        out
    }

    /// The other kana of a hiragana or katakana letter, or `c` itself.
    fn swap_kana(c: char) -> char {
        let code = u32::from(c);
        let swapped = match c {
            '\u{3041}'..='\u{3096}' => code + 0x60,
            '\u{30a1}'..='\u{30f6}' => code - 0x60,
            _ => code,
        };
        char::from_u32(swapped).expect("kana have twins")
    }

    /// `text` with every `%XX` decoded, invalid UTF-8 replaced.
    fn percent_decode(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let hex = bytes
                .get(at + 1..at + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            match (bytes[at], hex) {
                (b'%', Some(byte)) => {
                    out.push(byte);
                    at += 3;
                }
                (byte, _) => {
                    out.push(byte);
                    at += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    proptest! {
        #[test]
        fn scrubbed_text_never_contains_the_name_in_any_spelling(
            name in "[a-zA-Z\u{e0}\u{e2}\u{e7}\u{e9}\u{e8}\u{ea}\u{eb}\u{ee}\u{ef}\u{f4}\u{f6}\u{fb}\u{fc}\u{f1}\u{df}\u{1e9e}\u{153}\u{152}\u{130}\u{131}\u{141}\u{142}\u{d8}\u{f8}\u{110}\u{111}\u{d0}\u{f0}\u{126}\u{127}\u{166}\u{167}\u{de}\u{fe}\u{c6}\u{e6}]{4,12}",
            before in "[ .,:;!_@'\u{2019}0-9\u{65e5}\u{672c}\u{3055}-]{0,3}",
            after in "[ .,:;!_@'\u{2019}0-9\u{65e5}\u{672c}\u{3093}-]{0,3}",
        ) {
            let folded = fold_case(&name);
            prop_assume!(!"redacted".contains(&folded));
            // A name spelled with hex letters only, glued to a digit, may make
            // a word that looks like a hex id, which is kept on purpose.
            let hex_letters = folded.chars().all(|c| c.is_ascii_hexdigit());
            let glued_to_digit = before.ends_with(|c: char| c.is_ascii_digit())
                || after.starts_with(|c: char| c.is_ascii_digit());
            prop_assume!(!(hex_letters && glued_to_digit));
            let needles = vec![folded.clone()];
            let forms = [name.to_lowercase(), name.to_uppercase(), name.clone()];
            let decomposed = forms.iter().map(|v| v.nfd().collect::<String>());
            let stripped: Vec<String> = forms
                .iter()
                .map(|v| v.nfd().filter(|c| !is_combining_mark(*c)).collect())
                .collect();
            let plain: Vec<String> = stripped.iter().map(|v| spelled_out(v)).collect();
            let spellings: Vec<String> = forms
                .iter()
                .cloned()
                .chain(decomposed)
                .chain(stripped)
                .chain(plain)
                .collect();
            let encoded = spellings
                .iter()
                .flat_map(|v| [percent_encode(v, true), percent_encode(v, false)]);
            let variants: Vec<String> = spellings.iter().cloned().chain(encoded).collect();
            for variant in variants {
                let text = format!("{before}{variant}{after} {variant}");
                let scrubbed = erase_words(&text, &needles);
                prop_assert!(
                    !fold_case(&scrubbed).contains(&folded)
                        && !fold_case(&percent_decode(&scrubbed)).contains(&folded),
                    "{scrubbed:?} still contains {name:?}"
                );
            }
        }

        #[test]
        fn scrubbed_text_never_contains_a_name_of_an_unspaced_script(
            name in "[\u{3041}-\u{3096}\u{30a1}-\u{30fa}\u{30fc}\u{3105}-\u{312f}\u{4e00}-\u{4e80}\
                      \u{20000}-\u{20100}\u{ac00}-\u{ac80}\u{e01}-\u{e3a}\u{e40}-\u{e4e}\u{e81}-\u{e82}\
                      \u{e84}\u{e86}-\u{e8a}\u{e8c}-\u{ea3}\u{1780}-\u{17b3}\u{1000}-\u{102a}\
                      \u{f40}-\u{f47}\u{f49}-\u{f6c}\u{a000}-\u{a48c}\u{a984}-\u{a9b2}\u{1b05}-\u{1b33}\
                      \u{1b83}-\u{1ba0}\u{1a00}-\u{1a16}\u{1a20}-\u{1a54}\u{1950}-\u{196d}\
                      \u{1980}-\u{19ab}\u{aa80}-\u{aaaf}]{2,6}",
            before in "[ ,a1\u{3001}\u{3055}\u{3093}\u{69d8}\u{8001}\u{5e2b}\u{c528}\u{b2d8}\u{c740}\u{e04}\u{200b}]{0,3}",
            after in "[ ,a1\u{3001}\u{3055}\u{3093}\u{69d8}\u{8001}\u{5e2b}\u{c528}\u{b2d8}\u{c740}\u{e04}\u{200b}]{0,3}",
        ) {
            // A name of marks only is never erased, and one that folds to a
            // single letter is matched with its marks (tested on its own).
            let folded = sensitive_form(&name).unwrap_or_default();
            prop_assume!(NeedleKind::of(&folded) == NeedleKind::Unspaced);
            let needles = vec![folded.clone()];
            let joined: String = name.chars().flat_map(|c| [c, '\u{200d}']).collect();
            let other_kana: String = name.chars().map(swap_kana).collect();
            let variants = [
                name.clone(),
                name.nfd().collect(),
                percent_encode(&name, false),
                json_escape(&name),
                joined,
                other_kana,
            ];
            for variant in variants {
                let text = format!("{before}{variant}{after}{variant}");
                let scrubbed = erase_words(&text, &needles);
                let unescaped: String = serde_json::from_str(&format!("\"{scrubbed}\""))
                    .expect("scrubbed text is a JSON string body");
                prop_assert!(
                    !scrubbed.contains(&variant)
                        && !fold_case(&scrubbed).contains(&folded)
                        && !fold_case(&unescaped).contains(&folded),
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

    #[test]
    fn refuses_to_scrub_a_directory_into_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = run(dir.path(), dir.path()).expect_err("same directory");
        assert!(matches!(error, ScrubError::SameDirectory));
        assert_eq!(
            report(&error),
            "output directory must differ from the input directory"
        );
    }

    #[test]
    fn missing_input_directory_names_it_then_the_io_cause() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("absent");
        let error = run(&missing, &dir.path().join("out")).expect_err("no input");
        assert!(matches!(error, ScrubError::InputDir { .. }));
        assert!(
            report(&error).starts_with(&format!("bad input directory {}: ", missing.display()))
        );
    }

    #[test]
    fn unparseable_input_names_the_file_and_keeps_the_json_cause() {
        let dir = tempfile::tempdir().expect("tempdir");
        let input = dir.path().join("in");
        std::fs::create_dir(&input).expect("input");
        std::fs::write(input.join("a.json"), b"{").expect("file");
        let error = run(&input, &dir.path().join("out")).expect_err("bad json");
        assert!(matches!(error, ScrubError::Parse { .. }));
        assert!(report(&error).starts_with("could not parse a.json: "));
        assert!(std::error::Error::source(&error).is_some());
    }
}
