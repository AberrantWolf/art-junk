//! Right-side brush settings panel. The single visible home for brush
//! controls — a menu-bar entry would duplicate this surface, so there isn't
//! one. Future tool-settings sections (color, layers, effects) append here
//! as additional groups beneath `Brush`.

use aj_core::{BrushParams, BrushType, LinearRgba, MAX_WIDTH_MAX, MAX_WIDTH_MIN};

use super::{BrushAction, color_picker};

/// Human-readable label for a brush type. The forward-compat `Unknown`
/// variant only appears in documents written by future builds; we surface
/// it in the dropdown rather than hide it so users see when something is
/// off.
fn brush_type_label(brush_type: BrushType) -> &'static str {
    match brush_type {
        BrushType::Normal => "Normal",
        BrushType::Highlighter => "Highlighter",
        BrushType::Pigment => "Pigment (paint)",
        BrushType::Unknown => "Unknown (fallback)",
        _ => "Unknown",
    }
}

/// Tooltip string per brush type — explains the brush program in one sentence.
fn brush_type_tooltip(brush_type: BrushType) -> &'static str {
    match brush_type {
        BrushType::Normal => {
            "Standard alpha-over: opaque or semi-opaque ink that covers what's beneath. Most pens, markers, and inks behave this way."
        }
        BrushType::Highlighter => {
            "Multiplicative tint: yellow over black stays black, yellow over white tints toward yellow. Overlapping passes saturate further."
        }
        BrushType::Pigment => {
            "Paint-style mixing: Kubelka–Munk in spectral space against the current substrate. Yellow over blue mixes to green."
        }
        BrushType::Unknown => {
            "Forward-compat fallback. This document was written by a newer build that introduced a brush type this binary doesn't recognize."
        }
        _ => "",
    }
}

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
            ui.label("Brush type");
            // ComboBox::selected_text shows the *current* selection while
            // closed; the items inside are the choices. Clicking a different
            // item enqueues a SetBrushType action; the engine flips the live
            // brush, the next stroke picks up the new type (existing strokes
            // keep their frozen type — same live-vs-frozen rule as width/color).
            let current = brush.brush_type;
            egui::ComboBox::from_id_salt("aj_brush_type")
                .selected_text(brush_type_label(current))
                .show_ui(ui, |ui| {
                    for brush_type in
                        [BrushType::Normal, BrushType::Highlighter, BrushType::Pigment]
                    {
                        let resp = ui
                            .selectable_label(current == brush_type, brush_type_label(brush_type));
                        let resp = resp.on_hover_text(brush_type_tooltip(brush_type));
                        if resp.clicked() && current != brush_type {
                            pending.push(BrushAction::SetBrushType(brush_type));
                        }
                    }
                });

            // Opacity slider — controls `brush.color.a`, which every brush
            // program multiplies into the per-stroke mix weight (so a
            // 50%-opacity stroke deposits at half its full strength regardless
            // of whether it's alpha-over or pigment). Color picker doesn't
            // touch alpha, and this slider doesn't touch RGB; they're
            // orthogonal.
            let mut opacity = brush.color.a;
            let opacity_slider = ui.add(
                egui::Slider::new(&mut opacity, 0.0..=1.0)
                    .custom_formatter(|v, _| format!("{:.0}", v * 100.0))
                    .suffix(" %")
                    .text("Opacity"),
            );
            if opacity_slider.changed() {
                pending.push(BrushAction::SetColor(LinearRgba {
                    a: opacity.clamp(0.0, 1.0),
                    ..brush.color
                }));
            }

            ui.separator();
            ui.label("Color");
            color_picker::draw(ui, brush.color, pending);
        });
    });
}
