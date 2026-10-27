use std::fs;
use std::path::PathBuf;

use peloton::upstream;
use peloton_relay::model::{
    ClassDetails, Metrics, Profile, WorkoutDetail, WorkoutPage, WorkoutSummary,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Name prefixes of the library, schedule and Stack fixtures, which these
/// models do not map.
const LIBRARY_AND_SCHEDULING_FIXTURES: &[&str] = &[
    "library_archived",
    "ride_filters",
    "browse_categories",
    "schedule",
    "stack_",
    "check_class_in_stack",
];

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../peloton/tests/fixtures")
}

fn load<T: DeserializeOwned>(name: &str) -> T {
    let path = fixtures_dir().join(name);
    let body = fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_slice(&body).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn workout(kind: &str) -> WorkoutDetail {
    WorkoutDetail::from_upstream(&load::<upstream::Workout>(&format!("workout-{kind}.json")))
}

/// The raw fixture JSON, for expectations derived from the fixture itself
/// (personal values are never spelled out in tests).
fn raw(name: &str) -> Value {
    load(name)
}

/// End minus start of a workout fixture, in seconds.
fn raw_duration_s(kind: &str) -> Option<u32> {
    let workout = raw(&format!("workout-{kind}.json"));
    let seconds = workout["end_time"].as_i64()? - workout["start_time"].as_i64()?;
    u32::try_from(seconds).ok()
}

/// `total_work` of a workout fixture, in kJ.
fn raw_output_kj(kind: &str) -> f64 {
    raw(&format!("workout-{kind}.json"))["total_work"]
        .as_f64()
        .expect("total_work")
        / 1000.0
}

/// The class title of a workout fixture, which must be present and non-empty.
fn raw_title(name: &str) -> String {
    let title = raw(name)["ride"]["title"]
        .as_str()
        .unwrap_or_else(|| panic!("{name}: ride.title missing"))
        .to_owned();
    assert!(!title.is_empty(), "{name}: title");
    title
}

fn assert_close(actual: Option<f64>, expected: f64) {
    let actual = actual.expect("value present");
    assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
}

#[test]
fn maps_every_fixture_workout() {
    let mut workouts = 0;
    for entry in fs::read_dir(fixtures_dir()).expect("fixtures dir") {
        let name = entry
            .expect("entry")
            .file_name()
            .into_string()
            .expect("utf-8 name");
        if name.starts_with("workout-") {
            let detail = WorkoutDetail::from_upstream(&load(&name));
            assert!(detail.started_at.is_some(), "{name}: start time");
            workouts += 1;
        } else if name.starts_with("performance_graph-") {
            let metrics = Metrics::from_upstream(&load(&name));
            for series in &metrics.series {
                assert_eq!(series.values.len(), metrics.offsets_s.len(), "{name}");
            }
        } else if name.starts_with("ride_details-") {
            let details = ClassDetails::from_upstream(&load(&name));
            assert!(!details.segments.is_empty(), "{name}: segments");
        } else if name == "workouts.json" {
            let page = WorkoutPage::from_upstream(&load(&name));
            assert!(!page.workouts.is_empty());
        } else if name == "me.json" {
            let profile = Profile::from_upstream(&load(&name));
            assert!(profile.username.is_some());
        } else if LIBRARY_AND_SCHEDULING_FIXTURES
            .iter()
            .any(|prefix| name.starts_with(prefix))
        {
            // Decoded by the peloton crate's `upstream_new` tests.
        } else {
            panic!("unexpected fixture {name}");
        }
    }
    assert_eq!(workouts, 5, "one workout fixture per kind");
}

#[test]
fn just_ride_has_null_class_and_instructor() {
    let detail = workout("cycling-just-ride");
    assert_eq!(detail.discipline.as_deref(), Some("cycling"));
    assert!(detail.class.is_none());
    assert_close(detail.total_output_kj, raw_output_kj("cycling-just-ride"));
    let json = serde_json::to_value(&detail).expect("serialize");
    assert_eq!(json["class"], Value::Null);

    let summary = WorkoutSummary::from_upstream(&load("workout-cycling-just-ride.json"));
    assert!(summary.class_id.is_none());
    assert!(summary.class_title.is_none());
    assert!(summary.instructor_name.is_none());

    for ride in [json!(null), json!({"id": ""}), json!({"id": null})] {
        let raw = json!({
            "id": "0123456789abcdef0123456789abcdef",
            "fitness_discipline": "cycling",
            "ride": ride,
        });
        let parsed: upstream::Workout = serde_json::from_value(raw).expect("parse");
        assert!(WorkoutDetail::from_upstream(&parsed).class.is_none());
    }
}

#[test]
fn class_without_instructor_maps_to_null_instructor() {
    let raw = json!({
        "id": "0123456789abcdef0123456789abcdef",
        "fitness_discipline": "cycling",
        "ride": {
            "id": "fedcba9876543210fedcba9876543210",
            "title": "Scenic",
            "instructor_id": "",
            "instructor": null
        },
    });
    let parsed: upstream::Workout = serde_json::from_value(raw).expect("parse");
    let class = WorkoutDetail::from_upstream(&parsed).class.expect("class");
    assert!(class.instructor.is_none());
}

#[test]
fn empty_metrics_is_ok() {
    for raw in [
        json!({"duration": 0, "seconds_since_pedaling_start": [], "metrics": [], "summaries": [], "average_summaries": []}),
        json!({"seconds_since_pedaling_start": null, "metrics": null, "summaries": null}),
        json!({}),
    ] {
        let graph: upstream::PerformanceGraph = serde_json::from_value(raw).expect("parse");
        let metrics = Metrics::from_upstream(&graph);
        assert!(metrics.series.is_empty());
        assert!(metrics.summaries.is_empty());
        assert!(metrics.offsets_s.is_empty());
        assert_eq!(metrics.every_n_s, None);
    }
}

#[test]
fn cycling_class_snapshot() {
    let detail = workout("cycling-class");
    assert_eq!(detail.discipline.as_deref(), Some("cycling"));
    assert_eq!(detail.workout_type.as_deref(), Some("class"));
    assert!(detail.duration_s.is_some());
    assert_eq!(detail.duration_s, raw_duration_s("cycling-class"));
    assert_close(detail.total_output_kj, raw_output_kj("cycling-class"));
    let class = detail.class.expect("class");
    assert_eq!(class.title, Some(raw_title("workout-cycling-class.json")));
    assert_eq!(class.duration_s, Some(600));
    assert!(class.instructor.expect("instructor").name.is_some());
    assert!(
        detail.heart_rate_zones.is_none(),
        "this capture's single-workout response has no zone durations"
    );

    let page: upstream::WorkoutPage = load("workouts.json");
    let listed = page
        .data
        .iter()
        .find(|w| w.id == detail.id)
        .expect("listed");
    let zones = WorkoutDetail::from_upstream(listed)
        .heart_rate_zones
        .expect("the list shape carries zone durations");
    assert!(zones.zone3_s.is_some());
}

#[test]
fn scenic_cycling_snapshot() {
    let detail = workout("cycling-scenic");
    assert_eq!(detail.discipline.as_deref(), Some("cycling"));
    assert_eq!(detail.workout_type.as_deref(), Some("scenic"));
    assert!(detail.duration_s.is_some());
    assert_eq!(detail.duration_s, raw_duration_s("cycling-scenic"));
    assert_close(detail.total_output_kj, raw_output_kj("cycling-scenic"));
    assert_eq!(
        detail.class.expect("class").title,
        Some(raw_title("workout-cycling-scenic.json"))
    );
}

#[test]
fn stretching_snapshot() {
    let detail = workout("stretching");
    assert_eq!(detail.discipline.as_deref(), Some("stretching"));
    assert!(detail.duration_s.is_some());
    assert_eq!(detail.duration_s, raw_duration_s("stretching"));
    assert_eq!(
        detail.total_output_kj, None,
        "no output without pedaling metrics"
    );
    assert!(detail.class.is_some());
}

#[test]
fn yoga_snapshot() {
    let detail = workout("yoga");
    assert_eq!(detail.discipline.as_deref(), Some("yoga"));
    assert!(detail.duration_s.is_some());
    assert_eq!(detail.duration_s, raw_duration_s("yoga"));
    assert_eq!(detail.total_output_kj, None);
    assert_eq!(
        detail.class.expect("class").title,
        Some(raw_title("workout-yoga.json"))
    );
}

#[test]
fn cycling_metrics_have_series_and_summaries() {
    let metrics = Metrics::from_upstream(&load("performance_graph-cycling-class.json"));
    assert_eq!(metrics.every_n_s, Some(1));
    let raw_graph = raw("performance_graph-cycling-class.json");
    let samples = raw_graph["seconds_since_pedaling_start"]
        .as_array()
        .expect("offsets")
        .len();
    assert!(samples > 1);
    assert_eq!(metrics.offsets_s.len(), samples);
    let slugs: Vec<(&str, &str)> = metrics
        .series
        .iter()
        .map(|s| (s.metric.as_str(), s.unit.as_deref().unwrap_or("")))
        .collect();
    assert_eq!(
        slugs,
        [
            ("output", "watts"),
            ("cadence", "rpm"),
            ("resistance", "%"),
            ("speed", "mph"),
            ("heart_rate", "bpm")
        ]
    );
    let summary = |slug: &str| {
        metrics
            .summaries
            .iter()
            .find(|s| s.metric == slug)
            .unwrap_or_else(|| panic!("{slug}"))
    };
    assert_eq!(summary("total_output").unit.as_deref(), Some("kj"));
    assert_eq!(summary("avg_cadence").unit.as_deref(), Some("rpm"));
    assert_eq!(summary("total_calories").unit.as_deref(), Some("kcal"));
}

#[test]
fn scenic_metrics_keep_irregular_offsets() {
    let metrics = Metrics::from_upstream(&load("performance_graph-cycling-scenic.json"));
    assert_eq!(metrics.every_n_s, Some(1));
    let gaps: Vec<u32> = metrics.offsets_s.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(gaps.contains(&2), "the scenic capture has two-second gaps");
}

#[test]
fn non_pedaling_metrics_have_no_series() {
    let metrics = Metrics::from_upstream(&load("performance_graph-yoga.json"));
    assert!(metrics.series.is_empty());
    assert_eq!(metrics.summaries.len(), 1);
    assert_eq!(metrics.summaries[0].unit.as_deref(), Some("kcal"));
}

#[test]
fn class_details_have_segments_instructor_and_music() {
    let details = ClassDetails::from_upstream(&load("ride_details-cycling-class.json"));
    assert_eq!(
        details.class.title,
        Some(raw_title("workout-cycling-class.json"))
    );
    assert!(details.class.instructor.is_some());
    assert!(!details.class_types.is_empty());
    let first = &details.segments[0];
    assert_eq!(first.start_offset_s, Some(0));
    assert!(first.length_s.is_some());
    let music = details.music.expect("music");
    assert!(!music.is_empty());
    assert!(!music[0].artists.is_empty());
}

#[test]
fn workout_page_keeps_paging_and_order() {
    let page = WorkoutPage::from_upstream(&load("workouts.json"));
    let raw_page = raw("workouts.json");
    assert_eq!(Some(u64::from(page.total)), raw_page["total"].as_u64());
    assert!(page.total > 0);
    assert_eq!(page.page, 0);
    assert_eq!(
        Some(u64::from(page.page_count)),
        raw_page["page_count"].as_u64()
    );
    let starts: Vec<OffsetDateTime> = page
        .workouts
        .iter()
        .map(|w| w.started_at.expect("start"))
        .collect();
    assert!(starts.windows(2).all(|w| w[0] >= w[1]), "newest first");
}

#[test]
fn profile_maps_totals() {
    let profile = Profile::from_upstream(&load("me.json"));
    assert!(profile.total_workouts.is_some());
    assert_eq!(
        profile.total_workouts.map(u64::from),
        raw("me.json")["total_workouts"].as_u64()
    );
    assert!(profile.display_name.is_some());
    assert!(
        profile
            .workout_counts
            .iter()
            .any(|c| c.discipline == "cycling")
    );
}

#[test]
fn timestamps_serialize_as_rfc3339_utc() {
    let json = serde_json::to_value(workout("cycling-class")).expect("serialize");
    let started = json["started_at"].as_str().expect("string");
    assert!(started.ends_with('Z'), "{started}");
    OffsetDateTime::parse(started, &Rfc3339).expect("RFC 3339");
    assert_eq!(
        json["class"]["instructor"]["id"].as_str().map(str::len),
        Some(32)
    );
}

#[test]
fn distance_comes_from_the_graph_with_its_unit() {
    let class = Metrics::from_upstream(&load("performance_graph-cycling-class.json"));
    let raw_graph = raw("performance_graph-cycling-class.json");
    let reported = raw_graph["summaries"]
        .as_array()
        .expect("summaries")
        .iter()
        .find(|s| s["slug"] == "distance")
        .expect("distance");
    assert_eq!(reported["display_unit"], "mi");
    let miles = reported["value"].as_f64().expect("value");
    assert_close(class.distance_mi, miles);
    assert_close(class.distance_km, miles * 1.609_344);

    let yoga = Metrics::from_upstream(&load("performance_graph-yoga.json"));
    assert_eq!(yoga.distance_mi, None);
    assert_eq!(yoga.distance_km, None);

    let graph = |unit: &str| -> upstream::PerformanceGraph {
        serde_json::from_value(json!({
            "summaries": [{"slug": "distance", "value": 10.0, "display_unit": unit}]
        }))
        .expect("parse")
    };
    let metric = Metrics::from_upstream(&graph("km"));
    assert_close(metric.distance_km, 10.0);
    assert_close(metric.distance_mi, 10.0 / 1.609_344);
    let unknown = Metrics::from_upstream(&graph("furlongs"));
    assert_eq!(
        unknown.distance_km, None,
        "an unknown unit is never relabelled"
    );
    assert_eq!(unknown.distance_mi, None);
}

#[test]
fn workout_types_carry_no_unitless_distance() {
    let json = serde_json::to_value(workout("cycling-class")).expect("serialize");
    let summary = serde_json::to_value(WorkoutSummary::from_upstream(&load(
        "workout-cycling-class.json",
    )))
    .expect("serialize");
    for value in [json, summary] {
        let keys: Vec<&String> = value.as_object().expect("object").keys().collect();
        assert!(keys.iter().all(|k| !k.starts_with("distance")), "{keys:?}");
    }
}

#[test]
fn nameless_list_items_do_not_fail_the_response() {
    let me: upstream::Me = serde_json::from_value(json!({
        "id": "0123456789abcdef0123456789abcdef",
        "workout_counts": [
            {"slug": "cycling", "count": 3},
            {"name": "Mystery", "count": 1},
            {"name": "Yoga", "slug": "yoga"}
        ]
    }))
    .expect("parse");
    let profile = Profile::from_upstream(&me);
    let counts: Vec<(&str, Option<&str>, Option<u32>)> = profile
        .workout_counts
        .iter()
        .map(|c| (c.discipline.as_str(), c.name.as_deref(), c.count))
        .collect();
    assert_eq!(
        counts,
        [("cycling", None, Some(3)), ("yoga", Some("Yoga"), None)],
        "an entry without a discipline slug is skipped"
    );

    let workout: upstream::Workout = serde_json::from_value(json!({
        "id": "0123456789abcdef0123456789abcdef",
        "achievement_templates": [{"slug": "x"}, {"name": "Synthetic Badge"}]
    }))
    .expect("parse");
    let detail = WorkoutDetail::from_upstream(&workout);
    assert_eq!(detail.achievements, ["Synthetic Badge"]);
    assert_eq!(detail.discipline, None);

    let details: upstream::RideDetails = serde_json::from_value(json!({
        "ride": {"id": "fedcba9876543210fedcba9876543210"},
        "class_types": [{"id": "a"}, {"name": "Low Impact"}]
    }))
    .expect("parse");
    assert_eq!(
        ClassDetails::from_upstream(&details).class_types,
        ["Low Impact"]
    );

    let graph: upstream::PerformanceGraph = serde_json::from_value(json!({
        "seconds_since_pedaling_start": [1, 2],
        "metrics": [{"values": [1, 2]}, {"slug": "output", "values": [3, 4]}],
        "summaries": [{"value": 1}, {"slug": "total_output", "value": 5}]
    }))
    .expect("parse");
    let metrics = Metrics::from_upstream(&graph);
    assert_eq!(metrics.series.len(), 1);
    assert_eq!(metrics.series[0].metric, "output");
    assert_eq!(metrics.summaries.len(), 1);
}
