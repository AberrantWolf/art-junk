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

use aj_core::{PointerId, Sample, ToolCaps, ToolKind};
use kurbo::Point;
use winit::event::{ElementState, Force, MouseButton, Touch, TouchPhase, WindowEvent};

use crate::{Phase, StylusEvent};

#[cfg(any(target_os = "macos", test))]
pub(crate) mod mac;

#[cfg(any(target_os = "linux", test))]
pub(crate) mod wayland;

#[cfg(any(target_os = "linux", test))]
pub(crate) mod x11;

#[cfg(any(target_os = "windows", test))]
pub(crate) mod windows;

#[cfg(any(target_arch = "wasm32", test))]
pub(crate) mod web;

#[cfg(any(target_os = "ios", test))]
pub(crate) mod ios;

#[cfg(any(target_os = "android", test))]
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
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    next_update_index: u64,
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    mac_anchor: Option<PlatformTimestampAnchor>,
    #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
    wayland_anchor: Option<PlatformTimestampAnchor>,
    #[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
    windows_anchor: Option<PlatformTimestampAnchor>,
    #[cfg(any(target_arch = "wasm32", test))]
    web_anchor: Option<PlatformTimestampAnchor>,
    /// Per-gesture map from JS `pointerId` (stable only within a single
    /// Pointer-Events gesture) to the adapter's `PointerId`. Down allocates,
    /// Up / Cancel / Leave removes.
    #[cfg(any(target_arch = "wasm32", test))]
    web_pointers: HashMap<i32, web::WebPointerState>,
    #[cfg_attr(not(any(target_os = "ios", test)), allow(dead_code))]
    ios_anchor: Option<PlatformTimestampAnchor>,
    /// Per-iOS-touch map from the `UITouch` identity (hashed) to the
    /// adapter's `PointerId`. iOS pencil gestures have their own id space
    /// that survives coalesced/predicted replays of the same touch.
    #[cfg(any(target_os = "ios", test))]
    ios_pointers: HashMap<u64, PointerId>,
    #[cfg_attr(not(any(target_os = "android", test)), allow(dead_code))]
    android_anchor: Option<PlatformTimestampAnchor>,
    /// Map from Android `MotionEvent` pointer id (distinct per pointer in
    /// a multi-touch gesture) to the adapter's `PointerId`. Android reuses
    /// pointer ids within a gesture; adapter allocates fresh on first
    /// sight, clears on Up/Cancel.
    #[cfg(any(target_os = "android", test))]
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
    first_platform_secs: f64,
    adapter_duration_at_first: Duration,
}

impl PlatformTimestampAnchor {
    /// Translate `platform_secs` to adapter-timeline `Duration`, anchoring on
    /// the first call if `slot` is empty. `adapter_epoch` is `StylusAdapter::epoch`.
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
            #[cfg(any(target_arch = "wasm32", test))]
            web_anchor: None,
            #[cfg(any(target_arch = "wasm32", test))]
            web_pointers: HashMap::new(),
            ios_anchor: None,
            #[cfg(any(target_os = "ios", test))]
            ios_pointers: HashMap::new(),
            android_anchor: None,
            #[cfg(any(target_os = "android", test))]
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

    /// Translate one winit event into zero or more queued `StylusEvent`s.
    pub fn on_window_event(&mut self, event: &WindowEvent) {
        // If a pen is actively driving the pointer, the corresponding mouse
        // events winit emits are duplicates of the richer pen samples we've
        // already queued — drop them. Still track cursor position because
        // we don't want the winit-path's view of it to go stale if the pen
        // stops driving and the mouse resumes.
        if self.active_pen_pointer.is_some() {
            if let WindowEvent::CursorMoved { position, .. } = event {
                self.cursor_position = Some(Point::new(position.x, position.y));
            }
            return;
        }

        match event {
            WindowEvent::CursorMoved { position, .. } => {
                let p = Point::new(position.x, position.y);
                self.cursor_position = Some(p);
                if self.mouse_down {
                    self.emit_mouse(p, Phase::Move);
                }
            }
            WindowEvent::MouseInput { button: MouseButton::Left, state, .. } => match state {
                ElementState::Pressed => {
                    if !self.mouse_down
                        && let Some(p) = self.cursor_position
                    {
                        self.mouse_down = true;
                        self.emit_mouse(p, Phase::Down);
                    }
                }
                ElementState::Released => {
                    if self.mouse_down {
                        self.mouse_down = false;
                        let p = self.cursor_position.unwrap_or(Point::ZERO);
                        self.emit_mouse(p, Phase::Up);
                    }
                }
            },
            // CursorLeft is intentionally a no-op: if the user drags off the
            // window mid-stroke, we want the stroke to keep going until the real
            // Released event arrives. All desktop OSes deliver a Released event
            // for the eventual button-up even when the cursor is outside the
            // window, via the implicit capture set up by the Pressed on entry.
            WindowEvent::Touch(touch) => {
                self.handle_touch(touch);
            }
            _ => {}
        }
    }

    /// Empty the output queue. Callers process the returned events in FIFO order.
    pub fn drain(&mut self) -> std::collections::vec_deque::Drain<'_, StylusEvent> {
        self.queue.drain(..)
    }

    /// Called when the app loses focus (`NSWindowDidResignKey` or
    /// `NSApplicationDidResignActive`). Any active strokes get synthesized
    /// Cancel events so downstream tears them down instead of leaving
    /// half-drawn lines stranded.
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
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

    fn handle_touch(&mut self, touch: &Touch) {
        let position = Point::new(touch.location.x, touch.location.y);
        let force = touch.force.map(normalize_force);
        let caps = if force.is_some() { ToolCaps::PRESSURE } else { ToolCaps::empty() };
        let ts = self.timestamp();

        let (pointer_id, phase) = match touch.phase {
            TouchPhase::Started => {
                let id = alloc_pointer_id(&mut self.next_pointer_id);
                self.touches.insert(touch.id, id);
                (id, Phase::Down)
            }
            TouchPhase::Moved => {
                let Some(&id) = self.touches.get(&touch.id) else {
                    return;
                };
                (id, Phase::Move)
            }
            TouchPhase::Ended => {
                let Some(id) = self.touches.remove(&touch.id) else { return };
                (id, Phase::Up)
            }
            TouchPhase::Cancelled => {
                let Some(id) = self.touches.remove(&touch.id) else { return };
                (id, Phase::Cancel)
            }
        };

        let sample = Sample::finger(position, ts, pointer_id, force);
        self.queue.push_back(StylusEvent::Sample { sample, phase, caps });
    }
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

// Re-export platform types at the adapter level so backend files
// (`crate::macos_tablet`, `crate::windows_tablet`, …) don't need to reach
// into `adapter::mac` etc. Gated on real target-os so a host test build
// (which compiles all platform submodules via the `, test` gate) doesn't
// flag these as unused — the backend file that consumes them is only
// compiled on its native target.
#[cfg(target_os = "android")]
pub(crate) use android::{AndroidProximitySample, AndroidRawSample, AndroidSourcePhase};
#[cfg(target_os = "macos")]
pub(crate) use mac::{
    MacTabletOrigin, MacTabletPhase, MacTabletProximitySample, MacTabletRawSample,
};
#[cfg(target_os = "linux")]
pub(crate) use wayland::{WaylandProximitySample, WaylandRawSample, WaylandTabletPhase};
#[cfg(target_arch = "wasm32")]
pub(crate) use web::{WebPointerType, WebProximitySample, WebRawSample, WebSourcePhase};
#[cfg(target_os = "windows")]
pub(crate) use windows::{WindowsPointerPhase, WindowsProximitySample, WindowsRawSample};
#[cfg(target_os = "linux")]
pub(crate) use x11::{X11ProximitySample, X11RawSample, X11TabletPhase};

// iOS ships as a stub backend today that doesn't reference the adapter
// types directly; re-export behind the same real-target gate for when the
// UIView body lands.
#[cfg(target_os = "ios")]
pub(crate) use ios::{
    IosEstimatedProperties, IosTouchPhase, IosTouchProximitySample, IosTouchRawSample,
};
