//! W3 harness conformance: the transcript machinery itself. These vectors pin the
//! properties every later golden replay relies on — fresh-but-honest runs compare
//! equal, semantic drift fails loudly and by location, and files round-trip.

mod common;

use common::{Dir, Transcript};
use serde_json::{json, Value};

/// A scripted bilateral lifecycle, with volatile fields supplied by the caller so
/// the determinism tests can prove they are normalized away, not accidentally
/// constant.
fn scripted_exchange(message_ids: &[&str], timestamps: &[&str]) -> Transcript {
    let mut t = Transcript::new();
    let kinds = [
        ("session.open", Dir::AToB),
        ("session.features", Dir::BToA),
        ("contract.proposed", Dir::AToB),
        ("contract.accepted", Dir::BToA),
        ("contract.frozen", Dir::AToB),
        ("submission.delivered", Dir::BToA),
        ("verification.delivered", Dir::AToB),
    ];
    for (i, (kind, dir)) in kinds.iter().enumerate() {
        t.record_value(
            *dir,
            json!({
                "protocol": "HACP/2.0",
                "message_id": message_ids[i],
                "session_id": "s-fixed-0001",
                "from": if *dir == Dir::AToB { "urn:hacp:agent:a-1" } else { "urn:hacp:agent:b-1" },
                "to": if *dir == Dir::AToB { "urn:hacp:agent:b-1" } else { "urn:hacp:agent:a-1" },
                "kind": kind,
                "timestamp": timestamps[i],
                "body": {"note": "scripted"},
            }),
        );
    }
    t
}

fn fresh_ids() -> Vec<String> {
    (0..7)
        .map(|_| format!("m-{}", uuid::Uuid::new_v4().simple()))
        .collect()
}

fn fresh_timestamps() -> Vec<String> {
    (0..7).map(|i| format!("2026-09-04T11:30:{:02}Z", i)).collect()
}

fn strs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

const VOLATILE: &[&str] = &["message_id", "timestamp"];

#[test]
fn two_fresh_runs_render_byte_identical() {
    let ids_a = fresh_ids();
    let ts_a = fresh_timestamps();
    let ids_b = fresh_ids();
    let ts_b = fresh_timestamps();
    let a = scripted_exchange(&strs(&ids_a), &strs(&ts_a));
    let b = scripted_exchange(&strs(&ids_b), &strs(&ts_b));
    a.compare(&b, VOLATILE)
        .expect("fresh ids and timestamps must normalize away");
    assert_eq!(a.len(), 7);
}

#[test]
fn volatile_fields_are_replaced_by_occurrence_ordinals() {
    // Same id reused across frames keeps one ordinal; a new id gets the next.
    let ids = vec!["m-aaa", "m-bbb", "m-aaa", "m-bbb", "m-ccc", "m-ccc", "m-aaa"];
    let ts = fresh_timestamps();
    let t = scripted_exchange(&ids, &strs(&ts));
    let rendered = t.render(VOLATILE).unwrap();
    assert!(rendered.contains("<<message_id#1>>"), "in: {rendered}");
    assert!(rendered.contains("<<message_id#2>>"), "in: {rendered}");
    assert!(rendered.contains("<<message_id#3>>"), "in: {rendered}");
    assert!(!rendered.contains("m-aaa"), "raw id leaked: {rendered}");
    assert!(!rendered.contains("2026-09-04"), "raw timestamp leaked: {rendered}");
}

#[test]
fn semantic_drift_is_pinpointed_by_frame_and_shown() {
    let ids_a = fresh_ids();
    let ts_a = fresh_timestamps();
    let mut b = scripted_exchange(&strs(&fresh_ids()), &strs(&fresh_timestamps()));
    // Frame 3's body changes: contract.accepted now answers something else.
    if let Value::Object(map) = &mut b.frames_mut()[3].envelope {
        map.insert("body".into(), json!({"note": "drifted"}));
    }
    let a = scripted_exchange(&strs(&ids_a), &strs(&ts_a));
    let err = a.compare(&b, VOLATILE).expect_err("drift must fail");
    assert!(err.contains("frame 3"), "got: {err}");
    assert!(err.contains("drifted"), "got: {err}");
}

#[test]
fn structural_drift_an_extra_frame_is_reported() {
    let mut a = scripted_exchange(&strs(&fresh_ids()), &strs(&fresh_timestamps()));
    a.record_value(
        Dir::BToA,
        json!({"kind": "heartbeat", "message_id": "m-x", "timestamp": "2026-09-04T11:31:00Z"}),
    );
    let b = scripted_exchange(&strs(&fresh_ids()), &strs(&fresh_timestamps()));
    let err = b.compare(&a, VOLATILE).expect_err("length mismatch must fail");
    assert!(err.contains("frame 7"), "got: {err}");
}

#[test]
fn save_and_load_round_trip_preserves_the_render() {
    let t = scripted_exchange(&strs(&fresh_ids()), &strs(&fresh_timestamps()));
    let before = t.render(VOLATILE).unwrap();
    let dir = std::env::temp_dir().join("hacp-w3-roundtrip.jsonl");
    t.save(&dir).unwrap();
    let loaded = Transcript::load(&dir).unwrap();
    assert_eq!(loaded.render(VOLATILE).unwrap(), before);
    // Raw frames survive too: an independent implementer reading the golden sees
    // real message ids, not normalization tokens.
    let raw = std::fs::read_to_string(&dir).unwrap();
    assert!(raw.contains("\"message_id\":\"m-"), "raw ids gone: {raw}");
    assert!(!raw.contains('\n') || raw.lines().count() == 7);
}
