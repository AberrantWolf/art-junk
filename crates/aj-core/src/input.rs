//! Brush and stroke-shape types. Input-sample types (`Sample`, `ToolCaps`,
//! `PointerId`, …) now live in `stylus-junk` and are re-exported from the
//! crate root; this module holds only the brush/stroke shape types that
//! are document-model concerns, not input concerns.

/// Color stored in linear RGB with straight (non-premultiplied) alpha, matching
/// the working color space called out in CLAUDE.md. Convert at system
/// boundaries: UI color pickers hand in sRGB bytes via `from_srgb8`; the
/// renderer sends bytes back out via `to_srgb8` for peniko. Internal math
/// (blending, interpolation, effects) happens in linear space unmodified.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub struct LinearRgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl LinearRgba {
    pub const BLACK: Self = Self { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
    pub const WHITE: Self = Self { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
    pub const TRANSPARENT: Self = Self { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };

    #[must_use]
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// sRGB u8 → linear-float. Alpha is linear in both spaces (no transfer
    /// curve on the alpha channel), so it passes through normalized by 255.
    #[must_use]
    pub fn from_srgb8(rgba: [u8; 4]) -> Self {
        let [r, g, b, a] = rgba;
        Self {
            r: srgb_to_linear(f32::from(r) / 255.0),
            g: srgb_to_linear(f32::from(g) / 255.0),
            b: srgb_to_linear(f32::from(b) / 255.0),
            a: f32::from(a) / 255.0,
        }
    }

    /// Linear-float → sRGB u8. Round-trip through `from_srgb8` preserves each
    /// input byte to within ±1 (quantization noise at 1/255 granularity).
    #[must_use]
    pub fn to_srgb8(self) -> [u8; 4] {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        fn quantize(x: f32) -> u8 {
            (x.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
        }
        [
            quantize(linear_to_srgb(self.r)),
            quantize(linear_to_srgb(self.g)),
            quantize(linear_to_srgb(self.b)),
            quantize(self.a),
        ]
    }
}

impl Default for LinearRgba {
    fn default() -> Self {
        Self::BLACK
    }
}

/// sRGB transfer curve: compressed display-space 0..=1 → linear-light 0..=1.
/// See IEC 61966-2-1; the piecewise threshold keeps the curve smooth at the
/// origin where a naive `x^2.4` would have zero slope.
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.040_45 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.003_130_8 { 12.92 * c } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
}

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
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub struct BrushParams {
    pub min_width: f32,
    pub max_width: f32,
    pub curve: PressureCurve,
    /// Linear-float RGBA. Stored as `LinearRgba` rather than `peniko::Color`
    /// so `aj-core` stays free of render-pipeline deps; the renderer converts
    /// once at the draw site via `to_srgb8`.
    pub color: LinearRgba,
}

impl Default for BrushParams {
    fn default() -> Self {
        Self {
            min_width: 0.5,
            max_width: 4.0,
            curve: PressureCurve::Linear,
            color: LinearRgba::from_srgb8([0, 200, 220, 255]),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[non_exhaustive]
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
        assert_eq!(b.color.to_srgb8(), [0, 200, 220, 255]);
    }

    #[test]
    fn srgb_roundtrip_is_byte_exact_at_corners() {
        for c in [
            [0, 0, 0, 255],
            [255, 255, 255, 255],
            [128, 64, 32, 200],
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
        ] {
            let linear = LinearRgba::from_srgb8(c);
            let back = linear.to_srgb8();
            assert_eq!(c, back, "round-trip mismatch for {c:?} (linear: {linear:?})");
        }
    }

    #[test]
    fn linear_rgba_constants() {
        assert_eq!(LinearRgba::BLACK.to_srgb8(), [0, 0, 0, 255]);
        assert_eq!(LinearRgba::WHITE.to_srgb8(), [255, 255, 255, 255]);
        assert_eq!(LinearRgba::TRANSPARENT.to_srgb8(), [0, 0, 0, 0]);
    }

    #[test]
    fn pressure_curve_linear_is_identity() {
        for p in [0.0, 0.25, 0.5, 0.75, 1.0] {
            assert!((PressureCurve::Linear.apply(p) - p).abs() < f32::EPSILON);
        }
    }
}
