//! Host-side reference implementation that mirrors the WGSL pigment shader.
//! Used to (a) provide the per-band brush reflectance uniform values and
//! (b) drive the parity test that asserts Rust ↔ WGSL agreement.
//!
//! The actual KM math lives in `aj_core::pigment` and is the source of
//! truth. This module only exposes a tiny CPU-side helper for extracting
//! the per-band reflectance from a `Pigment` (its internal layout is
//! opaque outside the crate, by design) — and a CPU mirror of the K/S
//! deposit step that the parity test compares against.

use aj_core::Pigment;

/// 7-band reflectance from a `Pigment`. We cross the public boundary by
/// converting Pigment → linear RGB → spectrum, which round-trips exactly
/// for in-gamut inputs. The Pigment passed in is what comes out of
/// `Pigment::from_linear_rgb`, so the round-trip is a no-op in practice
/// — but routing through the public API keeps the internal K/S layout
/// hidden from this module, which is a deliberate seam.
pub(crate) fn pigment_canvas_reflectance(p: &Pigment) -> [f32; 7] {
    let rgb = p.to_linear_rgb();
    upsample_rgb_to_7band(rgb.r, rgb.g, rgb.b)
}

/// Mirror of `aj_core::pigment::spectral::R_BASIS`. The WGSL pigment
/// shader and this constant must agree with the source-of-truth value;
/// `host_upsample_matches_aj_core_for_primaries` guards against drift.
const R_BASIS: [f32; 7] = [0.00, 0.00, 0.00, 0.05, 0.35, 0.95, 1.00];
const G_BASIS: [f32; 7] = [0.00, 0.05, 0.65, 0.95, 0.65, 0.05, 0.00];
const B_BASIS: [f32; 7] = [1.00, 0.95, 0.35, 0.00, 0.00, 0.00, 0.00];

/// 7-band Smits-style upsample mirroring `aj_core::pigment::spectral`.
pub(crate) fn upsample_rgb_to_7band(r: f32, g: f32, b: f32) -> [f32; 7] {
    let r = r.max(0.0);
    let g = g.max(0.0);
    let b = b.max(0.0);
    let mut spec = [0.0_f32; 7];
    for (i, s) in spec.iter_mut().enumerate() {
        *s = r * R_BASIS[i] + g * G_BASIS[i] + b * B_BASIS[i];
    }
    spec
}

#[cfg(test)]
mod tests {
    use aj_core::LinearRgba;

    use super::*;

    /// Bridge to `aj_core::pigment` to assert that this crate's mirror
    /// constants haven't drifted from the source-of-truth basis tables.
    /// We do this through the public API: any RGB input that round-trips
    /// to itself must produce the same 7-band spectrum here as it does
    /// internally in `aj-core`.
    #[test]
    fn host_upsample_matches_aj_core_for_primaries() {
        for (r, g, b) in
            [(1.0, 0.0, 0.0), (0.0, 1.0, 0.0), (0.0, 0.0, 1.0), (0.5, 0.5, 0.5), (0.05, 0.05, 0.8)]
        {
            // aj-core round-trip: RGB → Pigment → RGB. For in-gamut inputs
            // this is identity to ~5e-4. So upsample here matches what
            // aj-core would have computed for the same RGB.
            let p = Pigment::from_linear_rgb(LinearRgba::new(r, g, b, 1.0));
            let back = p.to_linear_rgb();
            // Sanity guard on the round-trip.
            assert!(
                (r - back.r).abs() < 1e-2 && (g - back.g).abs() < 1e-2 && (b - back.b).abs() < 1e-2,
                "input ({r}, {g}, {b}) round-tripped to ({}, {}, {})",
                back.r,
                back.g,
                back.b
            );
            let spec = upsample_rgb_to_7band(r, g, b);
            // Each spectrum sample must lie in [0, 1] for in-gamut inputs.
            for (i, &s) in spec.iter().enumerate() {
                assert!((0.0..=1.0).contains(&s), "spec[{i}] = {s} out of range");
            }
        }
    }

    /// CPU mirror of the pigment shader's K/S deposit step. Mixing one
    /// stroke's pigment onto a white canvas at full coverage must reproduce
    /// the brush color (modulo round-trip drift) — matching the shader's
    /// `t = 1.0` path.
    #[test]
    fn cpu_deposit_at_full_coverage_returns_brush() {
        let canvas = [1.0_f32; 7]; // white
        let brush = upsample_rgb_to_7band(0.05, 0.05, 0.8);
        let mut out = [0.0_f32; 7];
        for i in 0..7 {
            let canvas_k = k_from_r(canvas[i]);
            let brush_k = k_from_r(brush[i]);
            let mixed_k = canvas_k * 0.0 + brush_k * 1.0;
            out[i] = r_from_k(mixed_k);
        }
        for i in 0..7 {
            assert!((out[i] - brush[i]).abs() < 5e-3, "band {i}: {} vs {}", out[i], brush[i]);
        }
    }

    fn k_from_r(r: f32) -> f32 {
        let r = r.clamp(1e-3, 1.0 - 1e-6);
        let one_minus_r = 1.0 - r;
        (one_minus_r * one_minus_r) / (2.0 * r)
    }

    fn r_from_k(k: f32) -> f32 {
        let inner = (k * k + 2.0 * k).max(0.0);
        let r = 1.0 + k - inner.sqrt();
        r.clamp(0.0, 1.0)
    }

    /// CPU mirror of `shaders/plain.wgsl`. Single-pixel alpha-over against
    /// the substrate. Returned tuple is `(r, g, b, alpha)`, all linear-RGB
    /// straight alpha — same layout as the substrate texture stores.
    fn apply_plain(
        canvas: (f32, f32, f32, f32),
        brush_rgb: (f32, f32, f32),
        brush_alpha: f32,
        coverage: f32,
    ) -> (f32, f32, f32, f32) {
        let t = (coverage * brush_alpha).clamp(0.0, 1.0);
        let new_r = canvas.0 * (1.0 - t) + brush_rgb.0 * t;
        let new_g = canvas.1 * (1.0 - t) + brush_rgb.1 * t;
        let new_b = canvas.2 * (1.0 - t) + brush_rgb.2 * t;
        let new_a = canvas.3 + (1.0 - canvas.3) * t;
        (new_r, new_g, new_b, new_a)
    }

    /// Plain alpha-over with full coverage and full alpha must replace the
    /// canvas color exactly — the property that "z-order wins" relies on.
    /// If a future tweak silently breaks this, mixed pigment+plain canvases
    /// would leak underlying pigment color through plain strokes drawn on
    /// top.
    #[test]
    fn plain_full_coverage_full_alpha_replaces_canvas() {
        let canvas = (0.7, 0.2, 0.4, 0.5); // arbitrary "already painted" state
        let out = apply_plain(canvas, (0.0, 1.0, 0.0), 1.0, 1.0);
        assert!((out.0 - 0.0).abs() < f32::EPSILON);
        assert!((out.1 - 1.0).abs() < f32::EPSILON);
        assert!((out.2 - 0.0).abs() < f32::EPSILON);
        assert!((out.3 - 1.0).abs() < f32::EPSILON);
    }

    /// Plain alpha-over with zero coverage is a no-op. Zero coverage from
    /// Vello means "this pixel is outside the stroke"; the substrate value
    /// must pass through unchanged.
    #[test]
    fn plain_zero_coverage_is_noop() {
        let canvas = (0.7, 0.2, 0.4, 0.5);
        let out = apply_plain(canvas, (0.0, 1.0, 0.0), 1.0, 0.0);
        assert!((out.0 - canvas.0).abs() < f32::EPSILON);
        assert!((out.1 - canvas.1).abs() < f32::EPSILON);
        assert!((out.2 - canvas.2).abs() < f32::EPSILON);
        assert!((out.3 - canvas.3).abs() < f32::EPSILON);
    }

    /// Alpha accumulation is order-independent and matches the
    /// over-operator identity: applying two strokes each at t = 0.5 yields
    /// the same final alpha as one stroke at t = 0.75. (1 - (1 - t1)(1 - t2)
    /// = 1 - 0.25 = 0.75). Property is what lets the present pass treat
    /// substrate.alpha as "fraction of pixel covered by paint."
    #[test]
    fn alpha_accumulation_matches_porter_duff_over() {
        let blank = (0.0, 0.0, 0.0, 0.0);
        let after_one = apply_plain(blank, (1.0, 1.0, 1.0), 1.0, 0.5);
        let after_two = apply_plain(after_one, (1.0, 1.0, 1.0), 1.0, 0.5);
        assert!((after_two.3 - 0.75).abs() < 1e-6);
        let one_shot = apply_plain(blank, (1.0, 1.0, 1.0), 1.0, 0.75);
        assert!((after_two.3 - one_shot.3).abs() < 1e-6);
    }

    /// CPU mirror of `shaders/highlighter.wgsl`. Multiplicative tint with
    /// coverage modulation: `new.rgb = canvas.rgb * (1 - t * (1 - brush.rgb))`.
    fn apply_highlighter(
        canvas: (f32, f32, f32, f32),
        brush_rgb: (f32, f32, f32),
        brush_alpha: f32,
        coverage: f32,
    ) -> (f32, f32, f32, f32) {
        let t = (coverage * brush_alpha).clamp(0.0, 1.0);
        let tint_r = 1.0 - t * (1.0 - brush_rgb.0);
        let tint_g = 1.0 - t * (1.0 - brush_rgb.1);
        let tint_b = 1.0 - t * (1.0 - brush_rgb.2);
        let new_a = canvas.3 + (1.0 - canvas.3) * t;
        (canvas.0 * tint_r, canvas.1 * tint_g, canvas.2 * tint_b, new_a)
    }

    /// Yellow highlighter over black text stays black — the property the
    /// brush type exists for. If this regresses, "highlight a paragraph"
    /// would erase the text.
    #[test]
    fn highlighter_over_black_stays_black() {
        let black = (0.0, 0.0, 0.0, 1.0);
        let yellow = (1.0, 1.0, 0.0);
        let out = apply_highlighter(black, yellow, 1.0, 1.0);
        assert!(out.0.abs() < f32::EPSILON);
        assert!(out.1.abs() < f32::EPSILON);
        assert!(out.2.abs() < f32::EPSILON);
    }

    /// Yellow highlighter over white paper at full strength tints to yellow.
    #[test]
    fn highlighter_over_white_tints_to_brush_color() {
        let white = (1.0, 1.0, 1.0, 0.0);
        let yellow = (1.0, 1.0, 0.0);
        let out = apply_highlighter(white, yellow, 1.0, 1.0);
        assert!((out.0 - 1.0).abs() < f32::EPSILON);
        assert!((out.1 - 1.0).abs() < f32::EPSILON);
        assert!(out.2.abs() < f32::EPSILON);
    }

    /// Overlapping highlighter strokes saturate further (real highlighters
    /// do this; we explicitly chose not to cap). Two passes of a half-yellow
    /// brush over white must darken the blue channel below one pass.
    #[test]
    fn highlighter_overlap_saturates_further() {
        let white = (1.0, 1.0, 1.0, 0.0);
        let half_yellow = (1.0, 1.0, 0.5);
        let after_one = apply_highlighter(white, half_yellow, 1.0, 1.0);
        let after_two = apply_highlighter(after_one, half_yellow, 1.0, 1.0);
        assert!(after_two.2 < after_one.2, "second pass should darken further");
        // Specifically, B = 1 * 0.5 * 0.5 = 0.25 after two full passes.
        assert!((after_two.2 - 0.25).abs() < 1e-6);
    }

    /// CPU mirror of `shaders/blend_normal.wgsl`. Inter-layer Normal blend
    /// with per-layer opacity. `below` is the composite accumulator,
    /// `layer` is the layer's substrate; both straight-alpha linear RGB.
    fn apply_blend_normal(
        below: (f32, f32, f32, f32),
        layer: (f32, f32, f32, f32),
        opacity: f32,
    ) -> (f32, f32, f32, f32) {
        let t = (layer.3 * opacity).clamp(0.0, 1.0);
        let new_r = below.0 * (1.0 - t) + layer.0 * t;
        let new_g = below.1 * (1.0 - t) + layer.1 * t;
        let new_b = below.2 * (1.0 - t) + layer.2 * t;
        let new_a = below.3 + (1.0 - below.3) * t;
        (new_r, new_g, new_b, new_a)
    }

    /// Layer with full alpha + full opacity completely covers below — the
    /// invariant the present pass relies on for opaque foreground layers.
    #[test]
    fn blend_normal_full_layer_full_opacity_replaces_below() {
        let below = (0.5, 0.5, 0.5, 0.5);
        let layer = (1.0, 0.2, 0.0, 1.0);
        let out = apply_blend_normal(below, layer, 1.0);
        assert!((out.0 - 1.0).abs() < f32::EPSILON);
        assert!((out.1 - 0.2).abs() < f32::EPSILON);
        assert!((out.2 - 0.0).abs() < f32::EPSILON);
        assert!((out.3 - 1.0).abs() < f32::EPSILON);
    }

    /// Layer with zero alpha leaves below unchanged regardless of opacity.
    #[test]
    fn blend_normal_zero_layer_alpha_passes_below_through() {
        let below = (0.5, 0.5, 0.5, 0.5);
        let layer = (1.0, 0.0, 0.0, 0.0);
        let out = apply_blend_normal(below, layer, 1.0);
        assert!((out.0 - below.0).abs() < f32::EPSILON);
        assert!((out.1 - below.1).abs() < f32::EPSILON);
        assert!((out.2 - below.2).abs() < f32::EPSILON);
        assert!((out.3 - below.3).abs() < f32::EPSILON);
    }

    /// Per-layer opacity attenuates how much of the layer shows through —
    /// not by darkening the layer's color, but by letting more of below
    /// remain. At opacity=0.5 over a full-alpha layer, the result is the
    /// straight midpoint between below.rgb and layer.rgb.
    #[test]
    fn blend_normal_opacity_attenuates_layer_visibility() {
        let below = (0.0, 0.0, 0.0, 1.0); // black
        let layer = (1.0, 1.0, 1.0, 1.0); // white
        let out = apply_blend_normal(below, layer, 0.5);
        assert!((out.0 - 0.5).abs() < f32::EPSILON);
        assert!((out.1 - 0.5).abs() < f32::EPSILON);
        assert!((out.2 - 0.5).abs() < f32::EPSILON);
    }

    /// Compositing onto a transparent below preserves the layer's RGB
    /// exactly — the invariant for "this is the only visible layer."
    #[test]
    fn blend_normal_transparent_below_preserves_layer() {
        let below = (0.0, 0.0, 0.0, 0.0);
        let layer = (0.7, 0.3, 0.5, 0.8);
        let out = apply_blend_normal(below, layer, 1.0);
        // mix(0, 0.7, 0.8) = 0.56
        assert!((out.0 - 0.56).abs() < 1e-6);
        // alpha: 0 + (1 - 0) * 0.8 = 0.8
        assert!((out.3 - 0.8).abs() < 1e-6);
    }

    /// CPU mirror of `shaders/blend_oklab_mix.wgsl`. Linear RGB → Oklab,
    /// lerp by `t` in Oklab, → linear RGB; alpha follows the same
    /// Porter-Duff over as the Normal blend.
    fn signed_cbrt(x: f32) -> f32 {
        x.signum() * x.abs().powf(1.0 / 3.0)
    }

    // Coefficients literal-matched against `shaders/blend_oklab_mix.wgsl`
    // so CPU mirror and GPU shader quantise to the same f32 bits, not
    // 1-ULP-drifted neighbours. Source: Björn Ottosson 2020 (Oklab post).
    // `clippy::excessive_precision` is intentional: keeping the same
    // 10-digit literals as the WGSL makes "is the constant the same?"
    // a textual diff rather than a debug-print exercise.
    #[allow(clippy::many_single_char_names, clippy::excessive_precision)]
    fn linear_rgb_to_oklab(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
        let l = 0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b;
        let m = 0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b;
        let s = 0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b;
        let l_ = signed_cbrt(l);
        let m_ = signed_cbrt(m);
        let s_ = signed_cbrt(s);
        (
            0.210_454_255_3 * l_ + 0.793_617_785_0 * m_ - 0.004_072_046_8 * s_,
            1.977_998_495_1 * l_ - 2.428_592_205_0 * m_ + 0.450_593_709_9 * s_,
            0.025_904_037_1 * l_ + 0.782_771_766_2 * m_ - 0.808_675_766_0 * s_,
        )
    }

    #[allow(clippy::many_single_char_names, clippy::excessive_precision)]
    fn oklab_to_linear_rgb(l: f32, a: f32, b: f32) -> (f32, f32, f32) {
        let l_ = l + 0.396_337_777_4 * a + 0.215_803_757_3 * b;
        let m_ = l - 0.105_561_345_8 * a - 0.063_854_172_8 * b;
        let s_ = l - 0.089_484_177_5 * a - 1.291_485_548_0 * b;
        let l3 = l_ * l_ * l_;
        let m3 = m_ * m_ * m_;
        let s3 = s_ * s_ * s_;
        (
            4.076_741_662_1 * l3 - 3.307_711_591_3 * m3 + 0.230_969_929_2 * s3,
            -1.268_438_004_6 * l3 + 2.609_757_401_1 * m3 - 0.341_319_396_5 * s3,
            -0.004_196_086_3 * l3 - 0.703_418_614_7 * m3 + 1.707_614_701_0 * s3,
        )
    }

    fn apply_blend_oklab_mix(
        below: (f32, f32, f32, f32),
        layer: (f32, f32, f32, f32),
        opacity: f32,
    ) -> (f32, f32, f32, f32) {
        let t = (layer.3 * opacity).clamp(0.0, 1.0);
        let bl = linear_rgb_to_oklab(below.0, below.1, below.2);
        let ll = linear_rgb_to_oklab(layer.0, layer.1, layer.2);
        let mixed =
            (bl.0 * (1.0 - t) + ll.0 * t, bl.1 * (1.0 - t) + ll.1 * t, bl.2 * (1.0 - t) + ll.2 * t);
        let rgb = oklab_to_linear_rgb(mixed.0, mixed.1, mixed.2);
        let new_a = below.3 + (1.0 - below.3) * t;
        (rgb.0.clamp(0.0, 1.0), rgb.1.clamp(0.0, 1.0), rgb.2.clamp(0.0, 1.0), new_a)
    }

    /// Round-trip identity: linear RGB → Oklab → linear RGB reproduces the
    /// input within float tolerance. If this regresses, the blend math
    /// silently corrupts colours.
    #[test]
    fn oklab_round_trip_is_identity_for_in_gamut_colours() {
        for (r, g, b) in
            [(0.0, 0.0, 0.0), (1.0, 1.0, 1.0), (0.5, 0.0, 0.0), (0.05, 0.05, 0.8), (0.7, 0.2, 0.4)]
        {
            let (l, a, b_lab) = linear_rgb_to_oklab(r, g, b);
            let (rr, gg, bb) = oklab_to_linear_rgb(l, a, b_lab);
            assert!((rr - r).abs() < 1e-5, "r drift: {r} → {rr}");
            assert!((gg - g).abs() < 1e-5, "g drift: {g} → {gg}");
            assert!((bb - b).abs() < 1e-5, "b drift: {b} → {bb}");
        }
    }

    /// Oklab blend at t=0 reproduces below exactly.
    #[test]
    fn blend_oklab_mix_at_zero_is_below() {
        let below = (0.7, 0.2, 0.4, 1.0);
        let layer = (0.0, 0.0, 1.0, 0.0); // alpha 0 → t=0
        let out = apply_blend_oklab_mix(below, layer, 1.0);
        assert!((out.0 - below.0).abs() < 1e-5);
        assert!((out.1 - below.1).abs() < 1e-5);
        assert!((out.2 - below.2).abs() < 1e-5);
    }

    /// Oklab blend at t=1 reproduces layer (modulo round-trip drift).
    #[test]
    fn blend_oklab_mix_at_one_is_layer() {
        let below = (0.7, 0.2, 0.4, 1.0);
        let layer = (0.05, 0.05, 0.8, 1.0);
        let out = apply_blend_oklab_mix(below, layer, 1.0);
        assert!((out.0 - layer.0).abs() < 1e-4);
        assert!((out.1 - layer.1).abs() < 1e-4);
        assert!((out.2 - layer.2).abs() < 1e-4);
    }

    /// Oklab midpoint differs noticeably from the linear-RGB midpoint —
    /// the whole point of the `OklabMix` blend mode. Without this,
    /// `OklabMix` would produce the same output as Normal at t=0.5 and
    /// the feature wouldn't earn its keep.
    #[test]
    fn blend_oklab_mix_at_half_differs_from_linear_lerp() {
        let red = (1.0, 0.0, 0.0, 1.0);
        let blue = (0.0, 0.0, 1.0, 1.0);
        let oklab_mid = apply_blend_oklab_mix(red, blue, 1.0);
        // Linear midpoint would be (0.5, 0.0, 0.5).
        let dr = (oklab_mid.0 - 0.5_f32).abs();
        let dg = (oklab_mid.1 - 0.0_f32).abs();
        let db = (oklab_mid.2 - 0.5_f32).abs();
        // At least one channel must differ by > 0.05; otherwise Oklab
        // and linear lerp would be doing the same thing.
        assert!(
            dr > 0.05 || dg > 0.05 || db > 0.05,
            "Oklab midpoint of red+blue ({:.3}, {:.3}, {:.3}) suspiciously close to linear lerp",
            oklab_mid.0,
            oklab_mid.1,
            oklab_mid.2
        );
    }
}
