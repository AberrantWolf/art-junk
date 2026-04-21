//! Action dispatch, keyboard bindings, and menu-bar rendering.
//!
//! The same `Action` enum feeds both the keyboard handler (`match_action`) and the
//! egui menu bar (`draw_menu_bar`), so adding a new action is one enum variant plus
//! one dispatch arm — the menu picks it up automatically via `strum::IntoEnumIterator`.
//!
//! `ViewAction` is the parallel enum for the View menu (page toggles in M1; zoom/pan
//! in M2). Split from `Action` so `Action::iter()` stays scoped to the Edit menu and
//! each enum can model its own enable-state without reaching into the other's world.

use aj_core::{HistoryStatus, Page};
use aj_engine::{Command, Engine};
use strum::IntoEnumIterator;
use strum_macros::EnumIter;
use winit::keyboard::{Key, ModifiersState};

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

    /// Human-readable shortcut string for the menu label.
    pub fn shortcut_text(self) -> String {
        let accel = if cfg!(target_os = "macos") { "⌘" } else { "Ctrl+" };
        match self {
            Action::Undo => format!("{accel}Z"),
            Action::Redo => format!("{accel}Shift+Z"),
        }
    }
}

fn accel_held(mods: ModifiersState) -> bool {
    if cfg!(target_os = "macos") { mods.super_key() } else { mods.control_key() }
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

impl ViewAction {
    /// Platform-standard keyboard shortcut string for display in menus.
    pub fn shortcut_text(self) -> Option<String> {
        let accel = if cfg!(target_os = "macos") { "⌘" } else { "Ctrl+" };
        match self {
            ViewAction::ZoomIn => Some(format!("{accel}=")),
            ViewAction::ZoomOut => Some(format!("{accel}-")),
            ViewAction::ZoomTo100 => Some(format!("{accel}0")),
            _ => None,
        }
    }
}

/// Map a logical key + modifier state to a `ViewAction`. Edit-menu shortcuts live
/// in `match_action`; view shortcuts sit in a parallel function so the two enums
/// stay independent. `Ctrl/Cmd + =` zooms in (same physical key as `+`, so this also
/// catches the shifted form on most layouts).
pub fn match_view_action(key: &Key, mods: ModifiersState) -> Option<ViewAction> {
    let Key::Character(s) = key else { return None };
    if !accel_held(mods) {
        return None;
    }
    if s.as_str() == "=" || s.as_str() == "+" {
        return Some(ViewAction::ZoomIn);
    }
    if s.as_str() == "-" || s.as_str() == "_" {
        return Some(ViewAction::ZoomOut);
    }
    if s.as_str() == "0" {
        return Some(ViewAction::ZoomTo100);
    }
    None
}

/// Map a logical key + current modifier state to an `Action`, or `None` if the
/// combination isn't bound. Keeps the binding table in one place rather than
/// scattered across the event handler.
pub fn match_action(key: &Key, mods: ModifiersState) -> Option<Action> {
    let Key::Character(s) = key else { return None };

    if accel_held(mods) && s.eq_ignore_ascii_case("z") {
        return Some(if mods.shift_key() { Action::Redo } else { Action::Undo });
    }

    // Windows/Linux convention: Ctrl+Y as a second Redo binding.
    #[cfg(not(target_os = "macos"))]
    if mods.control_key() && !mods.shift_key() && s.eq_ignore_ascii_case("y") {
        return Some(Action::Redo);
    }

    None
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
                    let btn =
                        egui::Button::new(action.label()).shortcut_text(action.shortcut_text());
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
                    if let Some(sc) = action.shortcut_text() {
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
