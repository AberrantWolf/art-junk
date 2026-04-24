//! Linux Wayland tablet-v2 backend. Shares winit's `wl_display` via
//! `Backend::from_foreign_display` onto a fresh `EventQueue` pumped on a
//! dedicated thread; binds `wl_seat` + `zwp_tablet_manager_v2` on a registry
//! listener; accumulates per-tool axis events between `frame` commits into a
//! single `WaylandRawSample` and hands it to the shared `StylusAdapter` behind
//! an `Arc<Mutex<_>>`.
//!
//! Coordinate space: position arrives as `wl_fixed` surface-local logical
//! pixels; multiplied by the surface's preferred buffer scale to reach the
//! physical-pixel space the adapter expects. Timestamps: tablet-v2 `frame.time`
//! is millisecond-resolution relative-ordering from a compositor clock;
//! converted to seconds and anchored via `wayland_anchor`. One backend
//! instance == one clock timeline — do not mix with `Instant::now()` within
//! a single session.
//!
//! Design notes (see `.claude/skills/stylus-input/linux.md`):
//!
//! - A **separate `wayland-client` connection** would be rejected by the
//!   compositor: tablet proximity routing is scoped to the seat owning the
//!   surface's input focus, and a second connection has a distinct client
//!   identity. We therefore wrap the winit-owned `wl_display` pointer into
//!   our own `Backend` + `EventQueue`; the Wayland protocol explicitly permits
//!   multiple queues per connection.
//! - We **wait for `tool.done`** before forwarding any sample for a tool.
//!   Caps / type / serial arrive in a pre-`done` burst and a `proximity_in` /
//!   first axis event right after a new tool is possible — samples before
//!   `done` would be seen with stale caps and wrong type.
//! - `frame` is the commit boundary. Every other axis event updates a
//!   `pending_axes` scratch; only `frame` flushes to the adapter.
//! - SCTK 0.19 does not wrap the tablet protocol — this is a hand-rolled
//!   dispatcher over the raw `wayland-protocols::wp::tablet::zv2::client`
//!   bindings.

#![allow(unsafe_code)]
// The `Dispatch` trait forces us to bind each parameter, and the user-data
// type is `()` for every proxy here — binding `_: &()` is the idiomatic
// no-op but triggers clippy's ignored-unit-patterns lint on every impl.
#![allow(clippy::ignored_unit_patterns)]

use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::{Point, Tilt, ToolCaps, ToolKind};
use raw_window_handle::WaylandDisplayHandle;
use wayland_client::backend::Backend;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::tablet::zv2::client::{
    zwp_tablet_manager_v2, zwp_tablet_pad_v2, zwp_tablet_seat_v2, zwp_tablet_tool_v2, zwp_tablet_v2,
};

use crate::StylusAdapter;
use crate::adapter::{WaylandProximitySample, WaylandRawSample, WaylandTabletPhase};

#[derive(Debug, thiserror::Error)]
pub enum WaylandTabletInstallError {
    #[error("wayland display pointer was null")]
    NullDisplay,
    #[error("wayland dispatch failed: {0}")]
    Dispatch(String),
    #[error("failed to spawn wayland tablet dispatch thread: {0}")]
    ThreadSpawn(#[source] std::io::Error),
}

/// RAII guard owning the dedicated Wayland dispatch thread. Drop signals the
/// thread to exit on its next wake-up; the adapter clone it holds is released
/// when the thread joins.
pub struct WaylandTabletBackend {
    shutdown: Arc<Mutex<bool>>,
    thread: Option<JoinHandle<()>>,
}

impl WaylandTabletBackend {
    /// Install a tablet dispatcher on the given Wayland display. The
    /// `display` handle must outlive the returned guard — winit keeps its
    /// display alive for the lifetime of the window, and the guard should be
    /// dropped before the window is.
    ///
    /// # Safety
    ///
    /// `display.display` must point to a live `wl_display`. Callers obtain this
    /// from `raw-window-handle`'s `WaylandDisplayHandle`; winit guarantees the
    /// pointer is valid until the window is dropped.
    pub unsafe fn install(
        display: WaylandDisplayHandle,
        adapter: Arc<Mutex<StylusAdapter>>,
    ) -> Result<Self, WaylandTabletInstallError> {
        let display_ptr = display.display.as_ptr();
        if display_ptr.is_null() {
            return Err(WaylandTabletInstallError::NullDisplay);
        }

        // SAFETY: caller contract: `display.display` is a live `wl_display`
        // for at least the guard's lifetime. `from_foreign_display` does not
        // take ownership — the display is not disconnected on Backend drop,
        // matching winit's retention of the original.
        let backend = unsafe { Backend::from_foreign_display(display_ptr.cast()) };
        let conn = Connection::from_backend(backend);
        let queue: EventQueue<TabletDispatchState> = conn.new_event_queue();
        let qh = queue.handle();

        let display_proxy = conn.display();
        // Hold the registry on the state so it isn't destroyed at end of scope.
        let registry = display_proxy.get_registry(&qh, ());

        let shutdown = Arc::new(Mutex::new(false));
        let state = TabletDispatchState::new(adapter, registry);

        let thread_shutdown = Arc::clone(&shutdown);
        let thread = thread::Builder::new()
            .name("stylus-junk-wayland-tablet".to_string())
            .spawn(move || dispatch_loop(queue, state, &thread_shutdown))
            .map_err(WaylandTabletInstallError::ThreadSpawn)?;

        Ok(Self { shutdown, thread: Some(thread) })
    }
}

impl Drop for WaylandTabletBackend {
    fn drop(&mut self) {
        if let Ok(mut flag) = self.shutdown.lock() {
            *flag = true;
        }
        // Join will only complete on the next compositor event. That's fine:
        // we don't block app shutdown on the pen being idle — the thread is
        // detached-in-effect and will exit when the display is disconnected
        // by winit. We `take` the handle so Drop stays idempotent.
        drop(self.thread.take());
    }
}

fn dispatch_loop(
    mut queue: EventQueue<TabletDispatchState>,
    mut state: TabletDispatchState,
    shutdown: &Arc<Mutex<bool>>,
) {
    loop {
        match queue.blocking_dispatch(&mut state) {
            Ok(_) => {
                if *shutdown.lock().unwrap_or_else(std::sync::PoisonError::into_inner) {
                    break;
                }
            }
            Err(e) => {
                log::warn!("wayland tablet dispatch ended: {e}");
                break;
            }
        }
    }
}

struct TabletDispatchState {
    adapter: Arc<Mutex<StylusAdapter>>,
    _registry: wl_registry::WlRegistry,
    seat: Option<wl_seat::WlSeat>,
    manager: Option<zwp_tablet_manager_v2::ZwpTabletManagerV2>,
    tablet_seat: Option<zwp_tablet_seat_v2::ZwpTabletSeatV2>,
    tools: Vec<ToolEntry>,
    /// Physical-pixel scale factor to apply to `wl_fixed` surface-local
    /// coordinates. Without `wp_fractional_scale_v1` plumbing (future work),
    /// we default to 1.0; the renderer's `HiDPI` handling will still mostly
    /// work, with a visible stroke-offset issue on `HiDPI` compositors that we
    /// will wire through from winit's `preferred_buffer_scale`.
    surface_scale: f64,
    next_device_id: u32,
}

impl TabletDispatchState {
    fn new(adapter: Arc<Mutex<StylusAdapter>>, registry: wl_registry::WlRegistry) -> Self {
        Self {
            adapter,
            _registry: registry,
            seat: None,
            manager: None,
            tablet_seat: None,
            tools: Vec::new(),
            surface_scale: 1.0,
            next_device_id: 1,
        }
    }

    fn allocate_device_id(&mut self) -> u32 {
        let id = self.next_device_id;
        self.next_device_id = self.next_device_id.wrapping_add(1).max(1);
        id
    }

    fn try_attach_seat(&mut self, qh: &QueueHandle<Self>) {
        if self.tablet_seat.is_some() {
            return;
        }
        if let (Some(seat), Some(manager)) = (self.seat.as_ref(), self.manager.as_ref()) {
            self.tablet_seat = Some(manager.get_tablet_seat(seat, qh, ()));
        }
    }

    fn deliver_raw(&self, sample: WaylandRawSample) {
        match self.adapter.lock() {
            Ok(mut a) => a.handle_wayland_raw(sample),
            Err(poisoned) => {
                log::warn!("wayland tablet: adapter mutex poisoned, recovering for sample");
                let mut a = poisoned.into_inner();
                a.handle_wayland_raw(sample);
            }
        }
    }

    fn deliver_proximity(&self, prox: WaylandProximitySample) {
        match self.adapter.lock() {
            Ok(mut a) => a.handle_wayland_proximity(prox),
            Err(poisoned) => {
                log::warn!("wayland tablet: adapter mutex poisoned, recovering for proximity");
                let mut a = poisoned.into_inner();
                a.handle_wayland_proximity(prox);
            }
        }
    }

    fn tool_mut(&mut self, proxy: &zwp_tablet_tool_v2::ZwpTabletToolV2) -> Option<&mut ToolEntry> {
        self.tools.iter_mut().find(|t| t.proxy.id() == proxy.id())
    }

    fn flush_frame(&mut self, proxy: &zwp_tablet_tool_v2::ZwpTabletToolV2, time_ms: u32) {
        let scale = self.surface_scale;
        // Pull the snapshot we want to emit out from under the `&mut self`
        // borrow before handing the sample to the adapter.
        let Some((sample_opt, proximity_opt)) =
            self.tool_mut(proxy).map(|tool| tool.take_frame(time_ms, scale))
        else {
            return;
        };

        if let Some(prox) = proximity_opt {
            self.deliver_proximity(prox);
        }
        if let Some(sample) = sample_opt {
            self.deliver_raw(sample);
        }
    }
}

struct ToolEntry {
    proxy: zwp_tablet_tool_v2::ZwpTabletToolV2,
    device_id: u32,
    tool_kind: ToolKind,
    caps: ToolCaps,
    hardware_serial: Option<u64>,
    /// `true` after `zwp_tablet_tool_v2.done` — samples are suppressed before
    /// this because caps/type may still be in flight.
    done: bool,
    /// Last `proximity_in` we emitted but haven't paired with a matching
    /// `proximity_out` yet. Used to decide whether a pending frame should
    /// still flush (proximity-out-mid-stroke is the Cancel path).
    in_proximity: bool,
    /// Accumulated axis state between `frame` boundaries.
    pending: PendingAxes,
}

#[derive(Default)]
struct PendingAxes {
    /// Logical-coord `wl_fixed` motion — kept in f64 because `wl_fixed_to_double`
    /// returns f64.
    position: Option<(f64, f64)>,
    pressure: Option<f32>,
    tilt: Option<Tilt>,
    twist_deg: Option<f32>,
    tangential: Option<f32>,
    button_mask: u32,
    phase_transition: Option<WaylandTabletPhase>,
    enter: bool,
    leave: bool,
}

struct FrameOutcome {
    sample: Option<WaylandRawSample>,
    proximity: Option<WaylandProximitySample>,
}

impl ToolEntry {
    fn new(proxy: zwp_tablet_tool_v2::ZwpTabletToolV2, device_id: u32) -> Self {
        Self {
            proxy,
            device_id,
            tool_kind: ToolKind::Unknown,
            caps: ToolCaps::empty(),
            hardware_serial: None,
            done: false,
            in_proximity: false,
            pending: PendingAxes::default(),
        }
    }

    fn take_frame(
        &mut self,
        time_ms: u32,
        surface_scale: f64,
    ) -> (Option<WaylandRawSample>, Option<WaylandProximitySample>) {
        let outcome = self.flush(time_ms, surface_scale);
        self.pending = PendingAxes::default();
        (outcome.sample, outcome.proximity)
    }

    fn flush(&mut self, time_ms: u32, surface_scale: f64) -> FrameOutcome {
        if !self.done {
            // Pre-`done` events: stash but don't emit. Caps/type still
            // settling; adapter would see a wrongly-typed stroke.
            return FrameOutcome { sample: None, proximity: None };
        }

        let mut proximity = None;
        if self.pending.enter {
            self.in_proximity = true;
            proximity = Some(WaylandProximitySample {
                device_id: self.device_id,
                hardware_serial: self.hardware_serial,
                pointing_device_type: self.tool_kind,
                caps: self.caps,
                is_entering: true,
            });
        }

        let sample = self.pending.phase_transition.and_then(|phase| {
            // A `down` without an immediately-preceding `motion` is legal only
            // if `proximity_in` already carried a motion. Skip the sample if
            // we genuinely have no coordinate to place it at — the adapter's
            // Sample type requires a Point.
            self.pending.position.map(|(x, y)| WaylandRawSample {
                position_physical_px: Point::new(x * surface_scale, y * surface_scale),
                timestamp_secs: f64::from(time_ms) / 1000.0,
                pressure: self.pending.pressure.unwrap_or(0.0),
                tilt: self.pending.tilt,
                twist_deg: self.pending.twist_deg,
                tangential_pressure: self.pending.tangential,
                button_mask: self.pending.button_mask,
                device_id: self.device_id,
                hardware_serial: self.hardware_serial,
                pointing_device_type: self.tool_kind,
                source_phase: phase,
            })
        });

        if self.pending.leave {
            self.in_proximity = false;
            // An `enter` and a `leave` in the same frame is theoretically
            // possible under compositor replay. Prefer the leave when both
            // are set — it's the state-clearing signal the adapter needs to
            // synthesize Cancel on a mid-stroke proximity_out.
            proximity = Some(WaylandProximitySample {
                device_id: self.device_id,
                hardware_serial: self.hardware_serial,
                pointing_device_type: self.tool_kind,
                caps: self.caps,
                is_entering: false,
            });
        }

        FrameOutcome { sample, proximity }
    }
}

// --- registry dispatch ---

impl Dispatch<wl_registry::WlRegistry, ()> for TabletDispatchState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_seat" => {
                    let bound_version = version.min(wl_seat::WlSeat::interface().version);
                    let seat: wl_seat::WlSeat = registry.bind(name, bound_version, qh, ());
                    state.seat = Some(seat);
                    state.try_attach_seat(qh);
                }
                "zwp_tablet_manager_v2" => {
                    let bound =
                        version.min(zwp_tablet_manager_v2::ZwpTabletManagerV2::interface().version);
                    let manager: zwp_tablet_manager_v2::ZwpTabletManagerV2 =
                        registry.bind(name, bound, qh, ());
                    state.manager = Some(manager);
                    state.try_attach_seat(qh);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for TabletDispatchState {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_tablet_manager_v2::ZwpTabletManagerV2, ()> for TabletDispatchState {
    fn event(
        _: &mut Self,
        _: &zwp_tablet_manager_v2::ZwpTabletManagerV2,
        _: zwp_tablet_manager_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_tablet_seat_v2::ZwpTabletSeatV2, ()> for TabletDispatchState {
    fn event(
        state: &mut Self,
        _: &zwp_tablet_seat_v2::ZwpTabletSeatV2,
        event: zwp_tablet_seat_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // tablet_added / tool_added / pad_added create child proxies; dispatch
        // on those proxies lands in our per-interface impls. The seat event
        // payload is just the new_id + metadata already consumed by
        // `event_created_child` below, so there's nothing for us to do here.
        let _ = (state, event);
    }

    wayland_client::event_created_child!(TabletDispatchState, zwp_tablet_seat_v2::ZwpTabletSeatV2, [
        zwp_tablet_seat_v2::EVT_TABLET_ADDED_OPCODE => (zwp_tablet_v2::ZwpTabletV2, ()),
        zwp_tablet_seat_v2::EVT_TOOL_ADDED_OPCODE => (zwp_tablet_tool_v2::ZwpTabletToolV2, ()),
        zwp_tablet_seat_v2::EVT_PAD_ADDED_OPCODE => (zwp_tablet_pad_v2::ZwpTabletPadV2, ()),
    ]);
}

impl Dispatch<zwp_tablet_v2::ZwpTabletV2, ()> for TabletDispatchState {
    fn event(
        _: &mut Self,
        _: &zwp_tablet_v2::ZwpTabletV2,
        _: zwp_tablet_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_tablet_pad_v2::ZwpTabletPadV2, ()> for TabletDispatchState {
    fn event(
        _: &mut Self,
        _: &zwp_tablet_pad_v2::ZwpTabletPadV2,
        _: zwp_tablet_pad_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // v1 wiring: pad (express-keys) events are ignored. See linux.md.
    }
}

impl Dispatch<zwp_tablet_tool_v2::ZwpTabletToolV2, ()> for TabletDispatchState {
    // The event enum has ~20 arms — splitting this into per-arm helpers
    // fragments the axis-accumulation logic across methods and hurts
    // readability more than the line count.
    #[allow(clippy::too_many_lines)]
    fn event(
        state: &mut Self,
        proxy: &zwp_tablet_tool_v2::ZwpTabletToolV2,
        event: zwp_tablet_tool_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Ensure a ToolEntry exists. The seat dispatched the `tool_added`
        // creation; the first event here may be a description event that
        // created the proxy before we registered it.
        if state.tools.iter().all(|t| t.proxy.id() != proxy.id()) {
            let id = state.allocate_device_id();
            state.tools.push(ToolEntry::new(proxy.clone(), id));
        }

        match event {
            zwp_tablet_tool_v2::Event::Type { tool_type } => {
                if let Some(tool) = state.tool_mut(proxy) {
                    tool.tool_kind = map_tool_type(tool_type);
                }
            }
            zwp_tablet_tool_v2::Event::HardwareSerial {
                hardware_serial_hi,
                hardware_serial_lo,
            } => {
                if let Some(tool) = state.tool_mut(proxy) {
                    tool.hardware_serial =
                        Some((u64::from(hardware_serial_hi) << 32) | u64::from(hardware_serial_lo));
                }
            }
            zwp_tablet_tool_v2::Event::HardwareIdWacom { hardware_id_hi, hardware_id_lo } => {
                if let Some(tool) = state.tool_mut(proxy)
                    && tool.hardware_serial.is_none()
                {
                    // Wacom hardware_id is the fallback when hardware_serial
                    // is absent — same role (per-physical-pen identity).
                    tool.hardware_serial =
                        Some((u64::from(hardware_id_hi) << 32) | u64::from(hardware_id_lo));
                }
            }
            zwp_tablet_tool_v2::Event::Capability { capability } => {
                if let Some(tool) = state.tool_mut(proxy)
                    && let WEnum::Value(cap) = capability
                {
                    tool.caps |= map_capability(cap);
                }
            }
            zwp_tablet_tool_v2::Event::Done => {
                if let Some(tool) = state.tool_mut(proxy) {
                    tool.done = true;
                }
            }
            zwp_tablet_tool_v2::Event::Removed => {
                // If the tool is still in proximity, synthesize a final
                // proximity-out so the adapter tears down any stroke.
                if let Some(tool) = state.tool_mut(proxy) {
                    tool.pending.leave = true;
                }
                state.flush_frame(proxy, 0);
                state.tools.retain(|t| t.proxy.id() != proxy.id());
            }
            zwp_tablet_tool_v2::Event::ProximityIn { serial: _, tablet: _, surface: _ } => {
                if let Some(tool) = state.tool_mut(proxy) {
                    tool.pending.enter = true;
                }
            }
            zwp_tablet_tool_v2::Event::ProximityOut => {
                if let Some(tool) = state.tool_mut(proxy) {
                    tool.pending.leave = true;
                }
            }
            zwp_tablet_tool_v2::Event::Down { serial: _ } => {
                if let Some(tool) = state.tool_mut(proxy) {
                    tool.pending.phase_transition = Some(WaylandTabletPhase::Down);
                }
            }
            zwp_tablet_tool_v2::Event::Up => {
                if let Some(tool) = state.tool_mut(proxy) {
                    tool.pending.phase_transition = Some(WaylandTabletPhase::Up);
                }
            }
            zwp_tablet_tool_v2::Event::Motion { x, y } => {
                if let Some(tool) = state.tool_mut(proxy) {
                    tool.pending.position = Some((x, y));
                    if tool.pending.phase_transition.is_none() {
                        tool.pending.phase_transition = Some(WaylandTabletPhase::Move);
                    }
                }
            }
            zwp_tablet_tool_v2::Event::Pressure { pressure } => {
                if let Some(tool) = state.tool_mut(proxy) {
                    // Spec: 0..=65535 normalized pressure. Routing through f64
                    // before narrowing keeps the normalization exact; the
                    // final cast to f32 is lossless because the result is in
                    // [0, 1].
                    #[allow(clippy::cast_possible_truncation)]
                    let p = (f64::from(pressure) / 65535.0) as f32;
                    tool.pending.pressure = Some(p);
                }
            }
            zwp_tablet_tool_v2::Event::Tilt { tilt_x, tilt_y } => {
                if let Some(tool) = state.tool_mut(proxy) {
                    // wl_fixed in degrees per spec; no rescale.
                    #[allow(clippy::cast_possible_truncation)]
                    let tilt = Tilt { x_deg: tilt_x as f32, y_deg: tilt_y as f32 };
                    tool.pending.tilt = Some(tilt);
                }
            }
            zwp_tablet_tool_v2::Event::Rotation { degrees } => {
                if let Some(tool) = state.tool_mut(proxy) {
                    #[allow(clippy::cast_possible_truncation)]
                    let deg = degrees as f32;
                    tool.pending.twist_deg = Some(deg);
                }
            }
            zwp_tablet_tool_v2::Event::Slider { position } => {
                if let Some(tool) = state.tool_mut(proxy) {
                    // Spec: -65535..=65535 → -1..=1.
                    #[allow(clippy::cast_possible_truncation)]
                    let t = (f64::from(position) / 65535.0) as f32;
                    tool.pending.tangential = Some(t);
                }
            }
            zwp_tablet_tool_v2::Event::Button { serial: _, button, state: btn_state } => {
                if let Some(tool) = state.tool_mut(proxy) {
                    let pressed =
                        matches!(btn_state, WEnum::Value(zwp_tablet_tool_v2::ButtonState::Pressed));
                    let bit = linux_button_to_bit(button);
                    if pressed {
                        tool.pending.button_mask |= bit;
                    } else {
                        tool.pending.button_mask &= !bit;
                    }
                }
            }
            zwp_tablet_tool_v2::Event::Frame { time } => {
                state.flush_frame(proxy, time);
            }
            // Distance (hover-only cue) and Wheel (art-pen / airbrush — the
            // cap/tool-type disambiguation for mapping to tangential vs twist
            // happens out-of-band) are intentionally dropped until we have a
            // hover phase and a consolidated wheel-mapping site.
            _ => {}
        }
    }
}

// --- mapping helpers ---

fn map_tool_type(ty: WEnum<zwp_tablet_tool_v2::Type>) -> ToolKind {
    use zwp_tablet_tool_v2::Type;
    match ty {
        WEnum::Value(Type::Pen | Type::Brush | Type::Pencil | Type::Airbrush) => ToolKind::Pen,
        WEnum::Value(Type::Eraser) => ToolKind::Eraser,
        WEnum::Value(Type::Mouse | Type::Lens) => ToolKind::Mouse,
        _ => ToolKind::Unknown,
    }
}

fn map_capability(cap: zwp_tablet_tool_v2::Capability) -> ToolCaps {
    use zwp_tablet_tool_v2::Capability;
    match cap {
        Capability::Tilt => ToolCaps::TILT,
        Capability::Pressure => ToolCaps::PRESSURE,
        Capability::Distance => ToolCaps::DISTANCE,
        Capability::Rotation => ToolCaps::TWIST,
        // Slider and wheel both surface as a scalar tangential-pressure axis
        // in our model; the tool kind disambiguates (airbrush slider vs art
        // pen wheel) for UI purposes downstream.
        Capability::Slider | Capability::Wheel => ToolCaps::TANGENTIAL_PRESSURE,
        _ => ToolCaps::empty(),
    }
}

fn linux_button_to_bit(button: u32) -> u32 {
    // `<linux/input-event-codes.h>` pen buttons we care about:
    //   BTN_STYLUS  = 0x14b — lower barrel
    //   BTN_STYLUS2 = 0x14c — upper barrel
    //   BTN_STYLUS3 = 0x149 — Wacom Pro Pen 3 third button
    match button {
        0x14b => 0x1,
        0x14c => 0x2,
        0x149 => 0x4,
        _ => 0,
    }
}
