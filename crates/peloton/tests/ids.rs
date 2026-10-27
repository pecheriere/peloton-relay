//! Tests for Peloton's base64 class identifiers.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peloton::ids::{OnDemandClassId, PelotonId, ScheduledClassId};

const RIDE: &str = "0123456789abcdef0123456789abcdef";
const HOME: &str = "fedcba9876543210fedcba9876543210";

/// `base64.b64encode(json.dumps({"home_peloton_id": None, "ride_id": RIDE,
/// "studio_peloton_id": None, "type": "on_demand"}).encode())` in Python.
const PYTHON_ENCODED: &str = "eyJob21lX3BlbG90b25faWQiOiBudWxsLCAicmlkZV9pZCI6ICIwMTIzNDU2Nzg5YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZiIsICJzdHVkaW9fcGVsb3Rvbl9pZCI6IG51bGwsICJ0eXBlIjogIm9uX2RlbWFuZCJ9";

fn ride() -> PelotonId {
    RIDE.parse().expect("valid ride id")
}

fn b64(json: &str) -> String {
    STANDARD.encode(json)
}

fn event_json(home: &str, ride: &str, start: &str, kind: &str) -> String {
    format!(
        r#"{{"home_peloton_id": "{home}", "ride_id": "{ride}", "scheduled_start_time": {start}, "studio_peloton_id": null, "type": "{kind}"}}"#
    )
}

#[test]
fn on_demand_id_encodes_exact_bytes() {
    let layout = format!(
        r#"{{"home_peloton_id": null, "ride_id": "{RIDE}", "studio_peloton_id": null, "type": "on_demand"}}"#
    );
    let id = OnDemandClassId::from_ride(&ride());
    assert_eq!(id.as_str(), b64(&layout));
    assert_eq!(id.as_str(), PYTHON_ENCODED);
}

#[test]
fn on_demand_id_round_trips() {
    let id = OnDemandClassId::from_ride(&ride());
    let parsed: OnDemandClassId = id.as_str().parse().expect("own encoding parses");
    assert_eq!(parsed, id);
    assert_eq!(parsed.ride_id(), &ride());

    // A differently spaced but equivalent value is accepted and kept verbatim.
    let compact = b64(&format!(
        r#"{{"home_peloton_id":null,"ride_id":"{RIDE}","studio_peloton_id":null,"type":"on_demand"}}"#
    ));
    let parsed: OnDemandClassId = compact.parse().expect("compact layout parses");
    assert_eq!(parsed.as_str(), compact);
    assert_eq!(parsed.ride_id(), &ride());
}

#[test]
fn on_demand_id_rejects_other_shapes() {
    let event = b64(&event_json(HOME, RIDE, "1700000000", "event"));
    assert!(event.parse::<OnDemandClassId>().is_err());
    let bad_ride = b64(
        r#"{"home_peloton_id": null, "ride_id": "../x", "studio_peloton_id": null, "type": "on_demand"}"#,
    );
    assert!(bad_ride.parse::<OnDemandClassId>().is_err());
    let extra = b64(&format!(
        r#"{{"home_peloton_id": null, "ride_id": "{RIDE}", "studio_peloton_id": null, "type": "on_demand", "x": 1}}"#
    ));
    assert!(extra.parse::<OnDemandClassId>().is_err());
    assert!("not base64!".parse::<OnDemandClassId>().is_err());
    assert!(b64("[]").parse::<OnDemandClassId>().is_err());
    assert!("".parse::<OnDemandClassId>().is_err());
    assert!("A".repeat(4096).parse::<OnDemandClassId>().is_err());
}

#[test]
fn scheduled_id_parses_and_rejects() {
    let raw = b64(&event_json(HOME, RIDE, "1700000000", "event"));
    let id: ScheduledClassId = raw.parse().expect("valid event id");
    assert_eq!(id.as_str(), raw);
    assert_eq!(id.ride_id(), &ride());
    assert_eq!(id.scheduled_start_time().unix_timestamp(), 1_700_000_000);

    // type on_demand
    let on_demand = OnDemandClassId::from_ride(&ride());
    assert!(on_demand.as_str().parse::<ScheduledClassId>().is_err());
    assert!(
        b64(&event_json(HOME, RIDE, "1700000000", "on_demand"))
            .parse::<ScheduledClassId>()
            .is_err()
    );
    // non-hex ids
    assert!(
        b64(&event_json("zz", RIDE, "1700000000", "event"))
            .parse::<ScheduledClassId>()
            .is_err()
    );
    assert!(
        b64(&event_json(
            HOME,
            &RIDE.to_uppercase(),
            "1700000000",
            "event"
        ))
        .parse::<ScheduledClassId>()
        .is_err()
    );
    // missing or non-integer start time
    let missing = b64(&format!(
        r#"{{"home_peloton_id": "{HOME}", "ride_id": "{RIDE}", "studio_peloton_id": null, "type": "event"}}"#
    ));
    assert!(missing.parse::<ScheduledClassId>().is_err());
    for start in ["1700000000.5", "\"1700000000\"", "null"] {
        assert!(
            b64(&event_json(HOME, RIDE, start, "event"))
                .parse::<ScheduledClassId>()
                .is_err(),
            "start {start} must be rejected"
        );
    }
    // missing home id
    let no_home = b64(&format!(
        r#"{{"home_peloton_id": null, "ride_id": "{RIDE}", "scheduled_start_time": 1700000000, "studio_peloton_id": null, "type": "event"}}"#
    ));
    assert!(no_home.parse::<ScheduledClassId>().is_err());
    // garbage base64
    assert!("%%%".parse::<ScheduledClassId>().is_err());
    assert!("eyJ".parse::<ScheduledClassId>().is_err());
    assert!(b64("not json").parse::<ScheduledClassId>().is_err());
}

#[test]
fn scheduled_id_converts_to_and_from_url_safe_base64() {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    // Valid ids are ASCII JSON without '>', '?' or '~', so their standard
    // base64 never holds '+' or '/'; padding is what the conversion removes.
    let encoded = (1_894_017_600_i64..1_894_017_700)
        .map(|start| b64(&event_json(HOME, RIDE, &start.to_string(), "event")))
        .find(|encoded| encoded.ends_with('='))
        .expect("an id with padding");
    let id: ScheduledClassId = encoded.parse().expect("valid event id");
    let url_safe = id.url_safe();
    assert!(
        url_safe
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "{url_safe}"
    );
    assert_eq!(
        URL_SAFE_NO_PAD.decode(url_safe).unwrap(),
        STANDARD.decode(&encoded).unwrap()
    );
    let back = ScheduledClassId::from_url_safe(url_safe).expect("round trip");
    assert_eq!(back.as_str(), encoded);
    assert_eq!(back, id);
}

#[test]
fn scheduled_id_from_url_safe_rejects_anything_else() {
    use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};

    let event = event_json(HOME, RIDE, "1894017600", "event");
    let on_demand = OnDemandClassId::from_ride(&ride());
    let on_demand = URL_SAFE_NO_PAD.encode(STANDARD.decode(on_demand.as_str()).unwrap());
    for bad in [
        String::new(),
        // Standard base64 and padded URL-safe base64 are not the relay's form.
        STANDARD.encode(&event),
        URL_SAFE.encode(&event),
        format!("{}!", URL_SAFE_NO_PAD.encode(&event)),
        URL_SAFE_NO_PAD.encode(event_json(HOME, RIDE, "1894017600", "on_demand")),
        URL_SAFE_NO_PAD.encode(event_json(HOME, "XYZ", "1894017600", "event")),
        URL_SAFE_NO_PAD.encode(event_json(HOME, RIDE, "\"soon\"", "event")),
        URL_SAFE_NO_PAD.encode("not json"),
        on_demand,
        "A".repeat(1024),
    ] {
        assert!(
            ScheduledClassId::from_url_safe(&bad).is_err(),
            "accepted {bad:?}"
        );
    }
}
