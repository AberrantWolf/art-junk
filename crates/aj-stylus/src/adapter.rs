//! Stateful input translator. Accepts low-level events from winit (mouse +
//! touch) and, on macOS, from an `NSEvent`-based tablet backend; emits a
//! unified stream of `StylusEvent`s.
//!
//! Internal state:
//!
//! - a monotonic `PointerId` counter so each new touch or pen gets a stable id
//!   across the lifespan of one gesture,
//! - `mouse_down` + last-known cursor position, because winit separates
//!   `MouseInput` (no position) from `CursorMoved` (no button),
//! - a `touches` map from winit's `u64` finger id to our `PointerId`,
//! - a `pens` map keyed by macOS `deviceID` with caps and the current
//!   pointer id for each physical stylus, plus `active_pen_pointer` which
//!   gates the winit mouse path so a pen-driven mouse event doesn't produce
//!   a duplicate `StylusEvent::Sample`,
//! - `pending_estimated`, keyed by the `update_index` that was emitted with an
//!   `Estimated` sample, mapping to the `PointerId` whose stroke carries it.
//!   Keyed on `update_index` (not pointer) so iOS Pencil's out-of-order
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

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use aj_core::{
    PointerId, Sample, SampleClass, SampleRevision, StylusButtons, Tilt, ToolCaps, ToolKind,
};
use kurbo::Point;
use winit::event::{ElementState, Force, MouseButton, Touch, TouchPhase, WindowEvent};

use crate::{Phase, StylusEvent};

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
    next_update_index: u64,
    mac_anchor: Option<PlatformTimestampAnchor>,
    #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
    wayland_anchor: Option<PlatformTimestampAnchor>,
}

/// Per-stylus state, keyed by `NSEvent` `deviceID`. Learned from proximity
/// events where possible, synthesized optimistically on the first stroke if no
/// proximity was seen (app launched with pen already hovering).
pub(crate) struct PenState {
    pub(crate) active_pointer_id: Option<PointerId>,
    pub(crate) caps: ToolCaps,
    pub(crate) tool: ToolKind,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MacTabletOrigin {
    /// Rode on a regular mouse event (`LeftMouseDown/Up/Dragged`) with
    /// `subtype == .tabletPoint`.
    MouseSubtype,
    /// Rode on a native `NSEventTypeTabletPoint` (the exceptional path —
    /// fires between stylus-down and first drag, or during multi-tool use).
    NativeTabletPoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MacTabletPhase {
    Down,
    Move,
    Up,
}

/// Raw sample supplied by the macOS `NSEvent` backend (or a test harness).
/// The adapter owns timestamp translation, pointer-id allocation, and revision
/// emission — so this type is the stable seam between backend and adapter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MacTabletRawSample {
    pub position_physical_px: Point,
    /// `NSEvent.timestamp` — seconds since system boot, monotonic.
    pub timestamp_secs: f64,
    pub pressure: f32,
    pub tilt: Tilt,
    pub twist_deg: f32,
    pub tangential_pressure: f32,
    pub button_mask: u32,
    pub device_id: u32,
    pub pointing_device_type: ToolKind,
    pub origin: MacTabletOrigin,
    pub source_phase: MacTabletPhase,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MacTabletProximitySample {
    pub device_id: u32,
    pub unique_id: Option<u64>,
    pub pointing_device_type: ToolKind,
    /// Capabilities already translated from `NSEvent`'s `capabilityMask` bits
    /// into `ToolCaps`. The `NSEvent` bit-to-`ToolCaps` mapping lives in the
    /// macOS backend so the adapter stays platform-agnostic.
    pub caps: ToolCaps,
    pub is_entering: bool,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaylandTabletPhase {
    Down,
    Move,
    Up,
}

/// One hardware sample, already frame-boundary-accumulated by the Wayland
/// backend. The `frame` event is the commit boundary in tablet-v2, so the
/// backend collapses all axis events between two `frame`s into a single
/// `WaylandRawSample`. Pressure arrives pre-normalized to `0..=1` so the
/// adapter stays free of Wayland axis-range magic numbers.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WaylandRawSample {
    pub position_physical_px: Point,
    /// Frame timestamp — the compositor's millisecond `frame.time` divided to
    /// seconds. Relative-ordering only, monotonic within a backend session.
    pub timestamp_secs: f64,
    pub pressure: f32,
    pub tilt: Option<Tilt>,
    pub twist_deg: Option<f32>,
    pub tangential_pressure: Option<f32>,
    pub button_mask: u32,
    pub device_id: u32,
    pub hardware_serial: Option<u64>,
    pub pointing_device_type: ToolKind,
    pub source_phase: WaylandTabletPhase,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WaylandProximitySample {
    pub device_id: u32,
    pub hardware_serial: Option<u64>,
    pub pointing_device_type: ToolKind,
    pub caps: ToolCaps,
    pub is_entering: bool,
}

/// Optimistic cap set used when a pen event arrives before any proximity —
/// typical if the app launches with pen already hovering. A cursor-puck would
/// over-claim tilt/twist under this default; survivable, and proximity
/// corrects it as soon as the pen moves out and back in.
const OPTIMISTIC_PEN_CAPS: ToolCaps = ToolCaps::PRESSURE
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

    // --- macOS tablet seam ---
    //
    // Three `pub(crate)` entry points let the real NSEvent backend and the
    // tests drive the same adapter code. The NSEvent-specific translation
    // (objc2 calls, coordinate flip, capability-mask decoding) lives in
    // `macos_tablet.rs`; the adapter only sees already-translated raw samples.

    pub(crate) fn handle_mac_raw(&mut self, raw: MacTabletRawSample) {
        let ts = self.translate_mac_timestamp(raw.timestamp_secs);

        // Snapshot pen state in a narrow scope so the `&mut self.pens` borrow
        // is released before we call helpers like `take_pending_for_pointer`
        // that need `&mut self`.
        let (caps, tool, active_pid) = {
            let pen = self.pens.entry(raw.device_id).or_insert_with(|| PenState {
                active_pointer_id: None,
                caps: OPTIMISTIC_PEN_CAPS,
                tool: raw.pointing_device_type,
                unique_id: None,
                last_position: None,
            });
            pen.last_position = Some(raw.position_physical_px);
            (pen.caps, pen.tool, pen.active_pointer_id)
        };

        match raw.source_phase {
            MacTabletPhase::Down => {
                let pid = alloc_pointer_id(&mut self.next_pointer_id);
                if let Some(pen) = self.pens.get_mut(&raw.device_id) {
                    pen.active_pointer_id = Some(pid);
                }
                let update_index = self.next_update_index;
                self.next_update_index = self.next_update_index.wrapping_add(1);

                let mut sample = build_pen_sample(&raw, ts, pid, tool);
                sample.class = SampleClass::Estimated { update_index };
                self.pending_estimated.insert(update_index, pid);
                self.active_pen_pointer = Some(pid);
                self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Down, caps });
            }
            MacTabletPhase::Move | MacTabletPhase::Up => {
                let Some(pid) = active_pid else {
                    // Move or Up without a preceding Down for this pen; drop.
                    // Can happen if the adapter was constructed after the
                    // pen was already pressed (very unlikely in practice).
                    return;
                };

                // If a revision is pending, refine the earlier Estimated sample
                // with whatever fresher axis data this event carries. Mac emits
                // at most one pending estimate per stroke; the iteration stays
                // correct for platforms that emit several.
                for update_index in self.take_pending_for_pointer(pid) {
                    let revision = SampleRevision {
                        pressure: Some(raw.pressure),
                        tilt: Some(raw.tilt),
                        twist_deg: Some(raw.twist_deg),
                        tangential_pressure: Some(raw.tangential_pressure),
                    };
                    self.queue.push_back(StylusEvent::Revise {
                        pointer_id: pid,
                        update_index,
                        revision,
                    });
                }

                // Native `NSTabletPoint` events are supplemental — Apple's docs
                // describe them firing *between* a mouse-down and the first
                // drag, and during multi-tool scenarios. In practice some
                // drivers (including Wacom) interleave them with
                // `LeftMouseDragged` events at the same physical instant,
                // which produces duplicate samples and visible zig-zags at
                // integer-pixel boundaries. Treat them as revise-only: they
                // refine a pending Estimated Down but don't emit new Move/Up
                // samples. The mouse-subtype path is authoritative for
                // position flow.
                if matches!(raw.origin, MacTabletOrigin::NativeTabletPoint) {
                    return;
                }

                let phase = match raw.source_phase {
                    MacTabletPhase::Move => Phase::Move,
                    MacTabletPhase::Up => Phase::Up,
                    MacTabletPhase::Down => unreachable!(),
                };
                let sample = build_pen_sample(&raw, ts, pid, tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase, caps });

                if matches!(raw.source_phase, MacTabletPhase::Up) {
                    if let Some(pen) = self.pens.get_mut(&raw.device_id) {
                        pen.active_pointer_id = None;
                    }
                    if self.active_pen_pointer == Some(pid) {
                        self.active_pen_pointer = None;
                    }
                }
            }
        }
    }

    pub(crate) fn handle_mac_proximity(&mut self, prox: MacTabletProximitySample) {
        if prox.is_entering {
            // Refresh or insert — preserves active_pointer_id / last_position
            // if we already have state for this physical stylus.
            let entry = self.pens.entry(prox.device_id).or_insert(PenState {
                active_pointer_id: None,
                caps: prox.caps,
                tool: prox.pointing_device_type,
                unique_id: prox.unique_id,
                last_position: None,
            });
            entry.caps = prox.caps;
            entry.tool = prox.pointing_device_type;
            entry.unique_id = prox.unique_id;
        } else if let Some(mut pen) = self.pens.remove(&prox.device_id)
            && let Some(pid) = pen.active_pointer_id.take()
        {
            // Proximity-out with still-active stroke — Up was lost somehow.
            // Synthesize a Cancel so the app can tear the stroke down cleanly.
            let ts = self.current_duration();
            let mut sample = Sample::new_pen_placeholder(
                pen.last_position.unwrap_or(Point::ZERO),
                ts,
                pid,
                pen.tool,
            );
            // Keep caps=pen.caps on the cancel so downstream sees consistent
            // data even as the stroke tears down.
            let caps = pen.caps;
            sample.tool = pen.tool;
            self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Cancel, caps });
            let _ = self.take_pending_for_pointer(pid);
            if self.active_pen_pointer == Some(pid) {
                self.active_pen_pointer = None;
            }
        }
    }

    // --- Wayland tablet-v2 seam ---
    //
    // Wayland delivers fully-resolved samples at frame boundaries — no
    // Estimated/Revise cycle is necessary. The backend thread on its separate
    // `EventQueue` accumulates axis events between `frame` events and pushes
    // one `WaylandRawSample` per commit.

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn handle_wayland_raw(&mut self, raw: WaylandRawSample) {
        let ts = PlatformTimestampAnchor::translate_or_anchor(
            &mut self.wayland_anchor,
            raw.timestamp_secs,
            self.epoch,
        );

        let (caps, tool, active_pid) = {
            let pen = self.pens.entry(raw.device_id).or_insert_with(|| PenState {
                active_pointer_id: None,
                caps: OPTIMISTIC_PEN_CAPS,
                tool: raw.pointing_device_type,
                unique_id: raw.hardware_serial,
                last_position: None,
            });
            pen.last_position = Some(raw.position_physical_px);
            (pen.caps, pen.tool, pen.active_pointer_id)
        };

        match raw.source_phase {
            WaylandTabletPhase::Down => {
                let pid = alloc_pointer_id(&mut self.next_pointer_id);
                if let Some(pen) = self.pens.get_mut(&raw.device_id) {
                    pen.active_pointer_id = Some(pid);
                }
                self.active_pen_pointer = Some(pid);
                let sample = build_wayland_sample(&raw, ts, pid, tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Down, caps });
            }
            WaylandTabletPhase::Move | WaylandTabletPhase::Up => {
                let Some(pid) = active_pid else {
                    return;
                };
                let phase = match raw.source_phase {
                    WaylandTabletPhase::Move => Phase::Move,
                    WaylandTabletPhase::Up => Phase::Up,
                    WaylandTabletPhase::Down => unreachable!(),
                };
                let sample = build_wayland_sample(&raw, ts, pid, tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase, caps });

                if matches!(raw.source_phase, WaylandTabletPhase::Up) {
                    if let Some(pen) = self.pens.get_mut(&raw.device_id) {
                        pen.active_pointer_id = None;
                    }
                    if self.active_pen_pointer == Some(pid) {
                        self.active_pen_pointer = None;
                    }
                }
            }
        }
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn handle_wayland_proximity(&mut self, prox: WaylandProximitySample) {
        if prox.is_entering {
            let entry = self.pens.entry(prox.device_id).or_insert(PenState {
                active_pointer_id: None,
                caps: prox.caps,
                tool: prox.pointing_device_type,
                unique_id: prox.hardware_serial,
                last_position: None,
            });
            entry.caps = prox.caps;
            entry.tool = prox.pointing_device_type;
            entry.unique_id = prox.hardware_serial;
        } else if let Some(mut pen) = self.pens.remove(&prox.device_id)
            && let Some(pid) = pen.active_pointer_id.take()
        {
            // Proximity-out without a preceding Up — user lifted above hover
            // range mid-stroke. Synthesize Cancel so downstream tears the
            // stroke down cleanly.
            let ts = self.current_duration();
            let mut sample = Sample::new_pen_placeholder(
                pen.last_position.unwrap_or(Point::ZERO),
                ts,
                pid,
                pen.tool,
            );
            let caps = pen.caps;
            sample.tool = pen.tool;
            self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Cancel, caps });
            let _ = self.take_pending_for_pointer(pid);
            if self.active_pen_pointer == Some(pid) {
                self.active_pen_pointer = None;
            }
        }
    }

    /// Called when the app loses focus (`NSWindowDidResignKey` or
    /// `NSApplicationDidResignActive`). Any active strokes get synthesized
    /// Cancel events so downstream tears them down instead of leaving
    /// half-drawn lines stranded.
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

    fn current_duration(&self) -> Duration {
        Instant::now().saturating_duration_since(self.epoch)
    }

    fn translate_mac_timestamp(&mut self, nsevent_secs: f64) -> Duration {
        PlatformTimestampAnchor::translate_or_anchor(&mut self.mac_anchor, nsevent_secs, self.epoch)
    }

    /// Remove and return every pending-estimate `update_index` belonging to
    /// `pid`. Mac inserts at most one entry per stroke (typical return: 0 or 1
    /// elements); iOS may insert multiple per stroke (one per estimated axis).
    /// Used wherever a stroke terminates or advances past its Estimated Down.
    fn take_pending_for_pointer(&mut self, pid: PointerId) -> Vec<u64> {
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

fn alloc_pointer_id(counter: &mut u64) -> PointerId {
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

fn build_pen_sample(
    raw: &MacTabletRawSample,
    timestamp: Duration,
    pointer_id: PointerId,
    tool: ToolKind,
) -> Sample {
    let mut buttons = StylusButtons::CONTACT;
    if raw.button_mask & 0x1 != 0 {
        buttons |= StylusButtons::BARREL;
    }
    if raw.button_mask & 0x2 != 0 {
        buttons |= StylusButtons::SECONDARY;
    }
    if matches!(tool, ToolKind::Eraser) {
        buttons |= StylusButtons::INVERTED;
    }
    let mut sample = Sample::new_pen(raw.position_physical_px, timestamp, pointer_id, tool);
    sample.pressure = raw.pressure.clamp(0.0, 1.0);
    sample.tilt = Some(raw.tilt);
    sample.twist_deg = Some(raw.twist_deg);
    sample.tangential_pressure = Some(raw.tangential_pressure);
    sample.buttons = buttons;
    sample
}

#[cfg(any(target_os = "linux", test))]
fn build_wayland_sample(
    raw: &WaylandRawSample,
    timestamp: Duration,
    pointer_id: PointerId,
    tool: ToolKind,
) -> Sample {
    // Wayland's per-button bitmask is Linux `BTN_*` codes collapsed by the
    // backend into bit 0 = primary barrel, bit 1 = secondary — mirror the
    // mac layout so downstream stays platform-agnostic.
    let mut buttons = StylusButtons::CONTACT;
    if raw.button_mask & 0x1 != 0 {
        buttons |= StylusButtons::BARREL;
    }
    if raw.button_mask & 0x2 != 0 {
        buttons |= StylusButtons::SECONDARY;
    }
    if matches!(tool, ToolKind::Eraser) {
        buttons |= StylusButtons::INVERTED;
    }
    let mut sample = Sample::new_pen(raw.position_physical_px, timestamp, pointer_id, tool);
    sample.pressure = raw.pressure.clamp(0.0, 1.0);
    sample.tilt = raw.tilt;
    sample.twist_deg = raw.twist_deg;
    sample.tangential_pressure = raw.tangential_pressure;
    sample.buttons = buttons;
    sample
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalPosition;
    use winit::event::DeviceId;

    fn adapter() -> StylusAdapter {
        // Don't pre-seed the mac epoch: the real flow populates it on first
        // handle_mac_raw, and exercising that path is part of what the tests
        // cover.
        StylusAdapter::new()
    }

    fn raw(
        device_id: u32,
        phase: MacTabletPhase,
        pos: (f64, f64),
        ts: f64,
        pressure: f32,
    ) -> MacTabletRawSample {
        raw_with_origin(device_id, phase, pos, ts, pressure, MacTabletOrigin::MouseSubtype)
    }

    fn raw_with_origin(
        device_id: u32,
        phase: MacTabletPhase,
        pos: (f64, f64),
        ts: f64,
        pressure: f32,
        origin: MacTabletOrigin,
    ) -> MacTabletRawSample {
        MacTabletRawSample {
            position_physical_px: Point::new(pos.0, pos.1),
            timestamp_secs: ts,
            pressure,
            tilt: Tilt { x_deg: 0.0, y_deg: 0.0 },
            twist_deg: 0.0,
            tangential_pressure: 0.0,
            button_mask: 0,
            device_id,
            pointing_device_type: ToolKind::Pen,
            origin,
            source_phase: phase,
        }
    }

    fn proximity(
        device_id: u32,
        tool: ToolKind,
        caps: ToolCaps,
        is_entering: bool,
    ) -> MacTabletProximitySample {
        MacTabletProximitySample {
            device_id,
            unique_id: Some(42),
            pointing_device_type: tool,
            caps,
            is_entering,
        }
    }

    fn drained(a: &mut StylusAdapter) -> Vec<StylusEvent> {
        a.drain().collect()
    }

    fn expect_sample(ev: &StylusEvent) -> (&Sample, Phase, ToolCaps) {
        match ev {
            StylusEvent::Sample { sample, phase, caps } => (sample, *phase, *caps),
            other => panic!("expected Sample, got {other:?}"),
        }
    }

    #[test]
    fn pen_down_emits_estimated_sample_with_update_index() {
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, ToolCaps::PRESSURE, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (100.0, 200.0), 0.0, 0.1));

        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (sample, phase, caps) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Down);
        assert_eq!(sample.tool, ToolKind::Pen);
        assert!(matches!(sample.class, SampleClass::Estimated { .. }));
        assert!(caps.contains(ToolCaps::PRESSURE));
        assert!(a.active_pen_pointer.is_some());
    }

    #[test]
    fn pen_follow_up_sample_emits_revise_then_move() {
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (10.0, 10.0), 0.0, 0.0));
        let _ = drained(&mut a);

        a.handle_mac_raw(raw(1, MacTabletPhase::Move, (11.0, 11.0), 0.01, 0.8));
        let events = drained(&mut a);
        assert_eq!(events.len(), 2, "revise + move");

        match &events[0] {
            StylusEvent::Revise { revision, .. } => {
                assert!((revision.pressure.unwrap() - 0.8).abs() < f32::EPSILON);
            }
            other => panic!("expected Revise first, got {other:?}"),
        }
        let (sample, phase, _) = expect_sample(&events[1]);
        assert_eq!(phase, Phase::Move);
        assert_eq!(sample.class, SampleClass::Committed);
    }

    #[test]
    fn pen_subsequent_moves_emit_committed_only() {
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (0.0, 0.0), 0.0, 0.2));
        a.handle_mac_raw(raw(1, MacTabletPhase::Move, (1.0, 1.0), 0.01, 0.5));
        let _ = drained(&mut a);

        a.handle_mac_raw(raw(1, MacTabletPhase::Move, (2.0, 2.0), 0.02, 0.6));
        let events = drained(&mut a);
        assert_eq!(events.len(), 1, "no revise after pending cleared");
        let (sample, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Move);
        assert_eq!(sample.class, SampleClass::Committed);
    }

    #[test]
    fn pen_up_clears_active_pen_and_pending_estimated() {
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (0.0, 0.0), 0.0, 0.2));
        let _ = drained(&mut a);

        a.handle_mac_raw(raw(1, MacTabletPhase::Up, (0.0, 0.0), 0.05, 0.7));
        let _ = drained(&mut a);

        assert!(a.active_pen_pointer.is_none());
        assert!(a.pending_estimated.is_empty());
    }

    #[test]
    fn first_sample_without_prior_proximity_uses_optimistic_caps() {
        let mut a = adapter();
        // Skip proximity — simulate app launched with pen already hovering.
        a.handle_mac_raw(raw(5, MacTabletPhase::Down, (0.0, 0.0), 0.0, 0.3));

        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (_, _, caps) = expect_sample(&events[0]);
        assert!(caps.contains(ToolCaps::PRESSURE));
        assert!(caps.contains(ToolCaps::TILT));
        assert!(caps.contains(ToolCaps::TWIST));
        assert!(caps.contains(ToolCaps::TANGENTIAL_PRESSURE));
    }

    #[test]
    fn proximity_refreshes_caps_on_subsequent_strokes() {
        let mut a = adapter();
        a.handle_mac_raw(raw(7, MacTabletPhase::Down, (0.0, 0.0), 0.0, 0.3));
        a.handle_mac_raw(raw(7, MacTabletPhase::Up, (0.0, 0.0), 0.01, 0.3));
        let _ = drained(&mut a);

        // Proximity arrives with a stricter (no-TILT) cap set.
        a.handle_mac_proximity(proximity(7, ToolKind::Pen, ToolCaps::PRESSURE, true));
        a.handle_mac_raw(raw(7, MacTabletPhase::Down, (0.0, 0.0), 0.02, 0.4));
        let events = drained(&mut a);
        let (_, _, caps) = expect_sample(&events[0]);
        assert!(caps.contains(ToolCaps::PRESSURE));
        assert!(!caps.contains(ToolCaps::TILT), "second stroke uses real caps");
    }

    #[test]
    fn pen_eraser_flip_via_proximity_changes_tool_kind() {
        let mut a = adapter();
        a.handle_mac_proximity(proximity(3, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_proximity(proximity(3, ToolKind::Eraser, OPTIMISTIC_PEN_CAPS, true));

        a.handle_mac_raw(raw(3, MacTabletPhase::Down, (0.0, 0.0), 0.0, 0.5));
        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert_eq!(sample.tool, ToolKind::Eraser);
        assert!(sample.buttons.contains(StylusButtons::INVERTED));
    }

    #[test]
    fn active_pen_suppresses_winit_mouse_events() {
        use winit::event::{ElementState, MouseButton};
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (0.0, 0.0), 0.0, 0.3));
        let _ = drained(&mut a);

        // Synthesize a winit MouseInput(Pressed) that *would* arrive from the
        // same physical pen press (tablet driver sends both).
        a.on_window_event(&WindowEvent::MouseInput {
            device_id: DeviceId::dummy(),
            state: ElementState::Pressed,
            button: MouseButton::Left,
        });
        a.on_window_event(&WindowEvent::CursorMoved {
            device_id: DeviceId::dummy(),
            position: PhysicalPosition::new(1.0, 1.0),
        });
        assert!(drained(&mut a).is_empty(), "mouse events must be suppressed during pen stroke");
    }

    #[test]
    fn focus_loss_cancels_active_pen_stroke() {
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (10.0, 10.0), 0.0, 0.3));
        let _ = drained(&mut a);

        a.on_focus_lost();

        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
        assert!(a.active_pen_pointer.is_none());
        assert!(a.pending_estimated.is_empty());
    }

    #[test]
    fn focus_loss_cancels_mouse_and_touches() {
        let mut a = adapter();
        a.on_window_event(&WindowEvent::CursorMoved {
            device_id: DeviceId::dummy(),
            position: PhysicalPosition::new(5.0, 5.0),
        });
        a.on_window_event(&WindowEvent::MouseInput {
            device_id: DeviceId::dummy(),
            state: winit::event::ElementState::Pressed,
            button: MouseButton::Left,
        });
        a.on_window_event(&WindowEvent::Touch(Touch {
            device_id: DeviceId::dummy(),
            phase: TouchPhase::Started,
            location: PhysicalPosition::new(1.0, 1.0),
            force: None,
            id: 7,
        }));
        let _ = drained(&mut a);

        a.on_focus_lost();

        let events = drained(&mut a);
        let phases: Vec<Phase> = events.iter().map(|e| expect_sample(e).1).collect();
        assert!(phases.iter().all(|p| *p == Phase::Cancel));
        assert_eq!(phases.len(), 2, "one cancel per active pointer");
        assert!(!a.mouse_down);
        assert!(a.touches.is_empty());
    }

    #[test]
    fn proximity_out_with_active_pen_synthesizes_cancel() {
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (0.0, 0.0), 0.0, 0.3));
        let _ = drained(&mut a);

        // Pen leaves proximity while stroke is still active (Up was lost).
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, false));

        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
        assert!(a.active_pen_pointer.is_none());
        assert!(!a.pens.contains_key(&1), "pen removed on proximity-out");
    }

    #[test]
    fn mac_timestamps_are_monotonic_and_aligned_to_adapter_epoch() {
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (0.0, 0.0), 100.0, 0.3));
        a.handle_mac_raw(raw(1, MacTabletPhase::Move, (1.0, 1.0), 100.016, 0.4));
        a.handle_mac_raw(raw(1, MacTabletPhase::Up, (2.0, 2.0), 100.032, 0.4));

        let events = drained(&mut a);
        let timestamps: Vec<Duration> = events
            .iter()
            .filter_map(|e| {
                if let StylusEvent::Sample { sample, .. } = e {
                    Some(sample.timestamp)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(timestamps.len(), 3);
        for pair in timestamps.windows(2) {
            assert!(pair[0] <= pair[1]);
        }
    }

    #[test]
    fn native_tabletpoint_moves_do_not_emit_extra_sample() {
        // Some drivers (Wacom) interleave NSTabletPoint events with
        // LeftMouseDragged events at the same physical instant; treating
        // both as sample sources produces a visible zig-zag / pixel-jagged
        // stroke. Native TabletPoint should refine a pending Estimated
        // sample but never emit a new Move Sample.
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (10.0, 10.0), 0.0, 0.0));
        let _ = drained(&mut a); // drain Down

        // Native TabletPoint Move: should emit a Revise (pending from Down),
        // but no new Sample.
        a.handle_mac_raw(raw_with_origin(
            1,
            MacTabletPhase::Move,
            (10.5, 10.5),
            0.005,
            0.7,
            MacTabletOrigin::NativeTabletPoint,
        ));
        let events = drained(&mut a);
        assert_eq!(events.len(), 1, "native TabletPoint must emit only a Revise");
        match &events[0] {
            StylusEvent::Revise { revision, .. } => {
                assert!((revision.pressure.unwrap() - 0.7).abs() < f32::EPSILON);
            }
            other => panic!("expected Revise, got {other:?}"),
        }

        // Mouse-subtype Move arriving immediately after must emit a Move
        // Sample (the authoritative position source), no duplicate Revise.
        a.handle_mac_raw(raw(1, MacTabletPhase::Move, (11.0, 11.0), 0.008, 0.75));
        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Move);
    }

    #[test]
    fn native_tabletpoint_without_pending_revise_emits_nothing() {
        // After the pending Estimated has been resolved, a stray native
        // TabletPoint during the stroke should not emit anything — it's
        // neither a new position nor a revision.
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (0.0, 0.0), 0.0, 0.2));
        a.handle_mac_raw(raw(1, MacTabletPhase::Move, (1.0, 1.0), 0.01, 0.5));
        let _ = drained(&mut a);

        a.handle_mac_raw(raw_with_origin(
            1,
            MacTabletPhase::Move,
            (1.5, 1.5),
            0.012,
            0.6,
            MacTabletOrigin::NativeTabletPoint,
        ));
        assert!(drained(&mut a).is_empty(), "interleaved native TabletPoint must be silent");
    }

    // --- Wayland tablet-v2 seam ---
    //
    // Each call below stands in for one `frame` commit from the real backend;
    // the accumulation of axis events between `frame`s belongs in the backend.

    fn wl_raw(
        device_id: u32,
        phase: WaylandTabletPhase,
        pos: (f64, f64),
        ts: f64,
        pressure: f32,
    ) -> WaylandRawSample {
        WaylandRawSample {
            position_physical_px: Point::new(pos.0, pos.1),
            timestamp_secs: ts,
            pressure,
            tilt: None,
            twist_deg: None,
            tangential_pressure: None,
            button_mask: 0,
            device_id,
            hardware_serial: Some(0xABCD_1234_5678_9ABC),
            pointing_device_type: ToolKind::Pen,
            source_phase: phase,
        }
    }

    fn wl_prox(
        device_id: u32,
        tool: ToolKind,
        caps: ToolCaps,
        is_entering: bool,
    ) -> WaylandProximitySample {
        WaylandProximitySample {
            device_id,
            hardware_serial: Some(0xABCD_1234_5678_9ABC),
            pointing_device_type: tool,
            caps,
            is_entering,
        }
    }

    #[test]
    fn wayland_down_emits_committed_sample_no_estimated() {
        let mut a = adapter();
        a.handle_wayland_proximity(wl_prox(1, ToolKind::Pen, ToolCaps::PRESSURE, true));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Down, (10.0, 20.0), 1.0, 0.5));

        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (sample, phase, caps) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Down);
        assert_eq!(sample.class, SampleClass::Committed);
        assert!(caps.contains(ToolCaps::PRESSURE));
        assert!(a.active_pen_pointer.is_some());
    }

    #[test]
    fn wayland_frame_sequence_emits_one_sample_per_frame() {
        let mut a = adapter();
        a.handle_wayland_proximity(wl_prox(1, ToolKind::Pen, ToolCaps::PRESSURE, true));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Down, (0.0, 0.0), 0.0, 0.1));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Move, (1.0, 1.0), 0.008, 0.3));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Move, (2.0, 2.0), 0.016, 0.5));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Up, (3.0, 3.0), 0.024, 0.0));

        let events = drained(&mut a);
        let phases: Vec<Phase> = events.iter().map(|e| expect_sample(e).1).collect();
        assert_eq!(phases, vec![Phase::Down, Phase::Move, Phase::Move, Phase::Up]);
    }

    #[test]
    fn wayland_pressure_clamps_to_unit_range() {
        let mut a = adapter();
        a.handle_wayland_proximity(wl_prox(1, ToolKind::Pen, ToolCaps::PRESSURE, true));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Down, (0.0, 0.0), 0.0, 1.7));

        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert!((sample.pressure - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn wayland_eraser_tool_flags_inverted() {
        let mut a = adapter();
        a.handle_wayland_proximity(wl_prox(2, ToolKind::Eraser, OPTIMISTIC_PEN_CAPS, true));
        let mut raw = wl_raw(2, WaylandTabletPhase::Down, (0.0, 0.0), 0.0, 0.4);
        raw.pointing_device_type = ToolKind::Eraser;
        a.handle_wayland_raw(raw);

        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert_eq!(sample.tool, ToolKind::Eraser);
        assert!(sample.buttons.contains(StylusButtons::INVERTED));
    }

    #[test]
    fn wayland_proximity_out_mid_stroke_synthesizes_cancel() {
        let mut a = adapter();
        a.handle_wayland_proximity(wl_prox(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Down, (0.0, 0.0), 0.0, 0.4));
        let _ = drained(&mut a);

        a.handle_wayland_proximity(wl_prox(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, false));

        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
        assert!(a.active_pen_pointer.is_none());
        assert!(!a.pens.contains_key(&1));
    }

    #[test]
    fn wayland_hardware_serial_lands_on_pen_unique_id() {
        let mut a = adapter();
        let serial = 0xDEAD_BEEF_CAFE_BABE_u64;
        let prox = WaylandProximitySample {
            device_id: 9,
            hardware_serial: Some(serial),
            pointing_device_type: ToolKind::Pen,
            caps: ToolCaps::PRESSURE,
            is_entering: true,
        };
        a.handle_wayland_proximity(prox);

        let pen = a.pens.get(&9).expect("pen state created by proximity");
        assert_eq!(pen.unique_id, Some(serial));
    }

    #[test]
    fn wayland_timestamps_monotonic_and_anchored() {
        let mut a = adapter();
        a.handle_wayland_proximity(wl_prox(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Down, (0.0, 0.0), 1000.0, 0.3));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Move, (1.0, 1.0), 1000.008, 0.4));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Up, (2.0, 2.0), 1000.016, 0.4));

        let events = drained(&mut a);
        let timestamps: Vec<Duration> = events
            .iter()
            .filter_map(|e| {
                if let StylusEvent::Sample { sample, .. } = e {
                    Some(sample.timestamp)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(timestamps.len(), 3);
        for pair in timestamps.windows(2) {
            assert!(pair[0] <= pair[1]);
        }
    }
}
