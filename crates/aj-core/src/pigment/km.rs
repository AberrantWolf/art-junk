//! Kubelka–Munk K (absorption) / S (scattering) math.
//!
//! Public-domain physics from Kubelka & Munk (1931). For an opaque thick
//! layer the steady-state reflectance is
//!
//! ```text
//! R∞ = 1 + (K/S) − √((K/S)² + 2·(K/S))
//! ```
//!
//! Solved for `K/S` given `R∞`:
//!
//! ```text
//! K/S = (1 − R∞)² / (2·R∞)
//! ```
//!
//! We anchor `S = 1` per band on intake — only the K/S ratio is observable
//! from a single reflectance, and fixing `S` lets us encode pigments in the
//! single channel that matters for v1 mixing. A future variant that takes
//! per-pigment scattering data (e.g. for tinting-strength differences) can
//! lift this assumption without changing the call surface.

/// Lower clamp on reflectance during K-from-R inversion. `R = 0` would yield
/// infinite absorption; `1e-3` keeps the round-trip well-conditioned at
/// near-black with a perceptually invisible drift (the corresponding
/// reflectance round-trip lands at ~1e-3 instead of exactly 0).
const REFLECTANCE_FLOOR: f32 = 1e-3;

/// Upper clamp on reflectance. `R = 1` is fine analytically (K = 0) but
/// real-world reflectances above 1 are unphysical and would invert to
/// negative K, which the mixing math doesn't model. Clamp on the safe side.
const REFLECTANCE_CEIL: f32 = 1.0 - 1e-6;

/// Compute the Kubelka–Munk absorption coefficient `K` for a given reflectance,
/// assuming `S = 1`. Equivalent to `K/S = (1 − R)² / (2·R)` with R clamped
/// away from the singularities.
pub(super) fn k_from_reflectance(r: f32) -> f32 {
    let r = r.clamp(REFLECTANCE_FLOOR, REFLECTANCE_CEIL);
    let one_minus_r = 1.0 - r;
    (one_minus_r * one_minus_r) / (2.0 * r)
}

/// Compute reflectance from K and S. Returns 0 for non-positive S (degenerate
/// pigment with no scattering — physically a perfect absorber, mathematically
/// undefined under our parameterization).
pub(super) fn reflectance_from_ks(k: f32, s: f32) -> f32 {
    if s <= 0.0 {
        return 0.0;
    }
    let ratio = k / s;
    // R = 1 + ratio − √(ratio² + 2·ratio). For ratio ≥ 0 the bracketed term
    // is non-negative; clamp to guard against tiny negative results from
    // float rounding right at ratio = 0.
    let inner = (ratio * ratio + 2.0 * ratio).max(0.0);
    let r = 1.0 + ratio - inner.sqrt();
    r.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn reflectance_one_gives_zero_absorption() {
        let k = k_from_reflectance(1.0);
        // R clamps to (1 − 1e-6); K is therefore tiny but positive.
        assert!(k < 1e-5, "K at R=1 was {k}");
    }

    #[test]
    fn reflectance_zero_clamps_to_finite_high_k() {
        let k = k_from_reflectance(0.0);
        // Floor of 1e-3 ⇒ K = (0.999)² / 0.002 ≈ 499.
        assert!(k > 100.0 && k < 1000.0, "K at R=0 was {k}");
    }

    #[test]
    fn k_to_r_is_inverse_of_r_to_k_in_safe_range() {
        for r in [0.05_f32, 0.1, 0.25, 0.5, 0.75, 0.9, 0.95] {
            let k = k_from_reflectance(r);
            let back = reflectance_from_ks(k, 1.0);
            assert!(close(r, back, 1e-5), "round-trip {r} → {k} → {back}");
        }
    }

    #[test]
    #[allow(clippy::float_cmp)] // exact 0.0 is the documented early-return value for S ≤ 0.
    fn zero_scattering_resolves_to_zero_reflectance() {
        assert_eq!(reflectance_from_ks(1.0, 0.0), 0.0);
    }
}
