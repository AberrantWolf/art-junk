//! Right side panel hosting both the brush section and the layers section.
//! Sub-modules (`brush_panel`, `layers_panel`) provide the section bodies;
//! this module owns the `SidePanel::right` and orders them.

use aj_core::{BrushParams, Layer, LayerId};

use super::{BrushAction, LayerAction, brush_panel, layers_panel};

/// Render the right side panel if `visible`. Brush section first, layers
/// section second, separated by a horizontal rule. Each section pushes its
/// own action enum into the matching `pending` Vec; the main loop
/// dispatches them after egui exits so engine borrows aren't held across
/// egui closures.
pub fn draw(
    ctx: &egui::Context,
    brush: BrushParams,
    layers: &[Layer],
    active_layer: LayerId,
    visible: bool,
    pending_brush: &mut Vec<BrushAction>,
    pending_layer: &mut Vec<LayerAction>,
) {
    if !visible {
        return;
    }
    egui::SidePanel::right("aj_right_panel").default_width(240.0).resizable(true).show(ctx, |ui| {
        brush_panel::draw_section(ui, brush, pending_brush);
        ui.separator();
        layers_panel::draw_section(ui, layers, active_layer, pending_layer);
    });
}
