---
name: stylus-input
description: Reference for implementing per-platform pen/stylus input backends behind the `stylus-junk` crate's `StylusAdapter` seam. Cross-platform strategy, decision matrix, and links to per-platform details (iOS, Android, Windows, macOS, Linux, Web).
---

# Stylus input — cross-platform reference

`stylus-junk` centralizes pen/touch/mouse logic in a single `StylusAdapter`. Each platform backend translates OS-native events into a small, platform-agnostic shape and calls the adapter. Backends are thin; the adapter owns identity, timestamps, proximity state, estimated/revise cycles, and focus-loss cancellation.

This skill is the launchpad. Jump to a platform file for implementation detail.

- [macOS](macos.md) — done. NSEvent local monitor + focus-loss observer. Reference for all other platforms.
- [iOS](ios.md) — UITouch + UIEvent coalesced/predicted, UIPencilInteraction, hover via UIHoverGestureRecognizer.
- [Windows](windows.md) — Pointer Input API (WM_POINTER) with SetWindowSubclass, GetPointerPenInfoHistory.
- [Linux](linux.md) — two paths: Wayland tablet-v2 and X11/XInput2. Both ship.
- [Android](android.md) — MotionEvent via `android-activity`, with historical samples and optional MotionPredictor.
- [Web](web.md) — Pointer Events on the canvas, getCoalescedEvents / getPredictedEvents, `touch-action: none`.

---

## The adapter seam

Every backend produces two kinds of records and feeds them into the adapter on the main thread:

- **`*RawSample`** — one physical sample: position (physical px, client-relative), timestamp (monotonic seconds), pressure (0..=1), tilt (x_deg, y_deg, each -90..=90), twist_deg (0..=360 or signed), tangential_pressure (-1..=1), button_mask, device_id, pointing_device_type, source_phase (Down/Move/Up), origin tag.
- **`*ProximitySample`** — per-tool: device_id, unique_id (serial if available), pointing_device_type, ToolCaps bitfield, is_entering.

The macOS path in `crates/stylus-junk/src/adapter.rs` uses `MacTabletRawSample` / `MacTabletProximitySample` and entry points `handle_mac_raw` / `handle_mac_proximity` / `on_focus_lost`. Each new platform should mirror this shape:

1. Define `<platform>RawSample` and `<platform>ProximitySample` in the adapter crate (`pub(crate)`).
2. Define `pub(crate) fn handle_<platform>_raw` and `handle_<platform>_proximity` on `StylusAdapter`, following the same Estimated+Revise pattern already in `handle_mac_raw`.
3. Put OS-specific translation (objc2 / windows-sys / wayland-client / ndk / wasm-bindgen) in a sibling file (`<platform>_tablet.rs`). The adapter stays platform-agnostic.

Don't leak OS types into `stylus-junk::adapter`. The benefit is uniform behavior and testability — the macOS tests drive `handle_mac_raw` directly without any AppKit dependency; each new platform gets the same treatment.

## What the adapter already handles (do not re-implement in backends)

- **PointerId allocation** per gesture (monotonic counter; `PointerId::MOUSE` reserved for mouse).
- **Timestamp translation** via `translate_mac_timestamp` / equivalent — pull this into a shared `PlatformTimestampAnchor` when you add a new path (first iOS or Windows backend; the extraction is a small refactor).
- **Optimistic caps** when a first sample arrives before any proximity.
- **Estimated + Revise** cycle: first Down sample emitted as `SampleClass::Estimated { update_index }`, next sample emits a `StylusEvent::Revise` refining pressure/tilt/twist/tangential.
- **Mouse-suppression gate** (`active_pen_pointer`) so winit's duplicate mouse events during a pen stroke are dropped.
- **Focus-loss cancellation** (`on_focus_lost`) synthesizing Cancel phases for every active pointer.
- **Proximity-out-with-active-stroke** → synthesizes Cancel.

Every backend calls these existing entry points. What backends *must* do is translate OS native events and, where the OS provides them, deliver **proximity events with the right cap bits** and **Down samples with the right `source_phase`**. The adapter handles everything else.

## Feature matrix

| Feature | macOS | iOS | Windows | Linux (Wl) | Linux (X11) | Android | Web |
|---|---|---|---|---|---|---|---|
| Pressure | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Tilt X/Y | ✓ | ✓ (via altitude/azimuth) | ✓ | ✓ | ✓ (libinput) | ✓ (via tilt+orient) | ✓ |
| Twist | ✓ (Wacom Art Pen) | ✓ (Pencil Pro, 17.5+) | ✓ (Surface Slim Pen 2) | ✓ | limited | — | ✓ |
| Tangential | ✓ (airbrush) | — | — | ✓ (slider) | ✓ (Abs Z airbrush) | — | ✓ |
| Hover / proximity | ✓ | ✓ (M1+ iPads, 16.1+) | ✓ (INRANGE) | ✓ (explicit) | patchy | ✓ (stylus HW-dep) | ✓ (synthesized) |
| Eraser tool | ✓ (pointing device type) | via `UITouchProperties` + hardware | ✓ (PEN_FLAG_ERASER + INVERTED) | ✓ (tool type) | ✓ (separate device) | ✓ (TOOL_TYPE_ERASER) | ✓ (buttons & 0x20) |
| Coalesced / history | winit-native | UIEvent.coalescedTouches | GetPointerPenInfoHistory | per-`frame` batching | valuator deltas | getHistorical* | getCoalescedEvents |
| Predicted | — | UIEvent.predictedTouches | — | — | — | MotionPredictor (API 33) or Jetpack | getPredictedEvents |
| Stable per-pen ID | uniqueID | — (by update index within gesture) | sourceDevice handle | hardware_serial | synthesized | InputDevice | pointerId (per-gesture only) |

## Palm rejection — consistent policy

All platforms: **while any pen pointer is in proximity or contact, suppress concurrent touch pointers on the canvas.** The adapter's `active_pen_pointer` gate is the enforcement point for winit-routed events. Backends that surface touch independently (Wayland, XI2, Android MotionEvent) should apply the same rule before handing touch to the engine.

Additionally: honor platform cancel signals.
- iOS: `touchesCancelled`.
- Android: `ACTION_CANCEL` or `MotionEvent.FLAG_CANCELED`.
- Windows: `WM_POINTERCAPTURECHANGED`.
- Wayland: compositor `proximity_out` mid-stroke.
- X11: `XI_ProximityOut` mid-stroke (treat as synthesized Up + Cancel).

## Decision matrix (what to pick when there's more than one path)

| Platform | Options considered | Choice | Why |
|---|---|---|---|
| iOS | PencilKit vs raw UITouch | raw UITouch + UIEvent | PencilKit owns the surface; we have our own renderer. |
| iOS | Overlay UIView vs swizzle winit's | overlay with `hitTest` pass-through for non-pencil | Survives winit version bumps; finger touches still reach winit for egui. |
| Windows | WM_POINTER vs RealTimeStylus vs WinRT Ink | WM_POINTER | MS's own recommended modern path; in-thread; covers all axes. |
| Windows | Subclass HWND vs extend winit | SetWindowSubclass | Symmetric to macOS approach; no winit fork. |
| Linux | Wayland tablet vs X11 XI2 vs evdev | both tablet+XI2 | Session-dependent; XWayland means X11 path still matters. Evdev dies under sandboxing. |
| Linux | Shared wl_display vs separate connection | shared (multiple queues) | Tablet events are scoped to surfaces owned by the client; separate connections lose focus routing. |
| Linux/X11 | Master pointer vs slave devices | slave devices | Master aggregates away per-device identity and axis resolution. |
| Android | winit Touch vs raw MotionEvent | raw MotionEvent via android-activity | winit's Touch drops tilt/buttons/historical/tool type. |
| Android | MotionPredictor vs Jetpack MotionEventPredictor | Jetpack | Back-ports prediction to API 24+, same surface. |
| Web | Pointer Events vs WebHID/WebUSB vs Touch Events | Pointer Events | Portable; no permission prompt; covers all axes. |
| Web | winit's pointer events vs own listeners | own listeners on canvas | winit's web backend drops pressure/tilt/coalesced/predicted. |

## Implementation order suggestion

Testable-on-hand devices first: **Windows → Linux (both paths) → iOS**. Then: **Web → Android**.

- Windows first because the Pointer API is a single well-specified path and Surface/Wacom coverage is broad; tests the generalization of the existing mac-centric adapter code.
- Linux second because it has the highest architectural risk (two paths, winit surgery for Wayland). Doing it early flushes out adapter design issues.
- iOS third because it needs an overlay `UIView` subclass via `objc2::define_class!` and is fiddly but tractable.
- Web and Android last — both are straightforward once the adapter has settled.

## Adapter refactors the first non-mac backend should do

The existing adapter has macOS-specific fields (`mac_epoch`, `pending_estimated`). When you add the second platform:

1. Rename `MacEpoch` → `PlatformTimestampAnchor`, move to `adapter.rs` top, keep the mac-specific `translate_mac_timestamp` as a thin wrapper (or generalize).
2. Keep `pending_estimated` — it's already platform-agnostic.
3. Add a `SampleClass::Predicted { predict_index }` to `aj-core::Sample` when implementing the first platform with prediction (iOS, Android, or Web). Predicted samples never commit to the scene; the engine draws them to the Placeholder layer only.

## Cross-cutting gotchas

- **Physical vs logical pixels.** The adapter expects physical pixels, canvas-relative. Each backend is responsible for the conversion. Document which space the backend is in at the top of each file.
- **Monotonic timestamps.** All OS timestamps we use are monotonic (CLOCK_MONOTONIC-equivalent); none go backwards across sleep-for-our-process-alive-long-enough. Wall-clock time is never used.
- **Device serial portability.** macOS gives us `uniqueID`. Wayland gives us `hardware_serial`. Windows / Android / iOS / X11 / Web do **not** give a stable per-pen serial across sessions — use device-id hashes and document the limitation.
- **Proximity semantics vary.** Wayland and Windows have explicit proximity events. iOS and Android surface proximity only if the device hardware reports hover (Apple M1+ iPad; Pixel Tablet with USI; Samsung S-Pen). X11 is patchy. Web has no explicit proximity but can synthesize from `pointerover`+`pressure===0`. The adapter tolerates proximity being absent; backends should emit it when they can.
- **Coexistence with winit mouse events.** When the OS fires both pen *and* synthetic mouse events (macOS, Windows, sometimes Linux drivers), the adapter's `active_pen_pointer` gate suppresses the duplicate. Backends don't need to swallow mouse themselves except on Windows where OEM drivers send WM_MOUSEMOVE independently of WM_POINTER (swallow in the subclass).

## Testing approach that survives across platforms

The macOS tests in `stylus-junk/src/adapter.rs` drive `handle_mac_raw` and `handle_mac_proximity` directly with pure-Rust fixtures — no AppKit, no device. Every new platform gets the same treatment: `handle_<platform>_raw` is public-in-crate, and tests feed hand-constructed sample streams.

Golden-image tests (render a pen stroke from a replayed sample log, `dssim`-compare to committed golden) are the end-to-end check. Record a short stroke from real hardware on each platform we own; replay through the whole stack.

## Where to start editing code

- `crates/stylus-junk/src/lib.rs` — module registration and public types (`StylusEvent`, `Phase`).
- `crates/stylus-junk/src/adapter.rs` — platform-agnostic state machine. Extend with `handle_<platform>_*` entry points.
- `crates/stylus-junk/src/<platform>_tablet.rs` — new, per platform.
- `crates/stylus-junk/Cargo.toml` — add `[target.'cfg(...)'.dependencies]` for each platform's FFI crates.
- `crates/aj-app/src/main.rs` — install the backend (pass `Rc<RefCell<StylusAdapter>>`, RAII guard stored on the App).

The macOS pair (`macos_tablet.rs` + `adapter.rs::handle_mac_*`) is the reference implementation. Read it before writing a new backend.

## Where the research came from

Each per-platform file has a **References** section at the bottom pointing to the canonical vendor docs. Summary of best-source-per-topic:

- **Apple (iOS / macOS)** — `developer.apple.com/documentation/uikit` and `/appkit` are authoritative. WWDC session videos (e.g. WWDC24 10214 for Pencil Pro) are the best source for *why* APIs look the way they do. `objc2-*` crates on `docs.rs` for the Rust bindings.
- **Microsoft (Windows)** — `learn.microsoft.com/en-us/windows/win32/inputmsg/` for the pointer API. Microsoft's own inking guidance under `/windows/apps/design/input/pen-and-stylus-interactions` is good for intent; the Win32 API pages are the implementation reference. `windows-sys` crate on `docs.rs` for bindings.
- **Google (Android)** — `developer.android.com/reference/android/view/MotionEvent` is canonical. NDK reference at `/ndk/reference/group/input` covers the C API we actually call. `ndk` and `android-activity` on `docs.rs`.
- **Wayland / X11 (Linux)** — `wayland.app/protocols/tablet-v2` is the browsable spec; source of truth is the XML in `wayland-protocols`. XInput2 spec at `x.org/releases/current/doc/inputproto/XI2proto.txt`. `libinput` docs at `wayland.freedesktop.org/libinput/doc/latest/tablet-support.html` for behavior notes. `xf86-input-wacom` wiki on GitHub for Wacom-on-X11 specifics. `wayland-client` / `wayland-protocols` / `x11rb` on `docs.rs`.
- **W3C / MDN (Web)** — `w3c.github.io/pointerevents/` is the spec; MDN at `developer.mozilla.org/en-US/docs/Web/API/Pointer_events` is the practical reference with compatibility tables. `web-sys` on `docs.rs` for bindings.

When reading any of these, **verify against the `docs.rs` version of the crate actually in use** — API surfaces (`objc2-ui-kit`, `windows-sys`, `ndk`, `wayland-protocols`, `web-sys`) shift between releases. The per-platform files name specific crate versions current at time of writing; treat them as a floor and update on lock-file bump.
