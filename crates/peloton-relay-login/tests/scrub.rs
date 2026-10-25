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
    assert_eq!(workout["ride"]["original_air_time"], 1_700_000_000 + SHIFT);
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
