//! Peloton API responses, reduced to the fields the relay maps.
//!
//! Unknown fields are ignored. Anything Peloton omits for some disciplines is
//! an `Option` or an empty `Vec` (missing and `null` lists both decode as
//! empty). Only ids are required, so one odd list item never fails a whole
//! response. Epoch-second timestamps decode to UTC [`OffsetDateTime`]s, so a
//! timestamp out of range fails decoding instead of mapping; GraphQL's
//! RFC 3339 timestamps decode the same way.
//!
//! GraphQL unions are dispatched on `__typename`: a member the relay does not
//! know fails decoding, except among schedule items, where Peloton's other
//! kinds (invites) are kept as [`ScheduleItem::Other`]. Lists a write is built
//! from (the Stack) are required, so a missing list never reads as empty.

use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use time::OffsetDateTime;

use crate::ids::{PelotonId, ScheduledClassId};

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

/// Decodes an optional id leniently: a missing, `null` or malformed value is
/// `None`, so an odd value never fails a whole response. Only suitable where
/// `None` is the safe reading, such as an ownership check.
fn lenient_id<'de, D>(deserializer: D) -> Result<Option<PelotonId>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match Option::<Value>::deserialize(deserializer)? {
        Some(Value::String(id)) => id.parse().ok(),
        _ => None,
    })
}

/// Decodes a string with [`FromStr`](std::str::FromStr), for ids that are
/// validated on parse.
fn parsed<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: std::str::FromStr<Err: std::fmt::Display>,
{
    String::deserialize(deserializer)?
        .parse()
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
    /// The member who did the workout; `None` when missing or malformed.
    #[serde(default, deserialize_with = "lenient_id")]
    pub user_id: Option<PelotonId>,
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

/// `GET /v2/ride/archived`: one page of the class library.
#[derive(Debug, Clone, Deserialize)]
pub struct LibraryPage {
    /// Classes on this page.
    #[serde(deserialize_with = "list")]
    pub data: Vec<LibraryClass>,
    /// Zero-based page index.
    pub page: u32,
    /// Number of pages.
    pub page_count: u32,
    /// Number of classes across all pages.
    pub total: u32,
    /// The instructors of this page's classes, joined by
    /// [`LibraryClass::instructor_id`].
    #[serde(default, deserialize_with = "list")]
    pub instructors: Vec<Instructor>,
    /// Class types, joined by [`LibraryClass::class_type_ids`].
    #[serde(default, deserialize_with = "list")]
    pub class_types: Vec<LibraryClassType>,
}

/// One class of a [`LibraryPage`].
#[derive(Debug, Clone, Deserialize)]
pub struct LibraryClass {
    /// Class id.
    pub id: PelotonId,
    /// Class title.
    #[serde(default)]
    pub title: Option<String>,
    /// Class description.
    #[serde(default)]
    pub description: Option<String>,
    /// Discipline slug, e.g. "cycling".
    #[serde(default)]
    pub fitness_discipline: Option<String>,
    /// Discipline display name, e.g. "Cycling".
    #[serde(default)]
    pub fitness_discipline_display_name: Option<String>,
    /// Planned length in seconds.
    #[serde(default)]
    pub duration: Option<u32>,
    /// Estimated difficulty on Peloton's 0–10 scale.
    #[serde(default)]
    pub difficulty_estimate: Option<f64>,
    /// Difficulty level label, when Peloton sets one.
    #[serde(default)]
    pub difficulty_level: Option<String>,
    /// When the class first aired.
    #[serde(default, with = "time::serde::timestamp::option")]
    pub original_air_time: Option<OffsetDateTime>,
    /// The instructor; `None` when missing or malformed (the class is still
    /// listed, without an instructor name).
    #[serde(default, deserialize_with = "lenient_id")]
    pub instructor_id: Option<PelotonId>,
    /// Class types.
    #[serde(default, deserialize_with = "list")]
    pub class_type_ids: Vec<PelotonId>,
    /// How many times the member took the class.
    #[serde(default)]
    pub total_user_workouts: Option<u32>,
}

/// One of [`LibraryPage::class_types`].
#[derive(Debug, Clone, Deserialize)]
pub struct LibraryClassType {
    /// Class type id.
    pub id: PelotonId,
    /// Class type name, e.g. "Low Impact".
    #[serde(default)]
    pub name: Option<String>,
    /// Display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Discipline slug.
    #[serde(default)]
    pub fitness_discipline: Option<String>,
}

/// `GET /ride/filters`: the library filters and sorts of a category.
#[derive(Debug, Clone, Deserialize)]
pub struct RideFilters {
    /// Filters, each with its current values.
    #[serde(default, deserialize_with = "list")]
    pub filters: Vec<RideFilter>,
    /// Sort orders.
    #[serde(default, deserialize_with = "list")]
    pub sorts: Vec<RideSort>,
}

/// One of [`RideFilters::filters`].
#[derive(Debug, Clone, Deserialize)]
pub struct RideFilter {
    /// Query parameter name, e.g. "`instructor_id`".
    #[serde(default)]
    pub name: Option<String>,
    /// Display name, e.g. "Instructor".
    #[serde(default)]
    pub display_name: Option<String>,
    /// "collection" (pick a value) or "toggle".
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// Current values.
    #[serde(default, deserialize_with = "list")]
    pub values: Vec<FilterValue>,
}

/// One of [`RideFilter::values`].
#[derive(Debug, Clone, Deserialize)]
pub struct FilterValue {
    /// The query parameter value, e.g. an id or "300" (seconds).
    #[serde(default)]
    pub value: Option<String>,
    /// Display name, e.g. "5 min".
    #[serde(default)]
    pub display_name: Option<String>,
    /// Position in Peloton's own listing.
    #[serde(default)]
    pub list_order: Option<u32>,
}

/// One of [`RideFilters::sorts`].
#[derive(Debug, Clone, Deserialize)]
pub struct RideSort {
    /// Sort slug, e.g. "popular".
    #[serde(default)]
    pub slug: Option<String>,
    /// Display name, e.g. "Popular".
    #[serde(default)]
    pub display_name: Option<String>,
    /// The query parameters it stands for.
    #[serde(default)]
    pub value: Option<SortValue>,
}

/// [`RideSort::value`]: `sort_by` and `desc` as sent to the library.
#[derive(Debug, Clone, Deserialize)]
pub struct SortValue {
    /// `sort_by` value, e.g. "`original_air_time`".
    #[serde(default)]
    pub sort: Option<String>,
    /// Whether the order is descending.
    #[serde(default)]
    pub desc: Option<bool>,
}

/// `GET /browse_categories`.
#[derive(Debug, Clone, Deserialize)]
pub struct BrowseCategories {
    /// Library categories.
    #[serde(default, deserialize_with = "list")]
    pub browse_categories: Vec<BrowseCategory>,
}

/// One library category, e.g. cycling or yoga.
#[derive(Debug, Clone, Deserialize)]
pub struct BrowseCategory {
    /// Category slug, the `browse_category` library parameter.
    #[serde(default)]
    pub slug: Option<String>,
    /// Display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Position in Peloton's own listing.
    #[serde(default)]
    pub list_order: Option<u32>,
    /// Whether some of its classes are limited.
    #[serde(default)]
    pub has_limited_rides: Option<bool>,
}

/// GraphQL `userScheduledItemsList`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "__typename")]
pub enum ScheduleList {
    /// The member's scheduled items.
    YourScheduleItemList(ScheduleItems),
}

/// [`ScheduleList::YourScheduleItemList`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleItems {
    /// Items in Peloton's order.
    pub your_schedule_items: Vec<ScheduleItem>,
}

/// One of [`ScheduleItems::your_schedule_items`].
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "__typename")]
pub enum ScheduleItem {
    /// A class the member scheduled.
    ScheduledClass(Box<ScheduledClass>),
    /// Any other kind of item, such as an invite.
    #[serde(other)]
    Other,
}

/// GraphQL `scheduledClass` and the schedule mutations.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "__typename")]
pub enum ScheduleResult {
    /// The scheduled class.
    ScheduledClass(Box<ScheduledClass>),
    /// No such scheduled class.
    ScheduledClassDoesNotExistError(ScheduleError),
    /// Peloton refused to schedule the class.
    CannotScheduleClassError(ScheduleError),
}

/// An error member of [`ScheduleResult`].
#[derive(Debug, Clone, Deserialize)]
pub struct ScheduleError {
    /// Peloton's error code.
    #[serde(default)]
    pub code: Option<String>,
    /// Peloton's message.
    #[serde(default)]
    pub message: Option<String>,
}

/// A class on the member's schedule.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledClass {
    /// Scheduled-class id; Peloton issues a new one on reschedule.
    #[serde(deserialize_with = "parsed")]
    pub id: ScheduledClassId,
    /// When the class is scheduled to start.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub scheduled_start_time: Option<OffsetDateTime>,
    /// When it is scheduled to end.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub scheduled_end_time: Option<OffsetDateTime>,
    /// The class; `None` where it is not selected (removal).
    #[serde(default)]
    pub peloton_class: Option<PelotonClass>,
}

/// A class as the GraphQL API describes it. A scheduled on-demand class is a
/// "`LiveClass`", a stacked one an "`OnDemandInstructorClass`".
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PelotonClass {
    /// GraphQL type name.
    #[serde(default, rename = "__typename")]
    pub typename: Option<String>,
    /// Ride id; `None` when missing or malformed, so one odd Stack item
    /// never hides the others.
    #[serde(default, deserialize_with = "lenient_id")]
    pub class_id: Option<PelotonId>,
    /// Peloton's base64 class id: an on-demand class id in the Stack, the
    /// scheduled-class id on the schedule.
    #[serde(default)]
    pub join_token: Option<String>,
    /// Class title.
    #[serde(default)]
    pub title: Option<String>,
    /// Planned length in seconds.
    #[serde(default)]
    pub duration: Option<u32>,
    /// The discipline.
    #[serde(default)]
    pub fitness_discipline: Option<FitnessDiscipline>,
    /// When the class first aired.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub air_time: Option<OffsetDateTime>,
    /// Difficulty level, when Peloton sets one.
    #[serde(default)]
    pub difficulty_level: Option<DifficultyLevel>,
    /// The instructor.
    #[serde(default)]
    pub instructor: Option<ClassInstructor>,
}

/// [`PelotonClass::fitness_discipline`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FitnessDiscipline {
    /// Discipline slug, e.g. "CYCLING".
    #[serde(default)]
    pub slug: Option<String>,
    /// Display name, e.g. "Cycling".
    #[serde(default)]
    pub display_name: Option<String>,
}

/// [`PelotonClass::difficulty_level`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DifficultyLevel {
    /// Level slug.
    #[serde(default)]
    pub slug: Option<String>,
    /// Display name.
    #[serde(default)]
    pub display_name: Option<String>,
}

/// [`PelotonClass::instructor`].
#[derive(Debug, Clone, Deserialize)]
pub struct ClassInstructor {
    /// Display name.
    #[serde(default)]
    pub name: Option<String>,
}

/// GraphQL `viewUserStack` and the Stack mutations.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "__typename")]
pub enum StackResponse {
    /// The Stack.
    StackResponseSuccess(UserStack),
}

/// [`StackResponse::StackResponseSuccess`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserStack {
    /// Number of classes in the Stack.
    #[serde(default)]
    pub num_classes: Option<u32>,
    /// Their total length in seconds.
    #[serde(default)]
    pub total_time: Option<u32>,
    /// The classes; required, since the Stack is rewritten from it.
    pub user_stack: StackedClasses,
}

/// [`UserStack::user_stack`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackedClasses {
    /// Classes in play order; required, never read as empty.
    pub stacked_class_list: Vec<StackedClass>,
}

/// One class of the Stack.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackedClass {
    /// Position in the Stack, from 0.
    #[serde(default)]
    pub play_order: Option<u32>,
    /// The class; kept even when its fields are odd.
    #[serde(default)]
    pub peloton_class: Option<PelotonClass>,
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
    fn owner_id_is_lenient() {
        let owner = "000000000000000000000000000000f2";
        let parse = |user_id: serde_json::Value| -> Workout {
            serde_json::from_value(json!({
                "id": "0123456789abcdef0123456789abcdef",
                "user_id": user_id,
            }))
            .expect("parse")
        };
        assert_eq!(
            parse(json!(owner))
                .user_id
                .map(|id| id.to_string())
                .as_deref(),
            Some(owner)
        );
        for odd in [json!(null), json!("../me"), json!(7)] {
            assert!(parse(odd.clone()).user_id.is_none(), "{odd}");
        }
        let missing: Workout =
            serde_json::from_value(json!({"id": "0123456789abcdef0123456789abcdef"}))
                .expect("parse");
        assert!(missing.user_id.is_none());
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
