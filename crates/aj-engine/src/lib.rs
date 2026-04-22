//! Actor-thread engine, Command/Event plumbing, and snapshot publication for art-junk.

use std::sync::Arc;
use std::thread::{self, JoinHandle};

use aj_core::{
    AppSnapshot, BrushParams, DocumentState, Edit, HistoryStatus, Sample, SampleRevision, Size,
    Stroke, StrokeId, ToolCaps,
};
use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, unbounded};

/// Command is `Clone` (not `Copy`) because `Sample` carries optional platform
/// fields that may grow. Call sites send once, so losing `Copy` is free.
#[derive(Debug, Clone)]
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
    // TODO(undoable-page-edits): page mutations are currently non-undoable. They are
    // document-level attributes and arguably belong on the history stack; revisit
    // once we have a second mutation category that needs the same treatment.
    SetPageSize(Size),
    SetShowBounds(bool),
    SetClipToBounds(bool),
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
            if let Some(stroke) = state.doc.end_stroke(id) {
                // Commit: apply AddStroke as a forward edit and record its inverse.
                let inverse = Edit::AddStroke(stroke)
                    .apply(&mut state.doc)
                    .expect("AddStroke into empty slot is infallible");
                state.history.record(inverse);
            }
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
}
