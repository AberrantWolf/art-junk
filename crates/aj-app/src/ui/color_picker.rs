//! Inline color picker widget for the brush panel.
//!
//! Canonical picker state is **Okhsl** (hue, saturation, lightness in
//! Ottosson's perceptual-HSL space). Storing Okhsl — not Oklch — directly is
//! load-bearing:
//!
//! - Every pixel on the 2D picker plane is an `Okhsl(h, s, l)` we paint the
//!   plane from, so crosshair position and slider readouts come from the same
//!   coordinate we painted against — no conversion drift between what the
//!   user sees and what we report.
//! - `palette::Okhsl::from_color(Oklch)` has observable numerical instability
//!   near the sRGB gamut cusp that rounds `S` down to zero for colors that
//!   are still clearly saturated. Keeping Okhsl as the source of truth
//!   eliminates that round-trip on the hot display path.
//! - Okhsl's `hue` is a stored field that doesn't depend on `S`, so passing
//!   through `S = 0` (neutral gray) and back up doesn't reset the user's hue —
//!   no `last_nonzero_hue` bookkeeping required.
//!
//! We derive `LinearRgba` from `Okhsl` at commit time (via palette's built-in
//! Okhsl → `LinSrgb` conversion, which is gamut-safe because Okhsl is already
//! normalized against the gamut cusp). The engine never sees palette types.

use std::collections::VecDeque;

use aj_core::LinearRgba;
use egui::{Color32, ColorImage, Pos2, Rect, Sense, Stroke, TextureHandle, TextureOptions, Vec2};
use palette::{Okhsl, OklabHue, Oklch};

use crate::color;

use super::BrushAction;

/// Using `u16` (not `usize`) so the many `f32::from(...)` / `usize::from(...)`
/// call sites below stay lossless and `clippy::pedantic` stays clean.
const PLANE_SIZE: u16 = 128;
const HUE_STRIP_SIZE: u16 = 160;
const HUE_STRIP_HEIGHT: u16 = 14;
/// Re-paint the plane texture when the hue differs from the cached value by
/// more than this. A full plane re-paint is ~16 k conversions; this threshold
/// keeps us under one re-paint per hue-slider frame without visibly stuttering.
const HUE_REPAINT_EPS_DEG: f32 = 0.5;
/// The `L` and `C` used to paint the hue strip. Chosen so every hue gives a
/// vivid, in-gamut column — the strip is there to show "this is the hue," not
/// "this is your exact color."
const HUE_STRIP_L: f32 = 0.7;
const HUE_STRIP_C: f32 = 0.14;
const RECENT_CAP: usize = 12;
/// A color is "close enough" to avoid resetting state (e.g. from an undo that
/// restored the same color). Euclidean distance in linear RGB — rough but
/// the picker's own quantization is looser than 1/1000.
const COLOR_EQ_EPS: f32 = 1e-3;
/// Below this saturation, we treat the color as neutral and preserve the
/// user's last-set hue through state transitions rather than adopting the
/// (meaningless) hue that `palette` returns for an Okhsl-from-gray conversion.
const NEUTRAL_S_EPS: f32 = 1e-4;

#[derive(Clone)]
struct PickerState {
    /// Canonical picker coordinate. See module docs for why Okhsl, not Oklch.
    okhsl: Okhsl,
    expanded: bool,
    recent: VecDeque<LinearRgba>,
    plane_texture: Option<TextureHandle>,
    plane_hue_cache: Option<f32>,
    /// Hex input buffer; kept distinct from `okhsl` so the user can edit
    /// mid-type without the sliders snapping on every keystroke.
    hex_input: String,
}

impl Default for PickerState {
    fn default() -> Self {
        Self {
            okhsl: Okhsl::new(OklabHue::new(0.0), 0.0, 0.0),
            expanded: true,
            recent: VecDeque::with_capacity(RECENT_CAP),
            plane_texture: None,
            plane_hue_cache: None,
            hex_input: String::new(),
        }
    }
}

/// Draw the picker. Emits `BrushAction::SetColor(..)` into `out` on any
/// user-driven change.
pub fn draw(ui: &mut egui::Ui, current: LinearRgba, out: &mut Vec<BrushAction>) {
    let id = ui.make_persistent_id("aj_color_picker");
    let mut state: PickerState = ui.data_mut(|d| d.get_temp::<PickerState>(id)).unwrap_or_default();

    sync_from_engine(&mut state, current);

    draw_header(ui, &mut state, current);

    if state.expanded {
        ui.add_space(4.0);
        draw_plane(ui, &mut state, out);
        ui.add_space(4.0);
        draw_hue_strip(ui, &mut state, out);
        ui.add_space(4.0);
        draw_numeric_fields(ui, &mut state, out);
        ui.add_space(4.0);
        draw_swatches(ui, &mut state, out);
    }

    ui.data_mut(|d| d.insert_temp(id, state));
}

fn sync_from_engine(state: &mut PickerState, current: LinearRgba) {
    // If the engine's color is (within epsilon) what we'd produce from our
    // current Okhsl, keep state as-is — preserves UI hue across C≈0 moments.
    // Otherwise the color was changed from outside the picker (undo, keyboard
    // shortcut, future palette tool) — adopt it.
    let would_produce = color::okhsl_to_linear_rgba(state.okhsl, current.a);
    if linear_rgba_approx_eq(would_produce, current) {
        return;
    }
    let incoming = color::linear_rgba_to_okhsl(current);
    // For externally-set neutral colors, keep the hue the user had dialed in
    // so brightening / saturating back up stays on the same hue ray.
    let hue = if incoming.saturation < NEUTRAL_S_EPS { state.okhsl.hue } else { incoming.hue };
    state.okhsl = Okhsl::new(hue, incoming.saturation, incoming.lightness);
    state.plane_hue_cache = None;
    state.hex_input.clear();
}

fn draw_header(ui: &mut egui::Ui, state: &mut PickerState, current: LinearRgba) {
    ui.horizontal(|ui| {
        let swatch_size = Vec2::splat(22.0);
        let (rect, _) = ui.allocate_exact_size(swatch_size, Sense::hover());
        let [r, g, b, _] = current.to_srgb8();
        ui.painter().rect_filled(rect, 3.0, Color32::from_rgb(r, g, b));
        ui.painter().rect_stroke(
            rect,
            3.0,
            Stroke::new(1.0, ui.visuals().widgets.inactive.bg_stroke.color),
        );

        ui.monospace(format!("#{r:02X}{g:02X}{b:02X}"));

        let label = if state.expanded { "▼" } else { "▶" };
        if ui.small_button(label).clicked() {
            state.expanded = !state.expanded;
        }
    });
}

fn draw_plane(ui: &mut egui::Ui, state: &mut PickerState, out: &mut Vec<BrushAction>) {
    ensure_plane_texture(ui.ctx(), state);
    let tex = state.plane_texture.as_ref().expect("texture populated by ensure_plane_texture");

    let size = Vec2::new(f32::from(PLANE_SIZE) * 1.4, f32::from(PLANE_SIZE) * 1.1);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());

    ui.painter().image(
        tex.id(),
        rect,
        Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
        Color32::WHITE,
    );

    let crosshair_pos = plane_pos_from_hsl(rect, state.okhsl);
    paint_crosshair(ui.painter(), crosshair_pos);

    if let Some(pointer) = response.interact_pointer_pos()
        && (response.clicked() || response.dragged())
    {
        let s = ((pointer.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
        // y=0 at top (bright) → L=1. y=height at bottom (dark) → L=0.
        let l = (1.0 - (pointer.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
        state.okhsl = Okhsl::new(state.okhsl.hue, s, l);
        state.hex_input.clear();
        commit(state, out);
        if response.drag_stopped() || response.clicked() {
            push_recent(state);
        }
    }
}

fn ensure_plane_texture(ctx: &egui::Context, state: &mut PickerState) {
    let h = state.okhsl.hue.into_degrees();
    let needs_paint = match (state.plane_texture.as_ref(), state.plane_hue_cache) {
        (Some(_), Some(cached)) => (h - cached).abs() > HUE_REPAINT_EPS_DEG,
        _ => true,
    };
    if !needs_paint {
        return;
    }

    let plane_usize = usize::from(PLANE_SIZE);
    let mut pixels = Vec::with_capacity(plane_usize * plane_usize);
    let denom = f32::from(PLANE_SIZE) - 1.0;
    for py in 0..PLANE_SIZE {
        let lightness = 1.0 - f32::from(py) / denom;
        for px in 0..PLANE_SIZE {
            let saturation = f32::from(px) / denom;
            let hsl = Okhsl::new(OklabHue::new(h), saturation, lightness);
            let [r, g, b, _] = color::okhsl_to_srgb8(hsl);
            pixels.push(Color32::from_rgb(r, g, b));
        }
    }
    let image = ColorImage { size: [plane_usize, plane_usize], pixels };

    match state.plane_texture.as_mut() {
        Some(tex) => tex.set(image, TextureOptions::LINEAR),
        None => {
            state.plane_texture =
                Some(ctx.load_texture("aj_color_plane", image, TextureOptions::LINEAR));
        }
    }
    state.plane_hue_cache = Some(h);
}

fn plane_pos_from_hsl(rect: Rect, hsl: Okhsl) -> Pos2 {
    let s = hsl.saturation.clamp(0.0, 1.0);
    let l = hsl.lightness.clamp(0.0, 1.0);
    Pos2::new(rect.min.x + s * rect.width(), rect.min.y + (1.0 - l) * rect.height())
}

fn paint_crosshair(painter: &egui::Painter, pos: Pos2) {
    // White-on-black ring so the crosshair stays legible over any background.
    painter.circle_stroke(pos, 5.0, Stroke::new(1.5, Color32::BLACK));
    painter.circle_stroke(pos, 5.0, Stroke::new(0.75, Color32::WHITE));
}

fn draw_hue_strip(ui: &mut egui::Ui, state: &mut PickerState, out: &mut Vec<BrushAction>) {
    let size = Vec2::new(f32::from(PLANE_SIZE) * 1.4, f32::from(HUE_STRIP_HEIGHT) * 1.3);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());

    // The hue strip texture is cheap to compute each time (only HUE_STRIP_SIZE
    // × HUE_STRIP_HEIGHT ≈ 2240 pixels) and is independent of everything else
    // in picker state, so we paint it procedurally via Painter::rect_filled.
    let strip_f32 = f32::from(HUE_STRIP_SIZE);
    for x in 0..HUE_STRIP_SIZE {
        let xf = f32::from(x);
        let h = 360.0 * xf / (strip_f32 - 1.0);
        let oklch = Oklch::new(HUE_STRIP_L, HUE_STRIP_C, OklabHue::new(h));
        let [r, g, b, _] = color::oklch_to_srgb8(oklch);
        let x0 = rect.min.x + xf / strip_f32 * rect.width();
        let x1 = rect.min.x + (xf + 1.0) / strip_f32 * rect.width();
        let col = Rect::from_min_max(Pos2::new(x0, rect.min.y), Pos2::new(x1, rect.max.y));
        ui.painter().rect_filled(col, 0.0, Color32::from_rgb(r, g, b));
    }

    // Marker for current hue.
    let current_h = state.okhsl.hue.into_degrees().rem_euclid(360.0);
    let marker_x = rect.min.x + current_h / 360.0 * rect.width();
    ui.painter().line_segment(
        [Pos2::new(marker_x, rect.min.y), Pos2::new(marker_x, rect.max.y)],
        Stroke::new(2.0, Color32::BLACK),
    );
    ui.painter().line_segment(
        [Pos2::new(marker_x, rect.min.y), Pos2::new(marker_x, rect.max.y)],
        Stroke::new(1.0, Color32::WHITE),
    );

    if let Some(pointer) = response.interact_pointer_pos()
        && (response.clicked() || response.dragged())
    {
        let t = ((pointer.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
        let h = t * 360.0;
        state.okhsl.hue = OklabHue::new(h);
        state.plane_hue_cache = None; // plane needs a re-paint
        state.hex_input.clear();
        commit(state, out);
        if response.drag_stopped() || response.clicked() {
            push_recent(state);
        }
    }
}

fn draw_numeric_fields(ui: &mut egui::Ui, state: &mut PickerState, out: &mut Vec<BrushAction>) {
    ui.horizontal(|ui| {
        let mut l_pct = state.okhsl.lightness * 100.0;
        let resp = ui.add(
            egui::DragValue::new(&mut l_pct)
                .speed(0.5)
                .range(0.0..=100.0)
                .max_decimals(1)
                .suffix("%")
                .prefix("L "),
        );
        if resp.changed() {
            state.okhsl.lightness = (l_pct / 100.0).clamp(0.0, 1.0);
            state.hex_input.clear();
            commit(state, out);
        }

        let mut s_pct = state.okhsl.saturation * 100.0;
        let resp = ui.add(
            egui::DragValue::new(&mut s_pct)
                .speed(0.5)
                .range(0.0..=100.0)
                .max_decimals(1)
                .suffix("%")
                .prefix("S "),
        );
        if resp.changed() {
            state.okhsl.saturation = (s_pct / 100.0).clamp(0.0, 1.0);
            state.hex_input.clear();
            commit(state, out);
        }

        let mut h_deg = state.okhsl.hue.into_degrees().rem_euclid(360.0);
        let resp = ui.add(
            egui::DragValue::new(&mut h_deg)
                .speed(1.0)
                .range(0.0..=360.0)
                .max_decimals(1)
                .suffix("°")
                .prefix("H "),
        );
        if resp.changed() {
            state.okhsl.hue = OklabHue::new(h_deg);
            state.plane_hue_cache = None;
            state.hex_input.clear();
            commit(state, out);
        }
    });

    // Hex input. Populate the buffer from the current color on first draw
    // after a sync; let the user type freely until they commit with Enter or
    // lose focus.
    let current_hex = {
        let rgba = color::okhsl_to_linear_rgba(state.okhsl, 1.0);
        let [r, g, b, _] = rgba.to_srgb8();
        format!("#{r:02X}{g:02X}{b:02X}")
    };
    if state.hex_input.is_empty() {
        state.hex_input.clone_from(&current_hex);
    }
    ui.horizontal(|ui| {
        ui.label("Hex");
        let resp = ui.add(egui::TextEdit::singleline(&mut state.hex_input).desired_width(80.0));
        if resp.lost_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter) || i.pointer.any_click())
        {
            if let Some(rgba) = parse_hex(&state.hex_input) {
                let incoming = color::linear_rgba_to_okhsl(rgba);
                let hue = if incoming.saturation < NEUTRAL_S_EPS {
                    state.okhsl.hue
                } else {
                    incoming.hue
                };
                state.okhsl = Okhsl::new(hue, incoming.saturation, incoming.lightness);
                state.plane_hue_cache = None;
                let [r, g, b, _] = rgba.to_srgb8();
                state.hex_input = format!("#{r:02X}{g:02X}{b:02X}");
                commit(state, out);
            } else {
                // Invalid input — restore the buffer to the current color.
                state.hex_input = current_hex;
            }
        } else if !resp.has_focus() {
            // When not being edited, keep the buffer in sync with state.
            state.hex_input = current_hex;
        }
    });
}

fn draw_swatches(ui: &mut egui::Ui, state: &mut PickerState, out: &mut Vec<BrushAction>) {
    if state.recent.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label("Recent");
        let swatch_size = Vec2::splat(16.0);
        let recent: Vec<LinearRgba> = state.recent.iter().copied().collect();
        for rgba in recent {
            let (rect, resp) = ui.allocate_exact_size(swatch_size, Sense::click());
            let [r, g, b, _] = rgba.to_srgb8();
            ui.painter().rect_filled(rect, 2.0, Color32::from_rgb(r, g, b));
            ui.painter().rect_stroke(rect, 2.0, Stroke::new(1.0, Color32::BLACK));
            if resp.clicked() {
                let incoming = color::linear_rgba_to_okhsl(rgba);
                let hue = if incoming.saturation < NEUTRAL_S_EPS {
                    state.okhsl.hue
                } else {
                    incoming.hue
                };
                state.okhsl = Okhsl::new(hue, incoming.saturation, incoming.lightness);
                state.plane_hue_cache = None;
                state.hex_input.clear();
                commit(state, out);
            }
        }
    });
}

fn commit(state: &PickerState, out: &mut Vec<BrushAction>) {
    let rgba = color::okhsl_to_linear_rgba(state.okhsl, 1.0);
    out.push(BrushAction::SetColor(rgba));
}

fn push_recent(state: &mut PickerState) {
    let rgba = color::okhsl_to_linear_rgba(state.okhsl, 1.0);
    state.recent.retain(|r| !linear_rgba_approx_eq(*r, rgba));
    state.recent.push_front(rgba);
    while state.recent.len() > RECENT_CAP {
        state.recent.pop_back();
    }
}

fn linear_rgba_approx_eq(a: LinearRgba, b: LinearRgba) -> bool {
    (a.r - b.r).abs() < COLOR_EQ_EPS
        && (a.g - b.g).abs() < COLOR_EQ_EPS
        && (a.b - b.b).abs() < COLOR_EQ_EPS
        && (a.a - b.a).abs() < COLOR_EQ_EPS
}

fn parse_hex(s: &str) -> Option<LinearRgba> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(LinearRgba::from_srgb8([r, g, b, 255]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_accepts_common_forms() {
        assert_eq!(parse_hex("#ff8040"), Some(LinearRgba::from_srgb8([0xff, 0x80, 0x40, 255])));
        assert_eq!(parse_hex("FF8040"), Some(LinearRgba::from_srgb8([0xff, 0x80, 0x40, 255])));
        assert_eq!(parse_hex("  #000000  "), Some(LinearRgba::from_srgb8([0, 0, 0, 255])));
    }

    #[test]
    fn parse_hex_rejects_malformed() {
        assert_eq!(parse_hex(""), None);
        assert_eq!(parse_hex("#12345"), None); // too short
        assert_eq!(parse_hex("#1234567"), None); // too long
        assert_eq!(parse_hex("#gg0000"), None); // non-hex
    }
}
