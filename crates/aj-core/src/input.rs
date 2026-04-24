//! Brush and stroke-shape types. Input-sample types (`Sample`, `ToolCaps`,
//! `PointerId`, …) now live in `stylus-junk` and are re-exported from the
//! crate root; this module holds only the brush/stroke shape types that
//! are document-model concerns, not input concerns.

/// Lower bound on `BrushParams::max_width`. Not a perceptual floor — it only
/// prevents the multiplicative `[` / `]` shortcuts from trapping at zero.
/// A vector renderer handles sub-pixel widths fine.
pub const MAX_WIDTH_MIN: f32 = 0.01;

/// Upper bound on `BrushParams::max_width`. Chosen so the log-scale slider
/// feels sensible across its range — 64 pt is large enough for wide brushes,
/// small enough that 0.01–64 is a reasonable 4000× span.
pub const MAX_WIDTH_MAX: f32 = 64.0;

/// Shape of a brush as it affects stroke rendering. The renderer reads these
/// from `Stroke::brush` (frozen at `BeginStroke` time), not from the live
/// document brush.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrushParams {
    pub min_width: f32,
    pub max_width: f32,
    pub curve: PressureCurve,
    /// RGBA8. Stored as a byte array instead of `peniko::Color` so `aj-core`
    /// stays free of render-pipeline deps and serializes cleanly when
    /// `aj-format` lands. Renderer converts at the draw site.
    pub color: [u8; 4],
}

impl Default for BrushParams {
    fn default() -> Self {
        Self {
            min_width: 0.5,
            max_width: 4.0,
            curve: PressureCurve::Linear,
            color: [0, 200, 220, 255],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureCurve {
    Linear,
}

impl PressureCurve {
    /// Map a unit-interval pressure through this curve. Extension point for
    /// future non-linear variants; today the only variant is the identity.
    #[must_use]
    pub fn apply(self, p: f32) -> f32 {
        match self {
            Self::Linear => p,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brush_params_default_is_sensible() {
        let b = BrushParams::default();
        assert!(b.min_width < b.max_width);
        assert_eq!(b.curve, PressureCurve::Linear);
        assert_eq!(b.color, [0, 200, 220, 255]);
    }

    #[test]
    fn pressure_curve_linear_is_identity() {
        for p in [0.0, 0.25, 0.5, 0.75, 1.0] {
            assert!((PressureCurve::Linear.apply(p) - p).abs() < f32::EPSILON);
        }
    }
}
