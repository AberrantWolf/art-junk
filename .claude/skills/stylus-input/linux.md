---
name: stylus-input-linux
description: Linux pen/stylus backend for aj-stylus. Two paths — Wayland tablet-v2 (zwp_tablet_manager_v2) for native Wayland sessions, XInput2 for X11/XWayland. Both ship, selected at runtime.
---

# Linux stylus input

**Status: not started.** Unlike every other platform, Linux needs **two** backends shipped together: Wayland tablet protocol for native Wayland sessions, XInput2 for X11 and XWayland. Select at runtime via `raw-window-handle`.

## TL;DR

- Wayland: share winit's `wl_display`, create a **separate `EventQueue`** on the same connection, bind `zwp_tablet_manager_v2`, batch axis events between `frame` events into one sample.
- X11: open a **secondary `x11rb` connection**, enumerate XI2 slave devices, resolve valuator indices via atoms, subscribe to XI_Motion/ButtonPress/Release/ProximityIn/Out/HierarchyChanged.

Both run on a dedicated thread feeding samples into the adapter via the existing ingress path.

## The landscape

| Layer | Use? |
|---|---|
| Wayland tablet protocol (`zwp_tablet_v2`) | **Yes**, Wayland sessions |
| X11 / XInput2 (`XI2`) | **Yes**, X11 and XWayland |
| libinput (userspace abstraction) | No — compositors surface it to us |
| Raw evdev (`/dev/input/eventN`) | No — root/udev; broken under sandboxing |

Detect session at startup: inspect `raw-window-handle` → `RawDisplayHandle::Wayland` vs `Xlib`/`Xcb`. Both backends compile on Linux; exactly one runs.

## Wayland tablet protocol

### Object hierarchy

```
wl_registry
  └─ zwp_tablet_manager_v2
       └─ zwp_tablet_seat_v2      (per wl_seat)
            ├─ zwp_tablet_v2      (physical device)
            ├─ zwp_tablet_tool_v2 (pen, eraser, airbrush, ...)
            └─ zwp_tablet_pad_v2  (express keys — skip v1)
```

### Event flow for one stroke

```
tool.type(pen)                            setup
tool.capability(pressure|tilt|...)
tool.hardware_serial(0xABCD1234)
tool.done                                 ← tool fully described

tool.proximity_in(surface, tablet)
tool.frame(time_ms)

tool.down(serial)
tool.motion(x, y)                         surface coords, wl_fixed
tool.pressure(0..65535)
tool.tilt(x_deg, y_deg)                   wl_fixed degrees
tool.frame(time_ms)                       ← one hardware sample committed

... more motion/pressure/tilt/frame ...

tool.up
tool.frame(time_ms)

tool.proximity_out
tool.frame(time_ms)
```

**The `frame` event is the commit boundary.** Accumulate axis events in a scratch struct; emit to the adapter only on `frame`.

### Capabilities and types

Tool `type`: `pen | eraser | brush | pencil | airbrush | finger | mouse | lens`. Capabilities: `tilt | pressure | distance | rotation | slider | wheel`.

Map:
```rust
match tool_type {
    Pen | Brush | Pencil | Airbrush => ToolKind::Pen,
    Eraser => ToolKind::Eraser,
    Mouse | Lens => ToolKind::Mouse,
    _ => ToolKind::Unknown,
}
```

### Normalization

- Pressure: `0..=65535` → `/ 65535.0` → `0.0..=1.0`.
- Tilt: already degrees via `wl_fixed_to_double`. Range `-90..=90`. No rescale.
- Rotation: degrees (`0.0..=360.0`). Map to our `twist_deg`.
- Slider: `-65535..=65535` → `/ 65535.0` → `-1..=1` into `tangential_pressure`.

### Identity

`hardware_id_wacom` (u32 pair) and `hardware_serial` (u64) are per-physical-pen. Store as `ProximitySample.unique_id`. This is the **only** Linux path that gives a stable per-pen serial.

### Coordinates

`motion` is surface-local logical pixels as `wl_fixed` (24.8 fixed-point). Multiply by the surface's `scale` (via `wl_surface.preferred_buffer_scale` or `wp_fractional_scale_v1`). Must match the renderer's HiDPI scale.

### Winit integration — the hard part

Winit 0.30.x does not expose tablet protocol in its public API.

- ❌ Option A: upstream a patch. Too slow for near-term.
- ❌ Option B: separate `wayland-client` connection. Compositor won't route proximity to our surface — tablet events are scoped to the seat owning the surface's pointer focus, and a second connection gets its own client identity.
- ✅ **Option C: share the connection, separate queue.** Wayland explicitly permits multiple event queues per connection. Wrap winit's `wl_display` into a `Connection` via `Backend::from_foreign_display`, create a fresh `EventQueue`, bind globals on it, `blocking_dispatch` on a dedicated thread.

```rust
use wayland_client::{Connection, backend::Backend};

let RawWindowHandle::Wayland(wl) = window.raw_window_handle() else { unreachable!() };
let display_ptr = wl.display.as_ptr();

// SAFETY: winit keeps the display alive for the window's lifetime.
let backend = unsafe { Backend::from_foreign_display(display_ptr.cast()) };
let conn = Connection::from_backend(backend);
let mut queue = conn.new_event_queue::<TabletState>();
let qh = queue.handle();

let display = conn.display();
let _registry = display.get_registry(&qh, ());

let mut state = TabletState::default();
std::thread::spawn(move || loop {
    queue.blocking_dispatch(&mut state).unwrap();
});
```

Surface identity: store winit's raw `wl_surface` pointer from `WaylandWindowHandle::surface` and compare proxy IDs via `Proxy::id()` when `proximity_in` fires (our proxy of a shared-global surface refers to the same underlying compositor object).

### SCTK

`smithay-client-toolkit` 0.19 does **not** have a tablet wrapper. Drive the raw bindings from `wayland-protocols::wp::tablet::zv2::client::*`. A hand-rolled dispatcher is ~300 lines. File a tablet-wrapper request upstream.

### Estimated/Revise

Wayland delivers fully-resolved samples — no Estimated needed. Emit `SampleClass::Committed` directly.

## X11 / XInput2

### Device discovery

Requires XI2 ≥ 2.3.

```rust
let (conn, _) = x11rb::connect(None)?;
conn.xinput_xi_query_version(2, 3)?.reply()?;

let devices = conn.xinput_xi_query_device(xinput::Device::ALL as u16)?.reply()?;
```

Walk `info.classes`; a stylus carries `XIValuatorClass` entries with labels from interned atoms.

| Atom | Semantics |
|---|---|
| `Abs X`, `Abs Y` | Position |
| `Abs Pressure` | Pressure |
| `Abs Tilt X`, `Abs Tilt Y` | Tilt in degrees (under `xf86-input-libinput`) |
| `Abs Wheel` | Art Pen twist / airbrush wheel |
| `Abs Z` | Distance / hover |

Resolve atom names once at startup. Cache which valuator **index** is which axis *per device* — indices are not stable across devices.

### Event selection

Target **slave devices** not master. Master aggregates away per-device identity.

```rust
let mask = XIEventMask::MOTION
    | XIEventMask::BUTTON_PRESS | XIEventMask::BUTTON_RELEASE
    | XIEventMask::PROXIMITY_IN | XIEventMask::PROXIMITY_OUT
    | XIEventMask::DEVICE_CHANGED
    | XIEventMask::HIERARCHY_CHANGED
    | XIEventMask::ENTER | XIEventMask::LEAVE;
conn.xinput_xi_select_events(window_id, &[EventMask { deviceid, mask: vec![mask.into()] }])?;
```

Re-enumerate on `HierarchyChanged` (hotplug).

### Reading axes

`XI_Motion.axisvalues` is packed only for axes that changed. Cache per-device **last-known** values and merge — the adapter contract expects full samples, and X11 delivers deltas.

```rust
let mut iter = ev.axisvalues.iter();
for vi in 0..(ev.valuator_mask.len() * 32) {
    let word = ev.valuator_mask[vi / 32];
    if word & (1 << (vi % 32)) == 0 { continue; }
    let val = fp3232_to_f64(*iter.next().unwrap());
    update_cached_axis(device_id, vi, val);
}
```

### Normalization

- Pressure: `(raw - min) / (max - min)` using cached valuator min/max.
- Tilt: degrees directly under `xf86-input-libinput`. Under legacy Wacom driver, raw is in degree-equivalent units — use `(raw / max) * 90.0` defensively.
- Wheel: map to `twist_deg` linearly into `-180..=180`.
- Abs Z on airbrush: `tangential_pressure`.

### Pen vs eraser

Wacom exposes each tool as a separate slave device:
```
Wacom Intuos Pro L Pen stylus         id=12
Wacom Intuos Pro L Pen eraser         id=13
Wacom Intuos Pro L Pen cursor         id=14
```

Match device name substrings (`"stylus"`, `"eraser"`, `"cursor"`) at enumeration. Stable across Wacom for 15+ years. Same pattern under `xf86-input-libinput` for non-Wacom tablets.

No hardware serial — set `unique_id` to a hash of `(device_id, device_name)`. Not stable across sessions. Document the limitation.

### Proximity

`XI_ProximityIn/Out` fire under the Wacom driver. Under `xf86-input-libinput` they may not fire — detect at enumeration: if stylus device never emits proximity events, synthesize from `Enter`/`Leave`/first-motion/100ms silence timeout.

### Timestamps

`XIDeviceEvent.time` is ms since X server start — unreliable, not `CLOCK_MONOTONIC`. Ignore. Use `Instant::now()` at dispatch.

### Winit integration

Winit's X11 backend surfaces only basic `CursorMoved`/`MouseInput`. Open a **secondary `x11rb` connection** (`x11rb::connect(None)`). The server routes XI2 events to any client that called `XISelectEvents` on the window, regardless of connection. This is how GIMP and Krita have done it for 15+ years.

```rust
let (conn, _) = x11rb::connect(None)?;
let window_id = match handle {
    RawWindowHandle::Xlib(h) => h.window as u32,
    RawWindowHandle::Xcb(h) => h.window.get(),
    _ => unreachable!(),
};
// XIQueryVersion, enumerate, XISelectEvents on window_id, pump thread...
```

## Palm rejection

Wayland: pen events on the tablet protocol, touch on `wl_touch` — separate streams. X11: different XI2 devices. App-level suppression (drop touch while pen in proximity) is unchanged from macOS/Windows.

## Sandboxing

- Flatpak Wayland: `--socket=wayland`. No portal needed for tablet.
- Flatpak X11: `--socket=x11`. XInput2 is unsandboxed once X11 is available.
- Snap: `wayland` and/or `x11` plug.

## Rust crates

| Crate | Version | Purpose |
|---|---|---|
| `wayland-client` | 0.31 | Core dispatch (sans-I/O) |
| `wayland-protocols` | 0.31 | Tablet protocol bindings (`wp::tablet::zv2::client`) |
| `x11rb` | 0.13 | Safe XCB incl. `xinput` extension |
| `raw-window-handle` | 0.6 | Extract `wl_display` / XCB conn / window ID |

All MIT or MIT/Apache.

## Gotchas

- **Wacom mouse mode** (`xsetwacom --set ... Mode Relative`): only X11; Wayland always absolute. On X11, if `Abs Pressure` absent from a stylus-named device, warn + suggest `xsetwacom`.
- **XWayland**: native Wayland build sees Wayland tablet events; X11 build under XWayland sees XInput2. Runtime detection via `RawDisplayHandle` handles this.
- **NVIDIA + Wayland**: historically broken pointer locks; no known tablet-specific issues post-driver 535.
- **Compositor keyboard focus for pad events**: only matters for pad buttons (v2).
- **Proximity-out mid-stroke**: user lifted above hover range without up. Wayland emits `proximity_out` without `up` — treat as synthesized Up + Cancel (mirror macOS proximity-out path).
- **`done` event on tool creation**: don't emit samples until `done` arrives. Caps and type may still be streaming.
- **`frame` timestamps**: relative-ordering only, not absolute-wall. Don't mix with `Instant::now()` in the same timeline — use one clock per backend.

## Minimum viable implementation

### Wayland path

1. Detect Wayland via `RawDisplayHandle::Wayland`.
2. Construct `Connection::from_backend(Backend::from_foreign_display(wl_display))`. Create new `EventQueue`.
3. Bind `wl_seat` and `zwp_tablet_manager_v2` on a registry listener. `get_tablet_seat(&seat)` → `zwp_tablet_seat_v2`.
4. On `tool_added`: create `ToolState { id, type, caps, serial, pending_axes }`. Wait for `tool.done`.
5. Axis events (`motion`, `pressure`, `tilt`, `rotation`, `slider`, `wheel`, `button`): write into `pending_axes`. Do not emit.
6. On `frame`: build `WaylandRawSample` from `pending_axes`, tag origin `"wayland-tablet-v2"`, push to adapter.
7. `proximity_in`/`proximity_out`: emit `ProximitySample`. Match surface against winit's raw pointer.
8. Dedicated thread: `queue.blocking_dispatch(&mut state)` in loop.
9. On `ToolState` drop / `removed`: terminal `proximity_out` if still in proximity.

### X11 path

1. Detect X11 via `RawDisplayHandle::Xlib | Xcb`. Grab window ID.
2. Secondary `x11rb::connect(None)`. `xi_query_version(2, 3)`.
3. `xi_query_device(ALL)`, filter stylus/eraser/pen/tablet devices. Resolve valuator indices via interned atoms. Record `(min, max)` per valuator.
4. `xi_select_events(window_id, ...)` per slave device with the mask above.
5. Event pump thread: `conn.wait_for_event()`, match on the xinput events.
6. Per-device last-axes cache. Extract set bits, merge with cache, build full sample.
7. Normalize per §"Normalization" above.
8. Synthesize proximity under libinput-over-X11 via `Enter`/`Leave`/timeout if ProximityIn/Out never fire.
9. Stamp timestamps with `Instant::now()`.
10. Re-enumerate on `HierarchyChanged` and `DeviceChanged`.
11. Tag origin `"x11-xi2"`.
12. `unique_id` = hash of `(device_id, device_name)`; document non-stable-across-sessions.

## Testing

- **Hardware**: Wacom Intuos Pro, Cintiq, Huion Kamvas Pro 13 (needs kernel `hid-uclogic` since 5.11).
- **Compositor matrix minimum**: Mutter (GNOME) + sway (wlroots). Cover two families.
- **Headless**: `libinput debug-events --verbose` as reference. Record output on real hardware, replay by feeding synthesized `frame`-delimited events into the backend's internal dispatch. Expose dispatch entry point as `pub(crate)` for testing.
- **Xvfb + Xephyr**: supports XInput2 via input-wacom evdev driver; can exercise X11 path in CI with `evemu`.

## References

- [Wayland tablet v2 protocol XML](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/unstable/tablet/tablet-unstable-v2.xml)
- [Tablet v2 browsable](https://wayland.app/protocols/tablet-v2)
- [XInput2 protocol spec](https://www.x.org/releases/current/doc/inputproto/XI2proto.txt)
- [libinput tablet model](https://wayland.freedesktop.org/libinput/doc/latest/tablet-support.html)
- [xf86-input-wacom wiki](https://github.com/linuxwacom/xf86-input-wacom/wiki)
