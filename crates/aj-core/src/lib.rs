//! Scene-graph data types and domain model for art-junk.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub mod input;

pub use input::{
    BrushParams, PointerId, PressureCurve, Sample, SampleClass, SampleRevision, StylusButtons,
    Tilt, ToolCaps, ToolKind,
};
// TODO(f32-migration): stored point coordinates are currently f64 via kurbo. We may
// want to move to an f32 newtype once `aj-format` defines a persistence schema or
// long-session memory pressure becomes real. GPU render precision is f32 regardless
// (Vello downshifts at upload), so the choice only affects CPU-side storage + math.
pub use kurbo::{Affine, Point, Size, Vec2};

/// Document page: the bounded "paper" strokes live in. Orthogonal `show_bounds` /
/// `clip_to_bounds` flags span bounded-paper, infinite-canvas, and artboard-with-bleed
/// workflows from one primitive.
#[derive(Debug, Clone, Copy)]
pub struct Page {
    pub size: Size,
    pub show_bounds: bool,
    pub clip_to_bounds: bool,
}

impl Default for Page {
    fn default() -> Self {
        Self { size: Size::new(1920.0, 1080.0), show_bounds: true, clip_to_bounds: false }
    }
}

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
    pub samples: Vec<Sample>,
    pub caps: ToolCaps,
    pub brush: BrushParams,
}

/// Read-only view of the scene published to the renderer via `ArcSwap`. Includes
/// page state so the renderer reads everything it needs from one snapshot.
#[derive(Debug, Clone, Default)]
pub struct SceneSnapshot {
    pub page: Page,
    pub strokes: Vec<Stroke>,
}

/// Authoritative mutable document state. Owned exclusively by the engine thread.
// TODO(multi-page): today's `page` is implicitly the single active page. Multi-page
// will restructure this (PageId, per-page strokes, active selection, undo scope);
// today's single field is the deliberate simple shape until that feature lands.
#[derive(Debug, Default)]
pub struct DocumentState {
    page: Page,
    strokes: Vec<Stroke>,
    active: Option<Stroke>,
}

impl DocumentState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn page(&self) -> Page {
        self.page
    }

    pub fn set_page_size(&mut self, size: Size) {
        self.page.size = size;
    }

    pub fn set_show_bounds(&mut self, show: bool) {
        self.page.show_bounds = show;
    }

    pub fn set_clip_to_bounds(&mut self, clip: bool) {
        self.page.clip_to_bounds = clip;
    }

    pub fn begin_stroke(&mut self, stroke: Stroke) {
        self.active = Some(stroke);
    }

    pub fn add_sample(&mut self, id: StrokeId, sample: Sample) {
        if let Some(active) = self.active.as_mut()
            && active.id == id
        {
            active.samples.push(sample);
        }
    }

    /// Applies a revision to an earlier sample in the named stroke. Searches
    /// the active stroke first, then falls back to the most recently committed
    /// stroke — revisions can race with `EndStroke` on single-tap inputs, and
    /// the one-stroke fallback is cheap and avoids warn-spam. The sample must
    /// carry `SampleClass::Estimated { update_index }` matching the request;
    /// on apply, the class is promoted to `Committed` so future revisions for
    /// the same index are ignored.
    pub fn revise_sample(
        &mut self,
        id: StrokeId,
        update_index: u64,
        revision: SampleRevision,
    ) -> bool {
        if let Some(active) = self.active.as_mut()
            && active.id == id
            && revise_in_stroke(active, update_index, revision)
        {
            return true;
        }
        if let Some(last) = self.strokes.last_mut()
            && last.id == id
            && revise_in_stroke(last, update_index, revision)
        {
            return true;
        }
        log::warn!(
            "revise_sample: no Estimated sample with update_index {update_index} in stroke {id:?}"
        );
        false
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
        SceneSnapshot { page: self.page, strokes }
    }
}

/// Walk a stroke's samples in reverse (revisions are almost always for the
/// newest-1 sample) and apply the revision to the first matching Estimated
/// sample found.
fn revise_in_stroke(stroke: &mut Stroke, update_index: u64, revision: SampleRevision) -> bool {
    for sample in stroke.samples.iter_mut().rev() {
        if matches!(sample.class, SampleClass::Estimated { update_index: i } if i == update_index) {
            revision.apply_to(sample);
            sample.class = SampleClass::Committed;
            return true;
        }
    }
    false
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
    use std::time::Duration;

    use super::*;

    fn sample_at(x: f64, y: f64) -> Sample {
        Sample::mouse(Point::new(x, y), Duration::ZERO, PointerId::MOUSE)
    }

    fn stroke(id: u64, points: &[(f64, f64)]) -> Stroke {
        Stroke {
            id: StrokeId(id),
            samples: points.iter().map(|&(x, y)| sample_at(x, y)).collect(),
            caps: ToolCaps::empty(),
            brush: BrushParams::default(),
        }
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
                assert_eq!(restored.samples.len(), s.samples.len());
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
    fn default_page_is_1920x1080_bounded_unclipped() {
        let page = Page::default();
        assert_eq!(page.size, Size::new(1920.0, 1080.0));
        assert!(page.show_bounds);
        assert!(!page.clip_to_bounds);
    }

    #[test]
    fn snapshot_carries_page_state() {
        let mut doc = DocumentState::new();
        doc.set_show_bounds(false);
        doc.set_clip_to_bounds(true);
        doc.set_page_size(Size::new(800.0, 600.0));
        let snap = doc.snapshot();
        assert_eq!(snap.page.size, Size::new(800.0, 600.0));
        assert!(!snap.page.show_bounds);
        assert!(snap.page.clip_to_bounds);
    }

    #[test]
    fn end_stroke_returns_active_and_clears() {
        let mut doc = DocumentState::new();
        doc.begin_stroke(stroke(7, &[(0.0, 0.0)]));
        doc.add_sample(StrokeId(7), sample_at(1.0, 1.0));
        let s = doc.end_stroke(StrokeId(7)).expect("active stroke");
        assert_eq!(s.samples.len(), 2);
        assert!(!doc.has_active_stroke());
    }

    fn estimated_sample_at(x: f64, y: f64, update_index: u64, pressure: f32) -> Sample {
        let mut s = Sample::mouse(Point::new(x, y), Duration::ZERO, PointerId::MOUSE);
        s.class = SampleClass::Estimated { update_index };
        s.pressure = pressure;
        s
    }

    #[test]
    fn revise_sample_updates_active_stroke_and_promotes_to_committed() {
        let mut doc = DocumentState::new();
        let s = Stroke {
            id: StrokeId(1),
            samples: vec![estimated_sample_at(0.0, 0.0, 42, 0.0)],
            caps: ToolCaps::empty(),
            brush: BrushParams::default(),
        };
        doc.begin_stroke(s);
        let revision = SampleRevision { pressure: Some(0.75), ..SampleRevision::default() };

        assert!(doc.revise_sample(StrokeId(1), 42, revision));

        let active = doc.active.as_ref().expect("active stroke");
        assert!((active.samples[0].pressure - 0.75).abs() < f32::EPSILON);
        assert_eq!(active.samples[0].class, SampleClass::Committed);
    }

    #[test]
    fn revise_sample_falls_back_to_last_committed_stroke() {
        let mut doc = DocumentState::new();
        let s = Stroke {
            id: StrokeId(1),
            samples: vec![estimated_sample_at(0.0, 0.0, 99, 0.0)],
            caps: ToolCaps::empty(),
            brush: BrushParams::default(),
        };
        Edit::AddStroke(s).apply(&mut doc).unwrap();
        let revision = SampleRevision { pressure: Some(0.4), ..SampleRevision::default() };

        assert!(doc.revise_sample(StrokeId(1), 99, revision));
        assert!((doc.strokes[0].samples[0].pressure - 0.4).abs() < f32::EPSILON);
        assert_eq!(doc.strokes[0].samples[0].class, SampleClass::Committed);
    }

    #[test]
    fn revise_sample_is_no_op_when_update_index_missing() {
        let mut doc = DocumentState::new();
        doc.begin_stroke(stroke(1, &[(0.0, 0.0)])); // Committed sample, no Estimated
        assert!(!doc.revise_sample(StrokeId(1), 99, SampleRevision::default()));
    }

    #[test]
    fn revise_sample_second_revision_is_ignored() {
        let mut doc = DocumentState::new();
        let s = Stroke {
            id: StrokeId(1),
            samples: vec![estimated_sample_at(0.0, 0.0, 7, 0.0)],
            caps: ToolCaps::empty(),
            brush: BrushParams::default(),
        };
        doc.begin_stroke(s);
        let r1 = SampleRevision { pressure: Some(0.5), ..SampleRevision::default() };
        let r2 = SampleRevision { pressure: Some(0.9), ..SampleRevision::default() };

        assert!(doc.revise_sample(StrokeId(1), 7, r1));
        // Second revision finds no Estimated sample (was promoted to Committed).
        assert!(!doc.revise_sample(StrokeId(1), 7, r2));

        let active = doc.active.as_ref().unwrap();
        assert!((active.samples[0].pressure - 0.5).abs() < f32::EPSILON);
    }
}
