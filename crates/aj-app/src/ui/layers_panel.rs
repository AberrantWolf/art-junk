//! Layers section of the right side panel: per-layer visibility, opacity,
//! blend mode, active selection, plus add / remove / move buttons. Hosted
//! inside the right `SidePanel` set up by `right_panel::draw`.
//!
//! ## Layout
//!
//! Photoshop-style: the top of the canvas stack appears at the top of the
//! list. Internally `Vec<Layer>` is bottom-to-top (index 0 = bottom), so
//! the panel iterates `.rev()`.
//!
//! ## Active layer
//!
//! Clicking a layer row fires `SetActiveLayer`. The active row is
//! highlighted via egui's selectable-label visual. The active stroke's
//! "frozen target" is engine-side state (see `Layer.visible` ↔
//! `active_stroke_layer` in the snapshot doc-comment); the panel always
//! shows `Layer.visible` directly so the eye-icon never lies.
//!
//! ## Rename / drag-reorder (deferred)
//!
//! C3 ships add / remove / move-up / move-down buttons; inline rename and
//! drag-to-reorder land in a future phase.

use aj_core::{BlendMode, Layer, LayerId};

use super::LayerAction;

/// Human-readable label for a blend mode in the dropdown. `Unknown` is
/// shown only when a loaded document carries it (forward-compat
/// fallback) — the user-selectable list filters it out.
fn blend_mode_label(mode: BlendMode) -> &'static str {
    match mode {
        BlendMode::Normal => "Normal",
        BlendMode::OklabMix => "Oklab Mix",
        BlendMode::Unknown => "Unknown (fallback)",
        _ => "Unknown",
    }
}

/// Tooltip for each blend mode — explains the math in user terms.
fn blend_mode_tooltip(mode: BlendMode) -> &'static str {
    match mode {
        BlendMode::Normal => {
            "Standard alpha-over: layer on top of below in linear RGB. The default."
        }
        BlendMode::OklabMix => {
            "Lerp in Oklab — perceptually-uniform colour midpoints. Two saturated layers blend to a midpoint that 'looks right' instead of the muddy linear-RGB lerp."
        }
        BlendMode::Unknown => {
            "Forward-compat fallback. This document was written by a newer build that introduced a blend mode this binary doesn't recognise; it renders as Normal."
        }
        _ => "",
    }
}

/// Render the layers section as a `CollapsingHeader` inside `ui`. Emits
/// `LayerAction`s into `pending`; the main loop dispatches them after
/// egui exits.
pub fn draw_section(
    ui: &mut egui::Ui,
    layers: &[Layer],
    active_layer: LayerId,
    pending: &mut Vec<LayerAction>,
) {
    egui::CollapsingHeader::new("Layers").default_open(true).show(ui, |ui| {
        // List rows top-of-stack first (reverse the bottom-to-top Vec).
        for layer in layers.iter().rev() {
            draw_layer_row(ui, layer, active_layer, pending);
        }

        ui.separator();
        // Add / Remove / Move-up / Move-down toolbar.
        ui.horizontal(|ui| {
            if ui.button("+").on_hover_text("Add a new layer above all others").clicked() {
                // Name pattern matches the default ("Layer N"). Numbering
                // is by current count + 1 so successive Adds produce
                // unique names in the common case; collisions (after a
                // remove + add) are tolerated until the rename phase ships.
                let name = format!("Layer {}", layers.len() + 1);
                pending.push(LayerAction::AddLayer { name });
            }

            let can_remove = layers.len() > 1;
            let remove_btn = egui::Button::new("−");
            if ui
                .add_enabled(can_remove, remove_btn)
                .on_hover_text(if can_remove {
                    "Remove the active layer"
                } else {
                    "Cannot remove the only layer"
                })
                .clicked()
            {
                pending.push(LayerAction::RemoveLayer(active_layer));
            }

            // Move-up: increase array index of the active layer (toward
            // top of stack / top of UI list).
            let active_idx = layers.iter().position(|l| l.id == active_layer);
            let can_move_up = active_idx.is_some_and(|i| i + 1 < layers.len());
            let up_btn = egui::Button::new("▲");
            if ui.add_enabled(can_move_up, up_btn).on_hover_text("Move active layer up").clicked()
                && let Some(i) = active_idx
            {
                pending.push(LayerAction::MoveLayer { id: active_layer, new_index: i + 1 });
            }

            let can_move_down = active_idx.is_some_and(|i| i > 0);
            let down_btn = egui::Button::new("▼");
            if ui
                .add_enabled(can_move_down, down_btn)
                .on_hover_text("Move active layer down")
                .clicked()
                && let Some(i) = active_idx
            {
                pending.push(LayerAction::MoveLayer { id: active_layer, new_index: i - 1 });
            }
        });
    });
}

fn draw_layer_row(
    ui: &mut egui::Ui,
    layer: &Layer,
    active_layer: LayerId,
    pending: &mut Vec<LayerAction>,
) {
    let is_active = layer.id == active_layer;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal(|ui| {
            // Visibility toggle (eye icon as the leading control).
            let mut visible = layer.visible;
            if ui
                .add(egui::SelectableLabel::new(visible, "👁"))
                .on_hover_text(if visible { "Hide layer" } else { "Show layer" })
                .clicked()
            {
                visible = !visible;
                pending.push(LayerAction::SetLayerVisible { id: layer.id, visible });
            }

            // Layer name; clicking selects it as active. Selectable visual
            // gives the active row its highlight.
            let name_resp = ui.add(egui::SelectableLabel::new(is_active, layer.name.as_str()));
            if name_resp.clicked() && !is_active {
                pending.push(LayerAction::SetActiveLayer(layer.id));
            }
        });

        // Per-layer opacity slider.
        let mut opacity = layer.opacity;
        let resp = ui.add(
            egui::Slider::new(&mut opacity, 0.0..=1.0)
                .custom_formatter(|v, _| format!("{:.0}", v * 100.0))
                .suffix(" %")
                .text("Opacity"),
        );
        if resp.changed() {
            pending.push(LayerAction::SetLayerOpacity {
                id: layer.id,
                opacity: opacity.clamp(0.0, 1.0),
            });
        }

        // Blend mode dropdown. `Unknown` is filtered from the user-
        // selectable list (it only ever appears as the *current* selection
        // on a loaded doc written by a future build).
        ui.horizontal(|ui| {
            ui.label("Blend");
            egui::ComboBox::from_id_salt(("aj_layer_blend", layer.id.raw()))
                .selected_text(blend_mode_label(layer.blend_mode))
                .show_ui(ui, |ui| {
                    for mode in [BlendMode::Normal, BlendMode::OklabMix] {
                        let resp = ui
                            .selectable_label(layer.blend_mode == mode, blend_mode_label(mode))
                            .on_hover_text(blend_mode_tooltip(mode));
                        if resp.clicked() && layer.blend_mode != mode {
                            pending.push(LayerAction::SetLayerBlendMode { id: layer.id, mode });
                        }
                    }
                });
        });
    });
}
