//! Drives the engine's `apply` directly, without the actor thread, to verify
//! Undo/Redo/commit semantics under sequences of commands.

use aj_core::{Point, StrokeId};
use aj_engine::{Command, EngineState, apply};

fn pt(x: f64, y: f64) -> Point {
    Point::new(x, y)
}

fn draw_one(state: &mut EngineState, id: StrokeId) {
    apply(Command::BeginStroke { id, point: pt(0.0, 0.0) }, state);
    apply(Command::AddSample { id, point: pt(1.0, 1.0) }, state);
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
    apply(Command::BeginStroke { id: StrokeId(1), point: pt(0.0, 0.0) }, &mut state);
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
