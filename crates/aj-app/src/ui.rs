//! Action dispatch, keyboard bindings, and menu-bar rendering.
//!
//! The same `Action` enum feeds both the keyboard handler (`match_action`) and the
//! egui menu bar (`draw_menu_bar`), so adding a new action is one enum variant plus
//! one dispatch arm — the menu picks it up automatically via `strum::IntoEnumIterator`.

use aj_core::HistoryStatus;
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
pub fn draw_menu_bar(ctx: &egui::Context, history: HistoryStatus, pending: &mut Vec<Action>) {
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
                        pending.push(action);
                        ui.close_menu();
                    }
                }
            });
            ui.menu_button("View", |ui| {
                ui.add_enabled(false, egui::Button::new("Reset view"));
            });
        });
    });
}
