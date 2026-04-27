//! CBOR round-trip tests for `DocumentSnapshot` and its constituent types.
//! Gated on `feature = "serde"`; `ciborium` is a dev-only dep.

#![cfg(feature = "serde")]

use std::time::Duration;

use aj_core::{
    BlendMode, BrushParams, BrushType, DocumentSnapshot, DocumentState, Edit, Layer, LayerId,
    LinearRgba, Page, PointerId, PressureCurve, Sample, Size, Stroke, StrokeId, ToolCaps,
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

/// v2 fixture: layered shape, used by the round-trip tests after the C1
/// schema bump. v1 migration is exercised by a separate test.
fn snapshot_fixture() -> DocumentSnapshot {
    let mut layer = Layer::new("Layer 1");
    layer.strokes = vec![
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
    ];
    let active = layer.id;
    DocumentSnapshot {
        doc_version: 2,
        page: Page { size: Size::new(800.0, 600.0), show_bounds: true, clip_to_bounds: false },
        brush: BrushParams {
            min_width: 1.0,
            max_width: 12.0,
            curve: PressureCurve::Linear,
            color: LinearRgba::from_srgb8([200, 80, 40, 255]),
            brush_type: BrushType::Normal,
        },
        strokes: Vec::new(),
        layers: vec![layer],
        active_layer: Some(active),
    }
}

fn normalize_skipped_fields(snap: &mut DocumentSnapshot) {
    // Sample::pointer_id is serde(skip); reset expected to default before compare.
    for layer in &mut snap.layers {
        for stroke in &mut layer.strokes {
            for sample in &mut stroke.samples {
                sample.pointer_id = PointerId::default();
            }
        }
    }
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

    let layer = state.active_layer();
    let stroke = Stroke {
        id: StrokeId(5),
        samples: vec![sample_at(1.0, 2.0, 100), sample_at(3.0, 4.0, 200)],
        caps: ToolCaps::empty(),
        brush: state.brush(),
    };
    Edit::AddStroke { stroke: stroke.clone(), layer }.apply(&mut state).unwrap();

    let snap: DocumentSnapshot = (&state).into();
    assert_eq!(snap.doc_version, 2);
    assert_eq!(snap.layers.len(), 1);
    assert_eq!(snap.layers[0].strokes.len(), 1);
    assert!(snap.strokes.is_empty(), "v2 saves leave the legacy strokes field empty");
    assert_eq!(snap.page.size, Size::new(400.0, 300.0));
    assert!(snap.page.clip_to_bounds);

    // Round-trip via CBOR, then project back to DocumentState.
    let via_cbor: DocumentSnapshot = cbor_roundtrip(&snap);
    let reloaded: DocumentState = via_cbor.into();
    assert_eq!(reloaded.committed_strokes().count(), 1);
    assert_eq!(reloaded.committed_strokes().next().unwrap().id, StrokeId(5));
    assert!(!reloaded.has_active_stroke());
}

#[test]
fn load_bumps_stroke_id_counter_past_loaded_ids() {
    let mut snap = snapshot_fixture();
    // Inject a very high id so we can observe the counter bumping.
    snap.layers[0].strokes.push(Stroke {
        id: StrokeId(10_000),
        samples: vec![sample_at(0.0, 0.0, 0)],
        caps: ToolCaps::empty(),
        brush: BrushParams::default(),
    });
    let _state: DocumentState = snap.into();
    let next = StrokeId::next();
    assert!(next.0 > 10_000, "StrokeId::next() returned {next:?}, expected > 10000");
}

/// Mint a `LayerId` with a specific raw value. `LayerId(u64)` is a private-
/// inner newtype that only exposes `next()` for production use — but it
/// derives `serde(transparent)`, so we can fabricate a known id by
/// round-tripping the wire form (a bare u64). Used by tests that need to
/// place a specific id in the document, like the `bump_to`-on-load fence.
fn layer_id_from_raw(raw: u64) -> LayerId {
    let mut bytes = Vec::new();
    ciborium::into_writer(&raw, &mut bytes).expect("serialize raw u64");
    ciborium::from_reader(bytes.as_slice()).expect("deserialize as LayerId")
}

#[test]
fn load_bumps_layer_id_counter_past_loaded_ids() {
    // Hand-craft a snapshot whose layer carries a huge id; the loader must
    // bump the counter so subsequent `next()` calls don't collide.
    let huge = layer_id_from_raw(99_999);
    let snap = DocumentSnapshot {
        doc_version: 2,
        page: Page::default(),
        brush: BrushParams::default(),
        strokes: Vec::new(),
        layers: vec![Layer {
            id: huge,
            name: "Loaded".into(),
            strokes: Vec::new(),
            blend_mode: BlendMode::Normal,
            opacity: 1.0,
            visible: true,
        }],
        active_layer: Some(huge),
    };
    // Drive the production load path.
    let _state: DocumentState = snap.into();
    let next = LayerId::next();
    assert!(next.raw() > 99_999, "LayerId::next() returned {}, expected > 99999", next.raw());
}

#[test]
fn load_with_bogus_active_layer_falls_back_to_first_layer() {
    // Corrupt-file resilience: if `active_layer` points at a non-existent
    // layer id, the loader must fall back to `layers[0].id` rather than
    // produce a state that violates the "active_layer always valid" invariant.
    let real_layer = Layer::new("Layer 1");
    let real_id = real_layer.id;
    let bogus = layer_id_from_raw(424_242);
    let snap = DocumentSnapshot {
        doc_version: 2,
        page: Page::default(),
        brush: BrushParams::default(),
        strokes: Vec::new(),
        layers: vec![real_layer],
        active_layer: Some(bogus),
    };
    let state: DocumentState = snap.into();
    assert_eq!(state.active_layer(), real_id);
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
    // Active stroke is not in any layer's strokes — the projection commits-or-drops.
    assert!(
        snap.layers.iter().all(|l| l.strokes.iter().all(|s| s.id != StrokeId(77))),
        "active stroke leaked into save"
    );
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
        brush_type: BrushType::Normal,
    };
    let got: BrushParams = cbor_roundtrip(&b);
    assert_eq!(got, b);
}

#[test]
fn brush_type_roundtrips() {
    for mode in [BrushType::Normal, BrushType::Pigment] {
        let got: BrushType = cbor_roundtrip(&mode);
        assert_eq!(got, mode);
    }
}

#[test]
fn blend_mode_roundtrips() {
    for mode in [BlendMode::Normal, BlendMode::OklabMix] {
        let got: BlendMode = cbor_roundtrip(&mode);
        assert_eq!(got, mode);
    }
}

/// Forward-compat fence: a CBOR blob written by a hypothetical future build
/// with an unrecognised `brush_type` string (`latent_pigment` here) must
/// deserialize into `BrushType::Unknown` rather than hard-fail.
#[test]
fn unknown_brush_type_variant_falls_back_to_unknown() {
    let mut bytes = Vec::new();
    ciborium::into_writer(&"latent_pigment", &mut bytes).expect("serialize string");
    let got: BrushType = ciborium::from_reader(bytes.as_slice()).expect("deserialize unknown");
    assert_eq!(got, BrushType::Unknown);
}

/// Forward-compat fence for blend modes: same shape as the `BrushType` test.
/// A document with a future blend mode (e.g. `oklab_mix` once it ships, or
/// any string we don't recognise today) must deserialise as `Unknown`
/// rather than hard-fail.
#[test]
fn unknown_blend_mode_variant_falls_back_to_unknown() {
    let mut bytes = Vec::new();
    ciborium::into_writer(&"future_overlay", &mut bytes).expect("serialize string");
    let got: BlendMode = ciborium::from_reader(bytes.as_slice()).expect("deserialize unknown");
    assert_eq!(got, BlendMode::Unknown);
}

/// Backward-compat: a v1 document (top-level `strokes`, no `layers`) must
/// load by wrapping its strokes into a single default "Layer 1".
#[test]
fn v1_document_migrates_strokes_into_default_layer() {
    let strokes = vec![
        Stroke {
            id: StrokeId(7),
            samples: vec![sample_at(1.0, 1.0, 0)],
            caps: ToolCaps::empty(),
            brush: BrushParams::default(),
        },
        Stroke {
            id: StrokeId(8),
            samples: vec![sample_at(2.0, 2.0, 100)],
            caps: ToolCaps::empty(),
            brush: BrushParams::default(),
        },
    ];
    let v1_snap = DocumentSnapshot {
        doc_version: 1,
        page: Page::default(),
        brush: BrushParams::default(),
        strokes,
        layers: Vec::new(),
        active_layer: None,
    };
    // Round-trip via CBOR to exercise the deserialiser path (skip_serializing_if
    // would otherwise hide the strokes field if we converted directly).
    let via_cbor: DocumentSnapshot = cbor_roundtrip(&v1_snap);
    let reloaded: DocumentState = via_cbor.into();
    assert_eq!(reloaded.layers().len(), 1);
    assert_eq!(reloaded.layers()[0].name, "Layer 1");
    assert_eq!(reloaded.layers()[0].strokes.len(), 2);
    assert_eq!(reloaded.active_layer(), reloaded.layers()[0].id);
}

/// Fence: a CBOR blob minted without the `brush_type` key must still
/// deserialize into a current `BrushParams`, with `brush_type` defaulted to
/// `Normal`.
#[test]
fn brush_params_deserializes_legacy_blob_without_brush_type() {
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

    assert!((got.min_width - 0.5).abs() < f32::EPSILON);
    assert!((got.max_width - 4.0).abs() < f32::EPSILON);
    assert_eq!(got.curve, PressureCurve::Linear);
    assert_eq!(got.color.to_srgb8(), [0, 200, 220, 255]);
    assert_eq!(got.brush_type, BrushType::Normal);
}
