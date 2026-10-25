//! Peloton API responses, reduced to the fields the relay maps.
//!
//! Unknown fields are ignored. Anything Peloton omits for some disciplines is
//! an `Option` or an empty `Vec` (missing and `null` lists both decode as
//! empty). Only ids are required, so one odd list item never fails a whole
//! response. Epoch-second timestamps decode to UTC [`OffsetDateTime`]s, so a
//! timestamp out of range fails decoding instead of mapping.

use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use time::OffsetDateTime;

use crate::ids::PelotonId;

/// Decodes a list that may be missing or `null` as a possibly empty `Vec`.
fn list<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Decodes an object with an `id` as `None` when it is `null` or its id is
/// missing, `null`, empty or all zeros (Peloton's "no class" / "no
/// instructor" markers); any other id must be a valid [`PelotonId`].
fn identified<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let Some(value) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let absent = match value.get("id") {
        None | Some(Value::Null) => true,
        Some(Value::String(id)) => id.is_empty() || id.bytes().all(|b| b == b'0'),
        Some(_) => false,
    };
    if absent {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(D::Error::custom)
}

/// `GET /me`: the signed-in member.
#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    /// Member id.
    pub id: PelotonId,
    /// Public username.
    #[serde(default)]
    pub username: Option<String>,
    /// Full display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Account creation time.
    #[serde(default, with = "time::serde::timestamp::option")]
    pub created_at: Option<OffsetDateTime>,
    /// Number of workouts ever recorded.
    #[serde(default)]
    pub total_workouts: Option<u32>,
    /// Functional threshold power in watts; 0 when never set.
    #[serde(default)]
    pub cycling_ftp: Option<u32>,
    /// Member-set maximum heart rate in bpm; 0 when not set.
    #[serde(default)]
    pub customized_max_heart_rate: Option<u32>,
    /// Age-derived maximum heart rate in bpm; 0 when unknown.
    #[serde(default)]
    pub default_max_heart_rate: Option<u32>,
    /// Workout counts per discipline.
    #[serde(default, deserialize_with = "list")]
    pub workout_counts: Vec<WorkoutCount>,
}

/// One entry of [`Me::workout_counts`].
#[derive(Debug, Clone, Deserialize)]
pub struct WorkoutCount {
    /// Display name, e.g. "Cycling".
    #[serde(default)]
    pub name: Option<String>,
    /// Discipline slug, e.g. "cycling".
    #[serde(default)]
    pub slug: Option<String>,
    /// Number of workouts.
    #[serde(default)]
    pub count: Option<u32>,
}

/// `GET /user/{id}/workouts`: one page of workouts, newest first.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkoutPage {
    /// Workouts on this page.
    #[serde(deserialize_with = "list")]
    pub data: Vec<Workout>,
    /// Zero-based page index.
    pub page: u32,
    /// Number of pages.
    pub page_count: u32,
    /// Number of workouts across all pages.
    pub total: u32,
}

/// `GET /workout/{id}` (also the items of [`WorkoutPage`]).
#[derive(Debug, Clone, Deserialize)]
pub struct Workout {
    /// Workout id.
    pub id: PelotonId,
    /// Discipline slug: "cycling", "yoga", "stretching", ….
    #[serde(default)]
    pub fitness_discipline: Option<String>,
    /// "class", "scenic", "freestyle", ….
    #[serde(default)]
    pub workout_type: Option<String>,
    /// "COMPLETE", "`IN_PROGRESS`", ….
    #[serde(default)]
    pub status: Option<String>,
    /// When the workout was created.
    #[serde(default, with = "time::serde::timestamp::option")]
    pub created_at: Option<OffsetDateTime>,
    /// When the member started.
    #[serde(default, with = "time::serde::timestamp::option")]
    pub start_time: Option<OffsetDateTime>,
    /// When the member finished.
    #[serde(default, with = "time::serde::timestamp::option")]
    pub end_time: Option<OffsetDateTime>,
    /// Device the workout was recorded on, e.g. "`home_bike_plus`".
    #[serde(default)]
    pub device_type: Option<String>,
    /// Whether output, cadence and resistance were recorded.
    #[serde(default)]
    pub has_pedaling_metrics: Option<bool>,
    /// Total output in joules (0 without pedaling metrics).
    #[serde(default)]
    pub total_work: Option<f64>,
    /// Whether the total output is a personal record.
    #[serde(default)]
    pub is_total_work_personal_record: Option<bool>,
    /// Leaderboard rank, when the class had a leaderboard.
    #[serde(default)]
    pub leaderboard_rank: Option<u32>,
    /// Number of members on that leaderboard.
    #[serde(default)]
    pub total_leaderboard_users: Option<u32>,
    /// FTP in effect during the workout.
    #[serde(default)]
    pub ftp_info: Option<FtpInfo>,
    /// Heart-rate zone durations (workout list shape).
    #[serde(default)]
    pub effort_zones: Option<EffortZones>,
    /// Heart-rate zone durations (single workout shape).
    #[serde(default)]
    pub total_heart_rate_zone_durations: Option<HeartRateZoneDurations>,
    /// Badges earned with this workout.
    #[serde(default, deserialize_with = "list")]
    pub achievement_templates: Vec<Achievement>,
    /// The class taken; `None` for a Just Ride or any workout without a class.
    #[serde(default, deserialize_with = "identified")]
    pub ride: Option<Ride>,
}

/// [`Workout::ftp_info`].
#[derive(Debug, Clone, Deserialize)]
pub struct FtpInfo {
    /// FTP in watts; 0 when none was set.
    #[serde(default)]
    pub ftp: Option<u32>,
}

/// [`Workout::effort_zones`].
#[derive(Debug, Clone, Deserialize)]
pub struct EffortZones {
    /// Time spent per heart-rate zone.
    #[serde(default)]
    pub heart_rate_zone_durations: Option<HeartRateZoneDurations>,
}

/// Seconds spent in each of Peloton's five heart-rate zones.
#[derive(Debug, Clone, Deserialize)]
pub struct HeartRateZoneDurations {
    /// Zone 1, seconds.
    #[serde(default)]
    pub heart_rate_z1_duration: Option<u32>,
    /// Zone 2, seconds.
    #[serde(default)]
    pub heart_rate_z2_duration: Option<u32>,
    /// Zone 3, seconds.
    #[serde(default)]
    pub heart_rate_z3_duration: Option<u32>,
    /// Zone 4, seconds.
    #[serde(default)]
    pub heart_rate_z4_duration: Option<u32>,
    /// Zone 5, seconds.
    #[serde(default)]
    pub heart_rate_z5_duration: Option<u32>,
}

/// One of [`Workout::achievement_templates`].
#[derive(Debug, Clone, Deserialize)]
pub struct Achievement {
    /// Badge name.
    #[serde(default)]
    pub name: Option<String>,
}

/// A class ("ride" in Peloton's API, whatever the discipline).
#[derive(Debug, Clone, Deserialize)]
pub struct Ride {
    /// Class id.
    pub id: PelotonId,
    /// Class title.
    #[serde(default)]
    pub title: Option<String>,
    /// Class description.
    #[serde(default)]
    pub description: Option<String>,
    /// Discipline slug.
    #[serde(default)]
    pub fitness_discipline: Option<String>,
    /// Planned length in seconds.
    #[serde(default)]
    pub duration: Option<u32>,
    /// Estimated difficulty on Peloton's 0–10 scale.
    #[serde(default)]
    pub difficulty_estimate: Option<f64>,
    /// When the class first aired.
    #[serde(default, with = "time::serde::timestamp::option")]
    pub original_air_time: Option<OffsetDateTime>,
    /// The instructor (joined with `joins=ride.instructor`); `None` when the
    /// class has none.
    #[serde(default, deserialize_with = "identified")]
    pub instructor: Option<Instructor>,
}

/// A Peloton instructor.
#[derive(Debug, Clone, Deserialize)]
pub struct Instructor {
    /// Instructor id.
    pub id: PelotonId,
    /// Display name.
    #[serde(default)]
    pub name: Option<String>,
}

/// `GET /workout/{id}/performance_graph`.
#[derive(Debug, Clone, Deserialize)]
pub struct PerformanceGraph {
    /// Workout length in seconds.
    #[serde(default)]
    pub duration: Option<u32>,
    /// Offset in seconds of each sample of every metric series.
    #[serde(default, deserialize_with = "list")]
    pub seconds_since_pedaling_start: Vec<u32>,
    /// Time series (output, cadence, resistance, speed, heart rate, …).
    #[serde(default, deserialize_with = "list")]
    pub metrics: Vec<GraphMetric>,
    /// Totals (output, distance, calories, …).
    #[serde(default, deserialize_with = "list")]
    pub summaries: Vec<GraphSummary>,
    /// Averages (output, cadence, resistance, speed, …).
    #[serde(default, deserialize_with = "list")]
    pub average_summaries: Vec<GraphSummary>,
}

/// One series of [`PerformanceGraph::metrics`].
#[derive(Debug, Clone, Deserialize)]
pub struct GraphMetric {
    /// Metric slug, e.g. "output".
    #[serde(default)]
    pub slug: Option<String>,
    /// Display name, e.g. "Output".
    #[serde(default)]
    pub display_name: Option<String>,
    /// Unit, e.g. "watts".
    #[serde(default)]
    pub display_unit: Option<String>,
    /// Average over the workout.
    #[serde(default)]
    pub average_value: Option<f64>,
    /// Maximum over the workout.
    #[serde(default)]
    pub max_value: Option<f64>,
    /// Samples, aligned with [`PerformanceGraph::seconds_since_pedaling_start`].
    #[serde(default, deserialize_with = "list")]
    pub values: Vec<Option<f64>>,
}

/// One of [`PerformanceGraph::summaries`] or [`PerformanceGraph::average_summaries`].
#[derive(Debug, Clone, Deserialize)]
pub struct GraphSummary {
    /// Summary slug, e.g. "`total_output`".
    #[serde(default)]
    pub slug: Option<String>,
    /// Display name, e.g. "Total Output".
    #[serde(default)]
    pub display_name: Option<String>,
    /// Unit, e.g. "kj".
    #[serde(default)]
    pub display_unit: Option<String>,
    /// Value in that unit.
    #[serde(default)]
    pub value: Option<f64>,
}

/// `GET /ride/{id}/details`.
#[derive(Debug, Clone, Deserialize)]
pub struct RideDetails {
    /// The class.
    pub ride: Ride,
    /// Class types, e.g. "Low Impact".
    #[serde(default, deserialize_with = "list")]
    pub class_types: Vec<ClassType>,
    /// Whether this is a power zone class.
    #[serde(default)]
    pub is_power_zone_class: Option<bool>,
    /// Whether this is an FTP test.
    #[serde(default)]
    pub is_ftp_test: Option<bool>,
    /// Class structure.
    #[serde(default)]
    pub segments: Option<Segments>,
    /// Music, when the class has a playlist.
    #[serde(default)]
    pub playlist: Option<Playlist>,
}

/// One of [`RideDetails::class_types`].
#[derive(Debug, Clone, Deserialize)]
pub struct ClassType {
    /// Class type name.
    #[serde(default)]
    pub name: Option<String>,
}

/// [`RideDetails::segments`].
#[derive(Debug, Clone, Deserialize)]
pub struct Segments {
    /// Segments in class order.
    #[serde(default, deserialize_with = "list")]
    pub segment_list: Vec<Segment>,
}

/// One segment of a class.
#[derive(Debug, Clone, Deserialize)]
pub struct Segment {
    /// Segment name, e.g. "Warmup".
    #[serde(default)]
    pub name: Option<String>,
    /// Start, seconds from the class start.
    #[serde(default)]
    pub start_time_offset: Option<u32>,
    /// Length in seconds.
    #[serde(default)]
    pub length: Option<u32>,
    /// Intensity in METs.
    #[serde(default)]
    pub intensity_in_mets: Option<f64>,
}

/// [`RideDetails::playlist`].
#[derive(Debug, Clone, Deserialize)]
pub struct Playlist {
    /// Songs in play order.
    #[serde(default, deserialize_with = "list")]
    pub songs: Vec<Song>,
}

/// One song of a [`Playlist`].
#[derive(Debug, Clone, Deserialize)]
pub struct Song {
    /// Song title.
    #[serde(default)]
    pub title: Option<String>,
    /// Performing artists.
    #[serde(default, deserialize_with = "list")]
    pub artists: Vec<Artist>,
    /// Album.
    #[serde(default)]
    pub album: Option<Album>,
    /// Start, seconds from the class start.
    #[serde(default)]
    pub start_time_offset: Option<u32>,
}

/// One of [`Song::artists`].
#[derive(Debug, Clone, Deserialize)]
pub struct Artist {
    /// Artist name.
    #[serde(default)]
    pub artist_name: Option<String>,
}

/// [`Song::album`].
#[derive(Debug, Clone, Deserialize)]
pub struct Album {
    /// Album name.
    #[serde(default)]
    pub name: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::Workout;

    fn workout(ride: &serde_json::Value) -> Result<Workout, serde_json::Error> {
        serde_json::from_value(json!({
            "id": "0123456789abcdef0123456789abcdef",
            "fitness_discipline": "cycling",
            "ride": ride,
        }))
    }

    #[test]
    fn nil_ride_and_instructor_ids_mean_absent() {
        let nil = "00000000000000000000000000000000";
        assert!(workout(&json!({"id": nil})).expect("parse").ride.is_none());
        let ride = workout(&json!({
            "id": "fedcba9876543210fedcba9876543210",
            "instructor": {"id": nil, "name": "x"},
        }))
        .expect("parse")
        .ride
        .expect("ride");
        assert!(ride.instructor.is_none());
    }

    #[test]
    fn rejects_malformed_ids() {
        assert!(workout(&json!({"id": "../../me"})).is_err());
        assert!(workout(&json!({"id": 7})).is_err());
    }

    #[test]
    fn rejects_out_of_range_timestamps() {
        let parsed: Result<Workout, _> = serde_json::from_value(json!({
            "id": "0123456789abcdef0123456789abcdef",
            "fitness_discipline": "cycling",
            "start_time": i64::MAX,
        }));
        assert!(parsed.is_err());
    }
}
