//! Drives the engine's `apply` directly, without the actor thread, to verify
//! Undo/Redo/commit semantics under sequences of commands.

use std::time::Duration;

use aj_core::{
    BrushParams, Point, PointerId, Sample, SampleClass, SampleRevision, StrokeId, ToolCaps,
};
use aj_engine::{Command, EngineState, apply};

fn sample_at(x: f64, y: f64) -> Sample {
    Sample::mouse(Point::new(x, y), Duration::ZERO, PointerId::MOUSE)
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
    assert_eq!(snap.scene.strokes.len(), 1);
    assert!(snap.history.can_undo);
    assert!(!snap.history.can_redo);
}

#[test]
fn undo_then_redo_restores_stroke() {
    let mut state = EngineState::new();
    draw_one(&mut state, StrokeId(1));

    apply(Command::Undo, &mut state);
    let after_undo = state.snapshot();
    assert!(after_undo.scene.strokes.is_empty());
    assert!(!after_undo.history.can_undo);
    assert!(after_undo.history.can_redo);

    apply(Command::Redo, &mut state);
    let after_redo = state.snapshot();
    assert_eq!(after_redo.scene.strokes.len(), 1);
    assert_eq!(after_redo.scene.strokes[0].id, StrokeId(1));
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
    assert_eq!(snap.scene.strokes.len(), 1);
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
    assert_eq!(snap.scene.strokes.len(), 1);
    assert_eq!(snap.scene.strokes[0].id, StrokeId(2));
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
    assert_eq!(snap.scene.strokes.len(), 1);
    assert_eq!(snap.scene.strokes[0].id, StrokeId(1));

    apply(Command::Redo, &mut state);
    let snap = state.snapshot();
    assert_eq!(
        snap.scene.strokes.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![StrokeId(1), StrokeId(2)],
    );
}

#[test]
fn redo_without_prior_undo_is_noop() {
    let mut state = EngineState::new();
    draw_one(&mut state, StrokeId(1));
    apply(Command::Redo, &mut state);
    let snap = state.snapshot();
    assert_eq!(snap.scene.strokes.len(), 1);
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
    assert_eq!(snap.scene.strokes.len(), 1);
    let stroke = &snap.scene.strokes[0];
    assert!((stroke.samples[0].pressure - 0.8).abs() < f32::EPSILON);
    assert_eq!(stroke.samples[0].class, SampleClass::Committed);
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
