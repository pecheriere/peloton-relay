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
