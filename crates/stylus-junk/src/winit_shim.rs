//! Winit → primitive event translator.
//!
//! Gated on `feature = "winit"`. The adapter's primitive API
//! (`on_cursor_moved`, `on_mouse_button`, `on_touch`) is the source of truth;
//! this module is just a convenience for consumers who already have winit
//! events in hand. Calling it is equivalent to forwarding those events to
//! the primitives manually.
//!
//! `StylusAdapter` re-exports the `winit` crate at the crate root (`pub use
//! ::winit;`) so consumers can't accidentally link two winit versions.

use winit::event::{
    ElementState, Force, MouseButton as WinitMouseButton, Touch, TouchPhase as WinitTouchPhase,
    WindowEvent,
};

use crate::adapter::{ButtonState, MouseButton, TouchEvent, TouchPhase};
use crate::{Point, StylusAdapter};

impl StylusAdapter {
    /// Translate one winit event into zero or more queued `StylusEvent`s by
    /// forwarding it to the primitive API.
    pub fn on_window_event(&mut self, event: &WindowEvent) {
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                self.on_cursor_moved(Point::new(position.x, position.y));
            }
            WindowEvent::MouseInput { button, state, .. } => {
                let position = self.last_cursor_position().unwrap_or(Point::ZERO);
                self.on_mouse_button(translate_button(*button), translate_state(*state), position);
            }
            // CursorLeft is intentionally a no-op: if the user drags off the
            // window mid-stroke, we want the stroke to keep going until the
            // real Released event arrives. All desktop OSes deliver a
            // Released event for the eventual button-up even when the cursor
            // is outside the window, via the implicit capture set up by the
            // Pressed on entry.
            WindowEvent::Touch(touch) => {
                self.on_touch(translate_touch(touch));
            }
            _ => {}
        }
    }
}

fn translate_button(b: WinitMouseButton) -> MouseButton {
    match b {
        WinitMouseButton::Left => MouseButton::Left,
        WinitMouseButton::Right => MouseButton::Right,
        WinitMouseButton::Middle => MouseButton::Middle,
        WinitMouseButton::Other(n) => MouseButton::Other(n),
        // Back / Forward map to Other so they route through the "ignored"
        // path in the adapter. Variants this enum gains in future winit
        // majors fall through the same way via the catch-all.
        _ => MouseButton::Other(0),
    }
}

fn translate_state(s: ElementState) -> ButtonState {
    match s {
        ElementState::Pressed => ButtonState::Pressed,
        ElementState::Released => ButtonState::Released,
    }
}

fn translate_touch(t: &Touch) -> TouchEvent {
    TouchEvent {
        id: t.id,
        phase: match t.phase {
            WinitTouchPhase::Started => TouchPhase::Started,
            WinitTouchPhase::Moved => TouchPhase::Moved,
            WinitTouchPhase::Ended => TouchPhase::Ended,
            WinitTouchPhase::Cancelled => TouchPhase::Cancelled,
        },
        position: Point::new(t.location.x, t.location.y),
        force: t.force.map(normalize_force),
    }
}

fn normalize_force(force: Force) -> f32 {
    // Pressure lives in 0..=1 and is already constrained, so f64→f32 is safe here.
    #[allow(clippy::cast_possible_truncation)]
    match force {
        Force::Calibrated { force, max_possible_force, .. } => {
            if max_possible_force > 0.0 {
                (force / max_possible_force) as f32
            } else {
                0.0
            }
        }
        Force::Normalized(n) => n as f32,
    }
}
