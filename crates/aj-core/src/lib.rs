//! Scene-graph data types and domain model for art-junk.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub mod input;
pub mod layer;
pub mod pigment;

pub use input::{BrushParams, BrushType, LinearRgba, MAX_WIDTH_MAX, MAX_WIDTH_MIN, PressureCurve};
pub use layer::{BlendMode, Layer, LayerId};
pub use pigment::{KubelkaMunk, MixOp, Pigment};
// Input-sample types live in `stylus-junk`; re-export so existing aj-core
// consumers keep working with their current import paths.
pub use stylus_junk::{
    PointerId, Sample, SampleClass, SampleRevision, StylusButtons, Tilt, ToolCaps, ToolKind,
};
// TODO(f32-migration): stored point coordinates are currently f64 via kurbo. We may
// want to move to an f32 newtype once `aj-format` defines a persistence schema or
// long-session memory pressure becomes real. GPU render precision is f32 regardless
// (Vello downshifts at upload), so the choice only affects CPU-side storage + math.
pub use kurbo::{Affine, Point, Size, Vec2};

/// Document page: the bounded "paper" strokes live in. Orthogonal `show_bounds` /
/// `clip_to_bounds` flags span bounded-paper, infinite-canvas, and artboard-with-bleed
/// workflows from one primitive.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
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
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct StrokeId(pub u64);

static STROKE_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

impl StrokeId {
    #[must_use]
    pub fn next() -> Self {
        Self(STROKE_ID_COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    /// Raise the global counter so subsequent `StrokeId::next()` returns a
    /// value strictly greater than `target`. Used by the loader after
    /// deserializing a document, so new strokes don't collide with loaded
    /// ones. No-op if the counter is already ahead.
    pub fn bump_to(target: u64) {
        STROKE_ID_COUNTER.fetch_max(target.saturating_add(1), Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub struct Stroke {
    pub id: StrokeId,
    pub samples: Vec<Sample>,
    pub caps: ToolCaps,
    pub brush: BrushParams,
}

/// Read-only view of the scene published to the renderer via `ArcSwap`. Includes
/// page state so the renderer reads everything it needs from one snapshot.
///
/// **C1 transitional shape**: the snapshot carries both `layers` (the new
/// source of truth) AND a flattened `strokes` view (visible-layer strokes
/// concatenated bottom-to-top, plus the active mid-drag stroke). Today's
/// renderer reads `strokes`; C2 cuts it over to `layers` and the flat field
/// goes away. Engine cost is one extra Vec clone per snapshot — acceptable
/// for one phase of churn-bounding.
#[derive(Debug, Clone, Default)]
pub struct SceneSnapshot {
    pub page: Page,
    /// The *live* document brush — what a fresh stroke will be stamped with on
    /// the next `BeginStroke`. The UI reads this to populate slider values.
    /// The renderer does NOT read this; each `Stroke` carries its own
    /// `brush` frozen at `BeginStroke` time.
    pub brush: BrushParams,
    /// Flat view of visible-layer strokes plus any active mid-drag stroke.
    /// **Transitional in C1** — the renderer reads this; C2 deletes it in
    /// favour of iterating `layers`.
    pub strokes: Vec<Stroke>,
    /// Full per-layer state. Empty `Vec` is a valid "no layers yet" state
    /// only for the `Default` impl; once a `DocumentState` exists, snapshot
    /// will always populate at least one layer.
    pub layers: Vec<Layer>,
    /// Which layer the next `BeginStroke` will target. UI reads this to
    /// highlight the active row in the layers panel (C3).
    pub active_layer: LayerId,
}

/// Authoritative mutable document state. Owned exclusively by the engine thread.
// TODO(multi-page): today's `page` is implicitly the single active page. Multi-page
// will restructure this (PageId, per-page strokes, active selection, undo scope);
// today's single field is the deliberate simple shape until that feature lands.
#[derive(Debug)]
pub struct DocumentState {
    page: Page,
    brush: BrushParams,
    /// Layered storage. Invariant: `!layers.is_empty()` once `new()` has run;
    /// `Default` produces a single "Layer 1" so the invariant holds from
    /// first construction.
    layers: Vec<Layer>,
    /// Always points to a layer in `layers`. If the active layer is removed,
    /// this falls back to a sibling — see `remove_layer`.
    active_layer: LayerId,
    /// Mid-drag stroke (the one currently being drawn). Frozen at
    /// `BeginStroke` time, drained by `EndStroke` or `CancelStroke`.
    active: Option<Stroke>,
    /// Layer the active stroke targets. Captured at `BeginStroke` time so
    /// `EndStroke` commits to the layer the user was painting in even if
    /// `set_active_layer` was called mid-drag — same live-vs-frozen rule
    /// the brush params follow.
    active_stroke_layer: Option<LayerId>,
}

impl Default for DocumentState {
    fn default() -> Self {
        let initial = Layer::new("Layer 1");
        let active_layer = initial.id;
        Self {
            page: Page::default(),
            brush: BrushParams::default(),
            layers: vec![initial],
            active_layer,
            active: None,
            active_stroke_layer: None,
        }
    }
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

    #[must_use]
    pub fn brush(&self) -> BrushParams {
        self.brush
    }

    pub fn set_brush(&mut self, brush: BrushParams) {
        self.brush = brush;
    }

    /// Set the maximum width and propagate proportionally to `min_width`
    /// so the user-perceived ratio (`min/max`) is preserved exactly across
    /// the change. The ratio is the primary cognitive state; the absolute
    /// `min_width` is effectively a cache. No floor on the computed min —
    /// vector rendering handles sub-pixel widths.
    pub fn set_brush_max_width(&mut self, v: f32) {
        let old_max = self.brush.max_width;
        let old_min = self.brush.min_width;
        let new_max = v.clamp(input::MAX_WIDTH_MIN, input::MAX_WIDTH_MAX);
        // Guard against a divide-by-zero that can't happen given the clamp on
        // old_max, but belt-and-suspenders.
        let ratio = if old_max > 0.0 { old_min / old_max } else { 1.0 };
        self.brush.max_width = new_max;
        self.brush.min_width = ratio * new_max;
    }

    /// Set the minimum width directly (redefines the ratio). Clamped to
    /// `[0.0, current_max]`.
    pub fn set_brush_min_width(&mut self, v: f32) {
        let max = self.brush.max_width;
        self.brush.min_width = v.clamp(0.0, max);
    }

    /// Set the minimum as a ratio of the current max. `ratio` is clamped to
    /// `[0.0, 1.0]`. Used by the min-ratio slider and the `Alt+[` / `Alt+]`
    /// shortcuts; no drift across successive ratio edits.
    pub fn set_brush_min_ratio(&mut self, ratio: f32) {
        let ratio = ratio.clamp(0.0, 1.0);
        self.brush.min_width = ratio * self.brush.max_width;
    }

    /// Set the brush's color. The picker hands in a gamut-mapped `LinearRgba`;
    /// the engine does no further validation — clamp lives at the boundary.
    pub fn set_brush_color(&mut self, color: LinearRgba) {
        self.brush.color = color;
    }

    /// Set the brush's type. Affects only future strokes — in-flight strokes
    /// carry their `Stroke::brush.brush_type` frozen at `BeginStroke` time,
    /// mirroring the live-vs-frozen pattern that already applies to width and
    /// color.
    pub fn set_brush_type(&mut self, brush_type: BrushType) {
        self.brush.brush_type = brush_type;
    }

    /// All layers, in z-order (index 0 is bottom).
    #[must_use]
    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    /// The current active layer id — where `BeginStroke` will target next.
    #[must_use]
    pub fn active_layer(&self) -> LayerId {
        self.active_layer
    }

    /// Lookup a layer by id. Returns `None` if no such layer exists.
    #[must_use]
    pub fn layer(&self, id: LayerId) -> Option<&Layer> {
        self.layers.iter().find(|l| l.id == id)
    }

    /// Set the active layer. Returns `true` if the id was valid (and the
    /// active layer was changed); `false` if the id was unknown.
    pub fn set_active_layer(&mut self, id: LayerId) -> bool {
        if self.layers.iter().any(|l| l.id == id) {
            self.active_layer = id;
            true
        } else {
            log::warn!("set_active_layer: unknown LayerId {id:?}");
            false
        }
    }

    /// Append a new empty layer above the current top. Returns the id.
    /// **Non-undoable on its own**; the engine wraps `Edit::AddLayer` for
    /// undoable variants.
    pub fn add_layer(&mut self, name: impl Into<String>) -> LayerId {
        let layer = Layer::new(name);
        let id = layer.id;
        self.layers.push(layer);
        id
    }

    /// Set a layer's name. Returns `true` if the layer existed.
    pub fn set_layer_name(&mut self, id: LayerId, name: String) -> bool {
        if let Some(l) = self.layers.iter_mut().find(|l| l.id == id) {
            l.name = name;
            true
        } else {
            false
        }
    }

    /// Set a layer's visibility. Returns `true` if the layer existed.
    pub fn set_layer_visible(&mut self, id: LayerId, visible: bool) -> bool {
        if let Some(l) = self.layers.iter_mut().find(|l| l.id == id) {
            l.visible = visible;
            true
        } else {
            false
        }
    }

    /// Set a layer's opacity (clamped to `[0.0, 1.0]`). Returns `true` if
    /// the layer existed.
    pub fn set_layer_opacity(&mut self, id: LayerId, opacity: f32) -> bool {
        if let Some(l) = self.layers.iter_mut().find(|l| l.id == id) {
            l.opacity = opacity.clamp(0.0, 1.0);
            true
        } else {
            false
        }
    }

    /// Set a layer's blend mode. Returns `true` if the layer existed.
    pub fn set_layer_blend_mode(&mut self, id: LayerId, mode: BlendMode) -> bool {
        if let Some(l) = self.layers.iter_mut().find(|l| l.id == id) {
            l.blend_mode = mode;
            true
        } else {
            false
        }
    }

    pub fn begin_stroke(&mut self, stroke: Stroke) {
        self.active_stroke_layer = Some(self.active_layer);
        self.active = Some(stroke);
    }

    pub fn add_sample(&mut self, id: StrokeId, sample: Sample) {
        if let Some(active) = self.active.as_mut()
            && active.id == id
        {
            active.samples.push(sample);
        }
    }

    /// Applies a revision to an earlier sample in the named stroke.
    ///
    /// Searches the active stroke first; on miss, falls back to scanning
    /// every layer's strokes (revisions can race with `EndStroke` on
    /// single-tap inputs, so the just-committed stroke might already be
    /// in a layer). The cross-layer scan walks layers and their strokes
    /// in reverse since the relevant stroke is almost always the most
    /// recently committed one.
    ///
    /// The sample must carry `SampleClass::Estimated { update_index }`
    /// matching the request; on apply, the class is promoted to
    /// `Committed` so future revisions for the same index are ignored.
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
        // Cross-layer scan, newest-first. We only check each layer's last
        // stroke because the race-rescue case is "EndStroke just happened";
        // older strokes won't get late-arriving revisions.
        for layer in self.layers.iter_mut().rev() {
            if let Some(last) = layer.strokes.last_mut()
                && last.id == id
                && revise_in_stroke(last, update_index, revision)
            {
                return true;
            }
        }
        log::warn!(
            "revise_sample: no Estimated sample with update_index {update_index} in stroke {id:?}"
        );
        false
    }

    /// Finalizes and returns the active stroke if its id matches, along
    /// with the layer it should commit to. The caller (engine) wraps the
    /// stroke + layer in `Edit::AddStroke`.
    pub fn end_stroke(&mut self, id: StrokeId) -> Option<(Stroke, LayerId)> {
        let matches = matches!(self.active.as_ref(), Some(s) if s.id == id);
        if !matches {
            return None;
        }
        let stroke = self.active.take()?;
        // active_stroke_layer is set in begin_stroke; if it isn't (shouldn't
        // happen), fall back to current active_layer.
        let target = self.active_stroke_layer.take().unwrap_or(self.active_layer);
        Some((stroke, target))
    }

    #[must_use]
    pub fn has_active_stroke(&self) -> bool {
        self.active.is_some()
    }

    /// Builds a `SceneSnapshot` that includes the active (uncommitted) stroke
    /// in the flat `strokes` view, so the drawing appears live while the
    /// user is still dragging. The active stroke is placed in the active
    /// layer's slot in the `layers` snapshot too, mirroring how the renderer
    /// will see it once C2 cuts over.
    ///
    /// **Visibility quirk for the active stroke**: if the user hides the
    /// active stroke's target layer mid-drag, the *committed* strokes on
    /// that layer disappear from the flat view (correct), but the active
    /// stroke itself stays visible — hiding the layer shouldn't make a
    /// stroke-in-progress vanish under the user's pen. Treated as "in-flight
    /// preview always renders" rather than a layer-level state thing.
    #[must_use]
    pub fn snapshot(&self) -> SceneSnapshot {
        let mut layers = self.layers.clone();
        if let Some(active) = &self.active {
            let target = self.active_stroke_layer.unwrap_or(self.active_layer);
            if let Some(layer) = layers.iter_mut().find(|l| l.id == target) {
                layer.strokes.push(active.clone());
            }
        }
        // Flat view: visible-layer committed strokes, bottom-to-top, then
        // the active stroke on top regardless of its layer's visibility
        // (see doc-comment quirk above). We re-flatten from `self.layers`
        // (not `layers`) so the active stroke isn't double-counted on the
        // visible path.
        let mut strokes: Vec<Stroke> = self
            .layers
            .iter()
            .filter(|l| l.visible)
            .flat_map(|l| l.strokes.iter().cloned())
            .collect();
        if let Some(active) = &self.active {
            strokes.push(active.clone());
        }
        SceneSnapshot {
            page: self.page,
            brush: self.brush,
            strokes,
            layers,
            active_layer: self.active_layer,
        }
    }

    /// Iterator over all committed strokes across all layers, bottom-to-top.
    /// Excludes the active (mid-drag) stroke. Zero-cost — borrows from
    /// `self`.
    pub fn committed_strokes(&self) -> impl Iterator<Item = &Stroke> + '_ {
        self.layers.iter().flat_map(|l| l.strokes.iter())
    }
}

/// On-disk projection of a `DocumentState`. Distinct from both the runtime
/// state (has private fields + invariants) and the renderer's `SceneSnapshot`
/// (includes the active mid-drag stroke). Saving is a validating projection:
/// the active stroke is dropped, undo history is dropped, the live brush is
/// preserved, layer state persists in full, and the snapshot carries a
/// version tag for future migrations.
///
/// Round-trip is lossy by design — reloading a saved document gives you the
/// committed strokes, layer state, and live brush setup; in-flight gesture
/// state and undo history do not persist.
///
/// **Wire shape v1 vs v2**: v1 carried a top-level `strokes` field with no
/// layer concept. v2 carries `layers` + `active_layer`. The struct holds
/// both — `strokes` is `#[serde(default, skip_serializing_if = "...")]` so
/// v1 docs deserialise cleanly into the same struct; the loader migrates
/// to layers in `From<DocumentSnapshot> for DocumentState`.
#[cfg(feature = "serde")]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DocumentSnapshot {
    /// Wire version. Bump when a load-time migration is needed; `2` today.
    /// A loader that encounters an unknown version should warn and attempt
    /// best-effort parse rather than hard-fail.
    pub doc_version: u32,
    pub page: Page,
    /// The *live* brush — what a new stroke would be stamped with. User-
    /// visible state (slider positions); preserved across save/load.
    pub brush: BrushParams,
    /// **v1 path only.** Pre-layer documents stored strokes here. v2 saves
    /// always emit an empty `strokes` and populate `layers` instead, so
    /// `skip_serializing_if = "Vec::is_empty"` keeps the wire clean.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub strokes: Vec<Stroke>,
    /// **v2 path.** Layered storage. Empty on v1 documents (loader migrates).
    #[serde(default)]
    pub layers: Vec<Layer>,
    /// **v2 path.** Active layer id; `None` on v1 documents or if missing
    /// in the wire form. Loader resolves to `layers[0].id`.
    #[serde(default)]
    pub active_layer: Option<LayerId>,
}

#[cfg(feature = "serde")]
const CURRENT_DOC_VERSION: u32 = 2;

#[cfg(feature = "serde")]
impl From<&DocumentState> for DocumentSnapshot {
    fn from(state: &DocumentState) -> Self {
        Self {
            doc_version: CURRENT_DOC_VERSION,
            page: state.page,
            brush: state.brush,
            strokes: Vec::new(),
            layers: state.layers.clone(),
            active_layer: Some(state.active_layer),
        }
    }
}

#[cfg(feature = "serde")]
impl From<DocumentSnapshot> for DocumentState {
    fn from(snap: DocumentSnapshot) -> Self {
        if snap.doc_version > CURRENT_DOC_VERSION {
            log::warn!(
                "DocumentSnapshot has doc_version {}, current is {}; attempting best-effort load",
                snap.doc_version,
                CURRENT_DOC_VERSION
            );
        }

        // Decide v1 vs v2 by whichever side is populated. v2 takes
        // precedence — a malformed file with both populated still loads
        // sensibly (we trust the v2 layout).
        let (layers, active_layer) = if !snap.layers.is_empty() {
            // v2 path.
            let active = snap.active_layer.unwrap_or(snap.layers[0].id);
            // If active_layer points to a non-existent layer (corrupt
            // file), fall back to the first layer.
            let active =
                if snap.layers.iter().any(|l| l.id == active) { active } else { snap.layers[0].id };
            (snap.layers, active)
        } else if !snap.strokes.is_empty() {
            // v1 migration: wrap every committed stroke into a single
            // default layer named "Layer 1".
            let mut layer = Layer::new("Layer 1");
            layer.strokes = snap.strokes;
            let id = layer.id;
            (vec![layer], id)
        } else {
            // Empty doc; mint a fresh default layer.
            let layer = Layer::new("Layer 1");
            let id = layer.id;
            (vec![layer], id)
        };

        // Bump id counters so subsequent next() calls don't collide with
        // loaded ids.
        if let Some(max) = layers.iter().flat_map(|l| l.strokes.iter()).map(|s| s.id.0).max() {
            StrokeId::bump_to(max);
        }
        if let Some(max) = layers.iter().map(|l| l.id.raw()).max() {
            LayerId::bump_to(max);
        }

        Self {
            page: snap.page,
            brush: snap.brush,
            layers,
            active_layer,
            active: None,
            active_stroke_layer: None,
        }
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
#[non_exhaustive]
pub enum EditError {
    #[error("stroke {0:?} not found")]
    MissingStroke(StrokeId),
    #[error("layer {0:?} not found")]
    MissingLayer(LayerId),
    #[error("cannot remove the last layer; at least one must remain")]
    LastLayer,
}

/// A reversible unit of document change. `apply` consumes `self` and returns
/// the concrete inverse it computed against the live state — so e.g.
/// `RemoveStroke { id, layer }` returns `AddStroke { stroke, layer }`,
/// capturing the data before it's gone.
///
/// Each variant is **layer-scoped** so undo restores state to the exact
/// place it came from regardless of what the user has done with the active
/// layer in the meantime.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Edit {
    /// Add `stroke` to `layer`'s stroke list. Inverse: `RemoveStroke`.
    AddStroke { stroke: Stroke, layer: LayerId },
    /// Remove the named stroke from the named layer. Inverse: `AddStroke`
    /// (capturing the removed stroke).
    RemoveStroke { id: StrokeId, layer: LayerId },
    /// Insert `layer` at `index`. If `was_active` is true, also set it as
    /// the active layer (used when undoing a `RemoveLayer` that took out
    /// the previously-active layer). Inverse: `RemoveLayer`.
    AddLayer { layer: Layer, index: usize, was_active: bool },
    /// Remove the named layer. Apply captures the layer payload, its
    /// position, and whether it was active so the inverse can restore
    /// exactly. Inverse: `AddLayer`. Errors with `LastLayer` if it would
    /// leave zero layers.
    RemoveLayer { id: LayerId },
    /// Move the named layer to `new_index`. **`new_index` is silently
    /// clamped** to `[0, layers.len() - 1]` — out-of-bounds is a programmer
    /// error a UI shouldn't generate, but errors here would force every
    /// caller to validate up-front. Inverse: `MoveLayer` with the recovered
    /// old index.
    MoveLayer { id: LayerId, new_index: usize },
}

impl Edit {
    pub fn apply(self, doc: &mut DocumentState) -> Result<Edit, EditError> {
        match self {
            Edit::AddStroke { stroke, layer } => {
                let Some(l) = doc.layers.iter_mut().find(|l| l.id == layer) else {
                    return Err(EditError::MissingLayer(layer));
                };
                let id = stroke.id;
                l.strokes.push(stroke);
                Ok(Edit::RemoveStroke { id, layer })
            }
            Edit::RemoveStroke { id, layer } => {
                let Some(l) = doc.layers.iter_mut().find(|l| l.id == layer) else {
                    return Err(EditError::MissingLayer(layer));
                };
                let Some(idx) = l.strokes.iter().position(|s| s.id == id) else {
                    return Err(EditError::MissingStroke(id));
                };
                let stroke = l.strokes.remove(idx);
                Ok(Edit::AddStroke { stroke, layer })
            }
            Edit::AddLayer { layer, index, was_active } => {
                let id = layer.id;
                let idx = index.min(doc.layers.len());
                doc.layers.insert(idx, layer);
                if was_active {
                    doc.active_layer = id;
                }
                // Inverse is bare `RemoveLayer { id }` rather than
                // capturing `was_active` here. The reason: `RemoveLayer`'s
                // apply recomputes `was_active` from live state at the
                // moment of removal, so a chain like "AddLayer (inactive)
                // → user activates new layer → undo (RemoveLayer)" still
                // captures `was_active=true` for the redo, even though
                // the original AddLayer didn't activate. See
                // `undo_add_layer_clears_active_when_undoing_to_pre_activation`.
                Ok(Edit::RemoveLayer { id })
            }
            Edit::RemoveLayer { id } => {
                if doc.layers.len() <= 1 {
                    return Err(EditError::LastLayer);
                }
                let Some(idx) = doc.layers.iter().position(|l| l.id == id) else {
                    return Err(EditError::MissingLayer(id));
                };
                let was_active = doc.active_layer == id;
                let layer = doc.layers.remove(idx);
                if was_active {
                    // Fall back to the previous sibling if we removed the
                    // top, else the next sibling. There's always ≥1 layer
                    // left here because we checked above.
                    let new_idx = idx.min(doc.layers.len() - 1);
                    doc.active_layer = doc.layers[new_idx].id;
                }
                Ok(Edit::AddLayer { layer, index: idx, was_active })
            }
            Edit::MoveLayer { id, new_index } => {
                let Some(old_index) = doc.layers.iter().position(|l| l.id == id) else {
                    return Err(EditError::MissingLayer(id));
                };
                let target = new_index.min(doc.layers.len() - 1);
                if old_index == target {
                    return Ok(Edit::MoveLayer { id, new_index: old_index });
                }
                let layer = doc.layers.remove(old_index);
                doc.layers.insert(target, layer);
                Ok(Edit::MoveLayer { id, new_index: old_index })
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
        Sample::mouse(Point::new(x, y).into(), Duration::ZERO, PointerId::MOUSE)
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
    fn default_state_has_one_layer_and_active_points_at_it() {
        let doc = DocumentState::new();
        assert_eq!(doc.layers().len(), 1);
        assert_eq!(doc.active_layer(), doc.layers()[0].id);
        assert_eq!(doc.layers()[0].name, "Layer 1");
    }

    #[test]
    fn add_then_remove_stroke_is_roundtrip() {
        let mut doc = DocumentState::new();
        let target = doc.active_layer();
        let s = stroke(1, &[(0.0, 0.0), (1.0, 1.0)]);
        let undo = Edit::AddStroke { stroke: s.clone(), layer: target }.apply(&mut doc).unwrap();
        assert_eq!(doc.layers()[0].strokes.len(), 1);
        let redo = undo.apply(&mut doc).unwrap();
        assert!(doc.layers()[0].strokes.is_empty());
        match redo {
            Edit::AddStroke { stroke: restored, layer } => {
                assert_eq!(restored.id, s.id);
                assert_eq!(restored.samples.len(), s.samples.len());
                assert_eq!(layer, target);
            }
            _ => panic!("expected AddStroke as inverse of remove"),
        }
    }

    #[test]
    fn double_undo_via_returned_inverse_is_stable() {
        let mut doc = DocumentState::new();
        let target = doc.active_layer();
        let inv1 = Edit::AddStroke { stroke: stroke(1, &[(0.0, 0.0)]), layer: target }
            .apply(&mut doc)
            .unwrap();
        let inv2 = Edit::AddStroke { stroke: stroke(2, &[(1.0, 1.0)]), layer: target }
            .apply(&mut doc)
            .unwrap();
        assert_eq!(doc.layers()[0].strokes.len(), 2);
        // Undo in reverse order, as a history stack would.
        let _redo2 = inv2.apply(&mut doc).unwrap();
        let _redo1 = inv1.apply(&mut doc).unwrap();
        assert!(doc.layers()[0].strokes.is_empty());
    }

    #[test]
    fn remove_missing_stroke_returns_typed_error() {
        let mut doc = DocumentState::new();
        let target = doc.active_layer();
        let result = Edit::RemoveStroke { id: StrokeId(42), layer: target }.apply(&mut doc);
        assert_eq!(result.unwrap_err(), EditError::MissingStroke(StrokeId(42)));
    }

    #[test]
    fn remove_stroke_in_missing_layer_returns_typed_error() {
        let mut doc = DocumentState::new();
        let bogus = LayerId::next();
        let result = Edit::RemoveStroke { id: StrokeId(1), layer: bogus }.apply(&mut doc);
        assert_eq!(result.unwrap_err(), EditError::MissingLayer(bogus));
    }

    #[test]
    fn snapshot_includes_active_stroke_and_targets_layer_at_begin_time() {
        let mut doc = DocumentState::new();
        doc.begin_stroke(stroke(7, &[(0.0, 0.0)]));
        let snap = doc.snapshot();
        assert_eq!(snap.strokes.len(), 1);
        assert_eq!(snap.strokes[0].id, StrokeId(7));
        // Active stroke is in the layer that was active when begin_stroke ran.
        let layer = snap.layers.iter().find(|l| l.id == snap.active_layer).unwrap();
        assert_eq!(layer.strokes.len(), 1);
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
    fn end_stroke_returns_active_and_target_layer() {
        let mut doc = DocumentState::new();
        let original_layer = doc.active_layer();
        doc.begin_stroke(stroke(7, &[(0.0, 0.0)]));
        doc.add_sample(StrokeId(7), sample_at(1.0, 1.0));
        let (s, layer) = doc.end_stroke(StrokeId(7)).expect("active stroke");
        assert_eq!(s.samples.len(), 2);
        assert_eq!(layer, original_layer);
        assert!(!doc.has_active_stroke());
    }

    #[test]
    fn end_stroke_targets_layer_active_at_begin_not_at_end() {
        // Live-vs-frozen for layers: switching active layer mid-drag must
        // not redirect the in-flight stroke. The stroke commits to the
        // layer that was active when begin_stroke ran.
        let mut doc = DocumentState::new();
        let original = doc.active_layer();
        doc.begin_stroke(stroke(7, &[(0.0, 0.0)]));
        let other = doc.add_layer("Layer 2");
        doc.set_active_layer(other);
        let (_, target) = doc.end_stroke(StrokeId(7)).expect("active stroke");
        assert_eq!(target, original, "stroke must commit to layer active at BeginStroke time");
    }

    fn estimated_sample_at(x: f64, y: f64, update_index: u64, pressure: f32) -> Sample {
        let mut s = Sample::mouse(Point::new(x, y).into(), Duration::ZERO, PointerId::MOUSE);
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
    fn revise_sample_falls_back_to_last_committed_stroke_in_any_layer() {
        let mut doc = DocumentState::new();
        let target = doc.active_layer();
        let s = Stroke {
            id: StrokeId(1),
            samples: vec![estimated_sample_at(0.0, 0.0, 99, 0.0)],
            caps: ToolCaps::empty(),
            brush: BrushParams::default(),
        };
        Edit::AddStroke { stroke: s, layer: target }.apply(&mut doc).unwrap();
        let revision = SampleRevision { pressure: Some(0.4), ..SampleRevision::default() };

        assert!(doc.revise_sample(StrokeId(1), 99, revision));
        let layer = doc.layers().iter().find(|l| l.id == target).unwrap();
        assert!((layer.strokes[0].samples[0].pressure - 0.4).abs() < f32::EPSILON);
        assert_eq!(layer.strokes[0].samples[0].class, SampleClass::Committed);
    }

    #[test]
    fn revise_sample_finds_stroke_in_non_active_layer() {
        // A revision can race with EndStroke even after the user switches
        // active layer; the cross-layer scan must still find the stroke
        // and apply the revision (not just return true).
        let mut doc = DocumentState::new();
        let l1 = doc.active_layer();
        let s = Stroke {
            id: StrokeId(1),
            samples: vec![estimated_sample_at(0.0, 0.0, 99, 0.0)],
            caps: ToolCaps::empty(),
            brush: BrushParams::default(),
        };
        Edit::AddStroke { stroke: s, layer: l1 }.apply(&mut doc).unwrap();
        let l2 = doc.add_layer("Layer 2");
        doc.set_active_layer(l2);
        let revision = SampleRevision { pressure: Some(0.4), ..SampleRevision::default() };

        assert!(doc.revise_sample(StrokeId(1), 99, revision));
        let layer = doc.layer(l1).unwrap();
        assert!((layer.strokes[0].samples[0].pressure - 0.4).abs() < f32::EPSILON);
        assert_eq!(layer.strokes[0].samples[0].class, SampleClass::Committed);
    }

    #[test]
    fn revise_sample_is_no_op_when_update_index_missing() {
        let mut doc = DocumentState::new();
        doc.begin_stroke(stroke(1, &[(0.0, 0.0)])); // Committed sample, no Estimated
        assert!(!doc.revise_sample(StrokeId(1), 99, SampleRevision::default()));
    }

    #[test]
    fn default_brush_matches_brush_params_default() {
        let doc = DocumentState::new();
        assert_eq!(doc.brush(), BrushParams::default());
    }

    #[test]
    fn set_brush_max_width_preserves_ratio() {
        let mut doc = DocumentState::new();
        doc.set_brush(BrushParams { min_width: 2.0, max_width: 8.0, ..Default::default() });
        doc.set_brush_max_width(16.0);
        let b = doc.brush();
        assert!((b.max_width - 16.0).abs() < f32::EPSILON);
        assert!((b.min_width - 4.0).abs() < f32::EPSILON);
    }

    #[test]
    fn set_brush_max_width_clamps_to_bounds() {
        let mut doc = DocumentState::new();
        doc.set_brush_max_width(1000.0);
        assert!((doc.brush().max_width - MAX_WIDTH_MAX).abs() < f32::EPSILON);
        doc.set_brush_max_width(-5.0);
        assert!((doc.brush().max_width - MAX_WIDTH_MIN).abs() < f32::EPSILON);
    }

    #[test]
    fn set_brush_max_width_min_equals_max_scales_both() {
        let mut doc = DocumentState::new();
        doc.set_brush(BrushParams { min_width: 4.0, max_width: 4.0, ..Default::default() });
        doc.set_brush_max_width(8.0);
        assert!((doc.brush().min_width - 8.0).abs() < f32::EPSILON);
        assert!((doc.brush().max_width - 8.0).abs() < f32::EPSILON);
    }

    #[test]
    fn set_brush_max_width_below_current_min_drags_min_down() {
        let mut doc = DocumentState::new();
        doc.set_brush(BrushParams { min_width: 5.0, max_width: 8.0, ..Default::default() });
        doc.set_brush_max_width(3.0);
        let b = doc.brush();
        assert!((b.max_width - 3.0).abs() < f32::EPSILON);
        assert!((b.min_width - 1.875).abs() < 1e-5);
    }

    #[test]
    fn set_brush_min_width_clamps_to_max() {
        let mut doc = DocumentState::new();
        doc.set_brush(BrushParams { min_width: 1.0, max_width: 4.0, ..Default::default() });
        doc.set_brush_min_width(10.0);
        assert!((doc.brush().min_width - 4.0).abs() < f32::EPSILON);
        doc.set_brush_min_width(-1.0);
        assert!(doc.brush().min_width.abs() < f32::EPSILON);
    }

    #[test]
    fn set_brush_min_ratio_maps_to_width() {
        let mut doc = DocumentState::new();
        doc.set_brush(BrushParams { min_width: 1.0, max_width: 4.0, ..Default::default() });
        doc.set_brush_min_ratio(0.5);
        assert!((doc.brush().min_width - 2.0).abs() < f32::EPSILON);
        doc.set_brush_min_ratio(1.5);
        assert!((doc.brush().min_width - 4.0).abs() < f32::EPSILON);
        doc.set_brush_min_ratio(-0.5);
        assert!(doc.brush().min_width.abs() < f32::EPSILON);
    }

    #[test]
    fn snapshot_carries_brush() {
        let mut doc = DocumentState::new();
        doc.set_brush_max_width(12.0);
        let snap = doc.snapshot();
        assert!((snap.brush.max_width - 12.0).abs() < f32::EPSILON);
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
        assert!(!doc.revise_sample(StrokeId(1), 7, r2));

        let active = doc.active.as_ref().unwrap();
        assert!((active.samples[0].pressure - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn add_layer_appends_with_fresh_id_at_end() {
        let mut doc = DocumentState::new();
        let l1 = doc.layers()[0].id;
        let l2 = doc.add_layer("Layer 2");
        assert_eq!(doc.layers().len(), 2);
        assert_eq!(doc.layers()[1].id, l2);
        assert_ne!(l1, l2);
    }

    #[test]
    fn remove_layer_succeeds_when_more_than_one_remains() {
        let mut doc = DocumentState::new();
        let l1 = doc.layers()[0].id;
        let l2 = doc.add_layer("Layer 2");
        let inv = Edit::RemoveLayer { id: l2 }.apply(&mut doc).unwrap();
        assert_eq!(doc.layers().len(), 1);
        assert_eq!(doc.layers()[0].id, l1);
        match inv {
            Edit::AddLayer { layer, index, was_active } => {
                assert_eq!(layer.id, l2);
                assert_eq!(index, 1);
                assert!(!was_active);
            }
            _ => panic!("expected AddLayer as inverse"),
        }
    }

    #[test]
    fn remove_last_layer_errors() {
        let mut doc = DocumentState::new();
        let only = doc.layers()[0].id;
        let result = Edit::RemoveLayer { id: only }.apply(&mut doc);
        assert_eq!(result.unwrap_err(), EditError::LastLayer);
    }

    #[test]
    fn remove_active_layer_falls_back_to_sibling() {
        let mut doc = DocumentState::new();
        let l1 = doc.layers()[0].id;
        let l2 = doc.add_layer("Layer 2");
        doc.set_active_layer(l2);
        let inv = Edit::RemoveLayer { id: l2 }.apply(&mut doc).unwrap();
        assert_eq!(doc.active_layer(), l1);
        // And the inverse remembered we were active so undo restores it.
        match inv {
            Edit::AddLayer { was_active, .. } => assert!(was_active),
            _ => panic!("expected AddLayer as inverse"),
        }
    }

    #[test]
    fn undo_remove_active_layer_restores_active() {
        let mut doc = DocumentState::new();
        let l1 = doc.layers()[0].id;
        let l2 = doc.add_layer("Layer 2");
        doc.set_active_layer(l2);
        let inv = Edit::RemoveLayer { id: l2 }.apply(&mut doc).unwrap();
        assert_eq!(doc.active_layer(), l1);
        let _redo = inv.apply(&mut doc).unwrap();
        assert_eq!(doc.active_layer(), l2, "undo of RemoveLayer of the active layer restores it");
    }

    #[test]
    fn move_layer_changes_position_and_inverse_restores() {
        let mut doc = DocumentState::new();
        let l1 = doc.layers()[0].id;
        let l2 = doc.add_layer("Layer 2");
        let l3 = doc.add_layer("Layer 3");
        // Move l3 to position 0.
        let inv = Edit::MoveLayer { id: l3, new_index: 0 }.apply(&mut doc).unwrap();
        assert_eq!(doc.layers().iter().map(|l| l.id).collect::<Vec<_>>(), vec![l3, l1, l2]);
        // Undo restores.
        let _redo = inv.apply(&mut doc).unwrap();
        assert_eq!(doc.layers().iter().map(|l| l.id).collect::<Vec<_>>(), vec![l1, l2, l3]);
    }

    #[test]
    fn move_missing_layer_errors() {
        let mut doc = DocumentState::new();
        let bogus = LayerId::next();
        assert_eq!(
            Edit::MoveLayer { id: bogus, new_index: 0 }.apply(&mut doc).unwrap_err(),
            EditError::MissingLayer(bogus)
        );
    }

    #[test]
    fn move_layer_clamps_out_of_bounds_index() {
        // `Edit::MoveLayer` silently clamps `new_index` to
        // `layers.len() - 1`. Property is documented; this test fences it
        // so a future tweak to error-on-OOB doesn't go unnoticed.
        let mut doc = DocumentState::new();
        let l1 = doc.layers()[0].id;
        let l2 = doc.add_layer("Layer 2");
        // Try to move l1 to index 100 (way past the end).
        let inv = Edit::MoveLayer { id: l1, new_index: 100 }.apply(&mut doc).unwrap();
        assert_eq!(
            doc.layers().iter().map(|l| l.id).collect::<Vec<_>>(),
            vec![l2, l1],
            "out-of-bounds move clamps to last position"
        );
        // Inverse points back to the original position 0.
        match inv {
            Edit::MoveLayer { id, new_index } => {
                assert_eq!(id, l1);
                assert_eq!(new_index, 0);
            }
            _ => panic!("expected MoveLayer as inverse"),
        }
    }

    #[test]
    fn undo_add_layer_clears_active_when_undoing_to_pre_activation() {
        // Engine-level scenario condensed into core: AddLayer (inactive),
        // user activates the new layer via SetActiveLayer (non-undoable),
        // then Undo. Undo applies `Edit::RemoveLayer { id }` against live
        // state — RemoveLayer recomputes `was_active` from current state,
        // so the redo-info correctly remembers that the layer was active
        // when removed.
        let mut doc = DocumentState::new();
        let l1 = doc.layers()[0].id;
        let l2 = Layer::new("Layer 2");
        let new_id = l2.id;
        // Apply AddLayer with was_active=false (matches engine behaviour).
        let undo =
            Edit::AddLayer { layer: l2, index: 1, was_active: false }.apply(&mut doc).unwrap();
        // User activates the new layer outside the Edit machinery.
        doc.set_active_layer(new_id);
        // Now undo — applies the inverse RemoveLayer.
        let redo = undo.apply(&mut doc).unwrap();
        // Active falls back to the only remaining layer.
        assert_eq!(doc.active_layer(), l1);
        // Redo info captures was_active=true, so applying it restores
        // active to the resurrected layer.
        let _ = redo.apply(&mut doc).unwrap();
        assert_eq!(doc.active_layer(), new_id);
    }

    #[test]
    fn add_stroke_to_specific_layer_lands_there_not_in_active() {
        let mut doc = DocumentState::new();
        let l1 = doc.layers()[0].id;
        let l2 = doc.add_layer("Layer 2");
        doc.set_active_layer(l2);
        // Explicit target l1 (not active).
        Edit::AddStroke { stroke: stroke(1, &[(0.0, 0.0)]), layer: l1 }.apply(&mut doc).unwrap();
        let lay1 = doc.layer(l1).unwrap();
        let lay2 = doc.layer(l2).unwrap();
        assert_eq!(lay1.strokes.len(), 1);
        assert_eq!(lay2.strokes.len(), 0);
    }

    #[test]
    fn undo_after_active_layer_switch_targets_original_layer() {
        // Add stroke to L1, switch active to L2, undo. Stroke must come out
        // of L1, not L2 — Edit captures the layer.
        let mut doc = DocumentState::new();
        let l1 = doc.layers()[0].id;
        let l2 = doc.add_layer("Layer 2");
        let inv = Edit::AddStroke { stroke: stroke(1, &[(0.0, 0.0)]), layer: l1 }
            .apply(&mut doc)
            .unwrap();
        doc.set_active_layer(l2);
        let _redo = inv.apply(&mut doc).unwrap(); // Undo the AddStroke
        assert_eq!(doc.layer(l1).unwrap().strokes.len(), 0);
        assert_eq!(doc.layer(l2).unwrap().strokes.len(), 0);
    }
}
