//! Stateful input translator. Accepts low-level events from winit (mouse +
//! touch) and, per-platform, from native tablet backends; emits a unified
//! stream of `StylusEvent`s.
//!
//! Internal state lives on `StylusAdapter`:
//!
//! - a monotonic `PointerId` counter so each new touch or pen gets a stable id
//!   across the lifespan of one gesture,
//! - `mouse_down` + last-known cursor position, because winit separates
//!   `MouseInput` (no position) from `CursorMoved` (no button),
//! - a `touches` map from winit's `u64` finger id to our `PointerId`,
//! - a `pens` map keyed by platform `device_id` with caps and the current
//!   pointer id for each physical stylus, plus `active_pen_pointer` which
//!   gates the winit mouse path so a pen-driven mouse event doesn't produce
//!   a duplicate `StylusEvent::Sample`,
//! - `pending_estimated`, keyed by the `update_index` that was emitted with
//!   an `Estimated` sample, mapping to the `PointerId` whose stroke carries
//!   it. Keyed on `update_index` (not pointer) so iOS Pencil's out-of-order
//!   `touchesEstimatedPropertiesUpdated:` can resolve a specific estimate
//!   directly; mac looks up by pointer via `take_pending_for_pointer`.
//! - a per-platform-clock `PlatformTimestampAnchor` so native timestamps
//!   (`NSEvent.timestamp` seconds since boot, QPC ticks on Windows,
//!   `DOMHighResTimeStamp` ms on Web, etc.) translate into this adapter's
//!   `Duration`-since-`self.epoch` timeline. Each backend owns its own anchor
//!   — clocks must not mix.
//!
//! The `drain` iterator (rather than returning from each handler) exists so
//! one platform event can fan into multiple `StylusEvent`s: on macOS, the
//! first sample after a pen-down emits both a `Revise` (refining the Down's
//! pressure) and a new `Sample { phase: Move }`.
//!
//! # Module layout
//!
//! This module's submodules each own one platform's seam — raw-sample +
//! proximity types, the `handle_<platform>_*` methods on `StylusAdapter`,
//! and platform-specific tests. Shared state (the `StylusAdapter` struct
//! itself, `PenState`, `PlatformTimestampAnchor`, the winit mouse/touch
//! path, `on_focus_lost`) lives here in `mod.rs`.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::{Phase, Point, PointerId, Sample, StylusEvent, ToolCaps, ToolKind};

#[cfg(all(feature = "mac", any(target_os = "macos", test)))]
pub(crate) mod mac;

#[cfg(all(feature = "wayland", any(target_os = "linux", test)))]
pub(crate) mod wayland;

#[cfg(all(feature = "x11", any(target_os = "linux", test)))]
pub(crate) mod x11;

#[cfg(all(feature = "windows", any(target_os = "windows", test)))]
pub(crate) mod windows;

#[cfg(all(feature = "web", any(target_arch = "wasm32", test)))]
pub(crate) mod web;

#[cfg(all(feature = "ios", any(target_os = "ios", test)))]
pub(crate) mod ios;

#[cfg(all(feature = "android", any(target_os = "android", test)))]
pub(crate) mod android;

#[cfg(test)]
mod tests_common;

pub struct StylusAdapter {
    epoch: Instant,
    next_pointer_id: u64,
    touches: HashMap<u64, PointerId>,
    mouse_down: bool,
    cursor_position: Option<Point>,
    queue: VecDeque<StylusEvent>,
    pens: HashMap<u32, PenState>,
    active_pen_pointer: Option<PointerId>,
    /// Pending estimated samples awaiting refinement, keyed by the
    /// `update_index` emitted with the original `SampleClass::Estimated`.
    /// Value is the `PointerId` whose stroke owns the estimate.
    pending_estimated: HashMap<u64, PointerId>,
    #[cfg_attr(not(all(feature = "mac", any(target_os = "macos", test))), allow(dead_code))]
    next_update_index: u64,
    #[cfg_attr(not(all(feature = "mac", any(target_os = "macos", test))), allow(dead_code))]
    mac_anchor: Option<PlatformTimestampAnchor>,
    #[cfg_attr(not(all(feature = "wayland", any(target_os = "linux", test))), allow(dead_code))]
    wayland_anchor: Option<PlatformTimestampAnchor>,
    #[cfg_attr(not(all(feature = "windows", any(target_os = "windows", test))), allow(dead_code))]
    windows_anchor: Option<PlatformTimestampAnchor>,
    #[cfg(all(feature = "web", any(target_arch = "wasm32", test)))]
    web_anchor: Option<PlatformTimestampAnchor>,
    /// Per-gesture map from JS `pointerId` (stable only within a single
    /// Pointer-Events gesture) to the adapter's `PointerId`. Down allocates,
    /// Up / Cancel / Leave removes.
    #[cfg(all(feature = "web", any(target_arch = "wasm32", test)))]
    web_pointers: HashMap<i32, web::WebPointerState>,
    #[cfg_attr(not(all(feature = "ios", any(target_os = "ios", test))), allow(dead_code))]
    ios_anchor: Option<PlatformTimestampAnchor>,
    /// Per-iOS-touch map from the `UITouch` identity (hashed) to the
    /// adapter's `PointerId`. iOS pencil gestures have their own id space
    /// that survives coalesced/predicted replays of the same touch.
    #[cfg(all(feature = "ios", any(target_os = "ios", test)))]
    ios_pointers: HashMap<u64, PointerId>,
    #[cfg_attr(not(all(feature = "android", any(target_os = "android", test))), allow(dead_code))]
    android_anchor: Option<PlatformTimestampAnchor>,
    /// Map from Android `MotionEvent` pointer id (distinct per pointer in
    /// a multi-touch gesture) to the adapter's `PointerId`. Android reuses
    /// pointer ids within a gesture; adapter allocates fresh on first
    /// sight, clears on Up/Cancel.
    #[cfg(all(feature = "android", any(target_os = "android", test)))]
    android_pointers: HashMap<i32, PointerId>,
}

/// Per-stylus state, keyed by platform `device_id`. Learned from proximity
/// events where possible, synthesized optimistically on the first stroke if no
/// proximity was seen (app launched with pen already hovering).
pub(crate) struct PenState {
    pub(crate) active_pointer_id: Option<PointerId>,
    pub(crate) caps: ToolCaps,
    pub(crate) tool: ToolKind,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) unique_id: Option<u64>,
    pub(crate) last_position: Option<Point>,
}

/// Translates a platform's monotonic clock (in seconds since some
/// platform-specific epoch — system boot for macOS/Android `CLOCK_MONOTONIC`,
/// QPC-reference on Windows, navigation-start for Web) into a `Duration`
/// measured from `StylusAdapter::epoch`.
///
/// Each platform keeps its own anchor on the adapter (`mac_anchor`,
/// `windows_anchor`, …) because clocks must not be mixed: the adapter sees
/// one unified timeline out, but each backend's first sample anchors its own
/// platform timeline.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PlatformTimestampAnchor {
    #[allow(dead_code)] // read only when a platform backend is enabled
    first_platform_secs: f64,
    #[allow(dead_code)]
    adapter_duration_at_first: Duration,
}

impl PlatformTimestampAnchor {
    /// Translate `platform_secs` to adapter-timeline `Duration`, anchoring on
    /// the first call if `slot` is empty. `adapter_epoch` is `StylusAdapter::epoch`.
    #[allow(dead_code)] // called only by platform backends
    pub(crate) fn translate_or_anchor(
        slot: &mut Option<Self>,
        platform_secs: f64,
        adapter_epoch: Instant,
    ) -> Duration {
        let anchor = *slot.get_or_insert_with(|| Self {
            first_platform_secs: platform_secs,
            adapter_duration_at_first: Instant::now().saturating_duration_since(adapter_epoch),
        });
        let delta_secs = (platform_secs - anchor.first_platform_secs).max(0.0);
        anchor.adapter_duration_at_first + Duration::from_secs_f64(delta_secs)
    }
}

/// Optimistic cap set used when a pen event arrives before any proximity —
/// typical if the app launches with pen already hovering. A cursor-puck would
/// over-claim tilt/twist under this default; survivable, and proximity
/// corrects it as soon as the pen moves out and back in.
#[allow(dead_code)] // used only by platform backends
pub(crate) const OPTIMISTIC_PEN_CAPS: ToolCaps = ToolCaps::PRESSURE
    .union(ToolCaps::TILT)
    .union(ToolCaps::TWIST)
    .union(ToolCaps::TANGENTIAL_PRESSURE);

impl StylusAdapter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            // PointerId(0) is reserved for the mouse; start allocations at 1.
            next_pointer_id: 1,
            touches: HashMap::new(),
            mouse_down: false,
            cursor_position: None,
            queue: VecDeque::new(),
            pens: HashMap::new(),
            active_pen_pointer: None,
            pending_estimated: HashMap::new(),
            next_update_index: 1,
            mac_anchor: None,
            wayland_anchor: None,
            windows_anchor: None,
            #[cfg(all(feature = "web", any(target_arch = "wasm32", test)))]
            web_anchor: None,
            #[cfg(all(feature = "web", any(target_arch = "wasm32", test)))]
            web_pointers: HashMap::new(),
            ios_anchor: None,
            #[cfg(all(feature = "ios", any(target_os = "ios", test)))]
            ios_pointers: HashMap::new(),
            android_anchor: None,
            #[cfg(all(feature = "android", any(target_os = "android", test)))]
            android_pointers: HashMap::new(),
        }
    }

    /// True if any pointer is currently mid-gesture. The app uses this to keep
    /// feeding events to the adapter when the cursor crosses into chrome while a
    /// stroke is in progress (otherwise the stroke would get stranded mid-drag).
    #[must_use]
    pub fn is_tracking_pointer(&self) -> bool {
        self.mouse_down || !self.touches.is_empty() || self.active_pen_pointer.is_some()
    }

    /// Most recent cursor position fed into `on_cursor_moved` (or synthesized
    /// from a winit `CursorMoved` via the `winit` feature). The winit shim
    /// uses this to resolve the position of a `MouseInput` event when winit
    /// didn't deliver a paired `CursorMoved`; external consumers rarely need
    /// it directly.
    #[must_use]
    pub fn last_cursor_position(&self) -> Option<Point> {
        self.cursor_position
    }

    /// Cursor moved to `position` (in screen-space physical pixels). Emits a
    /// `Move` sample if the mouse button is currently down; otherwise just
    /// tracks the position so a subsequent `on_mouse_button` or
    /// `on_focus_lost` has something to work with.
    pub fn on_cursor_moved(&mut self, position: Point) {
        if self.active_pen_pointer.is_some() {
            self.cursor_position = Some(position);
            return;
        }
        self.cursor_position = Some(position);
        if self.mouse_down {
            self.emit_mouse(position, Phase::Move);
        }
    }

    /// Mouse `button` transitioned to `state` at `position`. Non-left buttons
    /// are ignored — stylus-junk tracks only the drawing button today; future
    /// versions may expose barrel/secondary events separately.
    pub fn on_mouse_button(&mut self, button: MouseButton, state: ButtonState, position: Point) {
        if self.active_pen_pointer.is_some() {
            return;
        }
        if button != MouseButton::Left {
            return;
        }
        self.cursor_position = Some(position);
        match state {
            ButtonState::Pressed => {
                if !self.mouse_down {
                    self.mouse_down = true;
                    self.emit_mouse(position, Phase::Down);
                }
            }
            ButtonState::Released => {
                if self.mouse_down {
                    self.mouse_down = false;
                    self.emit_mouse(position, Phase::Up);
                }
            }
        }
    }

    /// Touch (finger) event. The adapter allocates a `PointerId` on `Started`
    /// keyed by `event.id`, reuses it for `Moved`, and frees on `Ended` /
    /// `Cancelled`. `force` is in the unit interval; `None` means the platform
    /// doesn't report per-touch force.
    pub fn on_touch(&mut self, event: TouchEvent) {
        let caps = if event.force.is_some() { ToolCaps::PRESSURE } else { ToolCaps::empty() };
        let ts = self.timestamp();

        let (pointer_id, phase) = match event.phase {
            TouchPhase::Started => {
                let id = alloc_pointer_id(&mut self.next_pointer_id);
                self.touches.insert(event.id, id);
                (id, Phase::Down)
            }
            TouchPhase::Moved => {
                let Some(&id) = self.touches.get(&event.id) else {
                    return;
                };
                (id, Phase::Move)
            }
            TouchPhase::Ended => {
                let Some(id) = self.touches.remove(&event.id) else { return };
                (id, Phase::Up)
            }
            TouchPhase::Cancelled => {
                let Some(id) = self.touches.remove(&event.id) else { return };
                (id, Phase::Cancel)
            }
        };

        let sample = Sample::finger(event.position, ts, pointer_id, event.force);
        self.queue.push_back(StylusEvent::Sample { sample, phase, caps });
    }

    /// Empty the output queue. Callers process the returned events in FIFO order.
    pub fn drain(&mut self) -> std::collections::vec_deque::Drain<'_, StylusEvent> {
        self.queue.drain(..)
    }

    /// Called when the app loses focus (`NSWindowDidResignKey` or
    /// `NSApplicationDidResignActive`). Any active strokes get synthesized
    /// Cancel events so downstream tears them down instead of leaving
    /// half-drawn lines stranded.
    #[cfg_attr(not(all(feature = "mac", any(target_os = "macos", test))), allow(dead_code))]
    pub(crate) fn on_focus_lost(&mut self) {
        // Cancel mouse stroke.
        if self.mouse_down {
            self.mouse_down = false;
            let p = self.cursor_position.unwrap_or(Point::ZERO);
            self.emit_mouse(p, Phase::Cancel);
        }

        // Cancel any active pen strokes.
        if let Some(active_pid) = self.active_pen_pointer.take() {
            let ts = self.current_duration();
            for pen in self.pens.values_mut() {
                if pen.active_pointer_id == Some(active_pid) {
                    pen.active_pointer_id = None;
                    let position = pen.last_position.unwrap_or(Point::ZERO);
                    let caps = pen.caps;
                    let sample = Sample::new_pen_placeholder(position, ts, active_pid, pen.tool);
                    self.queue.push_back(StylusEvent::Sample {
                        sample,
                        phase: Phase::Cancel,
                        caps,
                    });
                    let _ = self.take_pending_for_pointer(active_pid);
                    break;
                }
            }
        }

        // Cancel touches.
        let ts = self.current_duration();
        let position = self.cursor_position.unwrap_or(Point::ZERO);
        let touches: Vec<PointerId> = self.touches.drain().map(|(_, pid)| pid).collect();
        for pid in touches {
            let sample = Sample::finger(position, ts, pid, None);
            self.queue.push_back(StylusEvent::Sample {
                sample,
                phase: Phase::Cancel,
                caps: ToolCaps::empty(),
            });
        }

        // Any pending revisions are moot once strokes are cancelled.
        self.pending_estimated.clear();
    }

    // --- helpers ---

    pub(crate) fn current_duration(&self) -> Duration {
        Instant::now().saturating_duration_since(self.epoch)
    }

    /// Remove and return every pending-estimate `update_index` belonging to
    /// `pid`. Mac inserts at most one entry per stroke (typical return: 0 or 1
    /// elements); iOS may insert multiple per stroke (one per estimated axis).
    /// Used wherever a stroke terminates or advances past its Estimated Down.
    pub(crate) fn take_pending_for_pointer(&mut self, pid: PointerId) -> Vec<u64> {
        let keys: Vec<u64> =
            self.pending_estimated.iter().filter_map(|(k, v)| (*v == pid).then_some(*k)).collect();
        for k in &keys {
            self.pending_estimated.remove(k);
        }
        keys
    }

    fn timestamp(&self) -> Duration {
        self.current_duration()
    }

    fn emit_mouse(&mut self, position: Point, phase: Phase) {
        let sample = Sample::mouse(position, self.timestamp(), PointerId::MOUSE);
        self.queue.push_back(StylusEvent::Sample { sample, phase, caps: ToolCaps::empty() });
    }
}

/// Which mouse button triggered an `on_mouse_button` call. Only `Left` is
/// routed into the stylus event stream today — barrel/secondary buttons on a
/// pen ride on `StylusButtons` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Other(u16),
}

/// Pressed/released state for `on_mouse_button`. Kept separate from
/// `crate::Phase` because mouse buttons don't have a hover concept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonState {
    Pressed,
    Released,
}

/// Touch-phase transition for the primitive `on_touch` entry point. Mirrors
/// the shape platform touch APIs converge on — started → moved → ended, with
/// cancelled as a separate terminal state for "OS took the gesture away"
/// (e.g., a swipe-in from a screen edge during a stroke).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    Started,
    Moved,
    Ended,
    Cancelled,
}

/// One touch point's delivery. `id` identifies this finger across the
/// gesture; the adapter maps it to a stable `PointerId`. `force` is 0..=1 if
/// the platform reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchEvent {
    pub id: u64,
    pub phase: TouchPhase,
    pub position: Point,
    pub force: Option<f32>,
}

impl Default for StylusAdapter {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn alloc_pointer_id(counter: &mut u64) -> PointerId {
    let id = *counter;
    *counter = counter.wrapping_add(1);
    PointerId(id)
}

// Re-export platform types at the adapter level so backend files
// (`crate::macos_tablet`, `crate::windows_tablet`, …) don't need to reach
// into `adapter::mac` etc. Gated on real target-os so a host test build
// (which compiles all platform submodules via the `, test` gate) doesn't
// flag these as unused — the backend file that consumes them is only
// compiled on its native target.
#[cfg(all(feature = "android", target_os = "android"))]
pub(crate) use android::{AndroidProximitySample, AndroidRawSample, AndroidSourcePhase};
#[cfg(all(feature = "mac", target_os = "macos"))]
pub(crate) use mac::{
    MacTabletOrigin, MacTabletPhase, MacTabletProximitySample, MacTabletRawSample,
};
#[cfg(all(feature = "wayland", target_os = "linux"))]
pub(crate) use wayland::{WaylandProximitySample, WaylandRawSample, WaylandTabletPhase};
#[cfg(all(feature = "web", target_arch = "wasm32"))]
pub(crate) use web::{WebPointerType, WebProximitySample, WebRawSample, WebSourcePhase};
#[cfg(all(feature = "windows", target_os = "windows"))]
pub(crate) use windows::{WindowsPointerPhase, WindowsProximitySample, WindowsRawSample};
#[cfg(all(feature = "x11", target_os = "linux"))]
pub(crate) use x11::{X11ProximitySample, X11RawSample, X11TabletPhase};

// iOS ships as a stub backend today that doesn't reference the adapter
// types directly; re-export behind the same real-target gate for when the
// UIView body lands.
#[cfg(all(feature = "ios", target_os = "ios"))]
pub(crate) use ios::{
    IosEstimatedProperties, IosTouchPhase, IosTouchProximitySample, IosTouchRawSample,
};
