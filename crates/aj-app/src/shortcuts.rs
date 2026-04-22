//! Central keyboard-shortcut registry.
//!
//! This module is THE answer to "what keyboard shortcuts does this app have?".
//! Every binding lives in [`BINDINGS`] below — read it top-to-bottom to see
//! every shortcut. Both the keyboard resolver ([`resolve`]) and the menu display
//! formatter ([`display_for`]) derive from that single table, so the "key that
//! works" and the "key the menu advertises" can't drift apart.
//!
//! Components (`Action`, `ViewAction`) still own their semantics — labels,
//! enablement, dispatch. This module only owns the mapping *from a keypress to
//! an action*. Adding a new shortcut is exactly one new entry here.
//!
//! ## Adding a new shortcut
//!
//! 1. Add (or reuse) a variant on the relevant action enum in `ui.rs`.
//! 2. Wrap it in [`AppAction`] and add a [`Binding`] entry to [`BINDINGS`].
//!
//! The conflict-detection test (`no_conflicting_bindings`) fails CI if you
//! collide with an existing binding.

use winit::keyboard::{Key, ModifiersState};

use crate::ui::{Action, ViewAction};

/// The flat union of every user-triggerable action that can be bound to a key.
/// Wrapping per-component enums keeps each component's semantics in its own
/// module while letting the registry own key → action binding in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppAction {
    Edit(Action),
    View(ViewAction),
}

/// Platform-sensitive accelerator modifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accel {
    /// Cmd on macOS, Ctrl elsewhere — the standard "app shortcut" modifier.
    Primary,
    /// Ctrl on all platforms. Reserved for future bindings that must be
    /// distinct from Cmd on macOS (none in v1).
    #[allow(dead_code)]
    Ctrl,
}

impl Accel {
    /// True if the modifier this `Accel` represents is currently held.
    fn held(self, mods: ModifiersState) -> bool {
        match self {
            Accel::Primary => {
                if cfg!(target_os = "macos") {
                    mods.super_key()
                } else {
                    mods.control_key()
                }
            }
            Accel::Ctrl => mods.control_key(),
        }
    }

    /// Menu-display prefix for this accel on the current platform.
    fn display_prefix(self) -> &'static str {
        match self {
            Accel::Primary => {
                if cfg!(target_os = "macos") {
                    "⌘"
                } else {
                    "Ctrl+"
                }
            }
            Accel::Ctrl => "Ctrl+",
        }
    }
}

/// Shift-modifier requirement. `Either` is for shift-variant key pairs like
/// `=`/`+` where the same physical key reports different characters depending
/// on shift state and we want the binding to fire either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShiftReq {
    Required,
    Forbidden,
    Either,
}

#[derive(Debug, Clone, Copy)]
pub struct Binding {
    /// Key characters (lowercase). A slice because the same binding may match
    /// multiple reported characters: e.g. `["=", "+"]` catches both the
    /// unshifted and shifted forms of one physical key on US layouts.
    pub keys: &'static [&'static str],
    pub accel: Accel,
    pub shift: ShiftReq,
    pub action: AppAction,
}

impl Binding {
    fn matches(&self, key: &Key, mods: ModifiersState) -> bool {
        let Key::Character(s) = key else { return false };
        let lowered = s.to_ascii_lowercase();
        if !self.keys.contains(&lowered.as_str()) {
            return false;
        }
        if !self.accel.held(mods) {
            return false;
        }
        match self.shift {
            ShiftReq::Required => mods.shift_key(),
            ShiftReq::Forbidden => !mods.shift_key(),
            ShiftReq::Either => true,
        }
    }

    /// Menu-display string. Uses the first entry of `keys` uppercased, prefixed
    /// with the platform-appropriate accel (and optionally shift) symbol.
    fn display(&self) -> String {
        let shift_str = if self.shift == ShiftReq::Required {
            if cfg!(target_os = "macos") { "⇧" } else { "Shift+" }
        } else {
            ""
        };
        let accel_prefix = self.accel.display_prefix();
        let key = self.keys[0].to_uppercase();
        format!("{accel_prefix}{shift_str}{key}")
    }
}

/// THE registry. Exhaustive. Every keyboard shortcut in the app is in this
/// slice — reading this declaration tells you every shortcut that exists.
pub const BINDINGS: &[Binding] = &[
    // --- Edit ---
    Binding {
        keys: &["z"],
        accel: Accel::Primary,
        shift: ShiftReq::Forbidden,
        action: AppAction::Edit(Action::Undo),
    },
    Binding {
        keys: &["z"],
        accel: Accel::Primary,
        shift: ShiftReq::Required,
        action: AppAction::Edit(Action::Redo),
    },
    // Windows/Linux convention: Ctrl+Y as a second Redo binding. Platform-gated
    // so it doesn't appear (or test-conflict) on macOS, where Cmd+Y is a
    // reserved system shortcut in many apps.
    #[cfg(not(target_os = "macos"))]
    Binding {
        keys: &["y"],
        accel: Accel::Primary,
        shift: ShiftReq::Forbidden,
        action: AppAction::Edit(Action::Redo),
    },
    // --- View ---
    // `=` and `+` are the same physical key with/without shift on US layouts;
    // accept either reported character and either shift state. Display uses
    // the first entry ("=") so the menu shows the canonical unshifted form.
    Binding {
        keys: &["=", "+"],
        accel: Accel::Primary,
        shift: ShiftReq::Either,
        action: AppAction::View(ViewAction::ZoomIn),
    },
    Binding {
        keys: &["-", "_"],
        accel: Accel::Primary,
        shift: ShiftReq::Either,
        action: AppAction::View(ViewAction::ZoomOut),
    },
    Binding {
        keys: &["0"],
        accel: Accel::Primary,
        shift: ShiftReq::Forbidden,
        action: AppAction::View(ViewAction::ZoomTo100),
    },
];

/// Resolve a keyboard event to the bound action, or `None` if unbound.
#[must_use]
pub fn resolve(key: &Key, mods: ModifiersState) -> Option<AppAction> {
    BINDINGS.iter().find(|b| b.matches(key, mods)).map(|b| b.action)
}

/// Menu-display string for an action, or `None` if the action has no keyboard
/// binding. If an action has multiple bindings, the first one in [`BINDINGS`]
/// is used for display — put the canonical/primary binding first.
#[must_use]
pub fn display_for(action: AppAction) -> Option<String> {
    BINDINGS.iter().find(|b| b.action == action).map(Binding::display)
}

#[cfg(test)]
mod tests {
    use winit::keyboard::SmolStr;

    use super::*;

    /// Construct the canonical input for a binding (first key, required mods).
    fn canonical_input(b: &Binding) -> (Key, ModifiersState) {
        let key = Key::Character(SmolStr::new_inline(b.keys[0]));
        let mut mods = ModifiersState::empty();
        match b.accel {
            Accel::Primary => {
                if cfg!(target_os = "macos") {
                    mods |= ModifiersState::SUPER;
                } else {
                    mods |= ModifiersState::CONTROL;
                }
            }
            Accel::Ctrl => mods |= ModifiersState::CONTROL,
        }
        if b.shift == ShiftReq::Required {
            mods |= ModifiersState::SHIFT;
        }
        (key, mods)
    }

    /// Same-platform conflict: do these two bindings' shift requirements leave
    /// any input that could satisfy both? `Required` + `Forbidden` is the only
    /// pair that cannot be simultaneously satisfied.
    fn shifts_can_coincide(a: ShiftReq, b: ShiftReq) -> bool {
        !matches!(
            (a, b),
            (ShiftReq::Required, ShiftReq::Forbidden) | (ShiftReq::Forbidden, ShiftReq::Required)
        )
    }

    fn accels_coincide_here(a: Accel, b: Accel) -> bool {
        // Two accels conflict only if they resolve to the same modifier on the
        // current test platform. On non-mac, Primary and Ctrl both mean Ctrl,
        // so they'd conflict; on mac, Primary=Cmd, Ctrl=Ctrl, so they don't.
        let modifier = |acc: Accel| -> ModifiersState {
            match acc {
                Accel::Primary => {
                    if cfg!(target_os = "macos") {
                        ModifiersState::SUPER
                    } else {
                        ModifiersState::CONTROL
                    }
                }
                Accel::Ctrl => ModifiersState::CONTROL,
            }
        };
        modifier(a) == modifier(b)
    }

    #[test]
    fn resolves_each_binding() {
        for b in BINDINGS {
            let (key, mods) = canonical_input(b);
            let resolved = resolve(&key, mods);
            assert!(
                resolved.is_some(),
                "no resolution for binding {b:?} using its own canonical input"
            );
            // Don't require `resolved == b.action` — if an earlier binding in
            // the slice also matches this input, that's the "no conflict" test's
            // job to catch; here we just verify the binding is reachable *at
            // all* (i.e., something resolves for the input it declared).
        }
    }

    #[test]
    fn displays_each_binding() {
        for b in BINDINGS {
            let s = display_for(b.action).expect("every bound action has a display string");
            assert!(!s.is_empty(), "empty display for {b:?}");
            assert!(
                s.starts_with(b.accel.display_prefix()),
                "display '{s}' does not start with accel prefix for {b:?}",
            );
        }
    }

    #[test]
    fn no_conflicting_bindings() {
        for (i, a) in BINDINGS.iter().enumerate() {
            for b in &BINDINGS[i + 1..] {
                // Same action with multiple bindings (e.g. Redo via Cmd+Shift+Z
                // AND Ctrl+Y on non-mac) is an alias, not a conflict.
                if a.action == b.action {
                    continue;
                }
                let keys_overlap = a.keys.iter().any(|k| b.keys.contains(k));
                if !keys_overlap {
                    continue;
                }
                if !accels_coincide_here(a.accel, b.accel) {
                    continue;
                }
                assert!(
                    !shifts_can_coincide(a.shift, b.shift),
                    "bindings conflict on this platform: {a:?} and {b:?}",
                );
            }
        }
    }

    #[test]
    fn case_insensitive_letter_match() {
        // Winit reports "Z" when Shift is held over the z key. Our matcher
        // lowercases before comparing, so the "z" binding with ShiftReq::Required
        // should fire on the reported-as-"Z" input.
        let key = Key::Character(SmolStr::new_inline("Z"));
        let mut mods = ModifiersState::empty() | ModifiersState::SHIFT;
        if cfg!(target_os = "macos") {
            mods |= ModifiersState::SUPER;
        } else {
            mods |= ModifiersState::CONTROL;
        }
        assert_eq!(resolve(&key, mods), Some(AppAction::Edit(Action::Redo)));
    }

    #[test]
    fn shifted_key_pair_variants_both_resolve() {
        let primary =
            if cfg!(target_os = "macos") { ModifiersState::SUPER } else { ModifiersState::CONTROL };
        // "=" (unshifted) ⇒ ZoomIn.
        let eq = Key::Character(SmolStr::new_inline("="));
        assert_eq!(resolve(&eq, primary), Some(AppAction::View(ViewAction::ZoomIn)));
        // "+" (the shifted form of the same physical key) ⇒ also ZoomIn.
        let plus = Key::Character(SmolStr::new_inline("+"));
        assert_eq!(
            resolve(&plus, primary | ModifiersState::SHIFT),
            Some(AppAction::View(ViewAction::ZoomIn))
        );
    }

    #[test]
    fn unbound_action_has_no_display() {
        assert_eq!(display_for(AppAction::View(ViewAction::ResetView)), None);
        assert_eq!(display_for(AppAction::View(ViewAction::ZoomToFit)), None);
        assert_eq!(display_for(AppAction::View(ViewAction::TogglePageBounds)), None);
        assert_eq!(display_for(AppAction::View(ViewAction::ToggleClipToBounds)), None);
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn ctrl_y_resolves_to_redo_on_non_mac() {
        let key = Key::Character(SmolStr::new_inline("y"));
        let mods = ModifiersState::CONTROL;
        assert_eq!(resolve(&key, mods), Some(AppAction::Edit(Action::Redo)));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn cmd_y_does_not_resolve_on_mac() {
        let key = Key::Character(SmolStr::new_inline("y"));
        let mods = ModifiersState::SUPER;
        assert_eq!(resolve(&key, mods), None);
    }

    #[test]
    fn bare_key_without_accel_does_not_resolve() {
        // Pressing "z" alone shouldn't trigger Undo — we require the accel.
        let key = Key::Character(SmolStr::new_inline("z"));
        assert_eq!(resolve(&key, ModifiersState::empty()), None);
    }

    #[test]
    fn non_character_key_does_not_resolve() {
        use winit::keyboard::NamedKey;
        let key: Key = Key::Named(NamedKey::Escape);
        let primary =
            if cfg!(target_os = "macos") { ModifiersState::SUPER } else { ModifiersState::CONTROL };
        assert_eq!(resolve(&key, primary), None);
    }
}
