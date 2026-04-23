---
name: stylus-input-android
description: Android pen/stylus backend for aj-stylus via MotionEvent through the android-activity crate. Historical samples for high-rate, optional Jetpack motion prediction, tilt+orientation → component tilt.
---

# Android stylus input

**Status: not started.** Target: Android 8+ (API 26), S-Pen / USI / Wacom EMR.

## TL;DR

Winit's `WindowEvent::Touch` drops everything interesting. Grab raw `MotionEvent`s from the `android-activity` input iterator, emit samples directly. Walk `historical` samples first (the history API is Android's coalesced-touches equivalent), then current. Decompose `AXIS_TILT`+`AXIS_ORIENTATION` into component tilts. Optional prediction via Jetpack `MotionEventPredictor` (back-ported to API 24+). Drop finger touches while any stylus is in proximity.

## Primary API — MotionEvent

All pointer input is `android.view.MotionEvent`. NDK mirror: `AInputEvent` / `AMotionEvent_*`, wrapped by the `ndk` Rust crate as `ndk::event::MotionEvent`.

### Tool types

| Constant | Value | Meaning |
|---|---|---|
| `TOOL_TYPE_UNKNOWN` | 0 | pre-typing |
| `TOOL_TYPE_FINGER` | 1 | skin |
| `TOOL_TYPE_STYLUS` | 2 | active stylus / S-Pen / USI |
| `TOOL_TYPE_MOUSE` | 3 | BT / USB mouse |
| `TOOL_TYPE_ERASER` | 4 | stylus held eraser-first |

Map directly to our `ToolKind`.

### Axes (via `AMotionEvent_getAxisValue(event, AMOTION_EVENT_AXIS_*, pointer_index)`)

- `AXIS_X`, `AXIS_Y` — logical pixels in view coords.
- `AXIS_PRESSURE` — normalized `0..=1` after driver calibration. May exceed 1.0 on firm press; clamp.
- `AXIS_SIZE` — contact area, `0..=1`. Useful for palm classification.
- `AXIS_TILT` — **radians**, `0` = perpendicular, `π/2` = flat.
- `AXIS_ORIENTATION` — **radians**, `-π..=π`, azimuthal direction of tilt.
- `AXIS_DISTANCE` — hover distance; units vary (some normalized, some cm-ish).
- `AXIS_TOOL_MAJOR/MINOR` — tool ellipse; finger-only in practice.

Android has **no** twist/barrel-rotation and **no** tangential pressure on any shipping hardware. Leave those fields `None`.

### Actions

Low 8 bits of `AMotionEvent_getAction`; upper bits are pointer index for `POINTER_DOWN/UP`.

- `ACTION_DOWN` (0) — first pointer down.
- `ACTION_MOVE` (2) — any pointer moved.
- `ACTION_UP` (1) — last pointer lifted.
- `ACTION_CANCEL` (3) — gesture cancelled.
- `ACTION_POINTER_DOWN/UP` (5, 6) — additional pointers.
- `ACTION_HOVER_ENTER/MOVE/EXIT` (9, 7, 10) — hover (stylus HW required).

### Historical samples — the coalesced-touches equivalent

When multiple sensor samples arrive between deliveries, Android bundles them into one `MotionEvent`. Read via:

```
getHistorySize()
getHistoricalAxisValue(axis, pointer_index, hist_pos)
getHistoricalEventTime(hist_pos)
```

Oldest-to-newest. Emit **all** historical as `Phase::Move` with their own timestamps before the current-frame sample. Essential for Samsung Tab S9's 240 Hz digitizer through a 120 Hz UI.

### Buttons

`AMotionEvent_getButtonState` bitfield:
- `BUTTON_STYLUS_PRIMARY` = 0x20 — S-Pen side button.
- `BUTTON_STYLUS_SECONDARY` = 0x40 — rare.
- `BUTTON_PRIMARY/SECONDARY/TERTIARY` — mouse buttons.

### Timestamps

`AMotionEvent_getEventTime` returns nanoseconds since boot (`CLOCK_MONOTONIC`-equivalent). Java `getEventTime` returns ms. Use the `PlatformTimestampAnchor` helper (extracted from `MacEpoch`) to translate to adapter-epoch `Duration`.

## Tilt decomposition

Android's two-angle form → our component tilts:

```rust
fn android_tilt_to_xy_deg(tilt_rad: f32, orientation_rad: f32) -> (f32, f32) {
    // tilt_rad: 0 = perpendicular, pi/2 = flat
    // orientation_rad: direction pen is tilted toward (0 = +Y down-screen in Android; pi/2 = +X right)
    let tilt_x = tilt_rad * orientation_rad.sin();
    let tilt_y = tilt_rad * orientation_rad.cos();
    (tilt_x.to_degrees(), tilt_y.to_degrees())
}
```

At `tilt_rad` approaching π/2 the components don't sum trivially, but both stay within ±90° and are monotonic — what a brush engine needs.

### Vendor quirks

- Samsung S-Pen: `AXIS_TILT` clamps near 1.05 rad (~60°), never reports full π/2.
- Some cheap USI pens report orientation with a 90° offset. Not fixable without per-device calibration; document as known issue.
- Wacom EMR tablets are the most spec-accurate.

## Prediction — Jetpack MotionEventPredictor

`androidx.input:input-motionprediction:1.0.0-beta05`. Back-ports `MotionPredictor` to **API 24+** (native `MotionPredictor` is API 33+). Same API surface, broad device coverage.

Java-only — requires JNI. Get `JavaVM` from `AndroidApp::vm_as_ptr()`, attach thread, call static methods. Keep JNI calls off the input-dispatch thread; run prediction on the render thread where you already have frame-time context.

Emit predicted samples as `SampleClass::Predicted`; never commit to scene, draw to Placeholder layer only.

## Low-latency hints

- **`Surface.setFrameRate(120.0f, FRAME_RATE_COMPATIBILITY_FIXED_SOURCE)`** — API 30+. JNI call on the surface obtained from `NativeWindow`. Version-gate; no-op below.
- **`androidx.graphics` front-buffer rendering** — Canvas-/GLES-only; not usable with wgpu/Vulkan. File as a future wgpu upstream ask. Skip v1.

## Winit integration

Winit's Android backend runs on the `android-activity` crate (`game-activity` flavor recommended for apps owning their input loop). Its `WindowEvent::Touch` carries only position, phase, id, and optional `force`. No tilt, orientation, button state, tool type, historical.

Approach (symmetric to the NSEvent monitor on macOS): read raw `MotionEvent` yourself from the `android-activity` app handle.

```rust
// aj-stylus/src/platform/android.rs
use android_activity::{AndroidApp, InputStatus};
use android_activity::input::{InputEvent, MotionEvent};

pub fn handle_motion(ev: &MotionEvent<'_>, adapter: &mut StylusAdapter) -> InputStatus {
    let action = ev.action();
    let pointer_count = ev.pointer_count();
    for hist in 0..ev.history_size() {
        for p in 0..pointer_count {
            emit_sample(ev, p, Some(hist), action, adapter);
        }
    }
    for p in 0..pointer_count {
        emit_sample(ev, p, None, action, adapter);
    }
    if any_stylus(ev) { InputStatus::Handled } else { InputStatus::Unhandled }
}
```

Drain `android_app.input_events_iter()` inside the winit event-loop pump. Return `Handled` for stylus events so winit doesn't also deliver `WindowEvent::Touch`; `Unhandled` for finger/mouse lets winit continue.

## Rust crates

- `ndk = "0.9"` — `MotionEvent`, axes, history, tool type, button state.
- `android-activity = "0.6"` — re-exports ndk event types; event-loop owner.
- `jni = "0.21"` — for `MotionEventPredictor` and `Surface.setFrameRate`.

## Palm rejection

- `MotionEvent.FLAG_CANCELED` (API 24+) on `ACTION_POINTER_UP`/`ACTION_CANCEL`: framework's palm detector says "undo this stroke." Adapter must support retroactive cancel — emit `Phase::Cancel` for that pointer_id.
- **While any `TOOL_TYPE_STYLUS` pointer is active, drop `TOOL_TYPE_FINGER`.** 250 ms cooldown after stylus leaves.

## S-Pen / Samsung specifics

AOSP surface is enough for v1: position, pressure, tilt, orientation, hover distance, side button (`BUTTON_STYLUS_PRIMARY`), eraser flip (`TOOL_TYPE_ERASER`). Samsung Spen SDK adds air actions / handwriting recognition but no new axes. Skip v1; revisit only for air-remote gestures.

Tab S9/S10 digitizer samples at 240 Hz native; historical reads essential.

## ChromeOS / USI

ChromeOS Android apps in ARCVM: same MotionEvent path; tilt and pressure supported. USI 2.0 color/preferred-width via `InputDevice.getKeyCharacterMap` + `AXIS_GENERIC_1..16` on API 34+. Not needed v1.

## Gotchas

- **Hover events** require hardware that reports proximity — Pixel Tablet USI and S-Pen yes; budget tablets no. Don't rely on hover to gate "stylus active"; a `TOOL_TYPE_STYLUS` pointer in `ACTION_DOWN` is the ground truth.
- **DeX / BT mouse / BT stylus** may report as `TOOL_TYPE_MOUSE` or `TOOL_TYPE_UNKNOWN`. Unknown → mouse-ish behavior.
- **Finger pressure** often synthesized as 1.0 or size-derived on cheap devices. Only trust pressure when `tool_type == STYLUS`.
- **`ACTION_CANCEL` from scroll interception**: a parent `ScrollView` can steal our gesture. Fix Java-side: `getParent().requestDisallowInterceptTouchEvent(true)` on `ACTION_DOWN`. Irrelevant for `NativeActivity`/`GameActivity` without scroll ancestor.

## Minimum API

**`minSdkVersion = 26` (Android 8).** Covers >97% active devices. Gets `FLAG_CANCELED` (24), `TOOL_TYPE_ERASER` (23), tilt/orientation (14). Prediction, `setFrameRate` are additive.

## Minimum viable implementation

1. Add `aj-stylus/src/platform/android.rs` under `#[cfg(target_os = "android")]`. Deps: `ndk = "0.9"`, `android-activity = { version = "0.6", features = ["game-activity"] }`, `jni = "0.21"`.
2. Define `AndroidRawSample`, `AndroidProximitySample`, adapter entry points `handle_android_raw` / `handle_android_proximity`.
3. Wire installer: `pub fn install_android(app: &AndroidApp, adapter: Rc<RefCell<StylusAdapter>>)`.
4. In winit pump, drain `android_app.input_events_iter()`. For each `InputEvent::Motion(m)`, call `handle_motion`.
5. `handle_motion`: walk pointers × (history + 1). Build samples.
6. Action → phase: `DOWN`/`POINTER_DOWN` → Down; `MOVE` → Move; `UP`/`POINTER_UP` → Up; `CANCEL` or `FLAG_CANCELED` → Cancel.
7. `HOVER_ENTER`/`HOVER_EXIT` → `ProximitySample`. Caps from `InputDevice` axes (JNI, cache per device).
8. Tilt decomposition per §"Tilt decomposition". Unit-test four cardinals + zero.
9. `PlatformTimestampAnchor` for timestamps (share with iOS / Windows / Web).
10. Palm rejection: drop `TOOL_TYPE_FINGER` while any `TOOL_TYPE_STYLUS` down/hovering; 250 ms cooldown.
11. Prediction (feature-gated): `MotionEventPredictor` via JNI; predicted samples as `SampleClass::Predicted`.
12. `Surface.setFrameRate(120, FIXED_SOURCE)` JNI call on surface-created, API 30+ only.
13. Tests: `aj-stylus/tests/android_adapter.rs` with pure-Rust `handle_android_raw` fixtures.

## Testing

- **Emulator**: Android Studio emulator (Flamingo+) has stylus pointer with pressure/tilt sliders in Extended Controls. Plumbing tests only.
- **Real devices**: Galaxy Tab S9/S10 (S-Pen, 240 Hz), Pixel Tablet + USI pen (clean AOSP), Chromebook USI (ARCVM).
- **Golden images**: record a canonical stroke; `dssim`-compare per testing policy.

## References

- [MotionEvent](https://developer.android.com/reference/android/view/MotionEvent)
- [NDK input reference](https://developer.android.com/ndk/reference/group/input)
- [MotionPredictor (native)](https://developer.android.com/reference/android/view/MotionPredictor)
- [Jetpack MotionEventPredictor](https://developer.android.com/reference/androidx/input/motionprediction/MotionEventPredictor)
- [android-activity docs](https://docs.rs/android-activity/)
