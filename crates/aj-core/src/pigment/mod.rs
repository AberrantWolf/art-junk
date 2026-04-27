//! Pigment-style color mixing.
//!
//! `Pigment` is the opaque internal representation of a color in a form that
//! supports paint-like mixing. The public surface is intentionally narrow —
//! `from_linear_rgb`, `to_linear_rgb`, `mix` — so the underlying math can
//! grow without churning callers.
//!
//! v1 ships **7-band Kubelka-Munk** with Smits-style spectral upsampling.
//! The basis spectra in [`spectral`] overlap in the green band, which is what
//! makes "blue + yellow = green" emerge from the math: mixed pigments retain
//! reflectance in the bands where both basis spectra have non-trivial signal,
//! and that's the green region for our chosen blue and yellow primaries. A
//! future tier-3 (Sochorová–Jamriška latent-space) swap stays local to
//! [`km`] and [`spectral`] — call sites do not see the change.
//!
//! See `.claude/skills/pigment-mixing/` for the conceptual reference and the
//! tier-2-vs-tier-3 trade-off.

mod km;
mod spectral;

use crate::LinearRgba;

/// Number of spectral bands used internally. 7 bands cover the visible
/// spectrum at ~50 nm resolution, enough for the green band to receive
/// reflectance from both blue-leaning and yellow-leaning basis spectra
/// (which is what carries pigment-mix energy across the green region).
/// No caller indexes the bands directly, so this can grow further (toward
/// a tier-3 spectral / latent-space representation) without breaking the
/// public API.
pub(crate) const BANDS: usize = 7;

/// Opaque pigment color. Internally stores Kubelka–Munk absorption (`K`) and
/// scattering (`S`) coefficients per spectral band; the layout may change.
/// Callers use [`Pigment::from_linear_rgb`] / [`Pigment::to_linear_rgb`] to
/// cross the boundary, [`Pigment::mix`] to combine two pigments, and the
/// [`MixOp`] trait for canvas-deposit operations in the renderer.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Pigment {
    k: [f32; BANDS],
    s: [f32; BANDS],
}

impl Pigment {
    /// White pigment: zero absorption, unit scattering. Round-trips to
    /// `LinearRgba::WHITE` (with alpha = 1).
    pub const WHITE: Self = Self { k: [0.0; BANDS], s: [1.0; BANDS] };

    /// Construct a pigment from a linear-RGB color. The alpha channel is
    /// dropped — pigment data carries chromatic information only; coverage is
    /// a separate per-deposit input on [`MixOp::deposit`].
    #[must_use]
    pub fn from_linear_rgb(c: LinearRgba) -> Self {
        let spec = spectral::upsample_rgb(c.r, c.g, c.b);
        let mut k = [0.0_f32; BANDS];
        for (k_band, r_band) in k.iter_mut().zip(spec.iter()) {
            *k_band = km::k_from_reflectance(*r_band);
        }
        Self { k, s: [1.0; BANDS] }
    }

    /// Resolve this pigment to a linear-RGB color (alpha = 1). For pure RGB
    /// inputs round-trip is exact within ~5e-4 (basis post-correction is
    /// analytical). Saturated mixtures can produce out-of-`[0,1]` channels
    /// where the inverse-correction matrix overshoots; we clamp at the
    /// boundary so `LinearRgba` stays well-defined.
    #[must_use]
    pub fn to_linear_rgb(self) -> LinearRgba {
        let mut spec = [0.0_f32; BANDS];
        for (i, r_band) in spec.iter_mut().enumerate() {
            *r_band = km::reflectance_from_ks(self.k[i], self.s[i]);
        }
        let (r, g, b) = spectral::integrate_to_rgb(&spec);
        LinearRgba::new(r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0), 1.0)
    }

    /// Linearly interpolate this pigment toward `other` by `t ∈ [0, 1]`. The
    /// mix is **in K/S space**, not in linear RGB — that's the whole point;
    /// it produces paint-like (non-linear) blends.
    #[must_use]
    pub fn mix(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let u = 1.0 - t;
        let mut k = [0.0_f32; BANDS];
        let mut s = [0.0_f32; BANDS];
        for ((dst_k, dst_s), i) in k.iter_mut().zip(s.iter_mut()).zip(0..BANDS) {
            *dst_k = u * self.k[i] + t * other.k[i];
            *dst_s = u * self.s[i] + t * other.s[i];
        }
        Self { k, s }
    }
}

/// Deposit operator: combine a brush pigment with the pigment already on the
/// canvas at the given coverage. The renderer dispatches through this trait
/// so a future tier-3 (latent-space) implementation can swap in without
/// changing layer-resolve plumbing.
///
/// `coverage` is in `[0, 1]`: 0 keeps the canvas as-is, 1 fully replaces
/// canvas pigment with brush pigment.
pub trait MixOp {
    fn deposit(&self, canvas: Pigment, brush: Pigment, coverage: f32) -> Pigment;
}

/// Tier-2 Kubelka–Munk deposit: linear weighted average of K and S, weighted
/// by `coverage`. Cheap, public-domain physics, license-clean.
#[derive(Debug, Default, Clone, Copy)]
pub struct KubelkaMunk;

impl MixOp for KubelkaMunk {
    fn deposit(&self, canvas: Pigment, brush: Pigment, coverage: f32) -> Pigment {
        canvas.mix(brush, coverage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    fn rgb_close(a: LinearRgba, b: LinearRgba, eps: f32) -> bool {
        approx_eq(a.r, b.r, eps) && approx_eq(a.g, b.g, eps) && approx_eq(a.b, b.b, eps)
    }

    #[test]
    fn white_roundtrips() {
        let p = Pigment::from_linear_rgb(LinearRgba::WHITE);
        let back = p.to_linear_rgb();
        assert!(rgb_close(back, LinearRgba::WHITE, 1e-4), "got {back:?}");
    }

    #[test]
    fn primaries_roundtrip_within_tolerance() {
        // Sample colors known to round-trip well in 3-band KM (avoid pure
        // 0.0 reflectance which is the K=∞ singularity).
        for (r, g, b) in [(0.05, 0.05, 0.8), (0.9, 0.85, 0.05), (0.5, 0.5, 0.5), (0.2, 0.7, 0.3)] {
            let c = LinearRgba::new(r, g, b, 1.0);
            let p = Pigment::from_linear_rgb(c);
            let back = p.to_linear_rgb();
            assert!(rgb_close(c, back, 1e-3), "round-trip drifted: {c:?} -> {back:?}");
        }
    }

    #[test]
    fn mix_endpoints_are_exact() {
        let a = Pigment::from_linear_rgb(LinearRgba::new(0.2, 0.5, 0.8, 1.0));
        let b = Pigment::from_linear_rgb(LinearRgba::new(0.7, 0.3, 0.1, 1.0));
        assert_eq!(a.mix(b, 0.0), a);
        assert_eq!(a.mix(b, 1.0), b);
    }

    #[test]
    fn mix_is_symmetric() {
        let a = Pigment::from_linear_rgb(LinearRgba::new(0.2, 0.5, 0.8, 1.0));
        let b = Pigment::from_linear_rgb(LinearRgba::new(0.7, 0.3, 0.1, 1.0));
        let m1 = a.mix(b, 0.3);
        let m2 = b.mix(a, 0.7);
        for i in 0..BANDS {
            assert!(approx_eq(m1.k[i], m2.k[i], 1e-5));
            assert!(approx_eq(m1.s[i], m2.s[i], 1e-5));
        }
    }

    /// Pigment mixing must not collapse to plain linear-RGB lerp — that's the
    /// whole reason this module exists. The lerp of blue and yellow gives
    /// olive-grey; pigment mixing must differ visibly.
    #[test]
    fn mix_differs_from_linear_lerp() {
        let blue = LinearRgba::new(0.05, 0.05, 0.80, 1.0);
        let yellow = LinearRgba::new(0.90, 0.85, 0.05, 1.0);
        let pigment_mix = Pigment::from_linear_rgb(blue)
            .mix(Pigment::from_linear_rgb(yellow), 0.5)
            .to_linear_rgb();
        let lerp_mix = LinearRgba::new(
            0.5 * (blue.r + yellow.r),
            0.5 * (blue.g + yellow.g),
            0.5 * (blue.b + yellow.b),
            1.0,
        );
        let dr = (pigment_mix.r - lerp_mix.r).abs();
        let dg = (pigment_mix.g - lerp_mix.g).abs();
        let db = (pigment_mix.b - lerp_mix.b).abs();
        let total = dr + dg + db;
        assert!(
            total > 0.05,
            "pigment mix {pigment_mix:?} is suspiciously close to linear lerp {lerp_mix:?}"
        );
    }

    /// The headline demo: blue + 50% yellow lands in Oklab green territory
    /// (negative `a`). This is the property a paint-vocabulary drawing app
    /// has to deliver to feel honest, and the whole reason for the spectral
    /// basis in `pigment::spectral`.
    #[test]
    fn blue_plus_yellow_is_green() {
        let blue = LinearRgba::new(0.05, 0.05, 0.80, 1.0);
        let yellow = LinearRgba::new(0.90, 0.85, 0.05, 1.0);
        let mix = Pigment::from_linear_rgb(blue)
            .mix(Pigment::from_linear_rgb(yellow), 0.5)
            .to_linear_rgb();
        let (_, a, b) = linear_rgb_to_oklab(mix.r, mix.g, mix.b);
        assert!(
            a < -0.03,
            "blue + yellow should land green-side in Oklab (negative a); got a={a}, b={b}, rgb={mix:?}"
        );
        assert!(mix.g > mix.r, "expected G > R for green mix; got {mix:?}");
        assert!(mix.g > mix.b, "expected G > B for green mix; got {mix:?}");
    }

    /// Linear-sRGB → Oklab. Hardcoded matrix coefficients from
    /// <https://bottosson.github.io/posts/oklab/>. Used only by
    /// `blue_plus_yellow_is_green`; not part of the module's public API.
    #[allow(clippy::many_single_char_names)] // (r, g, b) and (l, m, s) are the canonical Oklab names; renaming hurts readability.
    fn linear_rgb_to_oklab(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
        let l = 0.412_221_47 * r + 0.536_332_55 * g + 0.051_445_995 * b;
        let m = 0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b;
        let s = 0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b;
        let l_ = l.cbrt();
        let m_ = m.cbrt();
        let s_ = s.cbrt();
        (
            0.210_454_26 * l_ + 0.793_617_8 * m_ - 0.004_072_047 * s_,
            1.977_998_5 * l_ - 2.428_592_2 * m_ + 0.450_593_7 * s_,
            0.025_904_037 * l_ + 0.782_771_77 * m_ - 0.808_675_77 * s_,
        )
    }

    #[test]
    fn kubelka_munk_deposit_at_zero_coverage_keeps_canvas() {
        let canvas = Pigment::from_linear_rgb(LinearRgba::new(0.2, 0.5, 0.8, 1.0));
        let brush = Pigment::from_linear_rgb(LinearRgba::new(0.7, 0.3, 0.1, 1.0));
        assert_eq!(KubelkaMunk.deposit(canvas, brush, 0.0), canvas);
    }

    #[test]
    fn kubelka_munk_deposit_at_full_coverage_replaces_with_brush() {
        let canvas = Pigment::from_linear_rgb(LinearRgba::new(0.2, 0.5, 0.8, 1.0));
        let brush = Pigment::from_linear_rgb(LinearRgba::new(0.7, 0.3, 0.1, 1.0));
        assert_eq!(KubelkaMunk.deposit(canvas, brush, 1.0), brush);
    }
}
