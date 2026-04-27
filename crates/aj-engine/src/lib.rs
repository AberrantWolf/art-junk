//! Actor-thread engine, Command/Event plumbing, and snapshot publication for art-junk.

use std::sync::Arc;
use std::thread::{self, JoinHandle};

use aj_core::{
    AppSnapshot, BlendMode, BrushParams, BrushType, DocumentState, Edit, HistoryStatus, LayerId,
    LinearRgba, Sample, SampleRevision, Size, Stroke, StrokeId, ToolCaps,
};
use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, unbounded};

/// Command is `Clone` (not `Copy`) because `Sample` carries optional platform
/// fields that may grow. Call sites send once, so losing `Copy` is free.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Command {
    BeginStroke {
        id: StrokeId,
        sample: Sample,
        caps: ToolCaps,
        brush: BrushParams,
    },
    AddSample {
        id: StrokeId,
        sample: Sample,
    },
    /// Update an earlier `Estimated` sample with finalized field values. Sent
    /// by platforms that deliver initial samples before the hardware has fully
    /// reported (macOS `NSEvent` tablet, iOS Pencil). Pre-commit only — the
    /// revision mutates an existing sample in the active stroke (or the most
    /// recently committed stroke, as a race-rescue), never creates history.
    ReviseSample {
        stroke_id: StrokeId,
        update_index: u64,
        revision: SampleRevision,
    },
    EndStroke {
        id: StrokeId,
    },
    /// Discard the active stroke without committing it to history. Used when
    /// chrome ownership is detected after a stroke has already begun — e.g.
    /// the pen pressed on the macOS-only path landed inside a panel-resize
    /// grab-radius that `is_pointer_over_area` misses, and egui then claimed
    /// the drag. The first sample's been rendered for a frame at most; we
    /// drop it rather than leaving a stray dot in the committed document.
    CancelStroke {
        id: StrokeId,
    },
    // TODO(undoable-page-edits): page mutations are currently non-undoable. They are
    // document-level attributes and arguably belong on the history stack; revisit
    // once we have a second mutation category that needs the same treatment.
    // Brush commands follow the same pattern for the same reasons.
    SetPageSize(Size),
    SetShowBounds(bool),
    SetClipToBounds(bool),
    /// Sets `max_width` directly; propagates proportionally into `min_width`
    /// to preserve the user's ratio. Engine is authoritative for the math.
    SetBrushMaxWidth(f32),
    /// Sets `min_width` directly; redefines the ratio.
    SetBrushMinWidth(f32),
    /// Sets `min_width` as a ratio of the current max, clamped to `[0, 1]`.
    /// Used by the min-ratio slider and `Alt+[` / `Alt+]` shortcuts.
    SetBrushMinRatio(f32),
    /// Sets the brush color. Expected to be already gamut-mapped to sRGB by
    /// the picker — the engine is not the place to decide how to land a color.
    SetBrushColor(LinearRgba),
    /// Sets the brush's type (normal, pigment, …). Affects future strokes
    /// only; in-flight strokes carry the type frozen at `BeginStroke` time.
    SetBrushType(BrushType),
    /// Append a new empty layer named `name`. Undoable.
    AddLayer {
        name: String,
    },
    /// Remove the named layer. Refused (no-op + warn) while a stroke is in
    /// flight, mirroring the Undo / Redo guard at the bottom of `apply`. The
    /// removed layer's data — strokes, name, visibility — is captured in the
    /// inverse `AddLayer` so undo restores the layer exactly. If the removed
    /// layer was active, the inverse remembers that and re-selects it on undo.
    RemoveLayer {
        id: LayerId,
    },
    /// Reorder a layer to `new_index` (clamped to current bounds). Undoable.
    MoveLayer {
        id: LayerId,
        new_index: usize,
    },
    /// Switch the active layer — where the next `BeginStroke` will commit.
    /// Pure view-state: never recorded in history.
    SetActiveLayer {
        id: LayerId,
    },
    // TODO(undoable-layer-edits): the per-layer property setters below match
    // today's non-undoable pattern (SetPageSize, etc.). Real apps put layer
    // property changes on the undo stack; revisit alongside undoable-page-edits.
    SetLayerName {
        id: LayerId,
        name: String,
    },
    SetLayerVisible {
        id: LayerId,
        visible: bool,
    },
    SetLayerOpacity {
        id: LayerId,
        opacity: f32,
    },
    SetLayerBlendMode {
        id: LayerId,
        mode: BlendMode,
    },
    Undo,
    Redo,
    Shutdown,
}

/// Linear undo/redo history of reversible `Edit`s.
///
/// Each stack stores the edit you would apply to move one step in the respective direction.
/// `past` holds inverses of already-applied forward edits (apply one to undo); `future`
/// holds forward edits produced by undoing (apply one to redo). Storing the inverse
/// at commit time means edits that destroy data (e.g. a future `RemoveStroke`) can
/// capture the destroyed payload while it's still available.
#[derive(Debug, Default)]
pub struct History {
    past: Vec<Edit>,
    future: Vec<Edit>,
}

impl History {
    /// Record that a forward edit was applied; store the inverse it produced and drop
    /// any pending redo branch.
    pub fn record(&mut self, inverse_of_applied: Edit) {
        self.past.push(inverse_of_applied);
        self.future.clear();
    }

    #[must_use]
    pub fn status(&self) -> HistoryStatus {
        HistoryStatus { can_undo: !self.past.is_empty(), can_redo: !self.future.is_empty() }
    }
}

/// Engine-owned mutable state. Exposed so integration tests can drive [`apply`]
/// synchronously without spawning the actor thread.
#[derive(Debug, Default)]
pub struct EngineState {
    pub doc: DocumentState,
    pub history: History,
}

impl EngineState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the same `AppSnapshot` the actor would publish right now. Test-friendly.
    #[must_use]
    pub fn snapshot(&self) -> AppSnapshot {
        AppSnapshot { scene: Arc::new(self.doc.snapshot()), history: self.history.status() }
    }
}

/// Outcome of applying a single command: whether the actor should stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Continue,
    Shutdown,
}

/// Apply one command against engine state. Pure with respect to wall-clock/time/IO,
/// so tests drive it directly.
#[allow(clippy::too_many_lines)] // one match arm per Command variant; splitting hurts the per-command flow.
pub fn apply(cmd: Command, state: &mut EngineState) -> ApplyOutcome {
    match cmd {
        Command::BeginStroke { id, sample, caps, brush } => {
            state.doc.begin_stroke(Stroke { id, samples: vec![sample], caps, brush });
        }
        Command::AddSample { id, sample } => {
            state.doc.add_sample(id, sample);
        }
        Command::ReviseSample { stroke_id, update_index, revision } => {
            state.doc.revise_sample(stroke_id, update_index, revision);
        }
        Command::EndStroke { id } => {
            if let Some((stroke, layer)) = state.doc.end_stroke(id) {
                // Commit: apply AddStroke as a forward edit (targeting the
                // layer that was active at BeginStroke time) and record its
                // inverse on the history stack.
                let inverse = Edit::AddStroke { stroke, layer }
                    .apply(&mut state.doc)
                    .expect("AddStroke into existing layer is infallible");
                state.history.record(inverse);
            }
        }
        Command::CancelStroke { id } => {
            // Take and drop — the active slot clears, no history entry. If
            // the id doesn't match the current active stroke, nothing
            // happens (late CancelStroke for an already-ended stroke is a
            // no-op, same as EndStroke).
            drop(state.doc.end_stroke(id));
        }
        Command::SetPageSize(size) => {
            state.doc.set_page_size(size);
        }
        Command::SetShowBounds(show) => {
            state.doc.set_show_bounds(show);
        }
        Command::SetClipToBounds(clip) => {
            state.doc.set_clip_to_bounds(clip);
        }
        Command::SetBrushMaxWidth(v) => {
            state.doc.set_brush_max_width(v);
        }
        Command::SetBrushMinWidth(v) => {
            state.doc.set_brush_min_width(v);
        }
        Command::SetBrushMinRatio(r) => {
            state.doc.set_brush_min_ratio(r);
        }
        Command::SetBrushColor(c) => {
            state.doc.set_brush_color(c);
        }
        Command::SetBrushType(brush_type) => {
            state.doc.set_brush_type(brush_type);
        }
        Command::AddLayer { name } => {
            // AddLayer is naturally expressed as an Edit so undo/redo
            // captures the layer payload identically to other reversible
            // ops. We mint the layer ourselves so the engine controls the
            // id allocation.
            let layer = aj_core::Layer::new(name);
            let index = state.doc.layers().len();
            let edit = Edit::AddLayer { layer, index, was_active: false };
            match edit.apply(&mut state.doc) {
                Ok(inverse) => state.history.record(inverse),
                Err(err) => log::warn!("AddLayer failed: {err}"),
            }
        }
        Command::RemoveLayer { id } => {
            // Refuse mid-drag — same reasoning as the Undo/Redo guard. If
            // we removed the layer the active stroke targets, EndStroke
            // would commit into a non-existent layer.
            if state.doc.has_active_stroke() {
                log::warn!("RemoveLayer ignored while a stroke is in flight");
                return ApplyOutcome::Continue;
            }
            match (Edit::RemoveLayer { id }).apply(&mut state.doc) {
                Ok(inverse) => state.history.record(inverse),
                Err(err) => log::warn!("RemoveLayer failed: {err}"),
            }
        }
        Command::MoveLayer { id, new_index } => {
            match (Edit::MoveLayer { id, new_index }).apply(&mut state.doc) {
                Ok(inverse) => state.history.record(inverse),
                Err(err) => log::warn!("MoveLayer failed: {err}"),
            }
        }
        Command::SetActiveLayer { id } => {
            state.doc.set_active_layer(id);
        }
        Command::SetLayerName { id, name } => {
            state.doc.set_layer_name(id, name);
        }
        Command::SetLayerVisible { id, visible } => {
            state.doc.set_layer_visible(id, visible);
        }
        Command::SetLayerOpacity { id, opacity } => {
            state.doc.set_layer_opacity(id, opacity);
        }
        Command::SetLayerBlendMode { id, mode } => {
            state.doc.set_layer_blend_mode(id, mode);
        }
        Command::Undo => {
            if state.doc.has_active_stroke() {
                return ApplyOutcome::Continue;
            }
            if let Some(inverse) = state.history.past.pop() {
                match inverse.apply(&mut state.doc) {
                    Ok(forward_again) => state.history.future.push(forward_again),
                    Err(err) => log::warn!("undo failed: {err}"),
                }
            }
        }
        Command::Redo => {
            if state.doc.has_active_stroke() {
                return ApplyOutcome::Continue;
            }
            if let Some(forward) = state.history.future.pop() {
                match forward.apply(&mut state.doc) {
                    Ok(inverse_again) => state.history.past.push(inverse_again),
                    Err(err) => log::warn!("redo failed: {err}"),
                }
            }
        }
        Command::Shutdown => return ApplyOutcome::Shutdown,
    }
    ApplyOutcome::Continue
}

pub struct Engine {
    tx: Sender<Command>,
    snapshot: Arc<ArcSwap<AppSnapshot>>,
    thread: Option<JoinHandle<()>>,
}

impl Engine {
    #[must_use]
    pub fn spawn() -> Self {
        let (tx, rx) = unbounded();
        let snapshot = Arc::new(ArcSwap::new(Arc::new(AppSnapshot::default())));
        let snap_for_thread = snapshot.clone();
        let thread = thread::Builder::new()
            .name("aj-engine".into())
            .spawn(move || run_actor(&rx, &snap_for_thread))
            .expect("spawn aj-engine thread");
        Self { tx, snapshot, thread: Some(thread) }
    }

    pub fn send(&self, cmd: Command) {
        if let Err(err) = self.tx.send(cmd) {
            log::warn!("aj-engine send on closed channel: {err:?}");
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Arc<AppSnapshot> {
        self.snapshot.load_full()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

fn run_actor(rx: &Receiver<Command>, snapshot: &Arc<ArcSwap<AppSnapshot>>) {
    let mut state = EngineState::new();
    while let Ok(first) = rx.recv() {
        let mut stop = matches!(apply(first, &mut state), ApplyOutcome::Shutdown);
        while let Ok(cmd) = rx.try_recv() {
            if matches!(apply(cmd, &mut state), ApplyOutcome::Shutdown) {
                stop = true;
            }
        }
        snapshot.store(Arc::new(state.snapshot()));
        if stop {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_show_bounds_updates_snapshot() {
        let mut state = EngineState::new();
        assert!(state.doc.page().show_bounds, "default is show_bounds=true");
        apply(Command::SetShowBounds(false), &mut state);
        assert!(!state.doc.page().show_bounds);
        assert!(!state.snapshot().scene.page.show_bounds);
    }

    #[test]
    fn set_clip_to_bounds_updates_snapshot() {
        let mut state = EngineState::new();
        assert!(!state.doc.page().clip_to_bounds);
        apply(Command::SetClipToBounds(true), &mut state);
        assert!(state.doc.page().clip_to_bounds);
        assert!(state.snapshot().scene.page.clip_to_bounds);
    }

    #[test]
    fn set_page_size_updates_snapshot() {
        let mut state = EngineState::new();
        apply(Command::SetPageSize(Size::new(800.0, 600.0)), &mut state);
        assert_eq!(state.snapshot().scene.page.size, Size::new(800.0, 600.0));
    }

    #[test]
    fn page_commands_do_not_touch_history() {
        let mut state = EngineState::new();
        apply(Command::SetShowBounds(false), &mut state);
        apply(Command::SetClipToBounds(true), &mut state);
        apply(Command::SetPageSize(Size::new(800.0, 600.0)), &mut state);
        let status = state.history.status();
        assert!(!status.can_undo);
        assert!(!status.can_redo);
    }

    #[test]
    fn set_brush_max_width_propagates_through_snapshot() {
        let mut state = EngineState::new();
        state.doc.set_brush(BrushParams { min_width: 2.0, max_width: 8.0, ..Default::default() });
        apply(Command::SetBrushMaxWidth(16.0), &mut state);
        let b = state.snapshot().scene.brush;
        assert!((b.max_width - 16.0).abs() < f32::EPSILON);
        assert!((b.min_width - 4.0).abs() < f32::EPSILON);
    }

    #[test]
    fn set_brush_min_width_propagates_through_snapshot() {
        let mut state = EngineState::new();
        apply(Command::SetBrushMinWidth(1.5), &mut state);
        assert!((state.snapshot().scene.brush.min_width - 1.5).abs() < f32::EPSILON);
    }

    #[test]
    fn set_brush_min_ratio_propagates_through_snapshot() {
        let mut state = EngineState::new();
        state.doc.set_brush(BrushParams { min_width: 0.5, max_width: 4.0, ..Default::default() });
        apply(Command::SetBrushMinRatio(0.75), &mut state);
        assert!((state.snapshot().scene.brush.min_width - 3.0).abs() < f32::EPSILON);
    }

    #[test]
    fn brush_commands_do_not_touch_history() {
        let mut state = EngineState::new();
        apply(Command::SetBrushMaxWidth(10.0), &mut state);
        apply(Command::SetBrushMinWidth(2.0), &mut state);
        apply(Command::SetBrushMinRatio(0.3), &mut state);
        apply(Command::SetBrushColor(LinearRgba::WHITE), &mut state);
        let status = state.history.status();
        assert!(!status.can_undo);
        assert!(!status.can_redo);
    }

    #[test]
    fn set_brush_color_propagates_through_snapshot() {
        let mut state = EngineState::new();
        let c = LinearRgba::from_srgb8([10, 200, 30, 255]);
        apply(Command::SetBrushColor(c), &mut state);
        assert_eq!(state.snapshot().scene.brush.color, c);
    }

    #[test]
    fn cancel_stroke_drops_active_without_history() {
        use aj_core::{Point, PointerId, Sample};
        use std::time::Duration;
        let mut state = EngineState::new();
        let id = StrokeId::next();
        let sample = Sample::mouse(Point::new(0.0, 0.0).into(), Duration::ZERO, PointerId::MOUSE);
        apply(
            Command::BeginStroke {
                id,
                sample,
                caps: ToolCaps::empty(),
                brush: BrushParams::default(),
            },
            &mut state,
        );
        assert!(state.doc.has_active_stroke());
        apply(Command::CancelStroke { id }, &mut state);
        assert!(!state.doc.has_active_stroke(), "cancel clears active stroke");
        let status = state.history.status();
        assert!(!status.can_undo, "cancel does not record a history entry");
        assert!(state.snapshot().scene.strokes.is_empty(), "cancelled stroke not in snapshot");
    }

    #[test]
    fn cancel_stroke_for_wrong_id_is_noop() {
        use aj_core::{Point, PointerId, Sample};
        use std::time::Duration;
        let mut state = EngineState::new();
        let id = StrokeId::next();
        let other = StrokeId::next();
        let sample = Sample::mouse(Point::new(0.0, 0.0).into(), Duration::ZERO, PointerId::MOUSE);
        apply(
            Command::BeginStroke {
                id,
                sample,
                caps: ToolCaps::empty(),
                brush: BrushParams::default(),
            },
            &mut state,
        );
        apply(Command::CancelStroke { id: other }, &mut state);
        assert!(state.doc.has_active_stroke(), "wrong-id cancel leaves stroke intact");
    }
}
