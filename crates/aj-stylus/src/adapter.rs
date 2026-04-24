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
    #[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
    windows_anchor: Option<PlatformTimestampAnchor>,
    #[cfg(any(target_arch = "wasm32", test))]
    web_anchor: Option<PlatformTimestampAnchor>,
    /// Per-gesture map from JS `pointerId` (stable only within a single
    /// Pointer-Events gesture) to the adapter's `PointerId`. Down allocates,
    /// Up / Cancel / Leave removes.
    #[cfg(any(target_arch = "wasm32", test))]
    web_pointers: HashMap<i32, WebPointerState>,
    #[cfg_attr(not(any(target_os = "ios", test)), allow(dead_code))]
    ios_anchor: Option<PlatformTimestampAnchor>,
    /// Per-iOS-touch map from the `UITouch` identity (hashed) to the
    /// adapter's `PointerId`. iOS pencil gestures have their own id space
    /// that survives coalesced/predicted replays of the same touch.
    #[cfg(any(target_os = "ios", test))]
    ios_pointers: HashMap<u64, PointerId>,
}

/// Per-gesture state for one Pointer Events pointer. Owning the tool kind
/// here (rather than re-classifying from every raw sample) keeps the stroke
/// stable if the browser momentarily reports a different `buttons` bitmask
/// mid-stroke — observed with Wacom + Chromium when the side switch toggles
/// during a drag.
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) struct WebPointerState {
    pub(crate) adapter_pointer_id: PointerId,
    pub(crate) tool: ToolKind,
    pub(crate) caps: ToolCaps,
    pub(crate) last_position: Option<Point>,
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

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum X11TabletPhase {
    Down,
    Move,
    Up,
}

/// Raw sample supplied by the X11 / `XInput2` backend. The backend has already
/// merged per-device axis deltas with its last-known cache — X11 delivers only
/// changed valuators per `XI_Motion`, and the full-sample reconstruction
/// belongs in the backend, not the adapter. No `timestamp_secs` because
/// `XIDeviceEvent.time` is ms since X server start and not monotonic; the
/// adapter stamps with `Instant::now()` on receipt.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct X11RawSample {
    pub position_physical_px: Point,
    pub pressure: f32,
    pub tilt: Tilt,
    pub twist_deg: f32,
    pub tangential_pressure: f32,
    pub button_mask: u32,
    pub device_id: u32,
    pub pointing_device_type: ToolKind,
    pub source_phase: X11TabletPhase,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct X11ProximitySample {
    pub device_id: u32,
    pub unique_id: Option<u64>,
    pub pointing_device_type: ToolKind,
    pub caps: ToolCaps,
    pub is_entering: bool,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowsPointerPhase {
    Down,
    Move,
    Up,
    Cancel,
    /// Pointer `INRANGE && !INCONTACT` — pen hovering over the sensing
    /// volume without tip contact. Emits `Phase::Hover` without starting
    /// or continuing a stroke.
    Hover,
}

/// Raw sample supplied by the Windows Pointer Input API backend. Pressure
/// already normalized to `0..=1` (the raw `POINTER_PEN_INFO.pressure` is
/// `0..=1024`), tilt already in degrees (`-90..=90`), rotation already in
/// degrees (`0..=359`). `tangential_pressure` is always `None` — the Windows
/// Pointer API has no corresponding field.
#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WindowsRawSample {
    pub position_physical_px: Point,
    /// `POINTER_INFO.PerformanceCount` converted to seconds by dividing by
    /// the cached `QueryPerformanceFrequency`. Monotonic.
    pub timestamp_secs: f64,
    pub pressure: f32,
    pub tilt: Option<Tilt>,
    pub twist_deg: Option<f32>,
    pub button_mask: u32,
    /// Hashed `POINTER_INFO.sourceDevice` (`HANDLE`) — stable per physical
    /// digitizer for its plug-in lifetime. Not a cross-session identifier.
    pub device_id: u32,
    pub pointing_device_type: ToolKind,
    pub source_phase: WindowsPointerPhase,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WindowsProximitySample {
    pub device_id: u32,
    pub pointing_device_type: ToolKind,
    pub caps: ToolCaps,
    pub is_entering: bool,
}

/// `PointerEvent.pointerType`, mapped out of the JS string at the backend.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebPointerType {
    Pen,
    Touch,
    Mouse,
    Unknown,
}

/// Phase for a Web raw sample. `Hover` is synthesized by the backend when a
/// `pen` pointer moves with `pressure === 0` (no tip contact). `Predicted`
/// is for events drained from `getPredictedEvents()` — they render to the
/// Placeholder layer only and discard on the next real delivery.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebSourcePhase {
    Down,
    Move,
    Up,
    Cancel,
    Hover,
    Predicted,
}

/// Raw sample supplied by the Web Pointer Events backend. Timestamp is
/// `event.timeStamp / 1000.0` (`DOMHighResTimeStamp` is ms since navigation
/// start; monotonic). Position is physical px canvas-relative, pre-converted
/// by the backend from `(client_x - rect.left) * devicePixelRatio`.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WebRawSample {
    pub position_physical_px: Point,
    pub timestamp_secs: f64,
    pub pressure: f32,
    pub tilt: Option<Tilt>,
    pub twist_deg: Option<f32>,
    pub tangential_pressure: Option<f32>,
    pub button_mask: u32,
    /// JS `PointerEvent.pointerId` — stable for the gesture only, not across.
    pub pointer_id: i32,
    pub pointer_type: WebPointerType,
    pub source_phase: WebSourcePhase,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WebProximitySample {
    pub pointer_id: i32,
    pub pointer_type: WebPointerType,
    pub caps: ToolCaps,
    pub is_entering: bool,
}

#[cfg(any(target_os = "ios", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IosTouchPhase {
    Down,
    Move,
    Up,
    Cancel,
    /// Hover state from `UIHoverGestureRecognizer`. Emits `Phase::Hover`.
    Hover,
}

#[cfg(any(target_os = "ios", test))]
bitflags::bitflags! {
    /// Which axes are `.estimated` or `expecting update` on an iOS `UITouch`.
    /// Mirrors `UITouchProperties` but keeps the adapter free of UIKit types.
    /// Only the `ExpectsUpdate` bit gates whether the adapter inserts into
    /// `pending_estimated`; the `Estimated` bit is informational.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub(crate) struct IosEstimatedProperties: u8 {
        const FORCE         = 1 << 0;
        const AZIMUTH       = 1 << 1;
        const ALTITUDE      = 1 << 2;
        const LOCATION      = 1 << 3;
        const ROLL          = 1 << 4;
        /// At least one axis on this sample is estimated *and* the OS will
        /// later deliver a `touchesEstimatedPropertiesUpdated:` with the
        /// refinement.
        const EXPECTS_UPDATE = 1 << 5;
    }
}

/// Raw sample from the iOS overlay `UIView` backend. One
/// `IosTouchRawSample` per element of `UIEvent.coalescedTouches(for:)`
/// (all tagged `Committed`) plus one per element of
/// `UIEvent.predictedTouches(for:)` (tagged `Predicted`).
#[cfg(any(target_os = "ios", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct IosTouchRawSample {
    pub position_physical_px: Point,
    /// `UITouch.timestamp` — seconds, monotonic (shares clock with
    /// `CACurrentMediaTime()`).
    pub timestamp_secs: f64,
    /// Normalized `force / maximumPossibleForce`, pre-clamped to `0..=1`.
    pub pressure: f32,
    /// Apple's polar form, radians. `altitude=0` = flush, `π/2` = perpendicular.
    pub altitude_rad: f32,
    /// Radians, `-π..=π`. Direction the shaft points on the canvas plane.
    pub azimuth_rad: f32,
    /// Pencil Pro barrel-roll (iOS 17.5+). `None` on older Pencils.
    pub roll_rad: Option<f32>,
    /// Stable key from `UITouch.estimationUpdateIndex`; populated iff at
    /// least one axis is `.estimated`. Maps a later
    /// `touchesEstimatedPropertiesUpdated:` to this sample.
    pub estimation_update_index: Option<u64>,
    pub estimated_properties: IosEstimatedProperties,
    /// Per-touch opaque identity (hash of the `UITouch *` for the gesture's
    /// lifetime).
    pub touch_id: u64,
    pub pointing_device_type: ToolKind,
    pub source_phase: IosTouchPhase,
    /// `true` if this sample came from `predictedTouches(for:)` rather than
    /// `coalescedTouches(for:)`. Tags the emitted sample
    /// `SampleClass::Predicted`.
    pub predicted: bool,
}

#[cfg(any(target_os = "ios", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct IosTouchProximitySample {
    pub touch_id: u64,
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

/// Decompose Apple's polar (altitude, azimuth) into component tilts along
/// the view's X and Y axes. `altitude` is radians from the view plane
/// (`0` = flush, `π/2` = perpendicular); `azimuth` is radians around the
/// view normal. Returns degrees (`x_deg`, `y_deg`), each in `-90..=90`.
///
/// When the shaft is near-perpendicular (`sin(π/2 - altitude) < 0.02`)
/// azimuth is numerically noisy; zero both components in that region.
#[cfg(any(target_os = "ios", test))]
#[allow(clippy::similar_names)]
pub(crate) fn ios_altitude_azimuth_to_tilt_xy_deg(altitude: f32, azimuth: f32) -> (f32, f32) {
    let theta = std::f32::consts::FRAC_PI_2 - altitude;
    let sin_theta = theta.sin();
    if sin_theta < 0.02 {
        return (0.0, 0.0);
    }
    let sin_alt = altitude.sin().max(f32::EPSILON);
    let tilt_x = (sin_theta * azimuth.cos()).atan2(sin_alt);
    let tilt_y = (sin_theta * azimuth.sin()).atan2(sin_alt);
    (tilt_x.to_degrees(), tilt_y.to_degrees())
}

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

    // --- X11 / XInput2 seam ---
    //
    // X11 delivers fully-resolved samples (no Estimated/Revise cycle — the
    // backend merges axis deltas with its per-device cache), and no reliable
    // timestamp, so `handle_x11_raw` uses `Instant::now()`-relative timing
    // and emits `SampleClass::Committed` directly.

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn handle_x11_raw(&mut self, raw: X11RawSample) {
        let ts = self.current_duration();

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
            X11TabletPhase::Down => {
                let pid = alloc_pointer_id(&mut self.next_pointer_id);
                if let Some(pen) = self.pens.get_mut(&raw.device_id) {
                    pen.active_pointer_id = Some(pid);
                }
                self.active_pen_pointer = Some(pid);
                let sample = build_x11_pen_sample(&raw, ts, pid, tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Down, caps });
            }
            X11TabletPhase::Move | X11TabletPhase::Up => {
                let Some(pid) = active_pid else {
                    return;
                };
                let phase = match raw.source_phase {
                    X11TabletPhase::Move => Phase::Move,
                    X11TabletPhase::Up => Phase::Up,
                    X11TabletPhase::Down => unreachable!(),
                };
                let sample = build_x11_pen_sample(&raw, ts, pid, tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase, caps });

                if matches!(raw.source_phase, X11TabletPhase::Up) {
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
    pub(crate) fn handle_x11_proximity(&mut self, prox: X11ProximitySample) {
        if prox.is_entering {
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

    // --- Windows Pointer Input API seam ---
    //
    // Windows delivers fully-resolved pointer samples with `penMask` bits
    // indicating which axes are valid; the backend pre-filters by mask and
    // supplies `None` for missing axes. No Estimated/Revise cycle — unlike
    // macOS NSEvent, `WM_POINTERDOWN` carries pressure on the first sample.
    // Hover samples ride the same entry point with `source_phase: Hover`.

    #[cfg(any(target_os = "windows", test))]
    pub(crate) fn handle_windows_raw(&mut self, raw: WindowsRawSample) {
        let ts = PlatformTimestampAnchor::translate_or_anchor(
            &mut self.windows_anchor,
            raw.timestamp_secs,
            self.epoch,
        );

        // Hover does not allocate a stroke-owning PointerId; emit a Hover
        // sample tied to `PointerId::MOUSE` so the app can drive cursor /
        // brush-preview UI without disturbing mid-stroke state.
        if matches!(raw.source_phase, WindowsPointerPhase::Hover) {
            let sample = build_windows_sample(&raw, ts, PointerId::MOUSE, raw.pointing_device_type);
            let caps = self.pens.get(&raw.device_id).map_or(OPTIMISTIC_PEN_CAPS, |p| p.caps);
            self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Hover, caps });
            return;
        }

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
            WindowsPointerPhase::Down => {
                let pid = alloc_pointer_id(&mut self.next_pointer_id);
                if let Some(pen) = self.pens.get_mut(&raw.device_id) {
                    pen.active_pointer_id = Some(pid);
                }
                self.active_pen_pointer = Some(pid);
                let sample = build_windows_sample(&raw, ts, pid, tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Down, caps });
            }
            WindowsPointerPhase::Move | WindowsPointerPhase::Up | WindowsPointerPhase::Cancel => {
                let Some(pid) = active_pid else {
                    return;
                };
                let phase = match raw.source_phase {
                    WindowsPointerPhase::Move => Phase::Move,
                    WindowsPointerPhase::Up => Phase::Up,
                    WindowsPointerPhase::Cancel => Phase::Cancel,
                    WindowsPointerPhase::Down | WindowsPointerPhase::Hover => unreachable!(),
                };
                let sample = build_windows_sample(&raw, ts, pid, tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase, caps });

                if matches!(raw.source_phase, WindowsPointerPhase::Up | WindowsPointerPhase::Cancel)
                {
                    if let Some(pen) = self.pens.get_mut(&raw.device_id) {
                        pen.active_pointer_id = None;
                    }
                    if self.active_pen_pointer == Some(pid) {
                        self.active_pen_pointer = None;
                    }
                }
            }
            WindowsPointerPhase::Hover => unreachable!("handled above"),
        }
    }

    #[cfg(any(target_os = "windows", test))]
    pub(crate) fn handle_windows_proximity(&mut self, prox: WindowsProximitySample) {
        if prox.is_entering {
            let entry = self.pens.entry(prox.device_id).or_insert(PenState {
                active_pointer_id: None,
                caps: prox.caps,
                tool: prox.pointing_device_type,
                unique_id: None,
                last_position: None,
            });
            entry.caps = prox.caps;
            entry.tool = prox.pointing_device_type;
        } else if let Some(mut pen) = self.pens.remove(&prox.device_id)
            && let Some(pid) = pen.active_pointer_id.take()
        {
            // Pen left proximity with a stroke still active — the WM_POINTERUP
            // never arrived. Synthesize Cancel to match the mac proximity-out
            // contract.
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

    // --- Web Pointer Events seam ---
    //
    // Pointer Events deliver fully-resolved samples; no Estimated/Revise
    // cycle needed. `Predicted` phases emit `SampleClass::Predicted` so
    // the engine can draw them to the Placeholder layer only and discard
    // on next real delivery. Touch pointers are gated at the backend by
    // a live-pen set; finger-while-pen palm rejection happens before the
    // adapter sees the touch sample.

    #[cfg(any(target_arch = "wasm32", test))]
    pub(crate) fn handle_web_raw(&mut self, raw: WebRawSample) {
        let ts = PlatformTimestampAnchor::translate_or_anchor(
            &mut self.web_anchor,
            raw.timestamp_secs,
            self.epoch,
        );

        let tool = classify_web_tool(raw.pointer_type, raw.button_mask);

        // Hover: no stroke state; attach to MOUSE pointer id.
        if matches!(raw.source_phase, WebSourcePhase::Hover) {
            if matches!(raw.pointer_type, WebPointerType::Pen) {
                let caps =
                    self.web_pointers.get(&raw.pointer_id).map_or(OPTIMISTIC_PEN_CAPS, |s| s.caps);
                let sample = build_web_sample(&raw, ts, PointerId::MOUSE, tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Hover, caps });
            }
            return;
        }

        match raw.source_phase {
            WebSourcePhase::Down => {
                let pid = alloc_pointer_id(&mut self.next_pointer_id);
                self.web_pointers.insert(
                    raw.pointer_id,
                    WebPointerState {
                        adapter_pointer_id: pid,
                        tool,
                        caps: OPTIMISTIC_PEN_CAPS,
                        last_position: Some(raw.position_physical_px),
                    },
                );
                if matches!(raw.pointer_type, WebPointerType::Pen) {
                    self.active_pen_pointer = Some(pid);
                }
                let caps = OPTIMISTIC_PEN_CAPS;
                let sample = build_web_sample(&raw, ts, pid, tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Down, caps });
            }
            WebSourcePhase::Move | WebSourcePhase::Predicted => {
                let Some(state) = self.web_pointers.get_mut(&raw.pointer_id) else {
                    return;
                };
                state.last_position = Some(raw.position_physical_px);
                let pid = state.adapter_pointer_id;
                let pinned_tool = state.tool;
                let caps = state.caps;
                let mut sample = build_web_sample(&raw, ts, pid, pinned_tool);
                if matches!(raw.source_phase, WebSourcePhase::Predicted) {
                    sample.class = SampleClass::Predicted;
                }
                self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Move, caps });
            }
            WebSourcePhase::Up | WebSourcePhase::Cancel => {
                let Some(state) = self.web_pointers.remove(&raw.pointer_id) else {
                    return;
                };
                let pid = state.adapter_pointer_id;
                let caps = state.caps;
                let phase = match raw.source_phase {
                    WebSourcePhase::Up => Phase::Up,
                    WebSourcePhase::Cancel => Phase::Cancel,
                    _ => unreachable!(),
                };
                let sample = build_web_sample(&raw, ts, pid, state.tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase, caps });
                if self.active_pen_pointer == Some(pid) {
                    self.active_pen_pointer = None;
                }
            }
            WebSourcePhase::Hover => unreachable!("handled above"),
        }
    }

    #[cfg(any(target_arch = "wasm32", test))]
    pub(crate) fn handle_web_proximity(&mut self, prox: WebProximitySample) {
        if prox.is_entering {
            if let Some(state) = self.web_pointers.get_mut(&prox.pointer_id) {
                state.caps = prox.caps;
            }
        } else if let Some(state) = self.web_pointers.remove(&prox.pointer_id) {
            let ts = self.current_duration();
            let pid = state.adapter_pointer_id;
            let mut sample = Sample::new_pen_placeholder(
                state.last_position.unwrap_or(Point::ZERO),
                ts,
                pid,
                state.tool,
            );
            let caps = state.caps;
            sample.tool = state.tool;
            self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Cancel, caps });
            let _ = self.take_pending_for_pointer(pid);
            if self.active_pen_pointer == Some(pid) {
                self.active_pen_pointer = None;
            }
        }
    }

    // --- iOS overlay-UIView seam ---
    //
    // iOS delivers `UITouch` events with an Estimated/Revise cycle for
    // axes that weren't yet settled at touch time (pressure over BT, etc.).
    // Unlike mac, multiple axes per stroke can be in-flight, and
    // `touchesEstimatedPropertiesUpdated:` can arrive out of order — which
    // is why `pending_estimated` is keyed by `update_index` rather than
    // `PointerId`.

    #[cfg(any(target_os = "ios", test))]
    pub(crate) fn handle_ios_raw(&mut self, raw: IosTouchRawSample) {
        let ts = PlatformTimestampAnchor::translate_or_anchor(
            &mut self.ios_anchor,
            raw.timestamp_secs,
            self.epoch,
        );

        if matches!(raw.source_phase, IosTouchPhase::Hover) {
            let (tilt_x, tilt_y) =
                ios_altitude_azimuth_to_tilt_xy_deg(raw.altitude_rad, raw.azimuth_rad);
            let mut sample = Sample::new_pen(
                raw.position_physical_px,
                ts,
                PointerId::MOUSE,
                raw.pointing_device_type,
            );
            sample.pressure = 0.0;
            sample.tilt = Some(Tilt { x_deg: tilt_x, y_deg: tilt_y });
            sample.twist_deg = raw.roll_rad.map(f32::to_degrees);
            self.queue.push_back(StylusEvent::Sample {
                sample,
                phase: Phase::Hover,
                caps: OPTIMISTIC_PEN_CAPS,
            });
            return;
        }

        match raw.source_phase {
            IosTouchPhase::Down => {
                let pid = alloc_pointer_id(&mut self.next_pointer_id);
                self.ios_pointers.insert(raw.touch_id, pid);
                self.active_pen_pointer = Some(pid);
                let mut sample = build_ios_sample(&raw, ts, pid);
                if raw.estimated_properties.contains(IosEstimatedProperties::EXPECTS_UPDATE)
                    && let Some(update_index) = raw.estimation_update_index
                {
                    sample.class = SampleClass::Estimated { update_index };
                    self.pending_estimated.insert(update_index, pid);
                }
                self.queue.push_back(StylusEvent::Sample {
                    sample,
                    phase: Phase::Down,
                    caps: OPTIMISTIC_PEN_CAPS,
                });
            }
            IosTouchPhase::Move | IosTouchPhase::Up | IosTouchPhase::Cancel => {
                let Some(&pid) = self.ios_pointers.get(&raw.touch_id) else {
                    return;
                };
                let phase = match raw.source_phase {
                    IosTouchPhase::Move => Phase::Move,
                    IosTouchPhase::Up => Phase::Up,
                    IosTouchPhase::Cancel => Phase::Cancel,
                    _ => unreachable!(),
                };
                let mut sample = build_ios_sample(&raw, ts, pid);
                if raw.predicted {
                    sample.class = SampleClass::Predicted;
                } else if raw.estimated_properties.contains(IosEstimatedProperties::EXPECTS_UPDATE)
                    && let Some(update_index) = raw.estimation_update_index
                {
                    sample.class = SampleClass::Estimated { update_index };
                    self.pending_estimated.insert(update_index, pid);
                }
                self.queue.push_back(StylusEvent::Sample {
                    sample,
                    phase,
                    caps: OPTIMISTIC_PEN_CAPS,
                });

                if matches!(raw.source_phase, IosTouchPhase::Up | IosTouchPhase::Cancel) {
                    self.ios_pointers.remove(&raw.touch_id);
                    if self.active_pen_pointer == Some(pid) {
                        self.active_pen_pointer = None;
                    }
                }
            }
            IosTouchPhase::Hover => unreachable!("handled above"),
        }
    }

    /// Resolve a deferred estimate via `touchesEstimatedPropertiesUpdated:`.
    /// Keyed by `update_index` directly because iOS can deliver these out
    /// of order for Pencil-over-BT scenarios — unlike mac where the first
    /// Move closes the Down's estimate linearly.
    #[cfg(any(target_os = "ios", test))]
    pub(crate) fn handle_ios_estimated_update(
        &mut self,
        update_index: u64,
        revision: SampleRevision,
    ) {
        let Some(pid) = self.pending_estimated.remove(&update_index) else {
            return;
        };
        self.queue.push_back(StylusEvent::Revise { pointer_id: pid, update_index, revision });
    }

    #[cfg(any(target_os = "ios", test))]
    pub(crate) fn handle_ios_proximity(&mut self, prox: IosTouchProximitySample) {
        if !prox.is_entering
            && let Some(pid) = self.ios_pointers.remove(&prox.touch_id)
        {
            let ts = self.current_duration();
            let sample =
                Sample::new_pen_placeholder(Point::ZERO, ts, pid, prox.pointing_device_type);
            self.queue.push_back(StylusEvent::Sample {
                sample,
                phase: Phase::Cancel,
                caps: prox.caps,
            });
            let _ = self.take_pending_for_pointer(pid);
            if self.active_pen_pointer == Some(pid) {
                self.active_pen_pointer = None;
            }
        }
    }

    #[cfg(any(target_os = "ios", test))]
    pub(crate) fn handle_ios_pencil_interaction(
        &mut self,
        kind: crate::PencilInteractionKind,
        hover_pose: Option<crate::HoverPose>,
    ) {
        self.queue.push_back(StylusEvent::PencilInteraction { kind, hover_pose });
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

#[cfg(any(target_os = "linux", test))]
fn build_x11_pen_sample(
    raw: &X11RawSample,
    timestamp: Duration,
    pointer_id: PointerId,
    tool: ToolKind,
) -> Sample {
    let mut buttons = StylusButtons::CONTACT;
    if raw.button_mask & 0x2 != 0 {
        buttons |= StylusButtons::BARREL;
    }
    if raw.button_mask & 0x4 != 0 {
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

#[cfg(any(target_os = "windows", test))]
fn build_windows_sample(
    raw: &WindowsRawSample,
    timestamp: Duration,
    pointer_id: PointerId,
    tool: ToolKind,
) -> Sample {
    // `PEN_FLAG_BARREL` and `PEN_FLAG_INVERTED` are pre-folded by the backend
    // into button_mask bits: 0x1 barrel, 0x2 secondary, 0x4 inverted. Mirror
    // the mac layout so callers stay platform-agnostic.
    let mut buttons = StylusButtons::CONTACT;
    if raw.button_mask & 0x1 != 0 {
        buttons |= StylusButtons::BARREL;
    }
    if raw.button_mask & 0x2 != 0 {
        buttons |= StylusButtons::SECONDARY;
    }
    if raw.button_mask & 0x4 != 0 || matches!(tool, ToolKind::Eraser) {
        buttons |= StylusButtons::INVERTED;
    }
    let mut sample = Sample::new_pen(raw.position_physical_px, timestamp, pointer_id, tool);
    sample.pressure = raw.pressure.clamp(0.0, 1.0);
    sample.tilt = raw.tilt;
    sample.twist_deg = raw.twist_deg;
    sample.tangential_pressure = None;
    sample.buttons = buttons;
    sample
}

#[cfg(any(target_arch = "wasm32", test))]
fn classify_web_tool(pointer_type: WebPointerType, button_mask: u32) -> ToolKind {
    // Eraser is signalled by `buttons & 0x20` — Pointer Events has no
    // dedicated pointer_type for eraser, unlike iOS / Android.
    match (pointer_type, button_mask & 0x20 != 0) {
        (WebPointerType::Pen, true) => ToolKind::Eraser,
        (WebPointerType::Pen, false) => ToolKind::Pen,
        (WebPointerType::Mouse, _) => ToolKind::Mouse,
        (WebPointerType::Touch, _) => ToolKind::Finger,
        (WebPointerType::Unknown, _) => ToolKind::Unknown,
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn build_web_sample(
    raw: &WebRawSample,
    timestamp: Duration,
    pointer_id: PointerId,
    tool: ToolKind,
) -> Sample {
    // Pointer Events `buttons` bits: 0x1 primary (tip), 0x2 secondary/barrel,
    // 0x20 eraser. Map barrel → StylusButtons::BARREL; eraser already
    // handled above via tool classification.
    let mut buttons = StylusButtons::CONTACT;
    if raw.button_mask & 0x2 != 0 {
        buttons |= StylusButtons::BARREL;
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

#[cfg(any(target_os = "ios", test))]
fn build_ios_sample(raw: &IosTouchRawSample, timestamp: Duration, pointer_id: PointerId) -> Sample {
    let (tilt_x, tilt_y) = ios_altitude_azimuth_to_tilt_xy_deg(raw.altitude_rad, raw.azimuth_rad);
    let mut sample =
        Sample::new_pen(raw.position_physical_px, timestamp, pointer_id, raw.pointing_device_type);
    sample.pressure = raw.pressure.clamp(0.0, 1.0);
    sample.tilt = Some(Tilt { x_deg: tilt_x, y_deg: tilt_y });
    sample.twist_deg = raw.roll_rad.map(f32::to_degrees);
    let mut buttons = StylusButtons::CONTACT;
    if matches!(raw.pointing_device_type, ToolKind::Eraser) {
        buttons |= StylusButtons::INVERTED;
    }
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

    // --- Wayland tablet-v2 seam tests ---

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

    // --- X11 tablet seam tests ---

    fn x11_raw(
        device_id: u32,
        phase: X11TabletPhase,
        pos: (f64, f64),
        pressure: f32,
    ) -> X11RawSample {
        X11RawSample {
            position_physical_px: Point::new(pos.0, pos.1),
            pressure,
            tilt: Tilt { x_deg: 0.0, y_deg: 0.0 },
            twist_deg: 0.0,
            tangential_pressure: 0.0,
            button_mask: 0,
            device_id,
            pointing_device_type: ToolKind::Pen,
            source_phase: phase,
        }
    }

    fn x11_proximity(device_id: u32, tool: ToolKind, is_entering: bool) -> X11ProximitySample {
        X11ProximitySample {
            device_id,
            unique_id: Some(0xBEEF),
            pointing_device_type: tool,
            caps: ToolCaps::PRESSURE | ToolCaps::TILT,
            is_entering,
        }
    }

    #[test]
    fn x11_down_move_up_emits_committed_samples() {
        let mut a = adapter();
        a.handle_x11_proximity(x11_proximity(1, ToolKind::Pen, true));
        a.handle_x11_raw(x11_raw(1, X11TabletPhase::Down, (10.0, 20.0), 0.3));
        a.handle_x11_raw(x11_raw(1, X11TabletPhase::Move, (11.0, 21.0), 0.5));
        a.handle_x11_raw(x11_raw(1, X11TabletPhase::Up, (11.0, 21.0), 0.5));

        let events = drained(&mut a);
        assert_eq!(events.len(), 3);
        let phases: Vec<Phase> = events.iter().map(|e| expect_sample(e).1).collect();
        assert_eq!(phases, vec![Phase::Down, Phase::Move, Phase::Up]);
        for ev in &events {
            let (sample, _, _) = expect_sample(ev);
            assert_eq!(sample.class, SampleClass::Committed);
        }
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn x11_proximity_out_with_active_stroke_cancels() {
        let mut a = adapter();
        a.handle_x11_proximity(x11_proximity(2, ToolKind::Pen, true));
        a.handle_x11_raw(x11_raw(2, X11TabletPhase::Down, (0.0, 0.0), 0.4));
        let _ = drained(&mut a);

        a.handle_x11_proximity(x11_proximity(2, ToolKind::Pen, false));
        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn x11_eraser_tool_yields_inverted_buttons() {
        let mut a = adapter();
        a.handle_x11_proximity(x11_proximity(3, ToolKind::Eraser, true));
        let mut r = x11_raw(3, X11TabletPhase::Down, (5.0, 5.0), 0.2);
        r.pointing_device_type = ToolKind::Eraser;
        a.handle_x11_raw(r);

        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert_eq!(sample.tool, ToolKind::Eraser);
        assert!(sample.buttons.contains(StylusButtons::INVERTED));
    }

    #[test]
    fn x11_move_without_prior_down_is_dropped() {
        let mut a = adapter();
        a.handle_x11_proximity(x11_proximity(4, ToolKind::Pen, true));
        a.handle_x11_raw(x11_raw(4, X11TabletPhase::Move, (1.0, 1.0), 0.5));
        assert!(drained(&mut a).is_empty());
    }

    // --- Windows Pointer Input API seam tests ---

    fn win_raw(
        device_id: u32,
        phase: WindowsPointerPhase,
        pos: (f64, f64),
        ts: f64,
        pressure: f32,
    ) -> WindowsRawSample {
        WindowsRawSample {
            position_physical_px: Point::new(pos.0, pos.1),
            timestamp_secs: ts,
            pressure,
            tilt: Some(Tilt { x_deg: 0.0, y_deg: 0.0 }),
            twist_deg: Some(0.0),
            button_mask: 0,
            device_id,
            pointing_device_type: ToolKind::Pen,
            source_phase: phase,
        }
    }

    fn win_prox(
        device_id: u32,
        tool: ToolKind,
        caps: ToolCaps,
        is_entering: bool,
    ) -> WindowsProximitySample {
        WindowsProximitySample { device_id, pointing_device_type: tool, caps, is_entering }
    }

    #[test]
    fn windows_down_move_up_emits_committed_samples() {
        let mut a = adapter();
        a.handle_windows_proximity(win_prox(1, ToolKind::Pen, ToolCaps::PRESSURE, true));
        a.handle_windows_raw(win_raw(1, WindowsPointerPhase::Down, (10.0, 20.0), 100.0, 0.3));
        a.handle_windows_raw(win_raw(1, WindowsPointerPhase::Move, (11.0, 21.0), 100.008, 0.5));
        a.handle_windows_raw(win_raw(1, WindowsPointerPhase::Up, (11.0, 21.0), 100.016, 0.5));

        let events = drained(&mut a);
        let phases: Vec<Phase> = events.iter().map(|e| expect_sample(e).1).collect();
        assert_eq!(phases, vec![Phase::Down, Phase::Move, Phase::Up]);
        for ev in &events {
            let (sample, _, _) = expect_sample(ev);
            assert_eq!(sample.class, SampleClass::Committed);
        }
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn windows_hover_emits_hover_phase_without_stroke() {
        let mut a = adapter();
        a.handle_windows_proximity(win_prox(1, ToolKind::Pen, ToolCaps::PRESSURE, true));
        a.handle_windows_raw(win_raw(1, WindowsPointerPhase::Hover, (5.0, 5.0), 0.0, 0.0));

        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (sample, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Hover);
        assert_eq!(sample.pointer_id, PointerId::MOUSE);
        assert!(a.active_pen_pointer.is_none(), "hover must not start a stroke");
    }

    #[test]
    fn windows_capture_lost_emits_cancel() {
        let mut a = adapter();
        a.handle_windows_proximity(win_prox(1, ToolKind::Pen, ToolCaps::PRESSURE, true));
        a.handle_windows_raw(win_raw(1, WindowsPointerPhase::Down, (0.0, 0.0), 0.0, 0.4));
        let _ = drained(&mut a);

        a.handle_windows_raw(win_raw(1, WindowsPointerPhase::Cancel, (1.0, 1.0), 0.016, 0.4));

        let events = drained(&mut a);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn windows_eraser_tool_flags_inverted() {
        let mut a = adapter();
        a.handle_windows_proximity(win_prox(1, ToolKind::Eraser, OPTIMISTIC_PEN_CAPS, true));
        let mut raw = win_raw(1, WindowsPointerPhase::Down, (0.0, 0.0), 0.0, 0.4);
        raw.pointing_device_type = ToolKind::Eraser;
        a.handle_windows_raw(raw);

        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert_eq!(sample.tool, ToolKind::Eraser);
        assert!(sample.buttons.contains(StylusButtons::INVERTED));
    }

    #[test]
    fn windows_tangential_pressure_always_none() {
        let mut a = adapter();
        a.handle_windows_proximity(win_prox(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_windows_raw(win_raw(1, WindowsPointerPhase::Down, (0.0, 0.0), 0.0, 0.5));
        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert!(sample.tangential_pressure.is_none());
    }

    #[test]
    fn windows_proximity_out_with_active_stroke_cancels() {
        let mut a = adapter();
        a.handle_windows_proximity(win_prox(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_windows_raw(win_raw(1, WindowsPointerPhase::Down, (0.0, 0.0), 0.0, 0.4));
        let _ = drained(&mut a);

        a.handle_windows_proximity(win_prox(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, false));
        let events = drained(&mut a);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn windows_move_without_prior_down_is_dropped() {
        let mut a = adapter();
        a.handle_windows_proximity(win_prox(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_windows_raw(win_raw(1, WindowsPointerPhase::Move, (1.0, 1.0), 0.0, 0.5));
        assert!(drained(&mut a).is_empty());
    }

    // --- Web Pointer Events seam tests ---

    fn web_raw(
        pointer_id: i32,
        phase: WebSourcePhase,
        pos: (f64, f64),
        ts: f64,
        pressure: f32,
        pointer_type: WebPointerType,
    ) -> WebRawSample {
        WebRawSample {
            position_physical_px: Point::new(pos.0, pos.1),
            timestamp_secs: ts,
            pressure,
            tilt: Some(Tilt { x_deg: 0.0, y_deg: 0.0 }),
            twist_deg: Some(0.0),
            tangential_pressure: None,
            button_mask: 0,
            pointer_id,
            pointer_type,
            source_phase: phase,
        }
    }

    #[test]
    fn web_down_move_up_emits_committed_samples() {
        let mut a = adapter();
        a.handle_web_raw(web_raw(
            7,
            WebSourcePhase::Down,
            (0.0, 0.0),
            0.0,
            0.4,
            WebPointerType::Pen,
        ));
        a.handle_web_raw(web_raw(
            7,
            WebSourcePhase::Move,
            (1.0, 1.0),
            0.008,
            0.5,
            WebPointerType::Pen,
        ));
        a.handle_web_raw(web_raw(
            7,
            WebSourcePhase::Up,
            (1.0, 1.0),
            0.016,
            0.0,
            WebPointerType::Pen,
        ));

        let events = drained(&mut a);
        let phases: Vec<Phase> = events.iter().map(|e| expect_sample(e).1).collect();
        assert_eq!(phases, vec![Phase::Down, Phase::Move, Phase::Up]);
        for ev in &events {
            let (s, _, _) = expect_sample(ev);
            assert_eq!(s.class, SampleClass::Committed);
        }
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn web_predicted_samples_tagged_predicted_class() {
        let mut a = adapter();
        a.handle_web_raw(web_raw(
            1,
            WebSourcePhase::Down,
            (0.0, 0.0),
            0.0,
            0.3,
            WebPointerType::Pen,
        ));
        let _ = drained(&mut a);

        a.handle_web_raw(web_raw(
            1,
            WebSourcePhase::Predicted,
            (2.0, 2.0),
            0.016,
            0.4,
            WebPointerType::Pen,
        ));
        let events = drained(&mut a);
        let (sample, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Move);
        assert_eq!(sample.class, SampleClass::Predicted);
    }

    #[test]
    fn web_eraser_via_buttons_bit_0x20() {
        let mut a = adapter();
        let mut raw = web_raw(1, WebSourcePhase::Down, (0.0, 0.0), 0.0, 0.4, WebPointerType::Pen);
        raw.button_mask = 0x20;
        a.handle_web_raw(raw);

        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert_eq!(sample.tool, ToolKind::Eraser);
        assert!(sample.buttons.contains(StylusButtons::INVERTED));
    }

    #[test]
    fn web_pen_hover_emits_hover_phase() {
        let mut a = adapter();
        a.handle_web_raw(web_raw(
            1,
            WebSourcePhase::Hover,
            (5.0, 5.0),
            0.0,
            0.0,
            WebPointerType::Pen,
        ));

        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (sample, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Hover);
        assert_eq!(sample.pointer_id, PointerId::MOUSE);
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn web_pointercancel_synthesizes_cancel() {
        let mut a = adapter();
        a.handle_web_raw(web_raw(
            1,
            WebSourcePhase::Down,
            (0.0, 0.0),
            0.0,
            0.3,
            WebPointerType::Pen,
        ));
        let _ = drained(&mut a);

        a.handle_web_raw(web_raw(
            1,
            WebSourcePhase::Cancel,
            (1.0, 1.0),
            0.016,
            0.0,
            WebPointerType::Pen,
        ));
        let events = drained(&mut a);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn web_proximity_out_mid_stroke_cancels() {
        let mut a = adapter();
        a.handle_web_raw(web_raw(
            1,
            WebSourcePhase::Down,
            (0.0, 0.0),
            0.0,
            0.3,
            WebPointerType::Pen,
        ));
        let _ = drained(&mut a);

        a.handle_web_proximity(WebProximitySample {
            pointer_id: 1,
            pointer_type: WebPointerType::Pen,
            caps: OPTIMISTIC_PEN_CAPS,
            is_entering: false,
        });
        let events = drained(&mut a);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
    }

    #[test]
    fn web_mouse_pointer_maps_to_mouse_tool() {
        let mut a = adapter();
        a.handle_web_raw(web_raw(
            0,
            WebSourcePhase::Down,
            (0.0, 0.0),
            0.0,
            0.5,
            WebPointerType::Mouse,
        ));
        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert_eq!(sample.tool, ToolKind::Mouse);
    }

    #[test]
    fn web_move_without_prior_down_is_dropped() {
        let mut a = adapter();
        a.handle_web_raw(web_raw(
            1,
            WebSourcePhase::Move,
            (1.0, 1.0),
            0.0,
            0.5,
            WebPointerType::Pen,
        ));
        assert!(drained(&mut a).is_empty());
    }

    #[test]
    fn web_touch_and_unknown_pointer_types_map_correctly() {
        let mut a = adapter();
        a.handle_web_raw(web_raw(
            2,
            WebSourcePhase::Down,
            (0.0, 0.0),
            0.0,
            0.5,
            WebPointerType::Touch,
        ));
        let events = drained(&mut a);
        assert_eq!(expect_sample(&events[0]).0.tool, ToolKind::Finger);

        let mut b = adapter();
        b.handle_web_raw(web_raw(
            3,
            WebSourcePhase::Down,
            (0.0, 0.0),
            0.0,
            0.5,
            WebPointerType::Unknown,
        ));
        let events = drained(&mut b);
        assert_eq!(expect_sample(&events[0]).0.tool, ToolKind::Unknown);
    }

    // --- iOS overlay-UIView seam tests ---

    fn ios_raw(
        touch_id: u64,
        phase: IosTouchPhase,
        pos: (f64, f64),
        ts: f64,
        pressure: f32,
    ) -> IosTouchRawSample {
        IosTouchRawSample {
            position_physical_px: Point::new(pos.0, pos.1),
            timestamp_secs: ts,
            pressure,
            altitude_rad: std::f32::consts::FRAC_PI_4,
            azimuth_rad: 0.0,
            roll_rad: None,
            estimation_update_index: None,
            estimated_properties: IosEstimatedProperties::empty(),
            touch_id,
            pointing_device_type: ToolKind::Pen,
            source_phase: phase,
            predicted: false,
        }
    }

    #[test]
    fn ios_down_expects_update_emits_estimated_with_update_index() {
        let mut a = adapter();
        let mut raw = ios_raw(42, IosTouchPhase::Down, (10.0, 20.0), 0.0, 0.3);
        raw.estimation_update_index = Some(100);
        raw.estimated_properties =
            IosEstimatedProperties::FORCE | IosEstimatedProperties::EXPECTS_UPDATE;

        a.handle_ios_raw(raw);
        let events = drained(&mut a);
        let (sample, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Down);
        assert!(matches!(sample.class, SampleClass::Estimated { update_index: 100 }));
        assert!(a.pending_estimated.contains_key(&100));
    }

    #[test]
    fn ios_estimated_update_emits_revise_by_update_index() {
        let mut a = adapter();
        let mut down = ios_raw(7, IosTouchPhase::Down, (0.0, 0.0), 0.0, 0.2);
        down.estimation_update_index = Some(55);
        down.estimated_properties = IosEstimatedProperties::EXPECTS_UPDATE;
        a.handle_ios_raw(down);
        let _ = drained(&mut a);

        let revision = SampleRevision { pressure: Some(0.7), ..SampleRevision::default() };
        a.handle_ios_estimated_update(55, revision);

        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StylusEvent::Revise { update_index, revision, .. } => {
                assert_eq!(*update_index, 55);
                assert!((revision.pressure.unwrap() - 0.7).abs() < f32::EPSILON);
            }
            other => panic!("expected Revise, got {other:?}"),
        }
        assert!(!a.pending_estimated.contains_key(&55));
    }

    #[test]
    fn ios_out_of_order_estimated_updates_resolve_correctly() {
        let mut a = adapter();
        let mut d1 = ios_raw(1, IosTouchPhase::Down, (0.0, 0.0), 0.0, 0.2);
        d1.estimation_update_index = Some(10);
        d1.estimated_properties = IosEstimatedProperties::EXPECTS_UPDATE;
        a.handle_ios_raw(d1);
        let mut m1 = ios_raw(1, IosTouchPhase::Move, (1.0, 1.0), 0.008, 0.3);
        m1.estimation_update_index = Some(11);
        m1.estimated_properties = IosEstimatedProperties::EXPECTS_UPDATE;
        a.handle_ios_raw(m1);
        let _ = drained(&mut a);

        a.handle_ios_estimated_update(
            11,
            SampleRevision { pressure: Some(0.45), ..SampleRevision::default() },
        );
        a.handle_ios_estimated_update(
            10,
            SampleRevision { pressure: Some(0.25), ..SampleRevision::default() },
        );
        let events = drained(&mut a);
        assert_eq!(events.len(), 2, "each update emits one Revise");
        assert!(a.pending_estimated.is_empty());
    }

    #[test]
    fn ios_predicted_samples_tagged_predicted() {
        let mut a = adapter();
        a.handle_ios_raw(ios_raw(1, IosTouchPhase::Down, (0.0, 0.0), 0.0, 0.4));
        let _ = drained(&mut a);

        let mut p = ios_raw(1, IosTouchPhase::Move, (2.0, 2.0), 0.016, 0.5);
        p.predicted = true;
        a.handle_ios_raw(p);

        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert_eq!(sample.class, SampleClass::Predicted);
    }

    #[test]
    fn ios_tilt_decomposition_cardinals_and_zero() {
        let (x, y) = ios_altitude_azimuth_to_tilt_xy_deg(
            std::f32::consts::FRAC_PI_2,
            std::f32::consts::FRAC_PI_4,
        );
        assert!(x.abs() < 0.5 && y.abs() < 0.5, "near-perpendicular zero region");

        let (x, y) = ios_altitude_azimuth_to_tilt_xy_deg(std::f32::consts::FRAC_PI_4, 0.0);
        assert!(x > 30.0, "tilt toward +X should have positive x_deg, got {x}");
        assert!(y.abs() < 1.0, "no Y tilt component, got {y}");

        let (x, y) = ios_altitude_azimuth_to_tilt_xy_deg(
            std::f32::consts::FRAC_PI_4,
            std::f32::consts::FRAC_PI_2,
        );
        assert!(y > 30.0, "tilt toward +Y should have positive y_deg, got {y}");
        assert!(x.abs() < 1.0, "no X tilt component, got {x}");

        let (x, y) = ios_altitude_azimuth_to_tilt_xy_deg(0.0, 0.0);
        assert!((x - 90.0).abs() < 1.0, "flat toward +X: expect ~90°, got {x}");
        assert!(y.abs() < 1.0);
    }

    #[test]
    fn ios_full_stroke_down_move_up_cancel_phases() {
        let mut a = adapter();
        a.handle_ios_raw(ios_raw(1, IosTouchPhase::Down, (0.0, 0.0), 0.0, 0.3));
        a.handle_ios_raw(ios_raw(1, IosTouchPhase::Move, (1.0, 1.0), 0.008, 0.4));
        a.handle_ios_raw(ios_raw(1, IosTouchPhase::Up, (1.0, 1.0), 0.016, 0.0));
        let events = drained(&mut a);
        let phases: Vec<Phase> = events.iter().map(|e| expect_sample(e).1).collect();
        assert_eq!(phases, vec![Phase::Down, Phase::Move, Phase::Up]);
        assert!(a.active_pen_pointer.is_none());

        a.handle_ios_raw(ios_raw(2, IosTouchPhase::Down, (5.0, 5.0), 0.024, 0.3));
        a.handle_ios_raw(ios_raw(2, IosTouchPhase::Cancel, (5.0, 5.0), 0.030, 0.0));
        let events = drained(&mut a);
        let phases: Vec<Phase> = events.iter().map(|e| expect_sample(e).1).collect();
        assert_eq!(phases, vec![Phase::Down, Phase::Cancel]);
    }

    #[test]
    fn ios_hover_emits_hover_phase_on_mouse_pointer() {
        let mut a = adapter();
        let mut hover = ios_raw(99, IosTouchPhase::Hover, (5.0, 5.0), 0.0, 0.0);
        hover.pressure = 0.0;
        a.handle_ios_raw(hover);

        let events = drained(&mut a);
        let (sample, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Hover);
        assert_eq!(sample.pointer_id, PointerId::MOUSE);
    }

    #[test]
    fn ios_proximity_out_mid_stroke_synthesizes_cancel() {
        let mut a = adapter();
        a.handle_ios_raw(ios_raw(5, IosTouchPhase::Down, (0.0, 0.0), 0.0, 0.3));
        let _ = drained(&mut a);

        a.handle_ios_proximity(IosTouchProximitySample {
            touch_id: 5,
            pointing_device_type: ToolKind::Pen,
            caps: OPTIMISTIC_PEN_CAPS,
            is_entering: false,
        });
        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn ios_pencil_interaction_emits_variant() {
        let mut a = adapter();
        a.handle_ios_pencil_interaction(crate::PencilInteractionKind::Tap, None);
        let mut events: Vec<StylusEvent> = a.drain().collect();
        assert_eq!(events.len(), 1);
        match events.remove(0) {
            StylusEvent::PencilInteraction { kind, .. } => {
                assert_eq!(kind, crate::PencilInteractionKind::Tap);
            }
            other => panic!("expected PencilInteraction, got {other:?}"),
        }
    }
}
