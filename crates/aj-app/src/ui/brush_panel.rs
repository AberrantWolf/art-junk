//! Right-side brush settings panel. The single visible home for brush
//! controls — a menu-bar entry would duplicate this surface, so there isn't
//! one. Future tool-settings sections (color, layers, effects) append here
//! as additional groups beneath `Brush`.

use aj_core::{BrushParams, MAX_WIDTH_MAX, MAX_WIDTH_MIN};

use super::{BrushAction, color_picker};

/// Draw the brush panel if `visible`. Emits `BrushAction`s into `pending` on
/// slider changes; the main loop dispatches them alongside other pending
/// actions so we don't hold engine borrows across egui closures.
pub fn draw(
    ctx: &egui::Context,
    brush: BrushParams,
    visible: bool,
    pending: &mut Vec<BrushAction>,
) {
    if !visible {
        return;
    }
    egui::SidePanel::right("aj_brush_panel").default_width(220.0).resizable(true).show(ctx, |ui| {
        egui::CollapsingHeader::new("Brush").default_open(true).show(ui, |ui| {
            // Max-width slider — logarithmic so the useful 1–10 pt range
            // isn't a sliver of the track.
            let mut max = brush.max_width;
            let max_slider = ui.add(
                egui::Slider::new(&mut max, MAX_WIDTH_MIN..=MAX_WIDTH_MAX)
                    .logarithmic(true)
                    .suffix(" pt")
                    .text("Max width"),
            );
            if max_slider.changed() {
                pending.push(BrushAction::SetMaxWidth(max));
            }

            // Min-ratio slider — linear, 0 – 100 %. The displayed value
            // is min/max as a ratio; emitting SetMinRatio keeps ratio as
            // the primary cognitive state. Engine converts to absolute
            // min_width via the current max.
            let current_ratio = if brush.max_width > 0.0 {
                (brush.min_width / brush.max_width).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let mut ratio = current_ratio;
            let ratio_slider = ui.add(
                egui::Slider::new(&mut ratio, 0.0..=1.0)
                    .custom_formatter(|v, _| format!("{:.0}", v * 100.0))
                    .suffix(" %")
                    .text("Min ratio"),
            );
            if ratio_slider.changed() {
                pending.push(BrushAction::SetMinRatio(ratio));
            }

            ui.separator();
            ui.label("Color");
            color_picker::draw(ui, brush.color, pending);
        });
    });
}
