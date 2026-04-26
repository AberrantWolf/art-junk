//! 7-band spectral upsampling for pigment-style mixing.
//!
//! Given a linear-RGB color, produce a 7-band reflectance spectrum so that
//! Kubelka–Munk mixing has somewhere to live: the basis spectra **overlap in
//! the green band**, and that overlap is what makes "blue + yellow = green"
//! actually emerge from the math. Without spectral support — e.g. with a
//! naive 3-band representation where each channel is its own band — the mix
//! collapses to a dark gray because the mixed pigments' absorption
//! coefficients add up everywhere.
//!
//! ## Approach
//!
//! Three basis spectra `R_BASIS`, `G_BASIS`, `B_BASIS` are hand-tuned subject
//! to the constraint `R_BASIS + G_BASIS + B_BASIS = W` (constant 1) at every
//! band. Under that constraint, Smits' (1999) piecewise upsampling algorithm
//! collapses to a simple linear combination:
//!
//! ```text
//! spectrum[i] = r · R_BASIS[i] + g · G_BASIS[i] + b · B_BASIS[i]
//! ```
//!
//! For the inverse step, integrating the spectrum against the matching
//! curves `X_BAR`, `Y_BAR`, `Z_BAR` yields a tristimulus that is *not*
//! linear RGB (the basis isn't biorthogonal to the matching curves). A
//! precomputed 3×3 matrix `M_INV` post-corrects: it's the inverse of the
//! integration of the basis spectra against the matching curves, so by
//! construction `R_BASIS / G_BASIS / B_BASIS` round-trip to `(1,0,0)` /
//! `(0,1,0)` / `(0,0,1)` exactly. Achromatic input round-trips exactly too,
//! since `R + G + B = W` makes `r=g=b=k` produce a flat spectrum.
//!
//! ## Why not Smits' published basis directly
//!
//! Smits' paper hand-fits non-linear basis spectra so his piecewise
//! algorithm produces approximately-correct chromaticity for arbitrary
//! input. We get cleaner round-trip identity by adopting the linear
//! constraint above, at the cost of basis spectra that aren't an exact
//! match to physical primaries — fine for v1 demo behavior. The patent /
//! licensing-clean Sochorová–Jamriška latent-space approach is the eventual
//! tier-3 upgrade (see `.claude/skills/pigment-mixing/`).

use super::BANDS;

/// Per-band reflectance for a "pure red" pigment. Tuned together with
/// `G_BASIS` and `B_BASIS` so each row of `M_INV * (col-of-integrals)` is the
/// identity, giving exact RGB round-trip for pure inputs.
const R_BASIS: [f32; BANDS] = [0.00, 0.00, 0.00, 0.05, 0.35, 0.95, 1.00];
/// Per-band reflectance for a "pure green" pigment. Overlaps into the
/// adjacent blue and red bands by design — that overlap is what carries
/// pigment-mix energy across the green region.
const G_BASIS: [f32; BANDS] = [0.00, 0.05, 0.65, 0.95, 0.65, 0.05, 0.00];
/// Per-band reflectance for a "pure blue" pigment. Note the 0.35 reflectance
/// at band 2 (≈500 nm): without that shoulder, blue pigment would have zero
/// reflectance in the green band, and any mixture with yellow would be dark.
const B_BASIS: [f32; BANDS] = [1.00, 0.95, 0.35, 0.00, 0.00, 0.00, 0.00];

/// Approximate CIE-like RGB-tristimulus matching functions, normalized so
/// each sums to 1.0 over the 7 bands. Integrating the constant-1 spectrum
/// against any of these returns 1, so achromatic colors land on the
/// diagonal of M before post-correction.
const X_BAR: [f32; BANDS] = [0.00, 0.00, 0.05, 0.20, 0.40, 0.30, 0.05];
const Y_BAR: [f32; BANDS] = [0.00, 0.10, 0.35, 0.40, 0.10, 0.05, 0.00];
const Z_BAR: [f32; BANDS] = [0.30, 0.40, 0.20, 0.05, 0.05, 0.00, 0.00];

/// `M_INV`: precomputed inverse of the 3×3 matrix whose columns are the
/// integrals of `R_BASIS`, `G_BASIS`, `B_BASIS` against `X_BAR/Y_BAR/Z_BAR`
/// respectively. Multiplying integrated XYZ by `M_INV` undoes the basis
/// non-orthogonality, so pure inputs round-trip exactly.
///
/// Negative entries are physical: pigment "primaries" don't span a
/// rectangular gamut, so the inverse correction can over-shoot in one
/// channel and under-shoot in another. Out-of-`[0,1]` outputs from the
/// inverse for in-gamut inputs only happen for highly saturated mixtures
/// and are clamped at the `LinearRgba` boundary in [`super::Pigment::to_linear_rgb`].
const M_INV: [[f32; 3]; 3] =
    [[2.4574, -1.9719, 0.5145], [-0.3874, 1.9415, -0.5540], [0.0533, -0.5428, 1.4895]];

/// Linear RGB → 7-band reflectance spectrum.
pub(super) fn upsample_rgb(r: f32, g: f32, b: f32) -> [f32; BANDS] {
    let r = r.max(0.0);
    let g = g.max(0.0);
    let b = b.max(0.0);
    let mut spec = [0.0_f32; BANDS];
    for (i, s) in spec.iter_mut().enumerate() {
        *s = r * R_BASIS[i] + g * G_BASIS[i] + b * B_BASIS[i];
    }
    spec
}

/// 7-band reflectance spectrum → linear RGB. Performs CIE-like integration
/// followed by `M_INV` post-correction to land back in linear RGB. Output is
/// **not** clamped — callers (i.e. [`super::Pigment::to_linear_rgb`]) decide
/// how to handle out-of-gamut results from saturated mixtures.
pub(super) fn integrate_to_rgb(spec: &[f32; BANDS]) -> (f32, f32, f32) {
    let mut tristimulus = [0.0_f32; 3];
    for i in 0..BANDS {
        tristimulus[0] += spec[i] * X_BAR[i];
        tristimulus[1] += spec[i] * Y_BAR[i];
        tristimulus[2] += spec[i] * Z_BAR[i];
    }
    let row =
        |row: [f32; 3]| row[0] * tristimulus[0] + row[1] * tristimulus[1] + row[2] * tristimulus[2];
    (row(M_INV[0]), row(M_INV[1]), row(M_INV[2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    /// Pure-RGB primaries upsample to their basis spectra and integrate back
    /// to themselves within float-precision tolerance.
    #[test]
    fn primaries_roundtrip_exactly() {
        for (r, g, b) in [(1.0, 0.0, 0.0), (0.0, 1.0, 0.0), (0.0, 0.0, 1.0)] {
            let spec = upsample_rgb(r, g, b);
            let (r2, g2, b2) = integrate_to_rgb(&spec);
            assert!(close(r, r2, 5e-4), "R: {r} -> {r2}");
            assert!(close(g, g2, 5e-4), "G: {g} -> {g2}");
            assert!(close(b, b2, 5e-4), "B: {b} -> {b2}");
        }
    }

    /// Achromatic input upsamples to a flat spectrum (since R+G+B=W per
    /// band), and the post-correction matrix maps `(k, k, k)` integrated
    /// tristimulus back to `(k, k, k)` because each row of `M_INV` sums to 1.
    #[test]
    fn achromatic_roundtrips() {
        for k in [0.0_f32, 0.25, 0.5, 0.75, 1.0] {
            let spec = upsample_rgb(k, k, k);
            // Spectrum should be flat at value k.
            for s in spec {
                assert!(close(s, k, 1e-6), "achromatic spec band drifted: {s} vs {k}");
            }
            let (r, g, b) = integrate_to_rgb(&spec);
            assert!(close(r, k, 5e-4), "R drift at k={k}: {r}");
            assert!(close(g, k, 5e-4), "G drift at k={k}: {g}");
            assert!(close(b, k, 5e-4), "B drift at k={k}: {b}");
        }
    }

    /// Blue and yellow basis spectra both have non-trivial reflectance at
    /// band 2 (≈500 nm). This is the structural property that makes
    /// pigment mixing produce green; the rest of the system rests on it,
    /// so guard against accidental edits to the basis tables.
    #[test]
    fn basis_spectra_overlap_in_green_band() {
        // Blue input → spectrum.
        let spec_blue = upsample_rgb(0.0, 0.0, 1.0);
        assert!(spec_blue[2] > 0.10, "blue band-2 reflectance: {}", spec_blue[2]);
        // Yellow input → spectrum.
        let spec_yellow = upsample_rgb(1.0, 1.0, 0.0);
        assert!(spec_yellow[2] > 0.10, "yellow band-2 reflectance: {}", spec_yellow[2]);
    }
}
