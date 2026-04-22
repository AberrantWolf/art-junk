//! Action enums and menu-bar rendering.
//!
//! Keyboard-binding and display of shortcuts live in `shortcuts.rs`. This module
//! owns the *semantics* of each action (label, enablement, dispatch) and the
//! layout of the menu bar. Adding a new action here; binding it to a key there.

use aj_core::{HistoryStatus, Page};
use aj_engine::{Command, Engine};
use strum::IntoEnumIterator;
use strum_macros::EnumIter;

use crate::shortcuts::{self, AppAction};

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter)]
pub enum Action {
    Undo,
    Redo,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Undo => "Undo",
            Action::Redo => "Redo",
        }
    }

    pub fn enabled(self, h: HistoryStatus) -> bool {
        match self {
            Action::Undo => h.can_undo,
            Action::Redo => h.can_redo,
        }
    }

    pub fn dispatch(self, engine: &Engine) {
        engine.send(match self {
            Action::Undo => Command::Undo,
            Action::Redo => Command::Redo,
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewAction {
    ZoomIn,
    ZoomOut,
    ZoomTo100,
    ZoomToFit,
    ResetView,
    TogglePageBounds,
    ToggleClipToBounds,
}

/// Render the top menu bar and collect any actions the user triggered by clicking
/// menu entries. Dispatch happens outside so we don't hold engine borrows across
/// egui closures.
pub fn draw_menu_bar(
    ctx: &egui::Context,
    history: HistoryStatus,
    page: Page,
    pending_edit: &mut Vec<Action>,
    pending_view: &mut Vec<ViewAction>,
) {
    egui::TopBottomPanel::top("aj_menu_bar").show(ctx, |ui| {
        egui::menu::bar(ui, |ui| {
            ui.menu_button("File", |ui| {
                ui.add_enabled(false, egui::Button::new("Open…"));
                ui.add_enabled(false, egui::Button::new("Save…"));
            });
            ui.menu_button("Edit", |ui| {
                for action in Action::iter() {
                    let mut btn = egui::Button::new(action.label());
                    if let Some(sc) = shortcuts::display_for(AppAction::Edit(action)) {
                        btn = btn.shortcut_text(sc);
                    }
                    if ui.add_enabled(action.enabled(history), btn).clicked() {
                        pending_edit.push(action);
                        ui.close_menu();
                    }
                }
            });
            ui.menu_button("View", |ui| {
                let zoom_entries = [
                    (ViewAction::ZoomIn, "Zoom In"),
                    (ViewAction::ZoomOut, "Zoom Out"),
                    (ViewAction::ZoomTo100, "Zoom to 100%"),
                    (ViewAction::ZoomToFit, "Zoom to Fit"),
                    (ViewAction::ResetView, "Reset View"),
                ];
                for (action, label) in zoom_entries {
                    let mut btn = egui::Button::new(label);
                    if let Some(sc) = shortcuts::display_for(AppAction::View(action)) {
                        btn = btn.shortcut_text(sc);
                    }
                    if ui.add(btn).clicked() {
                        pending_view.push(action);
                        ui.close_menu();
                    }
                }
                ui.separator();
                // egui's checkbox wants a mutable bool, so we hand it a copy of the
                // current state. The actual flip goes through the engine: `.changed()`
                // enqueues a toggle action; the next snapshot reflects it.
                let mut show = page.show_bounds;
                if ui.checkbox(&mut show, "Show page bounds").changed() {
                    pending_view.push(ViewAction::TogglePageBounds);
                    ui.close_menu();
                }
                let mut clip = page.clip_to_bounds;
                if ui.checkbox(&mut clip, "Clip strokes to page").changed() {
                    pending_view.push(ViewAction::ToggleClipToBounds);
                    ui.close_menu();
                }
            });
        });
    });
}
