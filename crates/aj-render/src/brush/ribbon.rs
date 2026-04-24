//! Ribbon brush tessellation.
//!
//! Builds a filled polygon strip (with round caps) for a stroke, using:
//!
//! - **Catmull-Rom centripetal** (α = 0.5) spline fitting between consecutive
//!   samples to smooth out pen-tip jitter and pixel-quantization artifacts.
//!   Each interior sample-pair becomes one `CubicBez`. Boundary segments use
//!   duplicate-endpoint phantoms (`P0 = P1` at stroke start, `P3 = P2` at
//!   end).
//! - **Arc-length tessellation** at roughly one physical-pixel step
//!   (translated to world units via the current `world_to_screen` affine's
//!   determinant). Gives a constant perceptual step regardless of zoom.
//! - **Linear pressure interpolation** between sample endpoints within a
//!   segment. Splining pressure would manufacture data we don't have — the
//!   only ground truth is the two endpoint pressures.
//! - **Round caps** emitted as semicircle fans at the stroke's first and last
//!   sample positions, perpendicular to the local tangent, plus additional
//!   cusp caps wherever the offset rail would fold (pen-lift reversals, sharp
//!   hairpins). Fan vertex count scales with cap radius in physical pixels,
//!   bounded `[CAP_VERTEX_MIN, CAP_VERTEX_MAX]`.
//! - **Cusp splitting** at points where `|κ|·half_width ≥ CUSP_THRESHOLD`.
//!   The stroke is emitted as a series of sub-segment ribbons joined by round
//!   caps; their union approximates the Minkowski-swept-disk silhouette that
//!   a bristle brush actually traces.
//!
//! The returned `BezPath` is in world space and expected to be filled with
//! `Fill::NonZero`. Sub-segment ribbons and caps are emitted as separate
//! sub-paths; the non-zero winding rule handles the union.

// Geometry code casts between usize (vertex counts) and f64 (angles /
// arclens) frequently. All casts are bounded well inside safe ranges.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::f64::consts::PI;

use aj_core::{BrushParams, Sample, Stroke};
use vello::kurbo::{
    Affine, BezPath, CubicBez, ParamCurve, ParamCurveArclen, ParamCurveDeriv, Point, Vec2,
};

/// Minimum tessellation vertex count for round caps. Ensures small caps still
/// look round rather than faceted.
const CAP_VERTEX_MIN: usize = 8;
/// Maximum tessellation vertex count for round caps. Caps on very large
/// brushes won't explode the vertex count.
const CAP_VERTEX_MAX: usize = 64;
/// Target tessellation step in physical pixels. One-pixel step is below the
/// perceptual threshold of polygon faceting.
const WORLD_STEP_PER_PX: f64 = 1.0;
/// Accuracy parameter for `ParamCurveArclen`. Half a target step in world
/// units is more than enough — arc-length error this small is invisible.
const ARCLEN_ACCURACY_FACTOR: f64 = 0.5;
/// Below this tangent magnitude we treat the tangent as degenerate and fall
/// back to the previous step's tangent (or `(1, 0)` for the first step).
const TANGENT_EPSILON: f64 = 1e-6;
/// Signed curvature × half-width at which the offset rail cusps (analytically
/// this is exactly 1; we split slightly earlier to absorb numerical grazing
/// and to keep the cusp cap radius tangibly larger than the rail fold).
///
/// Sharp direction reversals (pen-lift artifacts, deliberate hairpins) make
/// the ribbon's two rails cross each other. Because the crossing region has
/// opposite winding from the rest of the ribbon, `Fill::NonZero` cancels it
/// out and punches a visible notch — the user sees a hole at the end of the
/// stroke. We split the stroke into sub-segments at these cusps and round-cap
/// each side so the union is a swept-disk silhouette.
///
/// This is the pragmatic version of the classical offset + overlap-removal
/// approach. The SOTA (Levien et al. 2024, "GPU-friendly Stroke Expansion")
/// uses Euler-spiral centerlines where cusps emerge naturally from adaptive
/// flattening; migrating there is a later-milestone rewrite.
const CUSP_THRESHOLD: f64 = 0.95;

/// Tessellate a stroke into a filled path. Empty for zero-sample strokes.
#[must_use]
pub(crate) fn tessellate_stroke(stroke: &Stroke, world_to_screen: Affine) -> BezPath {
    let samples = &stroke.samples;
    if samples.is_empty() {
        return BezPath::new();
    }

    let screen_scale = world_to_screen.determinant().abs().sqrt();
    let world_step =
        if screen_scale > 0.0 { WORLD_STEP_PER_PX / screen_scale } else { WORLD_STEP_PER_PX };

    if samples.len() == 1 {
        return single_sample_disc(&samples[0], stroke.brush, screen_scale);
    }

    let mut path = BezPath::new();
    let mut left: Vec<Point> = Vec::with_capacity(samples.len() * 4);
    let mut right: Vec<Point> = Vec::with_capacity(samples.len() * 4);
    let mut prev_tangent = Vec2::new(1.0, 0.0);
    // We capture first / last tangents during the walk so the endpoint caps
    // know how to orient themselves. `prev_position` and `prev_half` track
    // the previous step's centerline and half-width so a mid-walk cusp split
    // can cap off the closing sub-segment at the right place and radius.
    let mut first_tangent = Vec2::new(1.0, 0.0);
    let mut last_tangent = Vec2::new(1.0, 0.0);
    let mut prev_position: Point = samples[0].position.into();
    let mut prev_half: f32 = half_width_at_sample(&samples[0], stroke.brush);
    let mut wrote_any = false;

    for i in 0..samples.len() - 1 {
        let s_a = &samples[i];
        let s_b = &samples[i + 1];

        // Skip zero-length segments; they'd divide-by-zero in the CR math and
        // add nothing visible.
        if s_a.position == s_b.position {
            continue;
        }

        let p0: Point = if i == 0 { s_a.position } else { samples[i - 1].position }.into();
        let p3: Point =
            if i + 2 >= samples.len() { s_b.position } else { samples[i + 2].position }.into();

        let seg = centripetal_catmull_rom(p0, s_a.position.into(), s_b.position.into(), p3);
        let len = seg.arclen(world_step * ARCLEN_ACCURACY_FACTOR);
        let n_steps = ((len / world_step).ceil() as usize).max(1);

        for k in 0..=n_steps {
            // For the first step of segment 0 we emit the initial vertex.
            // For every subsequent segment we skip k=0 to avoid duplicating
            // the previous segment's end vertex.
            if k == 0 && wrote_any {
                continue;
            }
            let t = (k as f64) / (n_steps as f64);
            let arc = len * t;
            let t_param = seg.inv_arclen(arc, world_step * ARCLEN_ACCURACY_FACTOR).clamp(0.0, 1.0);
            let p = seg.eval(t_param);
            let raw_tangent = seg.deriv().eval(t_param).to_vec2();
            let tangent = if raw_tangent.hypot() < TANGENT_EPSILON {
                prev_tangent
            } else {
                raw_tangent / raw_tangent.hypot()
            };

            // Pressure interpolated linearly along the SEGMENT (not the
            // cubic's arc length — adjacent samples are already dense).
            // t_param is the cubic's t in [0,1], which maps monotonically to
            // the segment s_a → s_b.
            let pressure = lerp_f32(s_a.pressure, s_b.pressure, t_param as f32);
            let curved = stroke.brush.curve.apply(pressure);
            let width = lerp_f32(stroke.brush.min_width, stroke.brush.max_width, curved);
            let half_f32 = width / 2.0;
            let half = f64::from(half_f32);

            // Offset-rail cusp criterion: |κ| · half_width ≥ threshold. Only
            // meaningful once we have a sub-segment (≥ 2 rail points) to close.
            let kappa = signed_curvature(&seg, t_param);
            if kappa.abs() * half >= CUSP_THRESHOLD && left.len() >= 2 {
                emit_ribbon_subsegment(&mut path, &left, &right);
                // Close the incoming sub-segment with a forward-bulging cap
                // at its last rail position; open the new one with a
                // backward-bulging cap at this step's position. Two caps
                // rather than one because at a sharp cusp the incoming and
                // outgoing tangents don't share a common "outward" direction.
                append_round_cap(&mut path, prev_position, prev_tangent, prev_half, screen_scale);
                append_round_cap(&mut path, p, -tangent, half_f32, screen_scale);
                left.clear();
                right.clear();
            }

            prev_tangent = tangent;
            if !wrote_any {
                first_tangent = tangent;
            }
            last_tangent = tangent;

            let perp = Vec2::new(-tangent.y, tangent.x);
            left.push(p + perp * half);
            right.push(p - perp * half);
            prev_position = p;
            prev_half = half_f32;
            wrote_any = true;
        }
    }

    if !wrote_any {
        // All segments were zero-length. Fall back to a disc at the first
        // sample — this is extraordinarily rare (would require every sample
        // to coincide with its neighbour).
        return single_sample_disc(&samples[0], stroke.brush, screen_scale);
    }

    // Emit the final (or only) sub-segment's ribbon.
    emit_ribbon_subsegment(&mut path, &left, &right);

    // Endpoint caps. Leading cap faces away from the first segment; trailing
    // cap faces away from the last segment. These are independent of any
    // mid-stroke cusp caps emitted during the walk.
    let first_half = half_width_at_sample(&samples[0], stroke.brush);
    let last_half = half_width_at_sample(&samples[samples.len() - 1], stroke.brush);
    append_round_cap(
        &mut path,
        samples[0].position.into(),
        -first_tangent,
        first_half,
        screen_scale,
    );
    append_round_cap(
        &mut path,
        samples[samples.len() - 1].position.into(),
        last_tangent,
        last_half,
        screen_scale,
    );

    path
}

/// Emit one ribbon sub-segment as a closed subpath: left rail forward, right
/// rail reversed, close. Degenerate (< 2 rail points) sub-segments are
/// silently skipped — the caller's round caps already cover the footprint.
fn emit_ribbon_subsegment(path: &mut BezPath, left: &[Point], right: &[Point]) {
    debug_assert_eq!(left.len(), right.len());
    if left.len() < 2 {
        return;
    }
    path.move_to(left[0]);
    for p in &left[1..] {
        path.line_to(*p);
    }
    for p in right.iter().rev() {
        path.line_to(*p);
    }
    path.close_path();
}

/// Signed curvature of a cubic Bézier at parameter `t`.
///
/// κ(t) = (x'(t)·y''(t) − y'(t)·x''(t)) / (x'(t)² + y'(t)²)^(3/2)
///
/// Used to detect offset-rail cusps: the parallel curve at half-width `h` has
/// a cusp where `|κ| · h = 1`. Returns 0 if the curve is momentarily
/// stationary (speed near zero) to avoid a 0/0 at those points — we can't
/// decide a cusp condition there anyway, and adjacent steps will pick it up.
fn signed_curvature(cubic: &CubicBez, t: f64) -> f64 {
    let d1 = cubic.deriv().eval(t).to_vec2();
    let d2 = cubic.deriv().deriv().eval(t).to_vec2();
    let cross = d1.x * d2.y - d1.y * d2.x;
    let speed_sq = d1.hypot2();
    if speed_sq < TANGENT_EPSILON * TANGENT_EPSILON {
        0.0
    } else {
        cross / (speed_sq * speed_sq.sqrt())
    }
}

fn single_sample_disc(sample: &Sample, brush: BrushParams, screen_scale: f64) -> BezPath {
    let radius = f64::from(half_width_at_sample(sample, brush));
    disc_path(sample.position.into(), radius, screen_scale)
}

fn half_width_at_sample(sample: &Sample, brush: BrushParams) -> f32 {
    let curved = brush.curve.apply(sample.pressure);
    let width = lerp_f32(brush.min_width, brush.max_width, curved);
    width / 2.0
}

/// A filled circle as a `BezPath`, tessellated as a fan of `cap_vertex_count`
/// straight segments.
fn disc_path(center: Point, radius: f64, screen_scale: f64) -> BezPath {
    let mut path = BezPath::new();
    let n = cap_vertex_count(radius, screen_scale);
    // Start at angle 0 and sweep 2π.
    let first = center + Vec2::new(radius, 0.0);
    path.move_to(first);
    for k in 1..n {
        let angle = 2.0 * PI * (k as f64) / (n as f64);
        path.line_to(center + Vec2::new(radius * angle.cos(), radius * angle.sin()));
    }
    path.close_path();
    path
}

/// Append a round cap as its own sub-path (filled separately; non-zero
/// winding handles the union with the ribbon). `outward` is the direction
/// the cap bulges (unit-ish vector, magnitude doesn't matter — we renormalize).
fn append_round_cap(
    path: &mut BezPath,
    center: Point,
    outward: Vec2,
    radius: f32,
    screen_scale: f64,
) {
    let radius = f64::from(radius);
    if radius <= 0.0 {
        return;
    }
    let len = outward.hypot();
    if len < TANGENT_EPSILON {
        return;
    }
    let t = outward / len;
    let perp = Vec2::new(-t.y, t.x);
    let n = cap_vertex_count(radius, screen_scale).max(3);
    // Sweep π radians from +perp through +t to -perp.
    path.move_to(center + perp * radius);
    for k in 1..n {
        let angle = PI * (k as f64) / (n as f64);
        // Parameterize: at angle 0, on +perp. At angle π, on -perp. At π/2,
        // on +t (outward).
        let dir = perp * angle.cos() + t * angle.sin();
        path.line_to(center + dir * radius);
    }
    path.line_to(center - perp * radius);
    path.close_path();
}

fn cap_vertex_count(radius_world: f64, screen_scale: f64) -> usize {
    let radius_px = radius_world * screen_scale;
    let ideal = (radius_px * PI / 2.0).ceil() as usize;
    ideal.clamp(CAP_VERTEX_MIN, CAP_VERTEX_MAX)
}

/// Centripetal Catmull-Rom → cubic Bezier for the segment between `p1` and
/// `p2`. `p0` and `p3` are the control points outside the segment (with
/// duplicate-endpoint phantoms at the stroke boundaries).
fn centripetal_catmull_rom(p0: Point, p1: Point, p2: Point, p3: Point) -> CubicBez {
    let d01 = (p1 - p0).hypot().sqrt().max(TANGENT_EPSILON);
    let d12 = (p2 - p1).hypot().sqrt().max(TANGENT_EPSILON);
    let d23 = (p3 - p2).hypot().sqrt().max(TANGENT_EPSILON);

    let t0 = 0.0;
    let t1 = t0 + d01;
    let t2 = t1 + d12;
    let t3 = t2 + d23;

    // Tangent at p1 approximated as (p2 - p0) / (t2 - t0). Scale by (t2 - t1)
    // and divide by 3 for cubic Bezier control-point derivation.
    let tan_p1 = (p2 - p0) * ((t2 - t1) / ((t2 - t0).max(TANGENT_EPSILON) * 3.0));
    let tan_p2 = (p3 - p1) * ((t2 - t1) / ((t3 - t1).max(TANGENT_EPSILON) * 3.0));

    CubicBez::new(p1, p1 + tan_p1, p2 - tan_p2, p2)
}

fn lerp_f32(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use aj_core::{BrushParams, PointerId, PressureCurve, Sample, Stroke, StrokeId, ToolCaps};
    use vello::kurbo::{Affine, Shape};

    fn stroke_from(points: &[(f64, f64, f32)], brush: BrushParams) -> Stroke {
        let samples = points
            .iter()
            .map(|&(x, y, p)| {
                let mut s =
                    Sample::mouse(Point::new(x, y).into(), Duration::ZERO, PointerId::MOUSE);
                s.pressure = p;
                s
            })
            .collect();
        Stroke { id: StrokeId(1), samples, caps: ToolCaps::empty(), brush }
    }

    fn identity_transform() -> Affine {
        Affine::IDENTITY
    }

    #[test]
    fn empty_stroke_returns_empty_path() {
        let stroke = Stroke {
            id: StrokeId(1),
            samples: vec![],
            caps: ToolCaps::empty(),
            brush: BrushParams::default(),
        };
        let path = tessellate_stroke(&stroke, identity_transform());
        assert_eq!(path.elements().len(), 0);
    }

    #[test]
    fn single_sample_is_disc_of_expected_radius() {
        let brush = BrushParams {
            min_width: 0.1,
            max_width: 10.0,
            curve: PressureCurve::Linear,
            color: aj_core::LinearRgba::BLACK,
            ..BrushParams::default()
        };
        let stroke = stroke_from(&[(50.0, 50.0, 1.0)], brush);
        let path = tessellate_stroke(&stroke, identity_transform());
        let bb = path.bounding_box();
        // Radius ≈ 5 (= max_width / 2) at pressure 1.0.
        assert!((bb.x1 - bb.x0 - 10.0).abs() < 0.5, "width ≈ 10");
        assert!((bb.y1 - bb.y0 - 10.0).abs() < 0.5, "height ≈ 10");
    }

    #[test]
    fn two_sample_constant_pressure_is_ribbon_with_caps() {
        let brush = BrushParams {
            min_width: 1.0,
            max_width: 1.0,
            curve: PressureCurve::Linear,
            color: aj_core::LinearRgba::BLACK,
            ..BrushParams::default()
        };
        let stroke = stroke_from(&[(0.0, 0.0, 1.0), (100.0, 0.0, 1.0)], brush);
        let path = tessellate_stroke(&stroke, identity_transform());
        let bb = path.bounding_box();
        assert!(bb.x1 - bb.x0 >= 100.0, "covers the horizontal span");
        assert!(
            bb.y1 - bb.y0 >= 1.0 && bb.y1 - bb.y0 <= 2.0,
            "height ≈ width (1.0) plus cap bulges (negligible at pressure-1 min=max=1.0)"
        );
    }

    #[test]
    fn zoom_increases_vertex_count() {
        let brush = BrushParams::default();
        let stroke = stroke_from(&[(0.0, 0.0, 1.0), (100.0, 0.0, 1.0), (100.0, 100.0, 1.0)], brush);
        let small = tessellate_stroke(&stroke, Affine::IDENTITY);
        let large = tessellate_stroke(&stroke, Affine::scale(10.0));
        assert!(
            large.elements().len() > small.elements().len(),
            "{} > {}",
            large.elements().len(),
            small.elements().len()
        );
    }

    #[test]
    fn zero_length_midsegment_does_not_panic() {
        // Middle sample duplicates its neighbor's position; must not produce
        // NaN or divide-by-zero.
        let brush = BrushParams::default();
        let stroke = stroke_from(
            &[(0.0, 0.0, 0.5), (50.0, 50.0, 0.5), (50.0, 50.0, 0.5), (100.0, 0.0, 0.5)],
            brush,
        );
        let path = tessellate_stroke(&stroke, identity_transform());
        // Any finite path survives — just check no NaN in any coord.
        for el in path.elements() {
            let pts: Vec<Point> = match el {
                vello::kurbo::PathEl::MoveTo(p) | vello::kurbo::PathEl::LineTo(p) => vec![*p],
                _ => vec![],
            };
            for p in pts {
                assert!(p.x.is_finite(), "x must be finite: {p:?}");
                assert!(p.y.is_finite(), "y must be finite: {p:?}");
            }
        }
    }

    /// Count `MoveTo` elements — each starts a new closed subpath. Used to
    /// distinguish "one ribbon + caps" from "cusp-split multi-subpath".
    fn subpath_count(path: &BezPath) -> usize {
        path.elements().iter().filter(|el| matches!(el, vello::kurbo::PathEl::MoveTo(_))).count()
    }

    #[test]
    fn straight_stroke_has_no_cusp_split() {
        // Zero curvature everywhere — path is exactly 1 ribbon + 2 endpoint caps.
        let brush = BrushParams {
            min_width: 1.0,
            max_width: 1.0,
            curve: PressureCurve::Linear,
            color: aj_core::LinearRgba::BLACK,
            ..BrushParams::default()
        };
        let stroke = stroke_from(
            &[
                (0.0, 0.0, 1.0),
                (25.0, 0.0, 1.0),
                (50.0, 0.0, 1.0),
                (75.0, 0.0, 1.0),
                (100.0, 0.0, 1.0),
            ],
            brush,
        );
        let path = tessellate_stroke(&stroke, identity_transform());
        assert_eq!(
            subpath_count(&path),
            3,
            "straight stroke: 1 ribbon + 2 endpoint caps, got {} subpaths",
            subpath_count(&path)
        );
    }

    #[test]
    fn pen_lift_swerve_cusp_splits_stroke() {
        // Long vertical stroke with a sharp sideways hook at the end — the
        // classic pen-lift artifact that caused the notches in the original
        // bug report. The final segment has high enough curvature × half-width
        // to trip the cusp threshold.
        let brush = BrushParams {
            min_width: 4.0,
            max_width: 8.0,
            curve: PressureCurve::Linear,
            color: aj_core::LinearRgba::BLACK,
            ..BrushParams::default()
        };
        let stroke = stroke_from(
            &[
                (0.0, 0.0, 0.8),
                (0.0, 20.0, 0.8),
                (0.0, 40.0, 0.8),
                (0.0, 60.0, 0.8),
                (3.0, 55.0, 0.4),
            ],
            brush,
        );
        let path = tessellate_stroke(&stroke, identity_transform());
        // With a cusp split we get ≥ 2 ribbon subpaths + 2 cusp caps +
        // 2 endpoint caps = at least 5 subpaths. Without the split it's 3.
        assert!(
            subpath_count(&path) >= 5,
            "expected cusp split (≥5 subpaths), got {}",
            subpath_count(&path)
        );
        // Every emitted point must still be finite.
        for el in path.elements() {
            if let vello::kurbo::PathEl::MoveTo(p) | vello::kurbo::PathEl::LineTo(p) = el {
                assert!(p.x.is_finite() && p.y.is_finite(), "non-finite coord {p:?}");
            }
        }
    }

    #[test]
    fn signed_curvature_circle_matches_inverse_radius() {
        // Classical cubic Bézier approximation of a 90° unit-circle arc has
        // curvature ≈ 1/r = 1 at its midpoint. k is the control-point length
        // that minimizes max radial error.
        let k = 4.0 * (2.0_f64.sqrt() - 1.0) / 3.0;
        let cubic = CubicBez::new(
            Point::new(1.0, 0.0),
            Point::new(1.0, k),
            Point::new(k, 1.0),
            Point::new(0.0, 1.0),
        );
        let kappa = signed_curvature(&cubic, 0.5);
        assert!(
            (kappa.abs() - 1.0).abs() < 0.05,
            "|κ| at midpoint = {}, want ≈ 1 (radius = 1)",
            kappa.abs()
        );
    }

    #[test]
    fn pressure_ramp_widens_bounding_box() {
        // Straight stroke but widening pressure → bounding-box height grows
        // toward the end.
        let brush = BrushParams {
            min_width: 0.1,
            max_width: 20.0,
            curve: PressureCurve::Linear,
            color: aj_core::LinearRgba::BLACK,
            ..BrushParams::default()
        };
        let stroke = stroke_from(
            &[
                (0.0, 0.0, 0.0),
                (25.0, 0.0, 0.25),
                (50.0, 0.0, 0.5),
                (75.0, 0.0, 0.75),
                (100.0, 0.0, 1.0),
            ],
            brush,
        );
        let path = tessellate_stroke(&stroke, identity_transform());
        let bb = path.bounding_box();
        // Max width ≈ 20 at the end; min ≈ 0 at the start. Bounding box
        // height should approach max_width since the thickest point is max.
        assert!(bb.y1 - bb.y0 >= 15.0, "bbox height {} should be ≥ ~max_width", bb.y1 - bb.y0);
    }
}
