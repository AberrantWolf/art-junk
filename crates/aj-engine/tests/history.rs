//! Drives the engine's `apply` directly, without the actor thread, to verify
//! Undo/Redo/commit semantics under sequences of commands.

use std::time::Duration;

use aj_core::{
    AppSnapshot, BrushParams, LayerId, Point, PointerId, Sample, SampleClass, SampleRevision,
    Stroke, StrokeId, ToolCaps,
};
use aj_engine::{Command, EngineState, apply};

fn sample_at(x: f64, y: f64) -> Sample {
    Sample::mouse(Point::new(x, y).into(), Duration::ZERO, PointerId::MOUSE)
}

/// Flatten all strokes from all layers in the published snapshot. The active
/// mid-drag stroke is folded into its target layer's strokes by
/// `DocumentState::snapshot()`, so iterating layers picks it up too.
fn flat_strokes(snap: &AppSnapshot) -> Vec<&Stroke> {
    snap.scene.layers.iter().flat_map(|l| l.strokes.iter()).collect()
}

fn draw_one(state: &mut EngineState, id: StrokeId) {
    apply(
        Command::BeginStroke {
            id,
            sample: sample_at(0.0, 0.0),
            caps: ToolCaps::empty(),
            brush: BrushParams::default(),
        },
        state,
    );
    apply(Command::AddSample { id, sample: sample_at(1.0, 1.0) }, state);
    apply(Command::EndStroke { id }, state);
}

#[test]
fn end_stroke_commits_to_history() {
    let mut state = EngineState::new();
    draw_one(&mut state, StrokeId(1));
    let snap = state.snapshot();
    assert_eq!(flat_strokes(&snap).len(), 1);
    assert!(snap.history.can_undo);
    assert!(!snap.history.can_redo);
}

#[test]
fn undo_then_redo_restores_stroke() {
    let mut state = EngineState::new();
    draw_one(&mut state, StrokeId(1));

    apply(Command::Undo, &mut state);
    let after_undo = state.snapshot();
    assert!(flat_strokes(&after_undo).is_empty());
    assert!(!after_undo.history.can_undo);
    assert!(after_undo.history.can_redo);

    apply(Command::Redo, &mut state);
    let after_redo = state.snapshot();
    assert_eq!(flat_strokes(&after_redo).len(), 1);
    assert_eq!(flat_strokes(&after_redo)[0].id, StrokeId(1));
    assert!(after_redo.history.can_undo);
    assert!(!after_redo.history.can_redo);
}

#[test]
fn undo_during_active_stroke_is_noop() {
    let mut state = EngineState::new();
    // Start a stroke but don't end it.
    apply(
        Command::BeginStroke {
            id: StrokeId(1),
            sample: sample_at(0.0, 0.0),
            caps: ToolCaps::empty(),
            brush: BrushParams::default(),
        },
        &mut state,
    );
    apply(Command::Undo, &mut state);
    let snap = state.snapshot();
    // Active stroke still visible in published snapshot.
    assert_eq!(flat_strokes(&snap).len(), 1);
    // Nothing to undo — the active stroke wasn't committed.
    assert!(!snap.history.can_undo);
}

#[test]
fn new_commit_after_undo_truncates_redo() {
    let mut state = EngineState::new();
    draw_one(&mut state, StrokeId(1));
    apply(Command::Undo, &mut state);
    assert!(state.snapshot().history.can_redo);

    draw_one(&mut state, StrokeId(2));
    let snap = state.snapshot();
    assert_eq!(flat_strokes(&snap).len(), 1);
    assert_eq!(flat_strokes(&snap)[0].id, StrokeId(2));
    assert!(!snap.history.can_redo);
}

#[test]
fn multi_step_undo_and_redo_preserve_order() {
    let mut state = EngineState::new();
    draw_one(&mut state, StrokeId(1));
    draw_one(&mut state, StrokeId(2));
    draw_one(&mut state, StrokeId(3));

    apply(Command::Undo, &mut state);
    apply(Command::Undo, &mut state);
    let snap = state.snapshot();
    assert_eq!(flat_strokes(&snap).len(), 1);
    assert_eq!(flat_strokes(&snap)[0].id, StrokeId(1));

    apply(Command::Redo, &mut state);
    let snap = state.snapshot();
    assert_eq!(
        flat_strokes(&snap).iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![StrokeId(1), StrokeId(2)],
    );
}

#[test]
fn redo_without_prior_undo_is_noop() {
    let mut state = EngineState::new();
    draw_one(&mut state, StrokeId(1));
    apply(Command::Redo, &mut state);
    let snap = state.snapshot();
    assert_eq!(flat_strokes(&snap).len(), 1);
    assert!(!snap.history.can_redo);
}

#[test]
fn revise_sample_before_commit_is_folded_into_history_snapshot() {
    let mut state = EngineState::new();
    let id = StrokeId(1);

    // Begin with an Estimated sample tagged with update_index 17.
    let mut estimated = sample_at(0.0, 0.0);
    estimated.class = SampleClass::Estimated { update_index: 17 };
    estimated.pressure = 0.0;
    apply(
        Command::BeginStroke {
            id,
            sample: estimated,
            caps: ToolCaps::empty(),
            brush: BrushParams::default(),
        },
        &mut state,
    );

    apply(
        Command::ReviseSample {
            stroke_id: id,
            update_index: 17,
            revision: SampleRevision { pressure: Some(0.8), ..SampleRevision::default() },
        },
        &mut state,
    );

    apply(Command::AddSample { id, sample: sample_at(1.0, 1.0) }, &mut state);
    apply(Command::EndStroke { id }, &mut state);

    let snap = state.snapshot();
    assert_eq!(flat_strokes(&snap).len(), 1);
    let stroke = &flat_strokes(&snap)[0];
    assert!((stroke.samples[0].pressure - 0.8).abs() < f32::EPSILON);
    assert_eq!(stroke.samples[0].class, SampleClass::Committed);
}

#[test]
fn add_layer_command_pushes_history_and_appends_layer() {
    let mut state = EngineState::new();
    let initial_layers = state.doc.layers().len();
    apply(Command::AddLayer { name: "Layer 2".into() }, &mut state);
    let snap = state.snapshot();
    assert_eq!(snap.scene.layers.len(), initial_layers + 1);
    assert_eq!(snap.scene.layers.last().unwrap().name, "Layer 2");
    assert!(snap.history.can_undo);
}

#[test]
fn remove_layer_command_is_refused_during_active_stroke() {
    // The renderer + engine assume the active stroke's target layer still
    // exists at EndStroke time. Mid-drag RemoveLayer would violate that;
    // engine refuses (no-op + warn).
    let mut state = EngineState::new();
    apply(Command::AddLayer { name: "Layer 2".into() }, &mut state);
    let l2 = state.doc.layers().last().unwrap().id;
    apply(Command::SetActiveLayer { id: l2 }, &mut state);

    // Start a stroke into l2.
    apply(
        Command::BeginStroke {
            id: StrokeId(1),
            sample: sample_at(0.0, 0.0),
            caps: ToolCaps::empty(),
            brush: BrushParams::default(),
        },
        &mut state,
    );
    assert!(state.doc.has_active_stroke());

    let layers_before = state.doc.layers().len();
    apply(Command::RemoveLayer { id: l2 }, &mut state);
    assert_eq!(
        state.doc.layers().len(),
        layers_before,
        "RemoveLayer must be a no-op while a stroke is in flight"
    );
}

#[test]
fn begin_stroke_after_layer_switch_commits_into_new_layer() {
    let mut state = EngineState::new();
    let l1 = state.doc.active_layer();
    apply(Command::AddLayer { name: "Layer 2".into() }, &mut state);
    let l2 = state.doc.layers().last().unwrap().id;
    apply(Command::SetActiveLayer { id: l2 }, &mut state);

    draw_one(&mut state, StrokeId(1));

    // Stroke landed in l2, not l1.
    let l1_layer = state.doc.layers().iter().find(|l| l.id == l1).unwrap();
    let l2_layer = state.doc.layers().iter().find(|l| l.id == l2).unwrap();
    assert_eq!(l1_layer.strokes.len(), 0);
    assert_eq!(l2_layer.strokes.len(), 1);
}

#[test]
fn set_active_layer_with_unknown_id_is_rejected() {
    let mut state = EngineState::new();
    let bogus = LayerId::next();
    let active_before = state.doc.active_layer();
    apply(Command::SetActiveLayer { id: bogus }, &mut state);
    assert_eq!(
        state.doc.active_layer(),
        active_before,
        "active_layer must not change for an unknown id"
    );
}

#[test]
fn revise_sample_does_not_push_history_entry() {
    let mut state = EngineState::new();
    draw_one(&mut state, StrokeId(1));
    let undo_depth_before = state.snapshot().history.can_undo;

    // A revision targeting a stroke that has no Estimated samples is a no-op
    // that must not alter the history stack.
    apply(
        Command::ReviseSample {
            stroke_id: StrokeId(1),
            update_index: 999,
            revision: SampleRevision { pressure: Some(0.5), ..SampleRevision::default() },
        },
        &mut state,
    );

    let snap = state.snapshot();
    assert_eq!(snap.history.can_undo, undo_depth_before);
    assert!(!snap.history.can_redo);
}
