//! Layer types: container for strokes plus per-layer rendering attributes.
//!
//! `DocumentState` owns a `Vec<Layer>` plus an `active_layer: LayerId`. Stroke
//! commands route to the active layer; the user can add, remove, reorder,
//! rename layers and toggle visibility / opacity / blend mode independently.
//!
//! C1 establishes the data shape only — the renderer still flattens visible-
//! layer strokes into a single substrate. C2 will compose per-layer substrates
//! using the per-layer `BlendMode` + `opacity` + `visible`.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::Stroke;

/// Stable per-layer identifier. Monotonic across the program lifetime; once
/// a layer is removed its id is never reused. `LayerId(0)` is reserved as a
/// "missing/uninitialised" sentinel — `LayerId::next()` always returns ≥1,
/// and `Default` returns the sentinel for use in `SceneSnapshot::default()`
/// (the empty pre-engine-publish snapshot, where no real id is meaningful).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct LayerId(u64);

static LAYER_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

impl LayerId {
    /// Mint a fresh, never-before-issued `LayerId`. Always ≥1.
    #[must_use]
    pub fn next() -> Self {
        Self(LAYER_ID_COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    /// Raise the global counter so subsequent `LayerId::next()` returns a
    /// value strictly greater than `target`. Used by the loader after
    /// deserialising a document, so new layers don't collide with loaded
    /// ones. No-op if the counter is already ahead.
    pub fn bump_to(target: u64) {
        LAYER_ID_COUNTER.fetch_max(target.saturating_add(1), Ordering::Relaxed);
    }

    /// Raw integer id, for tests and debug output.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// How a layer composites onto the layers beneath it. C1 ships only `Normal`
/// (linear-RGB alpha-over) but the enum scaffold is in place so C4's OKLAB
/// Mix can land as one extra variant without a `doc_version` bump.
///
/// `#[non_exhaustive]` keeps `match` arms forward-compatible at compile
/// time; the `#[serde(other)]` `Unknown` variant keeps deserialization
/// forward-compatible at the wire — a document saved by a future build
/// with an unrecognized blend mode loads as `Unknown` rather than
/// hard-failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum BlendMode {
    /// Linear-RGB alpha-over composite — the standard. Below-layer's color
    /// shows through where this layer's alpha < 1.
    #[default]
    Normal,
    /// Forward-compat fallback: deserialised when an older binary loads a
    /// document written with a newer variant. Behaves as `Normal` in the
    /// renderer until upgraded; never serialised in this form.
    #[cfg_attr(feature = "serde", serde(other))]
    Unknown,
}

/// A single layer in the document. Owns its own strokes and per-layer
/// rendering attributes; layers composite bottom-to-top using `blend_mode`
/// and `opacity`, with `visible = false` skipping the layer entirely.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub struct Layer {
    pub id: LayerId,
    pub name: String,
    pub strokes: Vec<Stroke>,
    /// `#[serde(default)]` so v1 documents that didn't have layers (and so
    /// don't carry blend modes either) deserialise into `BlendMode::Normal`.
    #[cfg_attr(feature = "serde", serde(default))]
    pub blend_mode: BlendMode,
    /// 0.0 – 1.0. Multiplies into the inter-layer blend at composite time.
    /// `#[serde(default = "default_opacity")]` so missing-field loads default
    /// to fully opaque rather than fully transparent.
    #[cfg_attr(feature = "serde", serde(default = "default_opacity"))]
    pub opacity: f32,
    /// Hidden layers are skipped entirely at composite time. Strokes still
    /// live on disk; toggling visibility never mutates the strokes.
    #[cfg_attr(feature = "serde", serde(default = "default_visible"))]
    pub visible: bool,
}

#[cfg(feature = "serde")]
fn default_opacity() -> f32 {
    1.0
}

#[cfg(feature = "serde")]
fn default_visible() -> bool {
    true
}

impl Layer {
    /// Construct a new empty layer with a fresh id and the given name.
    /// Defaults: `BlendMode::Normal`, opacity = 1.0, visible = true.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: LayerId::next(),
            name: name.into(),
            strokes: Vec::new(),
            blend_mode: BlendMode::Normal,
            opacity: 1.0,
            visible: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_returns_strictly_increasing_ids() {
        let a = LayerId::next();
        let b = LayerId::next();
        assert!(b.raw() > a.raw());
    }

    #[test]
    fn next_never_returns_zero() {
        // LayerId(0) is the missing/uninitialised sentinel; `next()` must
        // always skip it. The counter starts at 1 so this is true by
        // construction; this test guards against a future tweak that would
        // accidentally let the sentinel leak into real ids.
        for _ in 0..16 {
            assert!(LayerId::next().raw() >= 1);
        }
    }

    #[test]
    fn bump_to_advances_counter_when_behind() {
        // Force the counter way ahead and assert next() respects it.
        LayerId::bump_to(100_000);
        let id = LayerId::next();
        assert!(id.raw() > 100_000);
    }

    #[test]
    fn bump_to_is_no_op_when_already_ahead() {
        // Bump twice; second bump (lower target) must not roll back.
        LayerId::bump_to(50);
        let _ = LayerId::next();
        let high = LayerId::next();
        LayerId::bump_to(10);
        let after = LayerId::next();
        assert!(after.raw() > high.raw());
    }

    #[test]
    fn layer_new_has_sensible_defaults() {
        let l = Layer::new("Test");
        assert_eq!(l.name, "Test");
        assert!(l.strokes.is_empty());
        assert_eq!(l.blend_mode, BlendMode::Normal);
        assert!((l.opacity - 1.0).abs() < f32::EPSILON);
        assert!(l.visible);
    }
}
