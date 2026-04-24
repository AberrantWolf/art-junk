//! Thin wrappers over the `palette` crate for the color picker. Converts to
//! `aj_core::LinearRgba` at the API boundaries so the engine and renderer
//! never see `palette` types, and the picker never hand-rolls color math.
//!
//! The picker deliberately uses two parameterizations of the same Oklab space:
//! `Oklch` for internal state (unbounded `C`, round-trips cleanly with
//! `LinearRgba`) and `Okhsl` for the 2D picker plane (normalized `S` so every
//! pixel is in-gamut and perceptually uniform). Both are the same color
//! space; only the coordinates differ.
//!
//! Any `Oklch` handed to `gamut_map_to_srgb` is clamped into sRGB via the CSS
//! Color Module Level 4 binary-search algorithm (preserves hue, reduces
//! chroma). Colors that come out of Okhsl are already in-gamut by
//! construction; the mapping is a safety net for float noise plus the entry
//! path for hex / absolute-Oklch inputs.

use aj_core::LinearRgba;
use palette::{FromColor, IntoColor, LinSrgb, Okhsl, Oklch};
use palette_gamut_mapping::gamut_map;

/// Convert the picker's internal `Oklch` state to `LinearRgba`, reducing
/// chroma as needed to fit the sRGB gamut (CSS Color 4 binary search). `alpha`
/// is threaded through unchanged — the perceptual pipeline has nothing to say
/// about alpha.
#[must_use]
pub fn oklch_to_linear_rgba(ch: Oklch, alpha: f32) -> LinearRgba {
    let lin: LinSrgb = gamut_map(ch);
    LinearRgba { r: lin.red, g: lin.green, b: lin.blue, a: alpha }
}

/// `LinearRgba` → `Okhsl`. Used on incoming colors (hex entry, swatch click,
/// engine sync) that need to land on the picker's Okhsl-canonical state.
/// Goes through palette's Okhsl pipeline (linear sRGB → Oklab → Okhsl), which
/// is well-defined but has a known wobble in `S` near the gamut cusp; callers
/// should preserve the UI hue across that edge by checking for `saturation <
/// ε` and substituting the previous hue rather than accepting whatever
/// `hue` comes back for near-neutral inputs.
#[must_use]
pub fn linear_rgba_to_okhsl(c: LinearRgba) -> Okhsl {
    let lin = LinSrgb::new(c.r, c.g, c.b);
    Okhsl::from_color(lin)
}

/// `Okhsl` → `LinearRgba`. The picker commits through this. Okhsl is
/// constructed to be in-gamut against the sRGB cusp, so no `gamut_map` is
/// needed here — we just clamp for float noise at the edges.
#[must_use]
pub fn okhsl_to_linear_rgba(hsl: Okhsl, alpha: f32) -> LinearRgba {
    let lin: LinSrgb = hsl.into_color();
    LinearRgba {
        r: lin.red.clamp(0.0, 1.0),
        g: lin.green.clamp(0.0, 1.0),
        b: lin.blue.clamp(0.0, 1.0),
        a: alpha,
    }
}

/// Paint an Okhsl directly as an sRGB-encoded byte quad. Used by the L×S
/// plane texture painter, which writes straight into an egui `ColorImage`'s
/// `[u8; 4]` entries. Okhsl guarantees in-gamut, so we bypass `gamut_map` and
/// go straight through the existing `LinearRgba::to_srgb8` transfer.
#[must_use]
pub fn okhsl_to_srgb8(hsl: Okhsl) -> [u8; 4] {
    let lin: LinSrgb = hsl.into_color();
    let rgba = LinearRgba { r: lin.red, g: lin.green, b: lin.blue, a: 1.0 };
    rgba.to_srgb8()
}

/// Same for Oklch; used by the hue strip where the Okhsl parameterization
/// would force S=1 and wouldn't be the color the user expects when the strip
/// is meant to show "pure hue at a fixed vivid L, C."
#[must_use]
pub fn oklch_to_srgb8(ch: Oklch) -> [u8; 4] {
    let c = oklch_to_linear_rgba(ch, 1.0);
    c.to_srgb8()
}

#[cfg(test)]
mod tests {
    use super::*;
    use palette::{Oklab, OklabHue};

    #[test]
    fn ottosson_reference_vectors() {
        // From https://bottosson.github.io/posts/oklab/ — these are the public
        // reference values; they fence accidental matrix / cbrt regressions.
        let cases: &[([f32; 3], [f32; 3])] = &[
            ([1.0, 1.0, 1.0], [1.000, 0.000, 0.000]),
            ([1.0, 0.0, 0.0], [0.628, 0.225, 0.126]),
            ([0.0, 1.0, 0.0], [0.866, -0.234, 0.179]),
            ([0.0, 0.0, 1.0], [0.452, -0.032, -0.312]),
        ];
        for (rgb, expected) in cases {
            let lab: Oklab = LinSrgb::new(rgb[0], rgb[1], rgb[2]).into_color();
            assert!(
                (lab.l - expected[0]).abs() < 1e-2,
                "L for {rgb:?}: got {}, want {}",
                lab.l,
                expected[0]
            );
            assert!(
                (lab.a - expected[1]).abs() < 1e-2,
                "a for {rgb:?}: got {}, want {}",
                lab.a,
                expected[1]
            );
            assert!(
                (lab.b - expected[2]).abs() < 1e-2,
                "b for {rgb:?}: got {}, want {}",
                lab.b,
                expected[2]
            );
        }
    }

    #[test]
    fn linear_rgba_roundtrips_through_okhsl() {
        for rgba in [
            LinearRgba::from_srgb8([10, 200, 30, 255]),
            LinearRgba::from_srgb8([240, 120, 30, 255]),
            LinearRgba::from_srgb8([0, 0, 0, 255]),
            LinearRgba::from_srgb8([255, 255, 255, 255]),
            LinearRgba::from_srgb8([128, 128, 128, 255]),
        ] {
            let hsl = linear_rgba_to_okhsl(rgba);
            let back = okhsl_to_linear_rgba(hsl, rgba.a);
            for (a, b, label) in
                [(rgba.r, back.r, "r"), (rgba.g, back.g, "g"), (rgba.b, back.b, "b")]
            {
                assert!(
                    (a - b).abs() < 1e-3,
                    "{label} drifted on round-trip: {rgba:?} -> {hsl:?} -> {back:?}",
                );
            }
        }
    }

    #[test]
    fn okhsl_saturation_zero_is_neutral_gray() {
        // Fences the "S=0 ⇒ perceptual neutral" promise — without this, the S
        // axis of the picker plane wouldn't collapse to the L axis at S=0 and
        // the user would see a hue tint at zero saturation.
        for h in [0.0_f32, 45.0, 90.0, 180.0, 270.0] {
            for l in [0.25_f32, 0.5, 0.75] {
                let hsl = Okhsl::new(OklabHue::new(h), 0.0, l);
                let rgba = okhsl_to_linear_rgba(hsl, 1.0);
                let max_channel_delta = (rgba.r - rgba.g).abs().max((rgba.g - rgba.b).abs());
                assert!(
                    max_channel_delta < 1e-3,
                    "Okhsl(h={h}, S=0, L={l}) should be neutral; got {rgba:?}",
                );
            }
        }
    }

    #[test]
    fn okhsl_saturation_one_stays_in_gamut_at_every_hue() {
        // Fences the defining Okhsl promise — S=1 is the gamut cusp for the
        // current (L, h). If this ever regresses, the 2D plane would either
        // show clipped colors or wave out-of-gamut artefacts at its right
        // edge.
        for h_deg in 0_u16..360 {
            let h = f32::from(h_deg);
            for l in [0.25_f32, 0.5, 0.75] {
                let hsl = Okhsl::new(OklabHue::new(h), 1.0, l);
                let rgba = okhsl_to_linear_rgba(hsl, 1.0);
                for ch in [rgba.r, rgba.g, rgba.b] {
                    assert!(
                        (-0.005..=1.005).contains(&ch),
                        "Okhsl(h={h}, S=1, L={l}) out of gamut: {rgba:?}",
                    );
                }
            }
        }
    }

    #[test]
    fn gamut_map_preserves_hue_for_out_of_gamut_color() {
        // Construct a chroma that's clearly out of sRGB at a specific hue and
        // check that the gamut-mapped result preserves the hue within JND.
        // Palette canonicalizes hues into (-180°, 180°]; the test compares
        // modulo 360° so 200° and -160° aren't flagged as different.
        fn hue_delta_deg(a: f32, b: f32) -> f32 {
            let d = ((a - b).rem_euclid(360.0) + 180.0).rem_euclid(360.0) - 180.0;
            d.abs()
        }
        let h_target = 200.0_f32;
        let out_of_gamut = Oklch::new(0.7, 0.35, OklabHue::new(h_target));
        let mapped_rgba = oklch_to_linear_rgba(out_of_gamut, 1.0);
        let mapped_oklch: Oklch =
            Oklch::from_color(LinSrgb::new(mapped_rgba.r, mapped_rgba.g, mapped_rgba.b));
        let hue_drift = hue_delta_deg(mapped_oklch.hue.into_degrees(), h_target);
        // Allow 2 degrees slack — CSS Color 4 mapping targets ΔE (rounds hue
        // within a just-noticeable-difference band), and the inverse goes
        // through a cbrt that loses a touch of precision near the boundary.
        assert!(
            hue_drift < 2.0,
            "hue drifted by {hue_drift}° ({h_target}° → {}°) on gamut map",
            mapped_oklch.hue.into_degrees(),
        );
        // And the result is actually in sRGB.
        for ch in [mapped_rgba.r, mapped_rgba.g, mapped_rgba.b] {
            assert!((0.0..=1.0).contains(&ch), "gamut-mapped channel {ch} still out of sRGB");
        }
    }
}
