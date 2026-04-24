//! X11 / XInput2 tablet-input backend. Opens a secondary `x11rb` connection
//! (sharing winit's is impossible — x11rb owns the protocol parser state),
//! enumerates stylus/eraser/cursor slave devices, caches per-device valuator
//! metadata, subscribes to XI2 motion/button/enter/leave/hierarchy events on
//! the winit-owned window, and pumps the connection on a dedicated thread
//! feeding `StylusAdapter` through `handle_x11_raw` / `handle_x11_proximity`.
//!
//! Design decisions (see `.claude/skills/stylus-input/linux.md`):
//!
//! - **Slave devices, not master.** The XI2 master pointer aggregates events
//!   from every slave, stripping per-device valuator identity and the
//!   stylus/eraser/cursor distinction that Wacom (and `xf86-input-libinput`)
//!   expose as separate slaves. We subscribe to each matching slave directly.
//! - **Unreliable timestamps.** `XIDeviceEvent.time` is milliseconds since X
//!   server start, not `CLOCK_MONOTONIC`; ignore and stamp with
//!   `Instant::now()` in the adapter.
//! - **Synthesized proximity.** `XI_ProximityIn/Out` only fire under the
//!   legacy Wacom driver. Under `xf86-input-libinput` they may not; we
//!   synthesize proximity from `XI_Enter` / `XI_Leave` so behavior is
//!   uniform across drivers. Real `ProximityIn/Out` still delivers via the
//!   `XinputProximityIn/Out` branches.
//! - **Secondary connection.** XI2 events route to every client that
//!   `XISelectEvents`'d on the window — our connection sees them even though
//!   winit owns a separate one. This is how GIMP/Krita have worked since XI2
//!   landed.
//! - **Axis delta merge.** `XI_Motion` packs only the valuators that changed;
//!   we merge with a per-device cache so the adapter receives full samples.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::{Point, Tilt, ToolCaps, ToolKind};
use raw_window_handle::{RawWindowHandle, XcbWindowHandle, XlibWindowHandle};
use x11rb::connection::Connection;
use x11rb::errors::{ConnectError, ConnectionError, ReplyError};
use x11rb::protocol::Event;
use x11rb::protocol::xinput::{
    self, ConnectionExt as XInputExt, DeviceClassData, DeviceClassDataValuator, DeviceId,
    DeviceType, EventMask, Fp3232, HierarchyMask, XIDeviceInfo, XIEventMask,
};
use x11rb::protocol::xproto::{Atom, ConnectionExt as XProtoExt, Window};
use x11rb::rust_connection::RustConnection;

use crate::StylusAdapter;
use crate::adapter::{X11ProximitySample, X11RawSample, X11TabletPhase};

#[derive(Debug, thiserror::Error)]
pub enum X11TabletInstallError {
    #[error("raw-window-handle is not Xlib or Xcb (backend is X11-only)")]
    NotX11Window,
    #[error("could not connect to the X server: {0}")]
    Connect(#[from] ConnectError),
    #[error("X server does not support XInput2 2.3 (tablet features require it)")]
    XInput2Unavailable,
    #[error("X protocol error during setup: {0}")]
    Protocol(String),
}

impl From<ConnectionError> for X11TabletInstallError {
    fn from(value: ConnectionError) -> Self {
        Self::Protocol(value.to_string())
    }
}

impl From<ReplyError> for X11TabletInstallError {
    fn from(value: ReplyError) -> Self {
        Self::Protocol(value.to_string())
    }
}

/// RAII guard for the X11 tablet backend. Drop signals the pump thread to
/// stop on its next event; we don't join it because `wait_for_event` blocks
/// on the connection and cannot be preempted without sending a wake-up event
/// to the X server. The process typically keeps this guard for the full
/// session, so early teardown is a non-goal.
pub struct X11TabletBackend {
    running: Arc<AtomicBool>,
    _pump: std::thread::JoinHandle<()>,
}

impl X11TabletBackend {
    /// Open a dedicated X11 connection, enumerate tablet slave devices,
    /// subscribe to their events on `handle`, and spawn the pump thread.
    pub fn install(
        adapter: Arc<Mutex<StylusAdapter>>,
        handle: RawWindowHandle,
    ) -> Result<Self, X11TabletInstallError> {
        let window = extract_window_id(&handle)?;

        let (conn, _screen) = x11rb::connect(None).map_err(X11TabletInstallError::from)?;
        let conn = Arc::new(conn);

        let reply = conn.xinput_xi_query_version(2, 3)?.reply()?;
        if reply.major_version < 2 || (reply.major_version == 2 && reply.minor_version < 3) {
            return Err(X11TabletInstallError::XInput2Unavailable);
        }

        let atoms = AxisAtoms::intern(&conn)?;
        let mut devices = DeviceCache::default();
        devices.reenumerate(&conn, &atoms)?;
        devices.select_events(&conn, window)?;

        let running = Arc::new(AtomicBool::new(true));
        let pump_conn = Arc::clone(&conn);
        let pump_running = Arc::clone(&running);
        let pump = std::thread::Builder::new()
            .name("stylus-junk-x11-pump".into())
            .spawn(move || {
                pump_loop(pump_conn, pump_running, atoms, devices, adapter, window);
            })
            .map_err(|e| X11TabletInstallError::Protocol(e.to_string()))?;

        Ok(Self { running, _pump: pump })
    }
}

impl Drop for X11TabletBackend {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
    }
}

fn extract_window_id(handle: &RawWindowHandle) -> Result<Window, X11TabletInstallError> {
    match handle {
        RawWindowHandle::Xlib(XlibWindowHandle { window, .. }) => Ok(*window as Window),
        RawWindowHandle::Xcb(XcbWindowHandle { window, .. }) => Ok(window.get()),
        _ => Err(X11TabletInstallError::NotX11Window),
    }
}

/// Interned atoms for the valuator labels we care about. Atoms are per-server
/// and stable for the connection's lifetime, so we intern once at startup and
/// then equality-compare by `Atom` against `DeviceClassDataValuator.label`.
#[derive(Debug, Clone, Copy)]
struct AxisAtoms {
    abs_x: Atom,
    abs_y: Atom,
    abs_pressure: Atom,
    abs_tilt_x: Atom,
    abs_tilt_y: Atom,
    abs_wheel: Atom,
    abs_z: Atom,
}

impl AxisAtoms {
    fn intern(conn: &RustConnection) -> Result<Self, X11TabletInstallError> {
        let abs_x = conn.intern_atom(false, b"Abs X")?;
        let abs_y = conn.intern_atom(false, b"Abs Y")?;
        let abs_pressure = conn.intern_atom(false, b"Abs Pressure")?;
        let abs_tilt_x = conn.intern_atom(false, b"Abs Tilt X")?;
        let abs_tilt_y = conn.intern_atom(false, b"Abs Tilt Y")?;
        let abs_wheel = conn.intern_atom(false, b"Abs Wheel")?;
        let abs_z = conn.intern_atom(false, b"Abs Z")?;
        Ok(Self {
            abs_x: abs_x.reply()?.atom,
            abs_y: abs_y.reply()?.atom,
            abs_pressure: abs_pressure.reply()?.atom,
            abs_tilt_x: abs_tilt_x.reply()?.atom,
            abs_tilt_y: abs_tilt_y.reply()?.atom,
            abs_wheel: abs_wheel.reply()?.atom,
            abs_z: abs_z.reply()?.atom,
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct AxisRef {
    index: u16,
    min: f64,
    max: f64,
}

#[derive(Debug, Clone, Copy, Default)]
struct DeviceAxes {
    x: Option<AxisRef>,
    y: Option<AxisRef>,
    pressure: Option<AxisRef>,
    tilt_x: Option<AxisRef>,
    tilt_y: Option<AxisRef>,
    wheel: Option<AxisRef>,
    z: Option<AxisRef>,
}

/// Whether the backend treats this device's tilt valuator values as raw
/// scaled units (legacy Wacom) or already-in-degrees (libinput). We can't
/// reliably detect which driver is behind a device through the protocol, so
/// we default to Libinput (modern driver) and expose the legacy path for
/// future hardware-specific overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TiltScaling {
    Libinput,
    LegacyWacom,
}

#[derive(Debug, Clone)]
struct StylusDevice {
    id: DeviceId,
    name: String,
    tool: ToolKind,
    axes: DeviceAxes,
    tilt_scaling: TiltScaling,
    unique_id: u64,
    /// Last fully-resolved axis values from this device, used to merge the
    /// partial valuator set XI2 delivers on each motion event into a full
    /// sample. Keyed by the symbolic axis, not the valuator index, so the
    /// merge is stable across re-enumerations that shift indices.
    last: LastAxes,
    /// True once we've seen any proximity or synthesized it from an Enter
    /// event. Used to suppress re-synthesizing on every subsequent motion.
    in_proximity: bool,
    /// True if this device has ever delivered a real XI_ProximityIn/Out.
    /// Tracked so backends know whether to synthesize from Enter/Leave.
    has_real_proximity: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct LastAxes {
    x: f64,
    y: f64,
    pressure: f64,
    tilt_x: f64,
    tilt_y: f64,
    wheel: f64,
    z: f64,
}

#[derive(Debug, Default)]
struct DeviceCache {
    by_id: HashMap<DeviceId, StylusDevice>,
}

impl DeviceCache {
    fn reenumerate(
        &mut self,
        conn: &RustConnection,
        atoms: &AxisAtoms,
    ) -> Result<(), X11TabletInstallError> {
        let reply = conn.xinput_xi_query_device(u16::from(xinput::Device::ALL))?.reply()?;
        let mut fresh: HashMap<DeviceId, StylusDevice> = HashMap::new();
        for info in reply.infos {
            if !is_slave_pointer(info.type_) || !info.enabled {
                continue;
            }
            let Some(tool) = classify_by_name(&info.name) else {
                continue;
            };
            let axes = resolve_axes(&info, atoms);
            // If the device doesn't expose Abs X/Y it isn't usefully a
            // stylus — skip it silently rather than warn: libinput may
            // publish pad-button-only devices whose names match "pen".
            if axes.x.is_none() || axes.y.is_none() {
                continue;
            }
            let name = String::from_utf8_lossy(&info.name).into_owned();
            let unique_id = hash_unique_id(info.deviceid, &name);
            let prior = self.by_id.remove(&info.deviceid);
            fresh.insert(
                info.deviceid,
                StylusDevice {
                    id: info.deviceid,
                    name,
                    tool,
                    axes,
                    tilt_scaling: TiltScaling::Libinput,
                    unique_id,
                    last: prior.as_ref().map(|p| p.last).unwrap_or_default(),
                    in_proximity: prior.as_ref().is_some_and(|p| p.in_proximity),
                    has_real_proximity: prior.as_ref().is_some_and(|p| p.has_real_proximity),
                },
            );
        }
        self.by_id = fresh;
        Ok(())
    }

    fn select_events(
        &self,
        conn: &RustConnection,
        window: Window,
    ) -> Result<(), X11TabletInstallError> {
        // Subscribe on the global (deviceid=0) so HierarchyChanged fires
        // whenever any device is added/removed — even from devices we
        // don't otherwise care about.
        let hierarchy_only = EventMask {
            deviceid: u16::from(xinput::Device::ALL),
            mask: vec![XIEventMask::HIERARCHY],
        };

        let per_device = self.by_id.keys().map(|&id| EventMask {
            deviceid: id,
            mask: vec![
                XIEventMask::MOTION
                    | XIEventMask::BUTTON_PRESS
                    | XIEventMask::BUTTON_RELEASE
                    | XIEventMask::ENTER
                    | XIEventMask::LEAVE
                    | XIEventMask::DEVICE_CHANGED,
            ],
        });

        let masks: Vec<EventMask> = std::iter::once(hierarchy_only).chain(per_device).collect();
        conn.xinput_xi_select_events(window, &masks)?.check()?;
        Ok(())
    }
}

fn is_slave_pointer(ty: DeviceType) -> bool {
    // Only subscribe to slaves: masters aggregate away the per-device
    // valuator ranges and stylus/eraser/cursor distinction we rely on.
    ty == DeviceType::SLAVE_POINTER
}

fn classify_by_name(name: &[u8]) -> Option<ToolKind> {
    let lower = name.to_ascii_lowercase();
    let matches = |needle: &[u8]| lower.windows(needle.len()).any(|w| w == needle);
    if matches(b"eraser") {
        Some(ToolKind::Eraser)
    } else if matches(b"stylus") || matches(b"pen") {
        Some(ToolKind::Pen)
    } else if matches(b"cursor") {
        Some(ToolKind::Mouse)
    } else {
        None
    }
}

fn resolve_axes(info: &XIDeviceInfo, atoms: &AxisAtoms) -> DeviceAxes {
    let mut axes = DeviceAxes::default();
    for class in &info.classes {
        let DeviceClassData::Valuator(v) = &class.data else {
            continue;
        };
        let axis_ref =
            AxisRef { index: v.number, min: fp3232_to_f64(v.min), max: fp3232_to_f64(v.max) };
        match v.label {
            l if l == atoms.abs_x => axes.x = Some(axis_ref),
            l if l == atoms.abs_y => axes.y = Some(axis_ref),
            l if l == atoms.abs_pressure => axes.pressure = Some(axis_ref),
            l if l == atoms.abs_tilt_x => axes.tilt_x = Some(axis_ref),
            l if l == atoms.abs_tilt_y => axes.tilt_y = Some(axis_ref),
            l if l == atoms.abs_wheel => axes.wheel = Some(axis_ref),
            l if l == atoms.abs_z => axes.z = Some(axis_ref),
            _ => {}
        }
        // Silence unused in case an individual field isn't referenced.
        let _ = v as &DeviceClassDataValuator;
    }
    axes
}

fn hash_unique_id(device_id: DeviceId, name: &str) -> u64 {
    // No hardware serial on X11 — the closest we have to a stable id is
    // (numeric id, name). Neither is stable across sessions.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    device_id.hash(&mut hasher);
    name.hash(&mut hasher);
    hasher.finish()
}

fn fp3232_to_f64(v: Fp3232) -> f64 {
    // Fp3232 = signed 32.32 fixed-point. Integral part is i32; fractional is
    // u32 quantising the interval [0, 1). Decode by combining with the right
    // scale: total = integral + frac / 2^32.
    f64::from(v.integral) + f64::from(v.frac) / f64::from(u32::MAX)
}

fn ranged_normalize(raw: f64, min: f64, max: f64) -> f64 {
    let span = max - min;
    if span.abs() < f64::EPSILON { 0.0 } else { ((raw - min) / span).clamp(0.0, 1.0) }
}

fn caps_for_device(axes: &DeviceAxes) -> ToolCaps {
    let mut caps = ToolCaps::empty();
    if axes.pressure.is_some() {
        caps |= ToolCaps::PRESSURE;
    }
    if axes.tilt_x.is_some() || axes.tilt_y.is_some() {
        caps |= ToolCaps::TILT;
    }
    if axes.wheel.is_some() {
        caps |= ToolCaps::TWIST;
    }
    if axes.z.is_some() {
        caps |= ToolCaps::TANGENTIAL_PRESSURE;
    }
    caps
}

fn pump_loop(
    conn: Arc<RustConnection>,
    running: Arc<AtomicBool>,
    atoms: AxisAtoms,
    mut devices: DeviceCache,
    adapter: Arc<Mutex<StylusAdapter>>,
    window: Window,
) {
    while running.load(Ordering::Acquire) {
        let event = match conn.wait_for_event() {
            Ok(e) => e,
            Err(e) => {
                log::warn!("x11 tablet pump: {e}, exiting");
                return;
            }
        };
        if !running.load(Ordering::Acquire) {
            return;
        }
        handle_event(event, &conn, &atoms, &mut devices, &adapter, window);
    }
}

fn handle_event(
    event: Event,
    conn: &RustConnection,
    atoms: &AxisAtoms,
    devices: &mut DeviceCache,
    adapter: &Arc<Mutex<StylusAdapter>>,
    window: Window,
) {
    match event {
        Event::XinputMotion(e) => {
            on_axis_event(&e, X11TabletPhase::Move, devices, adapter);
        }
        Event::XinputButtonPress(e) => {
            on_axis_event(&e, X11TabletPhase::Down, devices, adapter);
        }
        Event::XinputButtonRelease(e) => {
            on_axis_event(&e, X11TabletPhase::Up, devices, adapter);
        }
        Event::XinputEnter(e) => {
            synthesize_proximity(e.sourceid, true, devices, adapter);
        }
        Event::XinputLeave(e) => {
            synthesize_proximity(e.sourceid, false, devices, adapter);
        }
        Event::XinputProximityIn(e) => {
            if let Some(dev) = devices.by_id.get_mut(&(e.device_id as DeviceId)) {
                dev.has_real_proximity = true;
            }
            real_proximity(e.device_id as DeviceId, true, devices, adapter);
        }
        Event::XinputProximityOut(e) => {
            if let Some(dev) = devices.by_id.get_mut(&(e.device_id as DeviceId)) {
                dev.has_real_proximity = true;
            }
            real_proximity(e.device_id as DeviceId, false, devices, adapter);
        }
        Event::XinputHierarchy(e) => {
            // Hotplug of tablet hardware or driver restart. Re-enumerate and
            // re-select; a no-op in the common case where the event was for
            // a keyboard or some other device we don't care about.
            let flags_relevant = e.flags
                & (HierarchyMask::SLAVE_ADDED
                    | HierarchyMask::SLAVE_REMOVED
                    | HierarchyMask::SLAVE_ATTACHED
                    | HierarchyMask::SLAVE_DETACHED)
                != HierarchyMask::from(0u32);
            if flags_relevant {
                if let Err(err) = devices.reenumerate(conn, atoms) {
                    log::warn!("x11 tablet: re-enumeration failed: {err}");
                }
                if let Err(err) = devices.select_events(conn, window) {
                    log::warn!("x11 tablet: re-selecting events failed: {err}");
                }
            }
        }
        Event::XinputDeviceChanged(_e) => {
            // Axis remap (e.g. Wacom tool change) — re-query classes for the
            // specific device so our axis indices stay accurate.
            if let Err(err) = devices.reenumerate(conn, atoms) {
                log::warn!("x11 tablet: device-changed re-enumeration failed: {err}");
            }
        }
        _ => {}
    }
}

fn on_axis_event(
    e: &xinput::ButtonPressEvent,
    phase: X11TabletPhase,
    devices: &mut DeviceCache,
    adapter: &Arc<Mutex<StylusAdapter>>,
) {
    // XI2 reports events for both the source slave and the master pointer;
    // we match on `sourceid` so each physical pen is distinct.
    let Some(device) = devices.by_id.get_mut(&e.sourceid) else {
        return;
    };

    merge_axis_values(device, &e.valuator_mask, &e.axisvalues);

    // Synthesize proximity-in on the first axis event if Enter hasn't fired.
    // Covers the case where the pen touches down inside the window without
    // traversing the boundary (launched with pen already on tablet).
    let mut prox_to_emit: Option<X11ProximitySample> = None;
    if !device.in_proximity {
        device.in_proximity = true;
        prox_to_emit = Some(X11ProximitySample {
            device_id: u32::from(device.id),
            unique_id: Some(device.unique_id),
            pointing_device_type: device.tool,
            caps: caps_for_device(&device.axes),
            is_entering: true,
        });
    }

    let sample = build_sample(device, phase);
    let device_tool = device.tool;
    let device_id_u32 = u32::from(device.id);

    if let Ok(mut a) = adapter.lock() {
        if let Some(p) = prox_to_emit {
            a.handle_x11_proximity(p);
        }
        a.handle_x11_raw(sample);
        // For eraser / cursor tools we still want button events tracked,
        // but the sample's `pointing_device_type` already carries the
        // eraser flag so the adapter tags INVERTED buttons. Nothing
        // further to do here.
        let _ = (device_tool, device_id_u32);
    } else {
        log::warn!("x11 tablet: adapter mutex poisoned, sample dropped");
    }
}

fn merge_axis_values(device: &mut StylusDevice, mask_words: &[u32], values: &[Fp3232]) {
    // Walk the set bits of `mask_words` in ascending valuator-index order
    // (the XI2 wire order); each set bit consumes one Fp3232 from `values`.
    let mut value_iter = values.iter();
    let bit_count = mask_words.len() * 32;
    for bit in 0..bit_count {
        let word = mask_words[bit / 32];
        if word & (1u32 << (bit % 32)) == 0 {
            continue;
        }
        let Some(raw) = value_iter.next() else {
            break;
        };
        let raw = fp3232_to_f64(*raw);
        let idx = bit as u16;
        if let Some(a) = device.axes.x
            && a.index == idx
        {
            device.last.x = raw;
        }
        if let Some(a) = device.axes.y
            && a.index == idx
        {
            device.last.y = raw;
        }
        if let Some(a) = device.axes.pressure
            && a.index == idx
        {
            device.last.pressure = raw;
        }
        if let Some(a) = device.axes.tilt_x
            && a.index == idx
        {
            device.last.tilt_x = raw;
        }
        if let Some(a) = device.axes.tilt_y
            && a.index == idx
        {
            device.last.tilt_y = raw;
        }
        if let Some(a) = device.axes.wheel
            && a.index == idx
        {
            device.last.wheel = raw;
        }
        if let Some(a) = device.axes.z
            && a.index == idx
        {
            device.last.z = raw;
        }
    }
}

fn build_sample(device: &StylusDevice, phase: X11TabletPhase) -> X11RawSample {
    let position = Point::new(device.last.x, device.last.y);
    let pressure = device
        .axes
        .pressure
        .map(|a| ranged_normalize(device.last.pressure, a.min, a.max) as f32)
        .unwrap_or(0.5);
    let tilt = compute_tilt(device);
    let twist_deg = device
        .axes
        .wheel
        .map(|a| {
            // Linear map wheel range to -180..=180 degrees of twist.
            let span = a.max - a.min;
            if span.abs() < f64::EPSILON {
                0.0
            } else {
                (((device.last.wheel - a.min) / span) * 360.0 - 180.0) as f32
            }
        })
        .unwrap_or(0.0);
    let tangential_pressure = device
        .axes
        .z
        .map(|a| {
            let span = a.max - a.min;
            if span.abs() < f64::EPSILON {
                0.0
            } else {
                (((device.last.z - a.min) / span) * 2.0 - 1.0) as f32
            }
        })
        .unwrap_or(0.0);

    X11RawSample {
        position_physical_px: position,
        pressure,
        tilt,
        twist_deg,
        tangential_pressure,
        button_mask: 0,
        device_id: u32::from(device.id),
        pointing_device_type: device.tool,
        source_phase: phase,
    }
}

fn compute_tilt(device: &StylusDevice) -> Tilt {
    let scale = |axis: Option<AxisRef>, raw: f64| -> f32 {
        let Some(a) = axis else {
            return 0.0;
        };
        match device.tilt_scaling {
            TiltScaling::Libinput => raw as f32,
            TiltScaling::LegacyWacom => {
                // Wacom driver reports tilt as driver-dependent raw units;
                // scale defensively into -90..=90 using the valuator range.
                let span = a.max - a.min;
                if span.abs() < f64::EPSILON {
                    0.0
                } else {
                    (((raw - a.min) / span) * 180.0 - 90.0) as f32
                }
            }
        }
    };
    Tilt {
        x_deg: scale(device.axes.tilt_x, device.last.tilt_x),
        y_deg: scale(device.axes.tilt_y, device.last.tilt_y),
    }
}

fn synthesize_proximity(
    source_id: DeviceId,
    entering: bool,
    devices: &mut DeviceCache,
    adapter: &Arc<Mutex<StylusAdapter>>,
) {
    let Some(device) = devices.by_id.get_mut(&source_id) else {
        return;
    };
    if device.has_real_proximity {
        // Real XI_ProximityIn/Out events are authoritative — don't
        // double-emit from Enter/Leave.
        return;
    }
    if device.in_proximity == entering {
        return;
    }
    device.in_proximity = entering;
    let sample = X11ProximitySample {
        device_id: u32::from(device.id),
        unique_id: Some(device.unique_id),
        pointing_device_type: device.tool,
        caps: caps_for_device(&device.axes),
        is_entering: entering,
    };
    if let Ok(mut a) = adapter.lock() {
        a.handle_x11_proximity(sample);
    }
}

fn real_proximity(
    source_id: DeviceId,
    entering: bool,
    devices: &mut DeviceCache,
    adapter: &Arc<Mutex<StylusAdapter>>,
) {
    let Some(device) = devices.by_id.get_mut(&source_id) else {
        return;
    };
    device.in_proximity = entering;
    let sample = X11ProximitySample {
        device_id: u32::from(device.id),
        unique_id: Some(device.unique_id),
        pointing_device_type: device.tool,
        caps: caps_for_device(&device.axes),
        is_entering: entering,
    };
    if let Ok(mut a) = adapter.lock() {
        a.handle_x11_proximity(sample);
    }
}
