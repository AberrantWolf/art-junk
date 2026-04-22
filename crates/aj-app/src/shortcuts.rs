//! Central keyboard-shortcut registry.
//!
//! This module is THE answer to "what keyboard shortcuts does this app have?".
//! Every binding lives in [`BINDINGS`] below — read it top-to-bottom to see
//! every shortcut. Both the keyboard resolver ([`resolve`]) and the menu display
//! formatter ([`display_for`]) derive from that single table, so the "key that
//! works" and the "key the menu advertises" can't drift apart.
//!
//! Components (`Action`, `ViewAction`, `BrushAction`) still own their semantics
//! — labels, enablement, dispatch. This module only owns the mapping *from a
//! keypress to an action*. Adding a new shortcut is exactly one new entry here.
//!
//! ## Key match semantics
//!
//! Bindings match on either the logical character (keyboard-layout robust —
//! `Cmd+Z` fires on the `z` key regardless of QWERTY/AZERTY/Dvorak) or the
//! physical key code (layout-insensitive — a bracket-pressed-with-Alt fires
//! even on macOS where the OS composes `Alt+[` into `"`). Use `Logical` for
//! alphabetic bindings; use `Physical` for symbol keys where OS composition
//! would interfere.

use winit::keyboard::{Key, KeyCode, ModifiersState, PhysicalKey};

use crate::ui::{Action, BrushAction, ViewAction};

/// The flat union of every user-triggerable action that can be bound to a key.
/// Wrapping per-component enums keeps each component's semantics in its own
/// module while letting the registry own key → action binding in one place.
///
/// `Eq` is deliberately NOT derived — `BrushAction::SetMaxWidth(f32)` carries
/// a float payload. `PartialEq` is enough for the conflict-detection test
/// (which compares action equality via `==`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AppAction {
    Edit(Action),
    View(ViewAction),
    Brush(BrushAction),
}

/// Platform-sensitive accelerator modifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accel {
    /// No modifier required. The shortcut fires on a bare keypress (e.g. `[`
    /// / `]` for brush size). The main event loop gates by egui input focus
    /// *before* calling into this resolver, so bare-key bindings don't hijack
    /// text-field input.
    None,
    /// Cmd on macOS, Ctrl elsewhere — the standard "app shortcut" modifier.
    Primary,
    /// Ctrl on all platforms. Reserved for future bindings that must be
    /// distinct from Cmd on macOS (none in v1).
    #[allow(dead_code)]
    Ctrl,
    /// Alt / Option. On macOS the Option key is the system's compose key, so
    /// `Alt+[` produces a composed character at the logical layer — these
    /// bindings MUST also use `KeyMatch::Physical`.
    Alt,
}

impl Accel {
    /// True if the modifier this `Accel` represents is currently held.
    fn held(self, mods: ModifiersState) -> bool {
        match self {
            Accel::None => !mods.super_key() && !mods.control_key() && !mods.alt_key(),
            Accel::Primary => {
                if cfg!(target_os = "macos") {
                    mods.super_key()
                } else {
                    mods.control_key()
                }
            }
            Accel::Ctrl => mods.control_key(),
            Accel::Alt => mods.alt_key(),
        }
    }

    /// Menu-display prefix for this accel on the current platform.
    fn display_prefix(self) -> &'static str {
        match self {
            Accel::None => "",
            Accel::Primary => {
                if cfg!(target_os = "macos") {
                    "⌘"
                } else {
                    "Ctrl+"
                }
            }
            Accel::Ctrl => "Ctrl+",
            Accel::Alt => {
                if cfg!(target_os = "macos") {
                    "⌥"
                } else {
                    "Alt+"
                }
            }
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

/// How a binding matches the key portion of an event.
#[derive(Debug, Clone, Copy)]
pub enum KeyMatch {
    /// Match on the logical character(s) reported by the OS, after case-lowered
    /// comparison. Use for alphabetic shortcuts where keyboard-layout
    /// compatibility matters more than the physical key position.
    Logical(&'static [&'static str]),
    /// Match on the physical key code, regardless of what character the OS
    /// composed. Use for symbol keys (e.g. `[` / `]`) where modifier-composition
    /// on macOS (Alt+[) would otherwise make the binding unreachable.
    Physical(&'static [KeyCode]),
}

#[derive(Debug, Clone, Copy)]
pub struct Binding {
    pub key: KeyMatch,
    pub accel: Accel,
    pub shift: ShiftReq,
    pub action: AppAction,
}

impl Binding {
    fn matches(&self, logical: &Key, physical: PhysicalKey, mods: ModifiersState) -> bool {
        match self.key {
            KeyMatch::Logical(keys) => {
                let Key::Character(s) = logical else { return false };
                let lowered = s.to_ascii_lowercase();
                if !keys.contains(&lowered.as_str()) {
                    return false;
                }
            }
            KeyMatch::Physical(codes) => {
                let PhysicalKey::Code(code) = physical else { return false };
                if !codes.contains(&code) {
                    return false;
                }
            }
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

    /// Menu-display string.
    fn display(&self) -> String {
        let shift_str = if self.shift == ShiftReq::Required {
            if cfg!(target_os = "macos") { "⇧" } else { "Shift+" }
        } else {
            ""
        };
        let accel_prefix = self.accel.display_prefix();
        let key = match self.key {
            KeyMatch::Logical(keys) => keys[0].to_uppercase(),
            KeyMatch::Physical(codes) => physical_key_label(codes[0]).to_owned(),
        };
        format!("{accel_prefix}{shift_str}{key}")
    }
}

/// Human-readable label for a physical key code. Limited to the codes we
/// actually bind; unknown codes fall back to the debug name.
fn physical_key_label(code: KeyCode) -> &'static str {
    match code {
        KeyCode::BracketLeft => "[",
        KeyCode::BracketRight => "]",
        _ => "?",
    }
}

/// Multiplicative factor for `[` on `max_width`. Exactly `1 / 1.2` so
/// `DECR * INCR == 1.0` bit-exact in f32 — reversibility guaranteed.
pub const BRUSH_WIDTH_FACTOR_DECR: f32 = 1.0 / 1.2;
/// Multiplicative factor for `]` on `max_width`.
pub const BRUSH_WIDTH_FACTOR_INCR: f32 = 1.2;
/// Additive step for `Alt+[` / `Alt+]` on min-ratio. Linear perception, no
/// zero-trap — from ratio 0 a single `Alt+]` moves to 0.1.
pub const BRUSH_MIN_RATIO_STEP: f32 = 0.1;

/// THE registry. Exhaustive. Every keyboard shortcut in the app is in this
/// slice — reading this declaration tells you every shortcut that exists.
pub const BINDINGS: &[Binding] = &[
    // --- Edit ---
    Binding {
        key: KeyMatch::Logical(&["z"]),
        accel: Accel::Primary,
        shift: ShiftReq::Forbidden,
        action: AppAction::Edit(Action::Undo),
    },
    Binding {
        key: KeyMatch::Logical(&["z"]),
        accel: Accel::Primary,
        shift: ShiftReq::Required,
        action: AppAction::Edit(Action::Redo),
    },
    #[cfg(not(target_os = "macos"))]
    Binding {
        key: KeyMatch::Logical(&["y"]),
        accel: Accel::Primary,
        shift: ShiftReq::Forbidden,
        action: AppAction::Edit(Action::Redo),
    },
    // --- View ---
    Binding {
        key: KeyMatch::Logical(&["=", "+"]),
        accel: Accel::Primary,
        shift: ShiftReq::Either,
        action: AppAction::View(ViewAction::ZoomIn),
    },
    Binding {
        key: KeyMatch::Logical(&["-", "_"]),
        accel: Accel::Primary,
        shift: ShiftReq::Either,
        action: AppAction::View(ViewAction::ZoomOut),
    },
    Binding {
        key: KeyMatch::Logical(&["0"]),
        accel: Accel::Primary,
        shift: ShiftReq::Forbidden,
        action: AppAction::View(ViewAction::ZoomTo100),
    },
    // --- Brush ---
    // Physical-key matched so Alt+[ / Alt+] aren't defeated by macOS's Option
    // compose behavior (which would otherwise turn `[` into `"`).
    Binding {
        key: KeyMatch::Physical(&[KeyCode::BracketLeft]),
        accel: Accel::None,
        shift: ShiftReq::Forbidden,
        action: AppAction::Brush(BrushAction::DecreaseMaxWidth),
    },
    Binding {
        key: KeyMatch::Physical(&[KeyCode::BracketRight]),
        accel: Accel::None,
        shift: ShiftReq::Forbidden,
        action: AppAction::Brush(BrushAction::IncreaseMaxWidth),
    },
    Binding {
        key: KeyMatch::Physical(&[KeyCode::BracketLeft]),
        accel: Accel::Alt,
        shift: ShiftReq::Forbidden,
        action: AppAction::Brush(BrushAction::DecreaseMinRatio),
    },
    Binding {
        key: KeyMatch::Physical(&[KeyCode::BracketRight]),
        accel: Accel::Alt,
        shift: ShiftReq::Forbidden,
        action: AppAction::Brush(BrushAction::IncreaseMinRatio),
    },
];

/// Resolve a keyboard event to the bound action, or `None` if unbound.
#[must_use]
pub fn resolve(logical: &Key, physical: PhysicalKey, mods: ModifiersState) -> Option<AppAction> {
    BINDINGS.iter().find(|b| b.matches(logical, physical, mods)).map(|b| b.action)
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
    fn canonical_input(b: &Binding) -> (Key, PhysicalKey, ModifiersState) {
        let (logical, physical) = match b.key {
            KeyMatch::Logical(keys) => (
                Key::Character(SmolStr::new_inline(keys[0])),
                PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified),
            ),
            KeyMatch::Physical(codes) => (
                Key::Unidentified(winit::keyboard::NativeKey::Unidentified),
                PhysicalKey::Code(codes[0]),
            ),
        };
        let mut mods = ModifiersState::empty();
        match b.accel {
            Accel::None => {}
            Accel::Primary => {
                if cfg!(target_os = "macos") {
                    mods |= ModifiersState::SUPER;
                } else {
                    mods |= ModifiersState::CONTROL;
                }
            }
            Accel::Ctrl => mods |= ModifiersState::CONTROL,
            Accel::Alt => mods |= ModifiersState::ALT,
        }
        if b.shift == ShiftReq::Required {
            mods |= ModifiersState::SHIFT;
        }
        (logical, physical, mods)
    }

    fn shifts_can_coincide(a: ShiftReq, b: ShiftReq) -> bool {
        !matches!(
            (a, b),
            (ShiftReq::Required, ShiftReq::Forbidden) | (ShiftReq::Forbidden, ShiftReq::Required)
        )
    }

    fn accels_coincide_here(a: Accel, b: Accel) -> bool {
        let modifier = |acc: Accel| -> ModifiersState {
            match acc {
                Accel::None => ModifiersState::empty(),
                Accel::Primary => {
                    if cfg!(target_os = "macos") {
                        ModifiersState::SUPER
                    } else {
                        ModifiersState::CONTROL
                    }
                }
                Accel::Ctrl => ModifiersState::CONTROL,
                Accel::Alt => ModifiersState::ALT,
            }
        };
        modifier(a) == modifier(b)
    }

    /// Can these two key-matches ever fire for the same input?
    fn keys_can_coincide(a: &KeyMatch, b: &KeyMatch) -> bool {
        match (a, b) {
            (KeyMatch::Logical(ka), KeyMatch::Logical(kb)) => ka.iter().any(|k| kb.contains(k)),
            (KeyMatch::Physical(ca), KeyMatch::Physical(cb)) => ca.iter().any(|c| cb.contains(c)),
            // A Logical and a Physical binding are disjoint as *match inputs*:
            // resolve either finds a logical-keys match (logical = Character)
            // OR a physical match (physical = Code), and the resolver walks
            // BINDINGS in order, returning the first hit. Different types of
            // match for the same physical keypress are allowed to coexist in
            // principle, but in practice a physical keycode that composes into
            // a bracket char would cause confusion — bracket assertion below.
            _ => false,
        }
    }

    #[test]
    fn resolves_each_binding() {
        for b in BINDINGS {
            let (logical, physical, mods) = canonical_input(b);
            let resolved = resolve(&logical, physical, mods);
            assert!(
                resolved.is_some(),
                "no resolution for binding {b:?} using its own canonical input"
            );
        }
    }

    #[test]
    fn displays_each_binding() {
        for b in BINDINGS {
            let s = display_for(b.action).expect("every bound action has a display string");
            assert!(!s.is_empty(), "empty display for {b:?}");
        }
    }

    #[test]
    fn no_conflicting_bindings() {
        for (i, a) in BINDINGS.iter().enumerate() {
            for b in &BINDINGS[i + 1..] {
                if a.action == b.action {
                    continue;
                }
                if !keys_can_coincide(&a.key, &b.key) {
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
    fn no_logical_bindings_use_bracket_chars() {
        // If we ever logical-bind `[` or `]`, macOS Alt-composition would
        // make such bindings either unreachable or ambiguous against the
        // Physical bracket bindings below. Forbid the pattern.
        for b in BINDINGS {
            if let KeyMatch::Logical(keys) = b.key {
                for k in keys {
                    assert!(*k != "[" && *k != "]", "use Physical for bracket keys: {b:?}");
                }
            }
        }
    }

    #[test]
    fn bracket_reversibility_factors() {
        // `[` then `]` must return max-width to bit-identical.
        let result = BRUSH_WIDTH_FACTOR_DECR * BRUSH_WIDTH_FACTOR_INCR;
        assert_eq!(result.to_bits(), 1.0_f32.to_bits(), "DECR * INCR != 1.0: {result}");
    }

    #[test]
    fn case_insensitive_letter_match() {
        let logical = Key::Character(SmolStr::new_inline("Z"));
        let physical = PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified);
        let mut mods = ModifiersState::empty() | ModifiersState::SHIFT;
        if cfg!(target_os = "macos") {
            mods |= ModifiersState::SUPER;
        } else {
            mods |= ModifiersState::CONTROL;
        }
        assert_eq!(resolve(&logical, physical, mods), Some(AppAction::Edit(Action::Redo)));
    }

    #[test]
    fn physical_bracket_matches_even_when_alt_composes_logical() {
        // Simulates macOS Alt+[: logical is "“" (U+201C), physical is BracketLeft.
        let logical = Key::Character(SmolStr::new_inline("“"));
        let physical = PhysicalKey::Code(KeyCode::BracketLeft);
        let mods = ModifiersState::ALT;
        assert_eq!(
            resolve(&logical, physical, mods),
            Some(AppAction::Brush(BrushAction::DecreaseMinRatio))
        );
    }

    #[test]
    fn bare_bracket_resolves_without_modifiers() {
        let logical = Key::Unidentified(winit::keyboard::NativeKey::Unidentified);
        let physical = PhysicalKey::Code(KeyCode::BracketRight);
        assert_eq!(
            resolve(&logical, physical, ModifiersState::empty()),
            Some(AppAction::Brush(BrushAction::IncreaseMaxWidth))
        );
    }

    #[test]
    fn bracket_with_primary_accel_does_not_resolve_as_brush() {
        // Cmd+] shouldn't fire the brush shortcut.
        let logical = Key::Unidentified(winit::keyboard::NativeKey::Unidentified);
        let physical = PhysicalKey::Code(KeyCode::BracketRight);
        let mods =
            if cfg!(target_os = "macos") { ModifiersState::SUPER } else { ModifiersState::CONTROL };
        assert_eq!(resolve(&logical, physical, mods), None);
    }

    #[test]
    fn shifted_key_pair_variants_both_resolve() {
        let primary =
            if cfg!(target_os = "macos") { ModifiersState::SUPER } else { ModifiersState::CONTROL };
        let physical = PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified);
        let eq = Key::Character(SmolStr::new_inline("="));
        assert_eq!(resolve(&eq, physical, primary), Some(AppAction::View(ViewAction::ZoomIn)));
        let plus = Key::Character(SmolStr::new_inline("+"));
        assert_eq!(
            resolve(&plus, physical, primary | ModifiersState::SHIFT),
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
        let logical = Key::Character(SmolStr::new_inline("y"));
        let physical = PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified);
        let mods = ModifiersState::CONTROL;
        assert_eq!(resolve(&logical, physical, mods), Some(AppAction::Edit(Action::Redo)));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn cmd_y_does_not_resolve_on_mac() {
        let logical = Key::Character(SmolStr::new_inline("y"));
        let physical = PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified);
        let mods = ModifiersState::SUPER;
        assert_eq!(resolve(&logical, physical, mods), None);
    }

    #[test]
    fn bare_key_without_accel_does_not_resolve_edit() {
        let logical = Key::Character(SmolStr::new_inline("z"));
        let physical = PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified);
        assert_eq!(resolve(&logical, physical, ModifiersState::empty()), None);
    }

    #[test]
    fn non_character_key_does_not_resolve_primary() {
        use winit::keyboard::NamedKey;
        let logical: Key = Key::Named(NamedKey::Escape);
        let physical = PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified);
        let primary =
            if cfg!(target_os = "macos") { ModifiersState::SUPER } else { ModifiersState::CONTROL };
        assert_eq!(resolve(&logical, physical, primary), None);
    }
}
