//! The relay's own response shapes.
//!
//! Units are in field names (`_s` seconds, `_kj` kilojoules, `_km`/`_mi`,
//! `_w` watts, `_bpm` beats per minute, `_mets` METs); where a unit is data
//! rather than fixed (time series and summaries) it is a `unit` field holding
//! Peloton's unit verbatim. Timestamps are RFC 3339 in UTC. A value Peloton
//! does not report is `null`, never a default. Mapping is infallible: all
//! validation happens when the upstream response is decoded.

use peloton::ids::PelotonId;
use peloton::upstream;
use serde::Serialize;
use time::OffsetDateTime;
use utoipa::ToSchema;

/// Treats Peloton's 0 ("not set") as absent.
fn nonzero(value: Option<u32>) -> Option<u32> {
    value.filter(|v| *v > 0)
}

/// Seconds between two instants, when both are known and in order.
fn seconds_between(start: Option<OffsetDateTime>, end: Option<OffsetDateTime>) -> Option<u32> {
    let (start, end) = (start?, end?);
    u32::try_from((end - start).whole_seconds()).ok()
}

/// Kilometres per mile.
const KM_PER_MI: f64 = 1.609_344;

/// Total output, which only exists with pedaling metrics.
fn total_output_kj(workout: &upstream::Workout) -> Option<f64> {
    if workout.has_pedaling_metrics == Some(true) {
        workout.total_work.map(|joules| joules / 1000.0)
    } else {
        None
    }
}

/// `(km, mi)` from the graph's `distance` summary, converted from the unit it
/// reports; `(None, None)` when absent or in an unknown unit.
fn distance(graph: &upstream::PerformanceGraph) -> (Option<f64>, Option<f64>) {
    let found = graph
        .summaries
        .iter()
        .find(|s| s.slug.as_deref() == Some("distance"));
    let Some(summary) = found else {
        return (None, None);
    };
    let unit = summary.display_unit.as_deref().map(str::to_ascii_lowercase);
    match (summary.value, unit.as_deref()) {
        (Some(mi), Some("mi")) => (Some(mi * KM_PER_MI), Some(mi)),
        (Some(km), Some("km")) => (Some(km), Some(km / KM_PER_MI)),
        _ => (None, None),
    }
}

/// `GET /v1/me`: the member's profile summary.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Profile {
    /// Member id.
    #[schema(value_type = String)]
    pub id: PelotonId,
    /// Public username.
    pub username: Option<String>,
    /// Display name.
    pub display_name: Option<String>,
    /// When the account was created.
    #[serde(with = "time::serde::rfc3339::option")]
    pub member_since: Option<OffsetDateTime>,
    /// Workouts ever recorded.
    pub total_workouts: Option<u32>,
    /// Current cycling functional threshold power.
    pub cycling_ftp_w: Option<u32>,
    /// Maximum heart rate: the member's own setting, else Peloton's estimate.
    pub max_heart_rate_bpm: Option<u32>,
    /// Workouts per discipline (entries without a discipline slug are left out).
    pub workout_counts: Vec<DisciplineCount>,
}

/// One entry of [`Profile::workout_counts`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct DisciplineCount {
    /// Discipline slug, e.g. "cycling".
    pub discipline: String,
    /// Display name, e.g. "Cycling".
    pub name: Option<String>,
    /// Number of workouts.
    pub count: Option<u32>,
}

impl Profile {
    /// Maps Peloton's `/me`.
    #[must_use]
    pub fn from_upstream(me: &upstream::Me) -> Self {
        Self {
            id: me.id.clone(),
            username: me.username.clone(),
            display_name: me.name.clone(),
            member_since: me.created_at,
            total_workouts: me.total_workouts,
            cycling_ftp_w: nonzero(me.cycling_ftp),
            max_heart_rate_bpm: nonzero(me.customized_max_heart_rate)
                .or(nonzero(me.default_max_heart_rate)),
            workout_counts: me
                .workout_counts
                .iter()
                .filter_map(|c| {
                    Some(DisciplineCount {
                        discipline: c.slug.clone()?,
                        name: c.name.clone(),
                        count: c.count,
                    })
                })
                .collect(),
        }
    }
}

/// `GET /v1/workouts`: one page of workouts, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct WorkoutPage {
    /// Workouts on this page.
    pub workouts: Vec<WorkoutSummary>,
    /// Zero-based page index.
    pub page: u32,
    /// Number of pages.
    pub page_count: u32,
    /// Number of workouts across all pages.
    pub total: u32,
}

impl WorkoutPage {
    /// Maps a page of Peloton's workout list.
    #[must_use]
    pub fn from_upstream(page: &upstream::WorkoutPage) -> Self {
        Self {
            workouts: page
                .data
                .iter()
                .map(WorkoutSummary::from_upstream)
                .collect(),
            page: page.page,
            page_count: page.page_count,
            total: page.total,
        }
    }
}

/// A workout as listed in [`WorkoutPage`].
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct WorkoutSummary {
    /// Workout id.
    #[schema(value_type = String)]
    pub id: PelotonId,
    /// Discipline slug: "cycling", "yoga", "stretching", ….
    pub discipline: Option<String>,
    /// "class", "scenic", "freestyle", ….
    pub workout_type: Option<String>,
    /// "COMPLETE", "`IN_PROGRESS`", ….
    pub status: Option<String>,
    /// When the member started.
    #[serde(with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    /// When the member finished.
    #[serde(with = "time::serde::rfc3339::option")]
    pub ended_at: Option<OffsetDateTime>,
    /// Time from start to finish.
    pub duration_s: Option<u32>,
    /// Class id; `null` without a class (Just Ride).
    #[schema(value_type = Option<String>)]
    pub class_id: Option<PelotonId>,
    /// Class title.
    pub class_title: Option<String>,
    /// Instructor name.
    pub instructor_name: Option<String>,
    /// Total output; `null` without pedaling metrics.
    pub total_output_kj: Option<f64>,
    /// Whether the total output is a personal record.
    pub total_output_personal_record: Option<bool>,
}

impl WorkoutSummary {
    /// Maps one workout of Peloton's list (or a single workout).
    #[must_use]
    pub fn from_upstream(workout: &upstream::Workout) -> Self {
        let ride = workout.ride.as_ref();
        Self {
            id: workout.id.clone(),
            discipline: workout.fitness_discipline.clone(),
            workout_type: workout.workout_type.clone(),
            status: workout.status.clone(),
            started_at: workout.start_time,
            ended_at: workout.end_time,
            duration_s: seconds_between(workout.start_time, workout.end_time),
            class_id: ride.map(|r| r.id.clone()),
            class_title: ride.and_then(|r| r.title.clone()),
            instructor_name: ride
                .and_then(|r| r.instructor.as_ref())
                .and_then(|i| i.name.clone()),
            total_output_kj: total_output_kj(workout),
            total_output_personal_record: workout.is_total_work_personal_record,
        }
    }
}

/// `GET /v1/workouts/{id}`: one workout with its class and instructor.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct WorkoutDetail {
    /// Workout id.
    #[schema(value_type = String)]
    pub id: PelotonId,
    /// Discipline slug: "cycling", "yoga", "stretching", ….
    pub discipline: Option<String>,
    /// "class", "scenic", "freestyle", ….
    pub workout_type: Option<String>,
    /// "COMPLETE", "`IN_PROGRESS`", ….
    pub status: Option<String>,
    /// When the workout was created.
    #[serde(with = "time::serde::rfc3339::option")]
    pub created_at: Option<OffsetDateTime>,
    /// When the member started.
    #[serde(with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    /// When the member finished.
    #[serde(with = "time::serde::rfc3339::option")]
    pub ended_at: Option<OffsetDateTime>,
    /// Time from start to finish.
    pub duration_s: Option<u32>,
    /// Device the workout was recorded on.
    pub device_type: Option<String>,
    /// Total output; `null` without pedaling metrics.
    pub total_output_kj: Option<f64>,
    /// Whether the total output is a personal record.
    pub total_output_personal_record: Option<bool>,
    /// FTP in effect during the workout.
    pub ftp_w: Option<u32>,
    /// Time per heart-rate zone; `null` without heart-rate data.
    pub heart_rate_zones: Option<HeartRateZones>,
    /// Leaderboard rank.
    pub leaderboard_rank: Option<u32>,
    /// Members on that leaderboard.
    pub leaderboard_size: Option<u32>,
    /// Badges earned with this workout.
    pub achievements: Vec<String>,
    /// The class taken; `null` for a Just Ride.
    pub class: Option<ClassInfo>,
}

impl WorkoutDetail {
    /// Maps Peloton's `/workout/{id}`.
    #[must_use]
    pub fn from_upstream(workout: &upstream::Workout) -> Self {
        let zones = workout.total_heart_rate_zone_durations.as_ref().or(workout
            .effort_zones
            .as_ref()
            .and_then(|e| e.heart_rate_zone_durations.as_ref()));
        Self {
            id: workout.id.clone(),
            discipline: workout.fitness_discipline.clone(),
            workout_type: workout.workout_type.clone(),
            status: workout.status.clone(),
            created_at: workout.created_at,
            started_at: workout.start_time,
            ended_at: workout.end_time,
            duration_s: seconds_between(workout.start_time, workout.end_time),
            device_type: workout.device_type.clone(),
            total_output_kj: total_output_kj(workout),
            total_output_personal_record: workout.is_total_work_personal_record,
            ftp_w: nonzero(workout.ftp_info.as_ref().and_then(|f| f.ftp)),
            heart_rate_zones: zones.map(HeartRateZones::from_upstream),
            leaderboard_rank: workout.leaderboard_rank,
            leaderboard_size: workout.total_leaderboard_users,
            achievements: workout
                .achievement_templates
                .iter()
                .filter_map(|a| a.name.clone())
                .collect(),
            class: workout.ride.as_ref().map(ClassInfo::from_upstream),
        }
    }
}

/// Time spent in each of Peloton's five heart-rate zones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct HeartRateZones {
    /// Zone 1.
    pub zone1_s: Option<u32>,
    /// Zone 2.
    pub zone2_s: Option<u32>,
    /// Zone 3.
    pub zone3_s: Option<u32>,
    /// Zone 4.
    pub zone4_s: Option<u32>,
    /// Zone 5.
    pub zone5_s: Option<u32>,
}

impl HeartRateZones {
    /// Maps Peloton's zone durations.
    #[must_use]
    pub fn from_upstream(zones: &upstream::HeartRateZoneDurations) -> Self {
        Self {
            zone1_s: zones.heart_rate_z1_duration,
            zone2_s: zones.heart_rate_z2_duration,
            zone3_s: zones.heart_rate_z3_duration,
            zone4_s: zones.heart_rate_z4_duration,
            zone5_s: zones.heart_rate_z5_duration,
        }
    }
}

/// A class.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ClassInfo {
    /// Class id.
    #[schema(value_type = String)]
    pub id: PelotonId,
    /// Title.
    pub title: Option<String>,
    /// Description.
    pub description: Option<String>,
    /// Discipline slug.
    pub discipline: Option<String>,
    /// Planned length.
    pub duration_s: Option<u32>,
    /// Peloton's difficulty estimate, 0–10.
    pub difficulty: Option<f64>,
    /// When the class first aired.
    #[serde(with = "time::serde::rfc3339::option")]
    pub aired_at: Option<OffsetDateTime>,
    /// Instructor; `null` when the class has none.
    pub instructor: Option<InstructorInfo>,
}

impl ClassInfo {
    /// Maps a Peloton ride.
    #[must_use]
    pub fn from_upstream(ride: &upstream::Ride) -> Self {
        Self {
            id: ride.id.clone(),
            title: ride.title.clone(),
            description: ride.description.clone(),
            discipline: ride.fitness_discipline.clone(),
            duration_s: ride.duration,
            difficulty: ride.difficulty_estimate,
            aired_at: ride.original_air_time,
            instructor: ride.instructor.as_ref().map(InstructorInfo::from_upstream),
        }
    }
}

/// An instructor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct InstructorInfo {
    /// Instructor id.
    #[schema(value_type = String)]
    pub id: PelotonId,
    /// Display name.
    pub name: Option<String>,
}

impl InstructorInfo {
    /// Maps a Peloton instructor.
    #[must_use]
    pub fn from_upstream(instructor: &upstream::Instructor) -> Self {
        Self {
            id: instructor.id.clone(),
            name: instructor.name.clone(),
        }
    }
}

/// `GET /v1/workouts/{id}/metrics`: time series and summaries.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Metrics {
    /// Workout length.
    pub duration_s: Option<u32>,
    /// Sampling interval: the smallest gap between samples; `null` with
    /// fewer than two samples. Samples can be further apart (dropouts), so
    /// use `offsets_s` for exact timing.
    pub every_n_s: Option<u32>,
    /// Offset of each sample from the start; every series aligns with it.
    pub offsets_s: Vec<u32>,
    /// Distance from the `distance` summary, converted from the unit
    /// Peloton reports it in; `null` when absent or in an unknown unit.
    pub distance_km: Option<f64>,
    /// The same distance in miles.
    pub distance_mi: Option<f64>,
    /// Time series; empty for disciplines without recorded metrics.
    /// Series without a slug are left out.
    pub series: Vec<MetricSeries>,
    /// Totals and averages (summaries without a slug are left out).
    pub summaries: Vec<MetricSummary>,
}

impl Metrics {
    /// Maps Peloton's performance graph.
    #[must_use]
    pub fn from_upstream(graph: &upstream::PerformanceGraph) -> Self {
        let offsets = &graph.seconds_since_pedaling_start;
        let (distance_km, distance_mi) = distance(graph);
        Self {
            duration_s: graph.duration,
            every_n_s: offsets
                .windows(2)
                .filter_map(|w| w[1].checked_sub(w[0]))
                .filter(|gap| *gap > 0)
                .min(),
            offsets_s: offsets.clone(),
            distance_km,
            distance_mi,
            series: graph
                .metrics
                .iter()
                .filter_map(MetricSeries::from_upstream)
                .collect(),
            summaries: graph
                .summaries
                .iter()
                .chain(&graph.average_summaries)
                .filter_map(MetricSummary::from_upstream)
                .collect(),
        }
    }
}

/// One time series, e.g. output in watts.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct MetricSeries {
    /// Peloton's slug: "output", "cadence", "resistance", "speed", "`heart_rate`", ….
    pub metric: String,
    /// Display name.
    pub display_name: Option<String>,
    /// Unit as Peloton reports it: "watts", "rpm", "%", "mph", "bpm", ….
    pub unit: Option<String>,
    /// Average over the workout.
    pub average: Option<f64>,
    /// Maximum over the workout.
    pub max: Option<f64>,
    /// Samples aligned with [`Metrics::offsets_s`]; `null` where missing.
    pub values: Vec<Option<f64>>,
}

impl MetricSeries {
    /// Maps one performance graph series; `None` without a slug.
    #[must_use]
    pub fn from_upstream(metric: &upstream::GraphMetric) -> Option<Self> {
        Some(Self {
            metric: metric.slug.clone()?,
            display_name: metric.display_name.clone(),
            unit: metric.display_unit.clone(),
            average: metric.average_value,
            max: metric.max_value,
            values: metric.values.clone(),
        })
    }
}

/// One total or average, e.g. total output in kJ.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct MetricSummary {
    /// Peloton's slug: "`total_output`", "distance", "`total_calories`", "`avg_cadence`", ….
    pub metric: String,
    /// Display name.
    pub display_name: Option<String>,
    /// Unit as Peloton reports it: "kj", "mi", "kcal", "rpm", ….
    pub unit: Option<String>,
    /// Value in that unit.
    pub value: Option<f64>,
}

impl MetricSummary {
    /// Maps one performance graph summary; `None` without a slug.
    #[must_use]
    pub fn from_upstream(summary: &upstream::GraphSummary) -> Option<Self> {
        Some(Self {
            metric: summary.slug.clone()?,
            display_name: summary.display_name.clone(),
            unit: summary.display_unit.clone(),
            value: summary.value,
        })
    }
}

/// `GET /v1/classes/{id}`: a class with its structure and music.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ClassDetails {
    /// The class and its instructor.
    pub class: ClassInfo,
    /// Class types, e.g. "Low Impact".
    pub class_types: Vec<String>,
    /// Whether this is a power zone class.
    pub power_zone_class: Option<bool>,
    /// Whether this is an FTP test.
    pub ftp_test: Option<bool>,
    /// Segments in class order.
    pub segments: Vec<ClassSegment>,
    /// Songs in play order; `null` when the class has no playlist.
    pub music: Option<Vec<SongInfo>>,
}

impl ClassDetails {
    /// Maps Peloton's `/ride/{id}/details`.
    #[must_use]
    pub fn from_upstream(details: &upstream::RideDetails) -> Self {
        Self {
            class: ClassInfo::from_upstream(&details.ride),
            class_types: details
                .class_types
                .iter()
                .filter_map(|t| t.name.clone())
                .collect(),
            power_zone_class: details.is_power_zone_class,
            ftp_test: details.is_ftp_test,
            segments: details
                .segments
                .iter()
                .flat_map(|s| &s.segment_list)
                .map(ClassSegment::from_upstream)
                .collect(),
            music: details
                .playlist
                .as_ref()
                .map(|p| p.songs.iter().map(SongInfo::from_upstream).collect()),
        }
    }
}

/// One segment of a class, e.g. the warmup.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ClassSegment {
    /// Segment name.
    pub name: Option<String>,
    /// Start from the class start.
    pub start_offset_s: Option<u32>,
    /// Length.
    pub length_s: Option<u32>,
    /// Intensity.
    pub intensity_mets: Option<f64>,
}

impl ClassSegment {
    /// Maps one class segment.
    #[must_use]
    pub fn from_upstream(segment: &upstream::Segment) -> Self {
        Self {
            name: segment.name.clone(),
            start_offset_s: segment.start_time_offset,
            length_s: segment.length,
            intensity_mets: segment.intensity_in_mets,
        }
    }
}

/// One song of a class playlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SongInfo {
    /// Title.
    pub title: Option<String>,
    /// Artist names.
    pub artists: Vec<String>,
    /// Album name.
    pub album: Option<String>,
    /// Start from the class start.
    pub start_offset_s: Option<u32>,
}

impl SongInfo {
    /// Maps one playlist song.
    #[must_use]
    pub fn from_upstream(song: &upstream::Song) -> Self {
        Self {
            title: song.title.clone(),
            artists: song
                .artists
                .iter()
                .filter_map(|a| a.artist_name.clone())
                .collect(),
            album: song.album.as_ref().and_then(|a| a.name.clone()),
            start_offset_s: song.start_time_offset,
        }
    }
}
