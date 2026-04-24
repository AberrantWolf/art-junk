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
//!   sample positions, perpendicular to the local tangent. Fan vertex count
//!   scales with cap radius in physical pixels, bounded `[CAP_VERTEX_MIN,
//!   CAP_VERTEX_MAX]`.
//!
//! The returned `BezPath` is in world space and expected to be filled with
//! `Fill::NonZero`. Caps are emitted as separate sub-paths; the non-zero
//! winding rule handles the union with the ribbon polygon.

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
    // We capture first / last tangents during the walk so the caps know how
    // to orient themselves.
    let mut first_tangent = Vec2::new(1.0, 0.0);
    let mut last_tangent = Vec2::new(1.0, 0.0);
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
            prev_tangent = tangent;
            if !wrote_any {
                first_tangent = tangent;
            }
            last_tangent = tangent;

            // Pressure interpolated linearly along the SEGMENT (not the
            // cubic's arc length — adjacent samples are already dense).
            // t_param is the cubic's t in [0,1], which maps monotonically to
            // the segment s_a → s_b.
            let pressure = lerp_f32(s_a.pressure, s_b.pressure, t_param as f32);
            let curved = stroke.brush.curve.apply(pressure);
            let width = lerp_f32(stroke.brush.min_width, stroke.brush.max_width, curved);
            let half = f64::from(width) / 2.0;
            let perp = Vec2::new(-tangent.y, tangent.x);
            left.push(p + perp * half);
            right.push(p - perp * half);
            wrote_any = true;
        }
    }

    if !wrote_any {
        // All segments were zero-length. Fall back to a disc at the first
        // sample — this is extraordinarily rare (would require every sample
        // to coincide with its neighbour).
        return single_sample_disc(&samples[0], stroke.brush, screen_scale);
    }

    // Emit the ribbon polygon: left rail forward, right rail reversed, close.
    path.move_to(left[0]);
    for p in &left[1..] {
        path.line_to(*p);
    }
    for p in right.iter().rev() {
        path.line_to(*p);
    }
    path.close_path();

    // Caps. Leading cap faces away from the first segment; trailing cap
    // faces away from the last segment.
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
