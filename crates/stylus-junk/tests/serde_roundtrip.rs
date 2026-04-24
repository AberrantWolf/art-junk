//! CBOR round-trip tests for the stored input types. Gated on
//! `feature = "serde"` — the feature carries the derives and `ciborium`
//! is a dev-only dep.
//!
//! Equality strategy: reset `pointer_id` on the expected side to the
//! `#[serde(skip, default)]` default before comparing. The inline
//! normalization at each assertion documents the skipped field without
//! requiring readers to consult the type definition.

#![cfg(feature = "serde")]

use std::time::Duration;

use stylus_junk::{
    PointerId, Sample, SampleClass, SampleRevision, StylusButtons, Tilt, ToolCaps, ToolKind,
};

fn cbor_roundtrip<T: serde::Serialize + serde::de::DeserializeOwned>(value: &T) -> T {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes).expect("serialize");
    ciborium::from_reader(bytes.as_slice()).expect("deserialize")
}

fn full_sample(update_index: u64) -> Sample {
    Sample {
        position: stylus_junk::Point { x: 12.5, y: -3.75 },
        timestamp: Duration::from_micros(1_234_567),
        pressure: 0.42,
        tool: ToolKind::Pen,
        buttons: StylusButtons::CONTACT | StylusButtons::BARREL,
        pointer_id: PointerId(99),
        class: SampleClass::Estimated { update_index },
        tilt: Some(Tilt { x_deg: 10.0, y_deg: -5.5 }),
        twist_deg: Some(33.0),
        tangential_pressure: Some(0.1),
        distance: Some(1.25),
        contact_size: Some(stylus_junk::Size { width: 2.0, height: 1.5 }),
    }
}

#[test]
fn sample_roundtrip_preserves_all_fields_except_pointer_id() {
    let mut expected = full_sample(7);
    let got: Sample = cbor_roundtrip(&expected);
    // pointer_id is adapter-session-scoped; serde(skip) drops it at save.
    expected.pointer_id = PointerId::default();
    assert_eq!(got, expected);
}

#[test]
fn sample_class_committed_roundtrips() {
    let mut s = full_sample(0);
    s.class = SampleClass::Committed;
    let got: Sample = cbor_roundtrip(&s);
    assert_eq!(got.class, SampleClass::Committed);
}

#[test]
fn sample_class_predicted_roundtrips() {
    let mut s = full_sample(0);
    s.class = SampleClass::Predicted;
    let got: Sample = cbor_roundtrip(&s);
    assert_eq!(got.class, SampleClass::Predicted);
}

#[test]
fn sample_class_estimated_preserves_update_index() {
    let mut s = full_sample(0);
    s.class = SampleClass::Estimated { update_index: 12345 };
    let got: Sample = cbor_roundtrip(&s);
    assert_eq!(got.class, SampleClass::Estimated { update_index: 12345 });
}

#[test]
fn every_tool_kind_roundtrips() {
    for tool in
        [ToolKind::Unknown, ToolKind::Mouse, ToolKind::Finger, ToolKind::Pen, ToolKind::Eraser]
    {
        let got: ToolKind = cbor_roundtrip(&tool);
        assert_eq!(got, tool);
    }
}

#[test]
fn tool_caps_all_flags_roundtrip() {
    let caps = ToolCaps::all();
    let got: ToolCaps = cbor_roundtrip(&caps);
    assert_eq!(got, caps);
}

#[test]
fn tool_caps_empty_roundtrips() {
    let caps = ToolCaps::empty();
    let got: ToolCaps = cbor_roundtrip(&caps);
    assert_eq!(got, caps);
}

#[test]
fn stylus_buttons_roundtrip() {
    let b = StylusButtons::CONTACT | StylusButtons::INVERTED;
    let got: StylusButtons = cbor_roundtrip(&b);
    assert_eq!(got, b);
}

#[test]
fn sample_revision_roundtrip() {
    let rev = SampleRevision {
        pressure: Some(0.75),
        tilt: Some(Tilt { x_deg: 1.0, y_deg: 2.0 }),
        twist_deg: None,
        tangential_pressure: Some(0.3),
    };
    let got: SampleRevision = cbor_roundtrip(&rev);
    assert_eq!(got, rev);
}

#[test]
fn sample_without_optional_fields_roundtrips() {
    let s = Sample::mouse(
        stylus_junk::Point { x: 1.0, y: 2.0 },
        Duration::from_millis(5),
        PointerId::MOUSE,
    );
    let mut expected = s.clone();
    let got: Sample = cbor_roundtrip(&s);
    expected.pointer_id = PointerId::default();
    assert_eq!(got, expected);
}

#[test]
fn cbor_bytes_are_nonzero_and_bounded() {
    // Sanity-check that the encoder isn't producing infinity / empty. Catches
    // accidental `Duration`-loses-to-infinity-serialization regressions.
    let s = full_sample(1);
    let mut bytes = Vec::new();
    ciborium::into_writer(&s, &mut bytes).unwrap();
    assert!(!bytes.is_empty());
    assert!(bytes.len() < 1_000, "sample encoded to {} bytes — suspiciously large", bytes.len());
}
