---
name: stylus-input-web
description: Web (WASM) pen/stylus backend for aj-stylus via Pointer Events on the canvas element, with getCoalescedEvents / getPredictedEvents and touch-action none.
---

# Web stylus input

**Status: not started.** Target: modern browsers (Chrome, Edge, Safari 13+, Firefox). Apple Pencil on iPad Safari, Surface Pen on Edge/Chrome, Wacom anywhere.

## TL;DR

Bypass winit's Web pointer handling — it drops pressure/tilt/coalesced/predicted. Attach our own `pointerdown/move/up/cancel/over/out/leave/lostpointercapture` listeners to the canvas via `wasm-bindgen` closures. On move, iterate `getCoalescedEvents()` and `getPredictedEvents()`. Set `touch-action: none` on the canvas. Call `canvas.setPointerCapture(pointerId)` on down. Eraser = `pointerType === "pen" && buttons & 0x20`.

## Why Pointer Events

| Option | Decision |
|---|---|
| **Pointer Events** | **Use**. Portable; no permission prompt; covers pressure/tilt/twist/tangential/buttons/width/height/predicted/coalesced. |
| Touch Events | Legacy fallback. Both fire on modern browsers; prefer Pointer and `preventDefault()` the Touch side. |
| WebHID / WebUSB | No. Permission prompt; Firefox/Safari missing; no BT pens. |

## Event axes

`PointerEvent` (extends `MouseEvent`):

| Property | Range | Notes |
|---|---|---|
| `pointerType` | `"pen"` / `"touch"` / `"mouse"` | No `"eraser"` type — eraser via `buttons & 0x20` |
| `pressure` | `0.0..=1.0` | `0.5` default for mouse with button, `0` on hover |
| `tangentialPressure` | `-1..=1` | barrel wheel |
| `tiltX`, `tiltY` | `-90..=90` deg | independent axes |
| `twist` | `0..=359` deg | rotation around shaft |
| `width`, `height` | CSS px | contact geometry |
| `buttons` | bitfield | 0x1 primary, 0x2 right, 0x4 middle, 0x8 back, 0x10 fwd, **0x20 eraser** |
| `isPrimary` | bool | true for the primary pointer in a multi-touch gesture |
| `timeStamp` | DOMHighResTimeStamp | ms since navigation start, monotonic per page |
| `pointerId` | i32 | stable for the gesture; not across gestures |

## getCoalescedEvents()

Browsers align pointer events to frame cadence. A pen sampling at 240–500 Hz drops samples unless you call `event.getCoalescedEvents()` on each `pointermove`. Returns full `PointerEvent`s with per-sample timestamps and axes.

**Essential.** Without it, strokes are stepped at fast pen speeds.

Support: Chrome/Edge/Firefox 79+/Safari 13+. If Safari returns an empty array for the synthetic primary, fall back to the dispatched event.

## getPredictedEvents()

Extrapolated future samples from recent velocity. Returns a sequence.

Support: Chrome/Edge yes. Safari recent. Firefox not yet. Feature-detect at startup:

```js
const hasPredicted = "getPredictedEvents" in PointerEvent.prototype;
```

Tag predicted samples `SampleClass::Predicted`; draw to placeholder layer only; discard on next real sample.

## Pointer capture

On `pointerdown`, call `canvas.setPointerCapture(e.pointerId)`. Subsequent moves/ups route to our canvas even when the pointer drifts into chrome or off-screen. Release is automatic on up/cancel.

## CSS — opt out of gestures

```css
canvas.aj-surface {
  touch-action: none;           /* critical: disables browser pan/zoom */
  user-select: none;
  -webkit-user-select: none;
  -webkit-touch-callout: none;  /* iOS press-and-hold preview */
  pointer-events: auto;
}
```

Also: `oncontextmenu="return false"` on the canvas element (no long-press context menu).

## Coordinate conversion

Pointer events give CSS pixels relative to viewport. Adapter wants **physical pixels relative to canvas origin**.

```rust
fn to_canvas_physical(e: &PointerEvent, canvas: &HtmlCanvasElement) -> (f64, f64) {
    let rect = canvas.get_bounding_client_rect();
    let dpr = web_sys::window().unwrap().device_pixel_ratio();
    let x = (e.client_x() as f64 - rect.left()) * dpr;
    let y = (e.client_y() as f64 - rect.top()) * dpr;
    (x, y)
}
```

Cache `rect` only for the duration of one dispatched event (including its coalesced/predicted sub-events). Layout can change between events. `visualViewport.scale` during pinch-zoom shifts the mapping; ignore v1 (drawing during page zoom is degenerate).

## Timestamps

`event.timeStamp` is DOMHighResTimeStamp (ms since navigation start), monotonic. Divide by 1000 for adapter seconds. Coalesced sub-events carry their own `timeStamp` — use those, not the parent's.

## Tool mapping

```rust
let tool = match (e.pointer_type().as_str(), e.buttons() & 0x20 != 0) {
    ("pen",   true)  => ToolKind::Eraser,
    ("pen",   false) => ToolKind::Pen,
    ("mouse",    _)  => ToolKind::Mouse,
    ("touch",    _)  => ToolKind::Unknown,   // palm-rejection gated
    _                => ToolKind::Unknown,
};
```

Surface Pen and Wacom on Windows report eraser via `buttons & 0x20`. Wacom on macOS may emit the eraser as a separate `pointerId` with tool flipping — our code handles it since `pointerId` stays stable per-gesture.

## Proximity / hover

Pointer Events has no explicit proximity event. Synthesize:
- `pointerover`/`pointerenter` → `ProximitySample { is_entering: true }`.
- `pointerout`/`pointerleave` → `ProximitySample { is_entering: false }`. If a stroke was active, adapter synthesizes cancel.
- Hover on Apple Pencil (M2+ iPad, iPadOS 16+, Safari): `pointermove` with `pointerType === "pen"` and `pressure === 0`. Adapter's existing hover path handles it.

## Palm rejection

Browser separates `"touch"` / `"pen"` already. Track live pen `pointerId`s:

```rust
struct PenGate { active_pens: HashSet<i32> }
```

While non-empty, drop `"touch"` events. Release on last pen's `pointerup`/`pointercancel`.

## Winit integration

Winit 0.30.x's web backend uses Pointer Events internally but surfaces a coarse subset — no pressure, tilt, twist, coalesced, predicted. Ignore winit's pointer events for the drawing surface; attach our own listeners:

1. Pull the `HtmlCanvasElement` (via `raw-window-handle` or winit's web extension).
2. `canvas.add_event_listener_with_callback("pointermove", ...)` with `{ passive: false, capture: true }` so `preventDefault()` works.
3. Listeners coexist with winit's — events dispatch to all.

## Rust crates

```toml
wasm-bindgen = "0.2"
js-sys = "0.3"

[dependencies.web-sys]
version = "0.3"
features = [
  "Window", "Document", "Element", "HtmlCanvasElement",
  "EventTarget", "AddEventListenerOptions",
  "MouseEvent", "PointerEvent", "TouchEvent",
  "DomRect", "VisualViewport",
]
```

If `get_coalesced_events` or `get_predicted_events` are missing from the installed `web-sys`, add a shim:

```rust
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(method, js_name = getCoalescedEvents)]
    fn get_coalesced_events(this: &web_sys::PointerEvent) -> js_sys::Array;
    #[wasm_bindgen(method, js_name = getPredictedEvents)]
    fn get_predicted_events(this: &web_sys::PointerEvent) -> js_sys::Array;
}
```

## Handler sketch

```rust
pub fn attach(canvas: &HtmlCanvasElement, adapter: Rc<RefCell<StylusAdapter>>) -> WebStylusBridge {
    let mut closures = Vec::new();

    // pointerdown
    {
        let canvas_cap = canvas.clone();
        let adapter = adapter.clone();
        let cb = Closure::<dyn FnMut(_)>::new(move |e: PointerEvent| {
            e.prevent_default();
            let _ = canvas_cap.set_pointer_capture(e.pointer_id());
            dispatch_sample(&mut adapter.borrow_mut(), &canvas_cap, &e, Phase::Down);
        });
        canvas.add_event_listener_with_callback("pointerdown", cb.as_ref().unchecked_ref()).unwrap();
        closures.push(cb);
    }

    // pointermove with coalesced + predicted drain
    {
        let canvas_cap = canvas.clone();
        let adapter = adapter.clone();
        let cb = Closure::<dyn FnMut(_)>::new(move |e: PointerEvent| {
            e.prevent_default();
            let mut a = adapter.borrow_mut();
            for sub in e.get_coalesced_events().iter() {
                let sub: PointerEvent = sub.dyn_into().unwrap();
                dispatch_sample(&mut a, &canvas_cap, &sub, Phase::Move);
            }
            for pred in e.get_predicted_events().iter() {
                let pred: PointerEvent = pred.dyn_into().unwrap();
                dispatch_predicted(&mut a, &canvas_cap, &pred);
            }
        });
        canvas.add_event_listener_with_callback("pointermove", cb.as_ref().unchecked_ref()).unwrap();
        closures.push(cb);
    }

    // pointerup / pointercancel / pointerleave / lostpointercapture — Up or Cancel phase
    // pointerover / pointerout — ProximitySample synthesize

    WebStylusBridge { _closures: closures }
}
```

## Gotchas

- **Firefox on Linux**: some Wacom pens historically reported `tiltX/tiltY === 0` even when OS had values. Detect on first pen-down; flag confidence.
- **Safari iPadOS double-dispatch**: both Pointer and Touch fire. `preventDefault()` on Touch + `touch-action: none` together are required on some Safari versions.
- **OffscreenCanvas on a Worker**: can't receive pointer events directly. Marshal from main-thread canvas. Out of scope v1.
- **PWA / standalone**: `touch-action: none` still required; iOS home-indicator swipe still wins.
- **Chrome "pen as mouse" fallback** on some Win10 configs: first stroke as `pointerType === "mouse"`. Log; not fixable JS-side.
- **iOS Scribble near form elements**: tapping near an `<input>` with pen may hijack to Scribble. Keep form elements away from canvas; wrap with `contentEditable="false"`.

## Minimum viable implementation

1. Add `aj-stylus/src/platform/web.rs` under `#[cfg(target_arch = "wasm32")]`. `web-sys` features per above.
2. Define `WebRawSample` / `WebProximitySample` and adapter entry points `handle_web_raw` / `handle_web_proximity`.
3. `pub fn attach(canvas: &HtmlCanvasElement, adapter: Rc<RefCell<StylusAdapter>>) -> WebStylusBridge`.
4. Apply CSS in §"CSS" via `canvas.style().set_css_text(...)` at attach time, or ship a stylesheet for host apps.
5. Register `Closure<dyn FnMut(PointerEvent)>` for `pointerdown`, `pointermove`, `pointerup`, `pointercancel`, `pointerover`, `pointerout`, `pointerleave`, `lostpointercapture`. Store closures in bridge.
6. `pointerdown`: `set_pointer_capture(pointerId)`, `preventDefault()`, dispatch Down.
7. `pointermove`: iterate `getCoalescedEvents()` for Move samples. Feature-detect `getPredictedEvents()`; dispatch as Predicted. No-op on Firefox.
8. Coordinate conversion per §"Coordinate conversion"; timestamp conversion per §"Timestamps".
9. Tool classification per §"Tool mapping".
10. Pen/touch gate: `HashSet<i32>` of live pen pointerIds; drop touch while non-empty.
11. `pointerover`/`pointerout` with `pointerType === "pen"` → proximity samples; hover (pressure === 0) routes through adapter's Phase::Hover.
12. `pointercancel`/`pointerleave`/`lostpointercapture` → cancel sample to finalize any active stroke.
13. `detach()`: `remove_event_listener_with_callback` per handler, drop closures. Needed for dev-server hot-reload and canvas teardown.
14. Wire into `aj-app`'s web entry: after winit creates its canvas, look it up, call `attach`, store bridge on app state.

## Testing

- Real browsers on real devices. Surface Pen (Edge/Chrome), Apple Pencil (iPad Safari), Wacom (Firefox/Chrome Linux/Mac/Win).
- Playwright / Puppeteer synthesize pointer events for flow/integration tests — no pressure/tilt, but good for plumbing.
- Adapter snapshot: extend existing Estimated/Revise tests with a Web-origin fixture of replayed coalesced samples (JSON).

## References

- [W3C Pointer Events](https://w3c.github.io/pointerevents/)
- [MDN Pointer events](https://developer.mozilla.org/en-US/docs/Web/API/Pointer_events)
- [getCoalescedEvents](https://developer.mozilla.org/en-US/docs/Web/API/PointerEvent/getCoalescedEvents)
- [getPredictedEvents](https://developer.mozilla.org/en-US/docs/Web/API/PointerEvent/getPredictedEvents)
- [setPointerCapture](https://developer.mozilla.org/en-US/docs/Web/API/Element/setPointerCapture)
- [touch-action](https://developer.mozilla.org/en-US/docs/Web/CSS/touch-action)
