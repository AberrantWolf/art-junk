//! Viewport — user's pan/zoom state for the drawing surface.
//!
//! The viewport lives entirely in `aj-app`: view state is not part of the document
//! and is deliberately excluded from undo/redo. `Viewport::to_affine` is the single
//! composition point for user zoom, points→CSS-pixel conversion, and the OS's DPI
//! scale factor; the renderer and input pipeline both go through it (input uses the
//! inverse).
//!
//! Unit convention:
//! - `translate_pt` is in document points — the world-space offset that maps to the
//!   top-left of the window. Stored in pt (rather than physical pixels) so that
//!   DPI / monitor changes don't require rescaling the stored translation.
//! - `scale` is unitless user zoom, where `1.0` = "one document pt renders at its
//!   natural physical size on the display" (best-effort against the OS's DPI scale).
//!
//! See `MEMORY.md` ("Zoom 100% = real-world physical size") for the rationale.
//
// TODO(dpi-calibration): the "natural physical size" interpretation of scale=1.0
// relies on the OS's reported `scale_factor`, which is often wrong on desktop
// monitors. A future per-monitor correction factor (user calibrates against a
// known real-world size like A4) would go here as an extra multiplier inside
// `to_affine` and the *_to_world/_to_screen helpers.

use aj_core::{Affine, Point, Size, Vec2};

/// Exact conversion: 1 pt = 4/3 CSS px (since 1 pt = 1/72″ and 1 CSS px = 1/96″).
pub const CSS_PER_PT: f64 = 4.0 / 3.0;

/// Zoom clamp. 0.01 = see a ~19-px-wide 1920pt page; 256 = each pt spans 256 screen
/// px. Well inside f32 screen-space precision for reasonable pan offsets.
pub const MIN_SCALE: f64 = 0.01;
pub const MAX_SCALE: f64 = 256.0;

/// Zoom step per scroll-line / keyboard press. 1.1× per step matches Figma/Inkscape.
pub const ZOOM_STEP: f64 = 1.1;

/// Fit-to-window leaves this fraction of each axis empty as margin.
const FIT_MARGIN: f64 = 0.05;

#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    /// Document-space offset: the world point that would map to screen-origin (0,0)
    /// if `scale × CSS_PER_PT × dpi == 1`. Stored in pt so DPI changes don't rescale.
    pub translate_pt: Vec2,
    /// User zoom; `1.0` is natural physical size.
    pub scale: f64,
}

impl Default for Viewport {
    fn default() -> Self {
        Self { translate_pt: Vec2::ZERO, scale: 1.0 }
    }
}

impl Viewport {
    /// Compose world-pt → physical-px affine, including OS DPI.
    #[must_use]
    pub fn to_affine(self, dpi_scale: f64) -> Affine {
        let k = self.scale * CSS_PER_PT * dpi_scale;
        // (world - translate) * k = world*k - translate*k
        Affine::translate(-self.translate_pt * k) * Affine::scale(k)
    }

    /// Convert a point from physical-pixel screen space to world-space pt.
    #[must_use]
    pub fn screen_to_world(&self, physical_px: Point, dpi_scale: f64) -> Point {
        let k = self.scale * CSS_PER_PT * dpi_scale;
        Point::new(physical_px.x / k + self.translate_pt.x, physical_px.y / k + self.translate_pt.y)
    }

    /// Convert a world-space pt to physical-pixel screen space. Symmetric with
    /// `screen_to_world`; currently only the tests call this in the binary build,
    /// but the forward direction belongs in the API so future callers (e.g.
    /// selection marquees, snap indicators) don't need to re-derive it.
    #[must_use]
    #[allow(dead_code)]
    pub fn world_to_screen(&self, world_pt: Point, dpi_scale: f64) -> Point {
        let k = self.scale * CSS_PER_PT * dpi_scale;
        Point::new((world_pt.x - self.translate_pt.x) * k, (world_pt.y - self.translate_pt.y) * k)
    }

    /// Zoom by `factor` while keeping the world point under `cursor_physical` fixed
    /// under that same physical-pixel position. Clamps scale; recomputes translate
    /// from the cursor-invariant rather than accumulating deltas (accumulation drifts
    /// on clamp and under f32 composition).
    pub fn zoom_at_cursor(&mut self, cursor_physical: Point, factor: f64, dpi_scale: f64) {
        let cursor_world = self.screen_to_world(cursor_physical, dpi_scale);
        let new_scale = (self.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
        let k_new = new_scale * CSS_PER_PT * dpi_scale;
        self.scale = new_scale;
        self.translate_pt = Vec2::new(
            cursor_world.x - cursor_physical.x / k_new,
            cursor_world.y - cursor_physical.y / k_new,
        );
    }

    /// Translate the view by `delta_physical` pixels.
    pub fn pan(&mut self, delta_physical: Vec2, dpi_scale: f64) {
        let k = self.scale * CSS_PER_PT * dpi_scale;
        self.translate_pt += delta_physical / k;
    }

    /// Reset to default (scale 1.0, origin at world 0,0).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Set zoom to an absolute factor, anchored at screen center.
    pub fn zoom_to(&mut self, scale: f64, window_px: Size, dpi_scale: f64) {
        let center = Point::new(window_px.width / 2.0, window_px.height / 2.0);
        let target = scale.clamp(MIN_SCALE, MAX_SCALE);
        let factor = target / self.scale;
        self.zoom_at_cursor(center, factor, dpi_scale);
    }

    /// Fit the given page inside the window, leaving `FIT_MARGIN` margin.
    pub fn zoom_to_fit(&mut self, window_px: Size, page_pt: Size, dpi_scale: f64) {
        if page_pt.width <= 0.0 || page_pt.height <= 0.0 {
            return;
        }
        let window_pt_w = window_px.width / (CSS_PER_PT * dpi_scale);
        let window_pt_h = window_px.height / (CSS_PER_PT * dpi_scale);
        let fit_w = window_pt_w / page_pt.width;
        let fit_h = window_pt_h / page_pt.height;
        let fit_scale = fit_w.min(fit_h) * (1.0 - 2.0 * FIT_MARGIN);
        self.scale = fit_scale.clamp(MIN_SCALE, MAX_SCALE);
        let k = self.scale * CSS_PER_PT * dpi_scale;
        let page_center = Point::new(page_pt.width / 2.0, page_pt.height / 2.0);
        let window_center = Point::new(window_px.width / 2.0, window_px.height / 2.0);
        self.translate_pt =
            Vec2::new(page_center.x - window_center.x / k, page_center.y - window_center.y / k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn default_is_identity_at_dpi_1() {
        let v = Viewport::default();
        let a = v.to_affine(1.0);
        // Default maps 0,0 → 0,0 and 72pt → 72 × 4/3 = 96 px at DPI 1.
        let origin = a * Point::new(0.0, 0.0);
        let p72 = a * Point::new(72.0, 72.0);
        assert!(close(origin.x, 0.0, 1e-9) && close(origin.y, 0.0, 1e-9));
        assert!(close(p72.x, 96.0, 1e-9) && close(p72.y, 96.0, 1e-9));
    }

    #[test]
    fn screen_to_world_is_inverse_of_world_to_screen() {
        let v = Viewport { translate_pt: Vec2::new(-50.0, 25.0), scale: 1.7 };
        let dpi = 2.0;
        for (wx, wy) in [(0.0, 0.0), (100.0, 200.0), (-17.5, 42.1)] {
            let world = Point::new(wx, wy);
            let screen = v.world_to_screen(world, dpi);
            let back = v.screen_to_world(screen, dpi);
            assert!(close(back.x, wx, 1e-9), "x round-trip: {} != {}", back.x, wx);
            assert!(close(back.y, wy, 1e-9), "y round-trip: {} != {}", back.y, wy);
        }
    }

    #[test]
    fn zoom_at_cursor_keeps_world_point_under_cursor() {
        let mut v = Viewport { translate_pt: Vec2::new(10.0, -5.0), scale: 1.0 };
        let dpi = 1.5;
        let cursor = Point::new(400.0, 300.0);
        let world_before = v.screen_to_world(cursor, dpi);
        v.zoom_at_cursor(cursor, 2.5, dpi);
        let world_after_under_cursor = v.screen_to_world(cursor, dpi);
        assert!(close(world_before.x, world_after_under_cursor.x, 1e-9));
        assert!(close(world_before.y, world_after_under_cursor.y, 1e-9));
    }

    #[test]
    fn zoom_round_trip_restores_state() {
        let mut v = Viewport { translate_pt: Vec2::new(-33.3, 77.7), scale: 1.0 };
        let dpi = 2.0;
        let cursor = Point::new(640.0, 480.0);
        v.zoom_at_cursor(cursor, 4.0, dpi);
        v.zoom_at_cursor(cursor, 1.0 / 4.0, dpi);
        assert!(close(v.scale, 1.0, 1e-9));
        assert!(close(v.translate_pt.x, -33.3, 1e-9));
        assert!(close(v.translate_pt.y, 77.7, 1e-9));
    }

    #[test]
    fn zoom_clamps_at_bounds() {
        let mut v = Viewport::default();
        v.zoom_at_cursor(Point::new(0.0, 0.0), 1e6, 1.0);
        assert!(close(v.scale, MAX_SCALE, 1e-9));
        v.zoom_at_cursor(Point::new(0.0, 0.0), 1e-6, 1.0);
        assert!(close(v.scale, MIN_SCALE, 1e-9));
    }

    #[test]
    fn pan_translates_view() {
        let mut v = Viewport::default();
        let dpi = 1.0;
        let before = v.screen_to_world(Point::new(100.0, 100.0), dpi);
        v.pan(Vec2::new(50.0, 0.0), dpi);
        let after = v.screen_to_world(Point::new(100.0, 100.0), dpi);
        // Panning by +50 px should move the world-point-under-cursor by
        // +50 / (scale × CSS_PER_PT × dpi) = +50 / (4/3) = +37.5 pt in x.
        assert!(close(after.x - before.x, 37.5, 1e-9));
        assert!(close(after.y - before.y, 0.0, 1e-9));
    }

    #[test]
    fn fit_centers_page_in_window() {
        let mut v = Viewport::default();
        let window = Size::new(1600.0, 900.0);
        let page = Size::new(1920.0, 1080.0);
        v.zoom_to_fit(window, page, 1.0);
        // Page center should project to window center.
        let page_center = Point::new(page.width / 2.0, page.height / 2.0);
        let projected = v.world_to_screen(page_center, 1.0);
        assert!(close(projected.x, window.width / 2.0, 1e-6));
        assert!(close(projected.y, window.height / 2.0, 1e-6));
    }
}
