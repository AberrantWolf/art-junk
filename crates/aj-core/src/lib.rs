//! Scene-graph data types and domain model for art-junk.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub use kurbo::Point;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StrokeId(pub u64);

impl StrokeId {
    #[must_use]
    pub fn next() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Debug, Clone)]
pub struct Stroke {
    pub id: StrokeId,
    pub points: Vec<Point>,
}

/// Read-only view of the scene published to the renderer via `ArcSwap`.
#[derive(Debug, Clone, Default)]
pub struct SceneSnapshot {
    pub strokes: Vec<Stroke>,
}

/// Authoritative mutable document state. Owned exclusively by the engine thread.
#[derive(Debug, Default)]
pub struct DocumentState {
    strokes: Vec<Stroke>,
    active: Option<Stroke>,
}

impl DocumentState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin_stroke(&mut self, stroke: Stroke) {
        self.active = Some(stroke);
    }

    pub fn add_sample(&mut self, id: StrokeId, point: Point) {
        if let Some(active) = self.active.as_mut()
            && active.id == id
        {
            active.points.push(point);
        }
    }

    /// Finalizes and returns the active stroke if its id matches.
    pub fn end_stroke(&mut self, id: StrokeId) -> Option<Stroke> {
        match self.active.as_ref() {
            Some(s) if s.id == id => self.active.take(),
            _ => None,
        }
    }

    #[must_use]
    pub fn has_active_stroke(&self) -> bool {
        self.active.is_some()
    }

    /// Builds a `SceneSnapshot` that includes the active (uncommitted) stroke, so the
    /// drawing appears live while the user is still dragging.
    #[must_use]
    pub fn snapshot(&self) -> SceneSnapshot {
        let mut strokes = self.strokes.clone();
        if let Some(active) = &self.active {
            strokes.push(active.clone());
        }
        SceneSnapshot { strokes }
    }
}

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum EditError {
    #[error("stroke {0:?} not found")]
    Missing(StrokeId),
}

/// A reversible unit of document change. `apply` consumes `self` and returns the concrete
/// inverse it computed against the live state — so e.g. `RemoveStroke(id)` returns
/// `AddStroke(stroke_data_that_was_removed)`, capturing the data before it's gone.
#[derive(Debug, Clone)]
pub enum Edit {
    AddStroke(Stroke),
    RemoveStroke(StrokeId),
}

impl Edit {
    pub fn apply(self, doc: &mut DocumentState) -> Result<Edit, EditError> {
        match self {
            Edit::AddStroke(stroke) => {
                let id = stroke.id;
                doc.strokes.push(stroke);
                Ok(Edit::RemoveStroke(id))
            }
            Edit::RemoveStroke(id) => {
                let Some(idx) = doc.strokes.iter().position(|s| s.id == id) else {
                    return Err(EditError::Missing(id));
                };
                let stroke = doc.strokes.remove(idx);
                Ok(Edit::AddStroke(stroke))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct HistoryStatus {
    pub can_undo: bool,
    pub can_redo: bool,
}

/// Published snapshot consumed by both the renderer (via `scene`) and the UI (via
/// `history`). Kept as one struct so the actor publishes once per frame; the renderer
/// and UI each read whichever field they need.
#[derive(Debug, Clone, Default)]
pub struct AppSnapshot {
    pub scene: Arc<SceneSnapshot>,
    pub history: HistoryStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(id: u64, points: &[(f64, f64)]) -> Stroke {
        Stroke { id: StrokeId(id), points: points.iter().map(|&(x, y)| Point::new(x, y)).collect() }
    }

    #[test]
    fn add_then_remove_is_roundtrip() {
        let mut doc = DocumentState::new();
        let s = stroke(1, &[(0.0, 0.0), (1.0, 1.0)]);
        let undo = Edit::AddStroke(s.clone()).apply(&mut doc).unwrap();
        assert_eq!(doc.strokes.len(), 1);
        let redo = undo.apply(&mut doc).unwrap();
        assert!(doc.strokes.is_empty());
        // Redo should contain the original stroke data (captured during remove).
        match redo {
            Edit::AddStroke(restored) => {
                assert_eq!(restored.id, s.id);
                assert_eq!(restored.points.len(), s.points.len());
            }
            Edit::RemoveStroke(_) => panic!("expected AddStroke as inverse of remove"),
        }
    }

    #[test]
    fn double_undo_via_returned_inverse_is_stable() {
        let mut doc = DocumentState::new();
        let inv1 = Edit::AddStroke(stroke(1, &[(0.0, 0.0)])).apply(&mut doc).unwrap();
        let inv2 = Edit::AddStroke(stroke(2, &[(1.0, 1.0)])).apply(&mut doc).unwrap();
        assert_eq!(doc.strokes.len(), 2);
        // Undo in reverse order, as a history stack would.
        let _redo2 = inv2.apply(&mut doc).unwrap();
        let _redo1 = inv1.apply(&mut doc).unwrap();
        assert!(doc.strokes.is_empty());
    }

    #[test]
    fn remove_missing_returns_typed_error() {
        let mut doc = DocumentState::new();
        let result = Edit::RemoveStroke(StrokeId(42)).apply(&mut doc);
        assert_eq!(result.unwrap_err(), EditError::Missing(StrokeId(42)));
    }

    #[test]
    fn snapshot_includes_active_stroke() {
        let mut doc = DocumentState::new();
        doc.begin_stroke(stroke(7, &[(0.0, 0.0)]));
        let snap = doc.snapshot();
        assert_eq!(snap.strokes.len(), 1);
        assert_eq!(snap.strokes[0].id, StrokeId(7));
    }

    #[test]
    fn end_stroke_returns_active_and_clears() {
        let mut doc = DocumentState::new();
        doc.begin_stroke(stroke(7, &[(0.0, 0.0)]));
        doc.add_sample(StrokeId(7), Point::new(1.0, 1.0));
        let s = doc.end_stroke(StrokeId(7)).expect("active stroke");
        assert_eq!(s.points.len(), 2);
        assert!(!doc.has_active_stroke());
    }
}
