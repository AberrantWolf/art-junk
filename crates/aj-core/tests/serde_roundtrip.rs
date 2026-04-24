//! CBOR round-trip tests for `DocumentSnapshot` and its constituent types.
//! Gated on `feature = "serde"`; `ciborium` is a dev-only dep.

#![cfg(feature = "serde")]

use std::time::Duration;

use aj_core::{
    BrushParams, DocumentSnapshot, DocumentState, Edit, LinearRgba, MixingMode, Page, PointerId,
    PressureCurve, Sample, Size, Stroke, StrokeId, ToolCaps,
};

fn cbor_roundtrip<T: serde::Serialize + serde::de::DeserializeOwned>(value: &T) -> T {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes).expect("serialize");
    ciborium::from_reader(bytes.as_slice()).expect("deserialize")
}

fn sample_at(x: f64, y: f64, t_micros: u64) -> Sample {
    Sample::mouse(
        aj_core::Point::new(x, y).into(),
        Duration::from_micros(t_micros),
        PointerId::MOUSE,
    )
}

fn snapshot_fixture() -> DocumentSnapshot {
    DocumentSnapshot {
        doc_version: 1,
        page: Page { size: Size::new(800.0, 600.0), show_bounds: true, clip_to_bounds: false },
        brush: BrushParams {
            min_width: 1.0,
            max_width: 12.0,
            curve: PressureCurve::Linear,
            color: LinearRgba::from_srgb8([200, 80, 40, 255]),
            mixing_mode: MixingMode::Additive,
        },
        strokes: vec![
            Stroke {
                id: StrokeId(1),
                samples: vec![sample_at(0.0, 0.0, 0), sample_at(10.0, 10.0, 1000)],
                caps: ToolCaps::PRESSURE,
                brush: BrushParams::default(),
            },
            Stroke {
                id: StrokeId(42),
                samples: vec![
                    sample_at(100.0, 100.0, 2000),
                    sample_at(200.0, 150.0, 3000),
                    sample_at(250.0, 180.0, 4000),
                ],
                caps: ToolCaps::PRESSURE | ToolCaps::TILT,
                brush: BrushParams { color: LinearRgba::BLACK, ..BrushParams::default() },
            },
        ],
    }
}

fn normalize_skipped_fields(snap: &mut DocumentSnapshot) {
    // Sample::pointer_id is serde(skip); reset expected to default before compare.
    for stroke in &mut snap.strokes {
        for sample in &mut stroke.samples {
            sample.pointer_id = PointerId::default();
        }
    }
}

#[test]
fn document_snapshot_roundtrips() {
    let mut expected = snapshot_fixture();
    let got: DocumentSnapshot = cbor_roundtrip(&expected);
    normalize_skipped_fields(&mut expected);
    assert_eq!(got, expected);
}

#[test]
fn document_snapshot_bytes_are_bounded() {
    let snap = snapshot_fixture();
    let mut bytes = Vec::new();
    ciborium::into_writer(&snap, &mut bytes).unwrap();
    assert!(!bytes.is_empty());
    assert!(bytes.len() < 10_000, "snapshot encoded to {} bytes", bytes.len());
}

#[test]
fn document_state_projects_and_loads() {
    let mut state = DocumentState::new();
    state.set_page_size(Size::new(400.0, 300.0));
    state.set_clip_to_bounds(true);
    state.set_brush_max_width(8.0);

    let stroke = Stroke {
        id: StrokeId(5),
        samples: vec![sample_at(1.0, 2.0, 100), sample_at(3.0, 4.0, 200)],
        caps: ToolCaps::empty(),
        brush: state.brush(),
    };
    Edit::AddStroke(stroke.clone()).apply(&mut state).unwrap();

    let snap: DocumentSnapshot = (&state).into();
    assert_eq!(snap.doc_version, 1);
    assert_eq!(snap.strokes.len(), 1);
    assert_eq!(snap.page.size, Size::new(400.0, 300.0));
    assert!(snap.page.clip_to_bounds);

    // Round-trip via CBOR, then project back to DocumentState.
    let via_cbor: DocumentSnapshot = cbor_roundtrip(&snap);
    let reloaded: DocumentState = via_cbor.into();
    assert_eq!(reloaded.committed_strokes().len(), 1);
    assert_eq!(reloaded.committed_strokes()[0].id, StrokeId(5));
    assert!(!reloaded.has_active_stroke());
}

#[test]
fn load_bumps_stroke_id_counter_past_loaded_ids() {
    let mut snap = snapshot_fixture();
    // Inject a very high id so we can observe the counter bumping.
    snap.strokes.push(Stroke {
        id: StrokeId(10_000),
        samples: vec![sample_at(0.0, 0.0, 0)],
        caps: ToolCaps::empty(),
        brush: BrushParams::default(),
    });
    let _state: DocumentState = snap.into();
    let next = StrokeId::next();
    assert!(next.0 > 10_000, "StrokeId::next() returned {next:?}, expected > 10000");
}

#[test]
fn active_stroke_is_dropped_on_save() {
    let mut state = DocumentState::new();
    let active = Stroke {
        id: StrokeId(77),
        samples: vec![sample_at(1.0, 1.0, 0)],
        caps: ToolCaps::empty(),
        brush: state.brush(),
    };
    state.begin_stroke(active);
    assert!(state.has_active_stroke());

    let snap: DocumentSnapshot = (&state).into();
    // Active stroke is not in snap.strokes — the projection commits-or-drops.
    assert!(snap.strokes.iter().all(|s| s.id != StrokeId(77)));
}

#[test]
fn every_pressure_curve_variant_roundtrips() {
    let got: PressureCurve = cbor_roundtrip(&PressureCurve::Linear);
    assert_eq!(got, PressureCurve::Linear);
}

#[test]
fn linear_rgba_roundtrips() {
    let c = LinearRgba::from_srgb8([128, 64, 32, 200]);
    let got: LinearRgba = cbor_roundtrip(&c);
    assert_eq!(got, c);
}

#[test]
fn page_roundtrips() {
    let p = Page { size: Size::new(111.0, 222.0), show_bounds: false, clip_to_bounds: true };
    let got: Page = cbor_roundtrip(&p);
    assert_eq!(got, p);
}

#[test]
fn brush_params_roundtrips() {
    let b = BrushParams {
        min_width: 0.3,
        max_width: 7.5,
        curve: PressureCurve::Linear,
        color: LinearRgba::from_srgb8([240, 120, 30, 255]),
        mixing_mode: MixingMode::Additive,
    };
    let got: BrushParams = cbor_roundtrip(&b);
    assert_eq!(got, b);
}

#[test]
fn mixing_mode_roundtrips() {
    let got: MixingMode = cbor_roundtrip(&MixingMode::Additive);
    assert_eq!(got, MixingMode::Additive);
}

/// Fence: a CBOR blob minted without the `mixing_mode` key (i.e. one produced
/// by a pre-field version of this schema) must still deserialize into a
/// current `BrushParams`, with `mixing_mode` defaulted to `Additive`. Without
/// `#[serde(default)]` on the field this test would fail with "missing field".
#[test]
fn brush_params_deserializes_legacy_blob_without_mixing_mode() {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    struct LegacyBrushParams {
        min_width: f32,
        max_width: f32,
        curve: PressureCurve,
        color: LinearRgba,
    }

    let legacy = LegacyBrushParams {
        min_width: 0.5,
        max_width: 4.0,
        curve: PressureCurve::Linear,
        color: LinearRgba::from_srgb8([0, 200, 220, 255]),
    };

    let mut bytes = Vec::new();
    ciborium::into_writer(&legacy, &mut bytes).expect("serialize legacy");
    let got: BrushParams = ciborium::from_reader(bytes.as_slice()).expect("deserialize current");

    assert_eq!(got.min_width, 0.5);
    assert_eq!(got.max_width, 4.0);
    assert_eq!(got.curve, PressureCurve::Linear);
    assert_eq!(got.color.to_srgb8(), [0, 200, 220, 255]);
    assert_eq!(got.mixing_mode, MixingMode::Additive);
}
