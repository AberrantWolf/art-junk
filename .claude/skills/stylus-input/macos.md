---
name: stylus-input-macos
description: macOS pen/stylus backend for aj-stylus. Reference implementation — already shipped. Document what exists so other platforms can mirror its shape.
---

# macOS stylus input — reference implementation

**Status: shipped.** See `crates/aj-stylus/src/macos_tablet.rs` (backend) and the `handle_mac_raw` / `handle_mac_proximity` / `on_focus_lost` paths in `crates/aj-stylus/src/adapter.rs` (adapter integration).

This file documents *why* the macOS code looks the way it does, so the same patterns can be copied to other platforms.

## Approach

- **Coexist with winit**: install an `NSEvent` local monitor via `+[NSEvent addLocalMonitorForEventsMatchingMask:handler:]`. The monitor runs *before* `-[NSApplication sendEvent:]` dispatches to views, so we see tablet events before winit does. The handler returns each event pointer unchanged so winit continues its normal dispatch.
- **Suppress duplicates**: winit's mouse dispatch would otherwise produce a redundant `StylusEvent::Sample` for the same physical pen event (tablet drivers send both a tablet-subtype mouse event and winit converts that to a `WindowEvent::MouseInput`). The adapter's `active_pen_pointer` flag gates the winit mouse path until the pen-up.
- **Focus loss**: `NSApplicationDidResignActiveNotification` observer calls `adapter.on_focus_lost()` to synthesize Cancel samples.

## Key API details

### Event sources

Three event flavors carry tablet data:

1. **Mouse events with `subtype == NSEventSubtype::TabletPoint`** — the authoritative position source. These are `LeftMouseDown/Dragged/Up` / `MouseMoved` / etc. with extra tablet fields.
2. **Native `NSEventType::TabletPoint`** — Apple docs describe these firing between stylus-down and the first drag and during multi-tool use. On some drivers (including Wacom) they interleave with mouse-subtype events at the same physical instant. **Treat as revise-only**: refine a pending Estimated Down, but do not emit new Move samples. The mouse-subtype path is authoritative for position flow. Ignoring this rule produces visible zig-zags at integer-pixel boundaries.
3. **`NSEventType::TabletProximity`** — fires on enter/exit proximity. Also fires as a subtype on some drivers.

### Event mask (narrow is better)

```rust
let mask = NSEventMask::LeftMouseDown | LeftMouseUp | LeftMouseDragged
    | RightMouseDown | RightMouseUp | RightMouseDragged
    | OtherMouseDown | OtherMouseUp | OtherMouseDragged
    | MouseMoved | TabletPoint | TabletProximity;
```

`NSEventMaskAny` would force filtering of gesture/scroll/periodic events inside the handler — avoid.

### Mouse coalescing disabled

`NSEvent::setMouseCoalescingEnabled(false)`. macOS's default merges consecutive mouse-moved/dragged events when the app can't pull them fast enough, dropping a pen's 200 Hz rate down to display refresh. Drawing apps must opt out.

### Fields read from NSEvent

Per tablet sample:
- `locationInWindow` → convert via `contentView.convertPoint_fromView(pt, None)` then multiply by `backingScaleFactor`. `WinitView.isFlipped == YES` so the conversion does the window→view→top-left-origin flip.
- `timestamp()` — seconds since system boot, monotonic.
- `pressure()` — 0..=1 float.
- `tilt()` — `(x, y)` each -1..=1 ratio against full tilt. Multiply by 90.0 for degrees.
- `rotation()` — degrees; twist.
- `tangentialPressure()` — -1..=1.
- `buttonMask()` — bit 0 barrel, bit 1 secondary.
- `deviceID()` — stable for the lifetime of the device.
- `pointingDeviceType()` — Pen / Eraser / Cursor (puck).

Per proximity event:
- `deviceID()`, `uniqueID()` (per-physical-stylus serial, 0 if unsupported), `pointingDeviceType()`, `capabilityMask()`, `isEnteringProximity()`.

### Capability mask bits (IOKit)

`NSEvent.capabilityMask()` bits are defined in `<IOKit/hidsystem/IOLLEvent.h>`, not AppKit. The constants are redeclared in `macos_tablet.rs`:

```rust
const NX_TABLET_CAPABILITY_TILT_X: u64 = 1 << 7;
const NX_TABLET_CAPABILITY_TILT_Y: u64 = 1 << 8;
const NX_TABLET_CAPABILITY_PRESSURE: u64 = 1 << 10;
const NX_TABLET_CAPABILITY_TANGENTIAL_PRESSURE: u64 = 1 << 11;
const NX_TABLET_CAPABILITY_ROTATION: u64 = 1 << 13;
const NX_TABLET_CAPABILITY_BUTTONS: u64 = 1 << 6;
```

### Optimistic caps

If a sample arrives before any proximity (app launched with pen already hovering), synthesize a `PenState` with `OPTIMISTIC_PEN_CAPS` (pressure+tilt+twist+tangential). A cursor-puck would over-claim tilt/twist under this default; survivable. Proximity corrects it on the next enter/exit cycle.

### Timestamp translation

`NSEvent.timestamp` is seconds-since-boot. Translate into adapter's `Duration` via a lazy-populated `MacEpoch { first_nsevent_secs, adapter_duration_at_first }`. Delta from first is added to the anchor duration — monotonic and cheap.

## RAII teardown

`MacTabletBackend` holds `Retained<AnyObject>` monitor and observer tokens. Drop calls `NSEvent::removeMonitor` and `NSNotificationCenter::removeObserver`. Both must run on the main thread; the struct is `!Send` by virtue of `Retained<AnyObject>`, so Drop naturally stays on the thread that installed it (winit's main thread).

## Reentrancy

The NSEvent monitor handler runs on the main thread, and `on_focus_lost` runs on the main thread too, but we share the adapter through `Rc<RefCell<StylusAdapter>>`. Drop samples rather than panic on borrow conflict:

```rust
match adapter.try_borrow_mut() {
    Ok(mut a) => f(&mut a),
    Err(_) => log::warn!("NSEvent monitor: adapter borrowed elsewhere, sample dropped"),
}
```

Under normal use there's no reentrancy — both paths are on the same thread and don't re-enter each other. The guard is defensive.

## Dependencies

```toml
objc2 = "0.6"
objc2-app-kit = { version = "0.3", features = ["NSEvent", "NSResponder", "NSView", "NSWindow", "NSApplication", "NSRunningApplication"] }
objc2-foundation = { version = "0.3", features = ["NSNotification", "NSString", "NSGeometry", "NSThread"] }
block2 = "0.6"
```

## Hardware tested

Wacom Intuos / Cintiq via Wacom's macOS driver. Apple Pencil via Sidecar not tested (should arrive through the same path, but routed from the iPad).

## What this platform implementation doesn't cover

- **Predicted samples** — macOS has no predictedTouches equivalent. When iOS/Android/Web add `SampleClass::Predicted`, macOS stays on Committed-only.
- **Hover** — macOS only fires proximity-enter/exit, no hover position samples between them. Acceptable.
- **Per-pen serial** — `uniqueID` is populated on supported hardware; 0 otherwise.

## References

- [NSEvent class reference](https://developer.apple.com/documentation/appkit/nsevent)
- [addLocalMonitorForEventsMatchingMask:handler:](https://developer.apple.com/documentation/appkit/nsevent/1535472-addlocalmonitorforeventsmatching)
- [NSEventTypeTabletPoint / NSEventTypeTabletProximity](https://developer.apple.com/documentation/appkit/nseventtype)
- [NSEvent tablet properties (tilt, rotation, pressure, tangentialPressure, capabilityMask, pointingDeviceType, uniqueID)](https://developer.apple.com/documentation/appkit/nsevent#3656188)
- [NSEventSubtype.tabletPoint](https://developer.apple.com/documentation/appkit/nsevent/eventsubtype)
- [NSApplicationDidResignActiveNotification](https://developer.apple.com/documentation/appkit/nsapplication/1428705-didresignactivenotification)
- IOKit tablet capability mask bits: `<IOKit/hidsystem/IOLLEvent.h>` in the macOS SDK (see Apple Open Source: `IOHIDSystem`)
- [`objc2-app-kit` on docs.rs](https://docs.rs/objc2-app-kit/)
- Internal: `crates/aj-stylus/src/macos_tablet.rs`, `crates/aj-stylus/src/adapter.rs` — `handle_mac_raw`, `handle_mac_proximity`, `on_focus_lost`.

## What other platforms should copy

The shape:

- `<platform>_tablet.rs` — OS-specific event installation (monitor/subscribe/subclass), translation into `*RawSample`/`*ProximitySample`, delivery to adapter via `Rc<RefCell>`.
- `handle_<platform>_raw` / `handle_<platform>_proximity` on the adapter — platform-agnostic state manipulation.
- Backend struct with RAII Drop that unregisters whatever was registered.
- Caps constants pulled out as named constants at the top of the backend file.
- Pure-Rust tests that drive `handle_<platform>_raw` without touching OS APIs.
