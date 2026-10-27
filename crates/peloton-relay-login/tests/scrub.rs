use peloton_relay_login::scrub::{ScrubParams, scrub_files};
use serde_json::{Value, json};

const USER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa01";
const WORKOUT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb02";
const RIDE: &str = "cccccccccccccccccccccccccccccc03";
const INSTRUCTOR: &str = "dddddddddddddddddddddddddddddd04";
const NIL: &str = "00000000000000000000000000000000";
const UUID: &str = "3d96f8b6-1c2d-4e5f-8a9b-0c1d2e3f4a5b";
const SHIFT: i64 = -86_400 * 400 - 4_567;
const CALORIE_FACTOR: f64 = 1.1;
const PARAMS: ScrubParams = ScrubParams {
    time_shift_s: SHIFT,
    calorie_factor: CALORIE_FACTOR,
};
/// UTC+3, the offset between device-local and UTC timestamps in the input.
const LOCAL_OFFSET: i64 = 3 * 3600;

const SENSITIVE: &[&str] = &[
    "Zorblax",
    "Quentin",
    "Vandersloot",
    "Quentin Vandersloot",
    "Gloomhaven",
    "quentin@example.org",
    "Marisol",
    "Okonkwo",
    "coach_marisol",
];

fn instructor() -> Value {
    json!({
        "id": INSTRUCTOR,
        "name": "Marisol Okonkwo",
        "first_name": "Marisol",
        "last_name": "Okonkwo",
        "username": "coach_marisol",
        "bio": "Marisol grew up riding.",
        "image_url": format!("https://cdn.example.com/{INSTRUCTOR}/marisol.png"),
        "instructor_page_destination_url": format!("peloton://instructor?id={UUID}"),
        "ordered_q_and_as": [["Favourite?", "Gloomhaven"]],
        "fitness_disciplines": ["cycling"]
    })
}

fn ride() -> Value {
    json!({
        "id": RIDE,
        "title": "20 min Synthetic Ride",
        "description": "Ride with Marisol today.",
        "instructor_id": INSTRUCTOR,
        "instructor": instructor(),
        "live_stream_id": format!("{RIDE}-live"),
        "location": "studio-x",
        "original_air_time": 1_700_000_000,
        "join_tokens": {"on_demand": "eyJob21lX3BlbG90b25faWQiOiJjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjYzAzIn0="}
    })
}

/// Zone durations in the input, as if measured with other thresholds.
const OLD_ZONE_SECONDS: [u64; 5] = [111, 222, 777, 44, 5];

fn zone_durations(seconds: [u64; 5]) -> Value {
    json!({
        "heart_rate_z1_duration": seconds[0],
        "heart_rate_z2_duration": seconds[1],
        "heart_rate_z3_duration": seconds[2],
        "heart_rate_z4_duration": seconds[3],
        "heart_rate_z5_duration": seconds[4],
    })
}

fn inputs() -> Vec<(String, Value)> {
    let me = json!({
        "id": USER,
        "username": "Zorblax",
        "name": "Quentin Vandersloot",
        "first_name": "Quentin",
        "last_name": "Vandersloot",
        "middle_initial": "",
        "email": "quentin@example.org",
        "obfuscated_email": "f".repeat(64),
        "location": "Gloomhaven, ZZ",
        "birthday": 123_456_789,
        "height": 58.3,
        "weight": 199.9,
        "created_at": 1_311_111_111,
        "image_url": format!("https://s3.example.com/profile/{USER}.jpg"),
        "cycling_ftp_workout_id": WORKOUT,
        "paired_devices": [{"name": "Quentin's headphones", "serial_number": "02:00:5E:10:00:01"}],
        "total_workouts": 321,
        "default_max_heart_rate": 190,
        "customized_max_heart_rate": 0,
        "default_heart_rate_zones": [0, 123.5, 142.5, 161.5, 180.5],
        "cycling_ftp": 233,
        "cycling_workout_ftp": 263,
        "estimated_cycling_ftp": 487,
        "workout_counts": [{"name": "Cycling", "slug": "cycling", "count": 143}]
    });
    let workout = json!({
        "id": WORKOUT,
        "user_id": USER,
        "fitness_discipline": "cycling",
        "created_at": 1_777_000_000,
        "device_time_created_at": 1_777_000_000 + LOCAL_OFFSET,
        "device_time_ended_at": 1_777_001_265 + LOCAL_OFFSET,
        "start_time": 1_777_000_065,
        "end_time": 1_777_001_265,
        "timezone": "Etc/GMT-3",
        "total_work": 123_456.7,
        "distance": 3.217,
        "calories": 333.33,
        "hr_total_points": 12.34,
        "total_heart_rate_zone_durations": zone_durations(OLD_ZONE_SECONDS),
        "effort_zones": {
            "total_effort_points": 13.57,
            "heart_rate_zone_durations": zone_durations(OLD_ZONE_SECONDS)
        },
        "ftp_info": {"ftp": 247, "ftp_source": "ftp_workout_source"},
        "leaderboard_rank": 24_681,
        "total_leaderboard_users": 98_765,
        "ride": ride()
    });
    let just_ride = json!({
        "id": WORKOUT,
        "ride": {"id": NIL, "instructor": null}
    });
    let list = json!({
        "data": [workout.clone()],
        "summary": {"1999-01": 37, "1999-02": 2},
        "next": {"workout_id": WORKOUT, "created_at": 1_776_000_000},
        "total": 321
    });
    let graph = json!({
        "seconds_since_pedaling_start": [1, 2, 3],
        "metrics": [
            {"slug": "output", "values": [100, 150, 200], "max_value": 200},
            {"slug": "heart_rate", "values": [140, 150, 160], "zones": [
                {"slug": "zone1", "min_value": 0, "max_value": 122, "range": "<123 bpm", "duration": 11},
                {"slug": "zone2", "min_value": 123, "max_value": 141, "range": "123-141 bpm", "duration": 22},
                {"slug": "zone3", "min_value": 142, "max_value": 160, "range": "142-160 bpm", "duration": 33},
                {"slug": "zone4", "min_value": 161, "max_value": 179, "range": "161-179 bpm", "duration": 44},
                {"slug": "zone5", "min_value": 180, "max_value": 190, "range": ">=180 bpm", "duration": 55}
            ]}
        ],
        "effort_zones": {
            "total_effort_points": 13.57,
            "heart_rate_zone_durations": zone_durations(OLD_ZONE_SECONDS)
        },
        "summaries": [
            {"slug": "total_output", "value": 199, "display_unit": "kj"},
            {"slug": "total_calories", "value": 222, "display_unit": "kcal"}
        ],
        "location_data": [{"coordinates": [
            {"latitude": 10.1234, "longitude": 20.5678, "altitude": 7.7777, "accuracy": 6.789},
            {"latitude": 10.1235, "longitude": 20.5679, "altitude": 7.7778, "accuracy": 6.789}
        ]}]
    });
    let details =
        json!({"ride": ride(), "segments": {"segment_list": [{"id": UUID, "name": "Warmup"}]}});
    vec![
        ("me.json".to_owned(), me),
        (format!("workouts-{USER}.json"), list),
        (format!("workout-{WORKOUT}.json"), workout),
        ("workout-just-ride.json".to_owned(), just_ride),
        (format!("performance_graph-{WORKOUT}.json"), graph),
        (format!("ride_details-{RIDE}.json"), details),
    ]
}

fn render(files: &[(String, Value)]) -> String {
    let mut text = String::new();
    for (name, value) in files {
        text.push_str(name);
        text.push('\n');
        text.push_str(&value.to_string());
        text.push('\n');
    }
    text
}

/// Every run of 32 or more hexadecimal characters, and every UUID, in `text`.
fn hex_ids(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut start = None;
    for (i, b) in bytes.iter().chain(std::iter::once(&b' ')).enumerate() {
        if b.is_ascii_hexdigit() {
            start.get_or_insert(i);
        } else if let Some(s) = start.take()
            && i - s >= 32
        {
            found.push(text[s..i].to_owned());
        }
    }
    for (i, _) in text.match_indices('-') {
        if i >= 8 && text.len() >= i + 28 {
            let candidate = &text[i - 8..i + 28];
            if is_uuid(candidate) {
                found.push(candidate.to_owned());
            }
        }
    }
    found
}

fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts.iter().map(|p| p.len()).eq([8, 4, 4, 4, 12])
        && parts
            .iter()
            .all(|p| p.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn find<'a>(files: &'a [(String, Value)], prefix: &str) -> Vec<&'a (String, Value)> {
    files
        .iter()
        .filter(|(n, _)| n.starts_with(prefix))
        .collect()
}

#[test]
fn scrub_removes_all_original_identifiers() {
    let input = inputs();
    let input_text = render(&input);
    let output = scrub_files(input, PARAMS);
    let output_text = render(&output);

    let originals: Vec<String> = hex_ids(&input_text)
        .into_iter()
        .filter(|id| id != NIL)
        .collect();
    assert!(originals.len() >= 6, "test input must carry ids");
    for id in &originals {
        assert!(!output_text.contains(id.as_str()), "id {id} survived");
    }
    for word in SENSITIVE {
        assert!(
            !output_text.to_lowercase().contains(&word.to_lowercase()),
            "{word} survived"
        );
    }
    for leak in [
        "02:00:5E",
        "190",
        "123.5",
        "180.5",
        "<123 bpm",
        "487",
        "233",
        "263",
        "247",
        "24681",
        "98765",
        "333.33",
        "222",
        "7.777",
        "13.57",
        "777",
        "6.789",
        "10.123",
        "20.567",
        "Etc/GMT-3",
        "eyJ",
        "cdn.example.com",
        "s3.example.com",
        "123456789",
        "58.3",
        "199.9",
        "1999-01",
    ] {
        assert!(!output_text.contains(leak), "{leak} survived");
    }
}

#[test]
fn scrub_maps_ids_consistently_across_files() {
    let output = scrub_files(inputs(), PARAMS);

    let me = &find(&output, "me.json")[0].1;
    let user = me["id"].as_str().expect("user id");
    assert_eq!(user.len(), 32);
    assert!(user.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));

    let list = find(&output, "workouts-");
    assert_eq!(list[0].0, format!("workouts-{user}.json"));
    let workout_id = list[0].1["data"][0]["id"].as_str().expect("workout id");
    assert_eq!(list[0].1["data"][0]["user_id"], user);
    assert_eq!(list[0].1["next"]["workout_id"], workout_id);
    assert_eq!(me["cycling_ftp_workout_id"], workout_id);

    let workout_file = format!("workout-{workout_id}.json");
    let workout = &find(&output, &workout_file)[0].1;
    assert_eq!(workout["id"], workout_id);
    let ride_id = workout["ride"]["id"].as_str().expect("ride id");
    assert!(find(&output, &format!("performance_graph-{workout_id}.json")).len() == 1);
    let details = &find(&output, &format!("ride_details-{ride_id}.json"))[0].1;
    assert_eq!(details["ride"]["id"], ride_id);
    assert_eq!(
        details["ride"]["instructor"]["id"],
        workout["ride"]["instructor"]["id"]
    );
    assert_eq!(
        details["ride"]["live_stream_id"],
        format!("{ride_id}-live").as_str()
    );

    let just_ride = &find(&output, "workout-just-ride.json")[0].1;
    assert_eq!(just_ride["id"], workout_id);
    assert_eq!(just_ride["ride"]["id"], NIL, "the nil id is not personal");

    let instructor = &workout["ride"]["instructor"];
    assert_eq!(
        instructor["name"], details["ride"]["instructor"]["name"],
        "placeholders are consistent across files"
    );
}

#[test]
fn scrub_shifts_timestamps_and_keeps_metrics() {
    let output = scrub_files(inputs(), PARAMS);
    let workout = &find(&output, "workout-")
        .into_iter()
        .find(|(n, _)| n != "workout-just-ride.json")
        .expect("workout")
        .1;
    assert_eq!(workout["created_at"], 1_777_000_000 + SHIFT);
    assert_eq!(workout["start_time"], 1_777_000_065 + SHIFT);
    assert_eq!(workout["end_time"], 1_777_001_265 + SHIFT);
    assert_eq!(
        workout["ride"]["original_air_time"], 1_700_000_000,
        "class times are public"
    );
    assert_eq!(workout["total_work"], 123_456.7);
    assert_eq!(workout["distance"], 3.217);
    assert_eq!(workout["fitness_discipline"], "cycling");
    assert_eq!(workout["ride"]["title"], "20 min Synthetic Ride");

    let me = &find(&output, "me.json")[0].1;
    assert_eq!(me["total_workouts"], 321);
    assert_eq!(me["workout_counts"][0]["slug"], "cycling");
    assert_eq!(me["workout_counts"][0]["name"], "Cycling");

    let graph = &find(&output, "performance_graph-")[0].1;
    assert_eq!(graph["metrics"][0]["values"], json!([100, 150, 200]));
    assert_eq!(graph["seconds_since_pedaling_start"], json!([1, 2, 3]));
    let track = graph["location_data"][0]["coordinates"]
        .as_array()
        .expect("track");
    assert_eq!(track.len(), 2, "GPS track keeps its length");

    let details = &find(&output, "ride_details-")[0].1;
    assert_eq!(details["segments"]["segment_list"][0]["name"], "Warmup");
}

#[test]
fn scrub_hides_utc_offset_age_and_weight() {
    let output = scrub_files(inputs(), PARAMS);
    for (name, value) in &output {
        let mut stack = vec![value];
        while let Some(node) = stack.pop() {
            match node {
                Value::Object(map) => {
                    for (key, child) in map {
                        if let Some(utc_key) = key.strip_prefix("device_time_") {
                            assert_eq!(Some(child), map.get(utc_key), "{name}: {key}");
                        }
                        stack.push(child);
                    }
                }
                Value::Array(items) => stack.extend(items),
                _ => {}
            }
        }
    }
    let workout = &find(&output, "workout-")
        .into_iter()
        .find(|(n, _)| n != "workout-just-ride.json")
        .expect("workout")
        .1;
    assert_eq!(workout["device_time_created_at"], workout["created_at"]);
    assert!(
        workout.get("device_time_ended_at").is_none(),
        "no UTC counterpart: dropped"
    );
    assert_eq!(workout["calories"], 366.66);
    assert_eq!(workout["leaderboard_rank"], 1_000);
    assert_eq!(workout["total_leaderboard_users"], 10_000);

    let me = &find(&output, "me.json")[0].1;
    assert_eq!(me["default_max_heart_rate"], 185);
    for key in [
        "cycling_ftp",
        "cycling_workout_ftp",
        "estimated_cycling_ftp",
    ] {
        assert_eq!(me[key], 200, "{key}");
    }
    assert_eq!(workout["ftp_info"]["ftp"], 200);
    assert_eq!(me["customized_max_heart_rate"], 0, "0 means not set");
    assert_eq!(
        me["default_heart_rate_zones"],
        json!([0.0, 120.25, 138.75, 157.25, 175.75])
    );

    let graph = &find(&output, "performance_graph-")[0].1;
    let zones = &graph["metrics"][1]["zones"];
    let bounds: Vec<(u64, u64, &str)> = zones
        .as_array()
        .expect("zones")
        .iter()
        .map(|z| {
            (
                z["min_value"].as_u64().expect("min"),
                z["max_value"].as_u64().expect("max"),
                z["range"].as_str().expect("range"),
            )
        })
        .collect();
    assert_eq!(
        bounds,
        [
            (0, 119, "<120 bpm"),
            (120, 137, "120-137 bpm"),
            (138, 156, "138-156 bpm"),
            (157, 174, "157-174 bpm"),
            (175, 185, ">=175 bpm"),
        ]
    );
    assert_eq!(graph["metrics"][1]["values"], json!([140, 150, 160]));
    assert_eq!(graph["summaries"][0]["value"], 199, "output is kept");
    assert_eq!(graph["summaries"][1]["value"], 244.2);
    let point = graph["location_data"][0]["coordinates"][0]
        .as_object()
        .expect("point");
    let mut keys: Vec<&str> = point.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["latitude", "longitude"], "GPS siblings are dropped");
}

#[test]
fn scrub_recomputes_zone_durations_from_the_hr_series() {
    let output = scrub_files(inputs(), PARAMS);
    // HR 140, 150, 160 at 1 s each: zone 3 (138-156) twice, zone 4 (157-174) once.
    let expected = zone_durations([0, 0, 2, 1, 0]);
    let workout = &find(&output, "workout-")
        .into_iter()
        .find(|(n, _)| n != "workout-just-ride.json")
        .expect("workout")
        .1;
    assert_eq!(workout["total_heart_rate_zone_durations"], expected);
    assert_eq!(
        workout["effort_zones"]["heart_rate_zone_durations"],
        expected
    );
    assert_eq!(workout["effort_zones"]["total_effort_points"], Value::Null);
    assert_eq!(workout["hr_total_points"], Value::Null);
    let listed = &find(&output, "workouts-")[0].1["data"][0];
    assert_eq!(listed["total_heart_rate_zone_durations"], expected);

    let graph = &find(&output, "performance_graph-")[0].1;
    assert_eq!(graph["effort_zones"]["heart_rate_zone_durations"], expected);
    let zone_seconds: Vec<u64> = graph["metrics"][1]["zones"]
        .as_array()
        .expect("zones")
        .iter()
        .map(|z| z["duration"].as_u64().expect("duration"))
        .collect();
    assert_eq!(zone_seconds, [0, 0, 2, 1, 0]);
}

#[test]
fn scrub_zeroes_zone_durations_without_an_hr_series() {
    let workout = json!({
        "id": WORKOUT,
        "total_heart_rate_zone_durations": zone_durations([10, 20, 30, 40, 50]),
    });
    let graph = json!({"seconds_since_pedaling_start": [1, 2], "metrics": []});
    let output = scrub_files(
        vec![
            (format!("workout-{WORKOUT}.json"), workout),
            (format!("performance_graph-{WORKOUT}.json"), graph),
        ],
        PARAMS,
    );
    let workout = &find(&output, "workout-")[0].1;
    assert_eq!(
        workout["total_heart_rate_zone_durations"],
        zone_durations([0; 5])
    );
}

/// Fixed zone floors (185 bpm maximum, 0/65/75/85/95 %).
const FIXED_FLOORS: [f64; 5] = [0.0, 120.0, 138.0, 157.0, 175.0];

fn fixture(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../peloton/tests/fixtures")
        .join(name);
    serde_json::from_slice(&std::fs::read(&path).expect("read fixture")).expect("parse fixture")
}

fn recompute(graph: &Value) -> [u64; 5] {
    let offsets: Vec<u64> = graph["seconds_since_pedaling_start"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    let mut seconds = [0_u64; 5];
    let series = graph["metrics"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["slug"] == "heart_rate"));
    let Some(series) = series else { return seconds };
    for (i, bpm) in series["values"]
        .as_array()
        .expect("values")
        .iter()
        .enumerate()
    {
        let Some(bpm) = bpm.as_f64().filter(|b| *b > 0.0) else {
            continue;
        };
        let step = match (offsets.get(i), offsets.get(i + 1)) {
            (Some(a), Some(b)) if b > a => b - a,
            _ => 1,
        };
        let zone = FIXED_FLOORS.iter().rposition(|f| bpm >= *f).expect("zone");
        seconds[zone] += step;
    }
    seconds
}

fn zone_values(durations: &Value) -> Option<[u64; 5]> {
    let mut out = [0; 5];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = durations
            .get(format!("heart_rate_z{}_duration", i + 1))?
            .as_u64()?;
    }
    Some(out)
}

#[test]
fn fixture_zone_durations_match_fixed_zones() {
    let page = fixture("workouts.json");
    let mut checked = 0;
    for kind in [
        "cycling-class",
        "cycling-scenic",
        "cycling-just-ride",
        "stretching",
        "yoga",
    ] {
        let expected = recompute(&fixture(&format!("performance_graph-{kind}.json")));
        let workout = fixture(&format!("workout-{kind}.json"));
        let listed = page["data"]
            .as_array()
            .expect("data")
            .iter()
            .find(|w| w["id"] == workout["id"])
            .expect("listed")
            .clone();
        let graph = fixture(&format!("performance_graph-{kind}.json"));
        let mut found = vec![
            zone_values(&workout["total_heart_rate_zone_durations"]),
            zone_values(&workout["effort_zones"]["heart_rate_zone_durations"]),
            zone_values(&listed["total_heart_rate_zone_durations"]),
            zone_values(&listed["effort_zones"]["heart_rate_zone_durations"]),
            zone_values(&graph["effort_zones"]["heart_rate_zone_durations"]),
        ];
        if let Some(zones) = graph["metrics"]
            .as_array()
            .and_then(|m| m.iter().find(|m| m["slug"] == "heart_rate"))
            .and_then(|m| m["zones"].as_array())
        {
            let mut seconds = [0; 5];
            for (slot, zone) in seconds.iter_mut().zip(zones) {
                *slot = zone["duration"].as_u64().expect("duration");
            }
            found.push(Some(seconds));
        }
        for durations in found.into_iter().flatten() {
            assert_eq!(durations, expected, "{kind}");
            checked += 1;
        }
    }
    assert!(checked >= 5, "fixtures carry zone durations ({checked})");
}

#[test]
fn scrub_rewrites_zones_of_the_heart_rate_metric_only() {
    let power_zones = json!([
        {"slug": "zone1", "min_value": 0, "max_value": 88, "range": "<89 watts", "duration": 31},
        {"slug": "zone2", "min_value": 89, "max_value": 121, "range": "89-121 watts", "duration": 42}
    ]);
    let graph = json!({
        "seconds_since_pedaling_start": [1, 2],
        "metrics": [
            {"slug": "output", "values": [90, 95], "zones": power_zones.clone()},
            {"slug": "heart_rate", "values": [130, 131], "zones": [
                {"slug": "zone1", "min_value": 0, "max_value": 99, "range": "<100 bpm", "duration": 9},
                {"slug": "zone2", "min_value": 100, "max_value": 129, "range": "100-129 bpm", "duration": 9}
            ]}
        ]
    });
    let output = scrub_files(
        vec![(format!("performance_graph-{WORKOUT}.json"), graph)],
        PARAMS,
    );
    let graph = &output[0].1;
    assert_eq!(
        graph["metrics"][0]["zones"], power_zones,
        "power zones untouched"
    );
    let hr = &graph["metrics"][1]["zones"];
    assert_eq!(hr[0]["range"], "<120 bpm");
    assert_eq!(hr[1]["range"], "120-137 bpm");
    assert_eq!(hr[1]["duration"], 2, "130 and 131 bpm fall in fixed zone 2");
}

/// Per-user event ids embedded in scheduled-class ids.
const HOME: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeee05";
const HOME_MOVED: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeee06";
const GUEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa07";
/// 2027-01-15T08:00:00Z, the original scheduled start.
const START: i64 = 1_800_000_000;
/// Where the earliest scheduled start of a run lands (2030-01-07T12:00:00Z).
const SYNTHETIC_START: i64 = 1_894_017_600;

fn event_id(home: &str, start: i64) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(format!(
        r#"{{"home_peloton_id": "{home}", "ride_id": "{RIDE}", "scheduled_start_time": {start}, "studio_peloton_id": null, "type": "event"}}"#
    ))
}

fn iso(epoch: i64) -> String {
    let format = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    time::OffsetDateTime::from_unix_timestamp(epoch)
        .expect("timestamp")
        .format(&format)
        .expect("format")
}

fn scheduled_class(home: &str, start: i64) -> Value {
    json!({
        "__typename": "ScheduledClass",
        "id": event_id(home, start),
        "scheduledStartTime": iso(start),
        "scheduledEndTime": iso(start + 1800),
        "pelotonClass": {
            "__typename": "LiveClass",
            "classId": RIDE,
            "joinToken": event_id(home, start),
            "title": "30 min Synthetic Ride",
            "airTime": "2026-10-26T17:00:00.000Z",
            "instructor": {"name": "Synthetic Instructor"}
        },
        "scheduledUsers": {
            "totalCount": 1,
            "edges": [{"node": {"id": GUEST, "assets": {"image": {"location": "https://cdn.example.com/guest.png"}}}}]
        },
        "getInvitedUsers": {"totalCount": 1, "edges": [{"inviteUserItem": {"invitedUser": {"id": GUEST, "username": "guest_grimsby"}}}]},
        "hostUser": {"id": USER, "username": "Zorblax"}
    })
}

fn schedule_inputs() -> Vec<(String, Value)> {
    let on_demand = peloton::ids::OnDemandClassId::from_ride(&RIDE.parse().expect("ride"));
    vec![
        (
            "schedule_add.json".to_owned(),
            json!({"data": {"addClassToSchedule": scheduled_class(HOME, START)}}),
        ),
        (
            "schedule_during.json".to_owned(),
            json!({"data": {"userScheduledItemsList": {
                "__typename": "YourScheduleItemList",
                "yourScheduleItems": [scheduled_class(HOME, START)]
            }}}),
        ),
        (
            "schedule_reschedule.json".to_owned(),
            json!({"data": {"rescheduleClass": scheduled_class(HOME_MOVED, START + 3600)}}),
        ),
        (
            "schedule_remove.json".to_owned(),
            json!({"data": {"removeClassFromSchedule": {
                "__typename": "ScheduledClass",
                "id": event_id(HOME_MOVED, START + 3600)
            }}}),
        ),
        (
            "stack_add.json".to_owned(),
            json!({"data": {"addClassToStack": {"userStack": {"stackedClassList": [
                {"playOrder": 0, "pelotonClass": {"classId": RIDE, "joinToken": on_demand.as_str()}}
            ]}}}}),
        ),
        (
            "library_archived.json".to_owned(),
            json!({"data": [{
                "id": RIDE,
                "home_peloton_id": HOME,
                "join_tokens": {"on_demand": on_demand.as_str()},
                "is_favorite": true,
                "total_user_workouts": 3,
                "total_following_workouts": 2,
                "total_workouts": 1000
            }]}),
        ),
    ]
}

/// Every string of `value` that decodes as standard base64 to JSON.
fn decoded_tokens(value: &Value, out: &mut Vec<Value>) {
    use base64::Engine as _;
    match value {
        Value::String(text) => {
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(text)
                && let Ok(decoded) = serde_json::from_slice(&bytes)
            {
                out.push(decoded);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| decoded_tokens(v, out)),
        Value::Object(map) => map.values().for_each(|v| decoded_tokens(v, out)),
        _ => {}
    }
}

fn scheduled(id: &Value) -> peloton::ids::ScheduledClassId {
    id.as_str()
        .expect("id")
        .parse()
        .expect("valid scheduled id")
}

#[test]
fn scrub_re_encodes_scheduled_ids_without_personal_values() {
    let input = schedule_inputs();
    let originals = [event_id(HOME, START), event_id(HOME_MOVED, START + 3600)];
    let output = scrub_files(input, PARAMS);
    let text = render(&output);
    let mut decoded = Vec::new();
    for (_, value) in &output {
        decoded_tokens(value, &mut decoded);
    }
    let decoded_text = serde_json::to_string(&decoded).expect("render");
    assert!(decoded.len() >= 8, "class ids are kept, re-encoded");

    for leak in [HOME, HOME_MOVED, RIDE, GUEST, USER] {
        assert!(!text.contains(leak), "{leak} survived");
        assert!(!decoded_text.contains(leak), "{leak} survived inside an id");
    }
    for original in &originals {
        assert!(!text.contains(original.as_str()), "original id survived");
    }
    for leak in [
        START.to_string(),
        (START + 3600).to_string(),
        "2027-01-15".to_owned(),
        "T08:00".to_owned(),
        "guest_grimsby".to_owned(),
        "Zorblax".to_owned(),
        "cdn.example.com".to_owned(),
    ] {
        assert!(!text.contains(&leak), "{leak} survived");
        assert!(
            !decoded_text.contains(&leak),
            "{leak} survived inside an id"
        );
    }
}

#[test]
fn scrub_maps_scheduled_ids_consistently() {
    let output = scrub_files(schedule_inputs(), PARAMS);
    let added = &find(&output, "schedule_add.json")[0].1["data"]["addClassToSchedule"];
    let listed = &find(&output, "schedule_during.json")[0].1["data"]["userScheduledItemsList"]["yourScheduleItems"]
        [0];
    let moved = &find(&output, "schedule_reschedule.json")[0].1["data"]["rescheduleClass"];
    let removed = &find(&output, "schedule_remove.json")[0].1["data"]["removeClassFromSchedule"];

    assert_eq!(added["id"], listed["id"], "same original, same fake");
    assert_eq!(moved["id"], removed["id"]);
    assert_ne!(added["id"], moved["id"]);
    for class in [added, listed, moved] {
        assert_eq!(class["id"], class["pelotonClass"]["joinToken"]);
        let id = scheduled(&class["id"]);
        assert_eq!(id.ride_id().to_string(), class["pelotonClass"]["classId"]);
        assert_eq!(
            class["scheduledStartTime"],
            iso(id.scheduled_start_time().unix_timestamp())
        );
        assert_eq!(
            class["scheduledEndTime"],
            iso(id.scheduled_start_time().unix_timestamp() + 1800)
        );
        assert_eq!(
            class["pelotonClass"]["airTime"], "2026-10-26T17:00:00.000Z",
            "public class data is kept"
        );
    }
    let first = scheduled(&added["id"]).scheduled_start_time();
    assert_eq!(first.unix_timestamp(), SYNTHETIC_START);
    assert_eq!(
        scheduled(&moved["id"])
            .scheduled_start_time()
            .unix_timestamp(),
        SYNTHETIC_START + 3600,
        "relative times are kept"
    );

    let stacked = &find(&output, "stack_add.json")[0].1["data"]["addClassToStack"]["userStack"]["stackedClassList"]
        [0]["pelotonClass"];
    let ride: peloton::ids::PelotonId = stacked["classId"]
        .as_str()
        .expect("id")
        .parse()
        .expect("ride");
    assert_eq!(ride.to_string(), added["pelotonClass"]["classId"]);
    let on_demand = peloton::ids::OnDemandClassId::from_ride(&ride);
    assert_eq!(
        stacked["joinToken"],
        on_demand.as_str(),
        "Peloton's exact layout"
    );
    let library = &find(&output, "library_archived.json")[0].1["data"][0];
    assert_eq!(library["join_tokens"]["on_demand"], on_demand.as_str());
}

#[test]
fn scrub_empties_guest_lists_and_resets_personal_library_fields() {
    let output = scrub_files(schedule_inputs(), PARAMS);
    let added = &find(&output, "schedule_add.json")[0].1["data"]["addClassToSchedule"];
    assert_eq!(added["scheduledUsers"], json!({}));
    assert_eq!(added["getInvitedUsers"], json!({}));
    assert_ne!(added["hostUser"]["username"], "Zorblax");

    let library = &find(&output, "library_archived.json")[0].1["data"][0];
    assert_eq!(library["is_favorite"], false);
    assert_eq!(library["total_user_workouts"], 0);
    assert_eq!(library["total_following_workouts"], 0);
    assert_eq!(library["total_workouts"], 1000, "public counts are kept");
}

#[test]
fn scrub_redacts_class_ids_of_unexpected_shape() {
    use base64::Engine as _;
    let encode = |json: &str| base64::engine::general_purpose::STANDARD.encode(json);
    let odd = [
        encode(&format!(
            r#"{{"home_peloton_id": "{HOME}", "ride_id": "{RIDE}", "scheduled_start_time": {START}, "studio_peloton_id": null, "type": "event", "user_id": "{USER}"}}"#
        )),
        encode(&format!(
            r#"{{"home_peloton_id": "{HOME}", "ride_id": "{RIDE}", "studio_peloton_id": null, "type": "live"}}"#
        )),
        encode(&format!(
            r#"{{"home_peloton_id": "{HOME}", "ride_id": "{RIDE}", "scheduled_start_time": "{START}", "studio_peloton_id": null, "type": "event"}}"#
        )),
        encode(&format!(
            r#"{{"home_peloton_id": "Zorblax's ride", "ride_id": "{RIDE}", "studio_peloton_id": null, "type": "on_demand"}}"#
        )),
    ];
    let files = vec![("odd.json".to_owned(), json!({"ids": odd.clone()}))];
    let output = scrub_files(files, PARAMS);
    assert_eq!(
        output[0].1["ids"],
        json!(["redacted", "redacted", "redacted", "redacted"])
    );
}

#[test]
fn scrub_replaces_unparseable_schedule_times() {
    let files = vec![(
        "schedule.json".to_owned(),
        json!({"scheduledStartTime": "next Monday 8pm", "scheduledEndTime": null}),
    )];
    let output = scrub_files(files, PARAMS);
    assert_eq!(output[0].1["scheduledStartTime"], iso(SYNTHETIC_START));
    assert_eq!(output[0].1["scheduledEndTime"], Value::Null);
}

#[test]
fn scrub_moves_end_times_without_a_start() {
    let files = vec![(
        "schedule.json".to_owned(),
        json!({"items": [
            {"scheduledEndTime": iso(START + 1800)},
            {"scheduledEndTime": iso(START + 5400)}
        ]}),
    )];
    let output = scrub_files(files, PARAMS);
    assert_eq!(
        output[0].1["items"],
        json!([
            {"scheduledEndTime": iso(SYNTHETIC_START)},
            {"scheduledEndTime": iso(SYNTHETIC_START + 3600)}
        ]),
        "the earliest end time lands on the synthetic instant"
    );
}

#[test]
fn scrub_redacts_scheduled_ids_shifted_out_of_range() {
    use base64::Engine as _;
    let extreme = base64::engine::general_purpose::STANDARD.encode(format!(
        r#"{{"home_peloton_id": "{HOME}", "ride_id": "{RIDE}", "scheduled_start_time": {}, "studio_peloton_id": null, "type": "event"}}"#,
        // 9999-12-31T23:59:59Z: valid, but the shift moves it past year 9999.
        253_402_300_799_i64
    ));
    let files = vec![
        (
            "schedule.json".to_owned(),
            json!({"scheduledStartTime": iso(START)}),
        ),
        ("odd.json".to_owned(), json!({"id": extreme})),
    ];
    let output = scrub_files(files, PARAMS);
    assert_eq!(find(&output, "odd.json")[0].1["id"], "redacted");
}

/// When the synthetic class first aired (public catalogue data).
const AIR: i64 = 1_650_000_000;

fn member_and_class_inputs() -> Vec<(String, Value)> {
    let class = json!({
        "id": RIDE,
        "title": "20 min Synthetic Ride",
        "original_air_time": AIR,
        "scheduled_start_time": AIR + 300,
        "instructor": {"id": INSTRUCTOR, "name": "Marisol Okonkwo", "created_at": AIR - 86_400}
    });
    vec![
        (
            format!("workout-{WORKOUT}.json"),
            json!({
                "id": WORKOUT,
                "created_at": 1_777_000_000,
                "device_time_created_at": 1_777_000_000 + LOCAL_OFFSET,
                "start_time": 1_777_000_065,
                "end_time": 1_777_001_265,
                "ride": class.clone()
            }),
        ),
        (
            "workout-no-air-time.json".to_owned(),
            json!({
                "id": WORKOUT,
                "start_time": 1_777_000_065,
                "ride": {"id": RIDE, "created_at": AIR + 600}
            }),
        ),
        (
            format!("ride_details-{RIDE}.json"),
            json!({"ride": class.clone(), "related_rides": {"rides": [class.clone()]}}),
        ),
        (
            "library_archived.json".to_owned(),
            json!({"data": [class], "page": 0, "page_count": 1, "total": 1}),
        ),
        (
            "me.json".to_owned(),
            json!({"id": USER, "created_at": 1_311_111_111}),
        ),
    ]
}

fn numbers(value: &Value, out: &mut Vec<i64>) {
    match value {
        Value::Number(n) => out.extend(n.as_i64()),
        Value::Array(items) => items.iter().for_each(|v| numbers(v, out)),
        Value::Object(map) => map.values().for_each(|v| numbers(v, out)),
        _ => {}
    }
}

#[test]
fn scrub_shifts_member_times_but_not_class_times() {
    let other = ScrubParams {
        time_shift_s: SHIFT - 86_400 * 333 - 777,
        calorie_factor: CALORIE_FACTOR,
    };
    let mut class_times = Vec::new();
    for params in [PARAMS, other] {
        let shift = params.time_shift_s;
        let output = scrub_files(member_and_class_inputs(), params);

        let workout = &find(&output, "workout-")
            .into_iter()
            .find(|(n, _)| n != "workout-no-air-time.json")
            .expect("workout")
            .1;
        assert_eq!(workout["created_at"], 1_777_000_000 + shift);
        assert_eq!(workout["device_time_created_at"], workout["created_at"]);
        assert_eq!(workout["start_time"], 1_777_000_065 + shift);
        assert_eq!(workout["end_time"], 1_777_001_265 + shift);
        let me = &find(&output, "me.json")[0].1;
        assert_eq!(me["created_at"], 1_311_111_111 + shift);

        let details = &find(&output, "ride_details-")[0].1;
        let library = &find(&output, "library_archived.json")[0].1;
        let classes = [
            &workout["ride"],
            &details["ride"],
            &details["related_rides"]["rides"][0],
            &library["data"][0],
        ];
        for class in classes {
            assert_eq!(class["original_air_time"], AIR, "{class}");
            assert_eq!(class["scheduled_start_time"], AIR + 300, "{class}");
            assert_eq!(class["instructor"]["created_at"], AIR - 86_400, "{class}");
        }
        let bare = &find(&output, "workout-no-air-time.json")[0].1;
        assert_eq!(bare["ride"]["created_at"], AIR + 600, "a ride is a class");
        assert_eq!(bare["start_time"], 1_777_000_065 + shift);
        class_times.push(classes.map(Value::clone));

        let mut all = Vec::new();
        for (_, value) in &output {
            numbers(value, &mut all);
        }
        for public in [AIR, AIR + 300, AIR - 86_400, AIR + 600] {
            assert!(
                !all.contains(&(public + shift)),
                "a shifted class time would reveal the shift"
            );
        }
    }
    assert_eq!(
        class_times[0], class_times[1],
        "class times do not depend on the run"
    );
}
