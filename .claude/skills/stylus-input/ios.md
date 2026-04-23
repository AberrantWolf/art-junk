---
name: stylus-input-ios
description: iOS pen/stylus backend for aj-stylus using UITouch + UIEvent coalesced/predicted, UIPencilInteraction, and UIHoverGestureRecognizer. Overlay UIView subclass above winit's view.
---

# iOS stylus input

**Status: not started.** Target: iPad with Apple Pencil 1 / 2 / Pro.

## TL;DR

Overlay a `UIView` subclass above winit's view. Pencil touches: consume, translate, emit to adapter. Finger/trackpad touches: pass through via `hitTest: -> nil`. Hook `touchesBegan/Moved/Ended/Cancelled` and `touchesEstimatedPropertiesUpdated:` for the Estimated/Revise cycle. Read coalesced samples from `UIEvent.coalescedTouches(for:)`. Attach a `UIHoverGestureRecognizer` for proximity and a `UIPencilInteraction` for tap/squeeze.

## Deployment target

**iOS 16.0.** Keeps hover and most Pencil 2 features first-class; Pencil Pro barrel-roll (`UITouch.rollAngle`) and squeeze (`UIPencilInteraction.Squeeze`) are iOS 17.5+, gated at runtime:

```rust
if objc2::available!(ios = 17.5) { /* read rollAngle */ }
```

## UITouch — the sample type

Per-touch properties to read:

| Property | Meaning |
|---|---|
| `type` (`UITouchType`) | `.direct` (finger), `.pencil`, `.indirect` (trackpad click), `.indirectPointer` (hover pointer) |
| `phase` | `.began/.moved/.ended/.cancelled/.stationary/.regionEntered/.regionMoved/.regionExited` |
| `timestamp` (`TimeInterval`) | Seconds, same clock as `CACurrentMediaTime()`. Monotonic. |
| `preciseLocation(in:)` | Sub-pixel precision location from the Pencil digitizer. Prefer over `location(in:)`. |
| `force` / `maximumPossibleForce` | Normalize as `force / maximumPossibleForce`. |
| `altitudeAngle` | Radians. 0 = flush to screen, π/2 = perpendicular. |
| `azimuthAngle(in: view)` | Radians. Projected shaft compass. Pass our canvas view. |
| `rollAngle` (iOS 17.5+) | Pencil Pro barrel-roll. This is our `twist_deg`. |
| `estimatedProperties` | Bits: `.force .azimuth .altitude .location .roll`. Which axes on *this* sample were estimated. |
| `estimatedPropertiesExpectingUpdates` | Which estimated axes will receive a later update. |
| `estimationUpdateIndex` (`NSNumber?`) | Stable key matching this touch to its future update. Our `update_index`. |

## UIEvent — coalesced and predicted

On `touchesMoved:withEvent:`:

- `event.coalescedTouches(for: touch)` → the full history of sub-frame samples since last delivery. Pencil samples at 240 Hz; without this you get frame-cadence strokes. Iterate all, emit each.
- `event.predictedTouches(for: touch)` → extrapolated future samples (~one frame). Tag as `SampleClass::Predicted`, render to placeholder layer only, discard on the next real delivery.

## touchesEstimatedPropertiesUpdated:

Fires later (20–50 ms after the original touch, over Bluetooth) with a `Set<UITouch>` whose `estimationUpdateIndex` matches earlier touches. Emit `StylusEvent::Revise` keyed by `update_index`. The adapter's `pending_estimated` map already handles this — just re-key from `PointerId` to `update_index` since iOS revisions can arrive out of order.

## Tilt math — altitude/azimuth → (x_deg, y_deg)

Apple's (altitude, azimuth) is a polar pair. Our backend wants component tilts. Exact form:

```rust
let theta = PI/2.0 - altitude;  // tilt from normal
let sin_a = altitude.sin();
let tilt_x_rad = (theta.sin() * azimuth.cos()).atan2(sin_a);
let tilt_y_rad = (theta.sin() * azimuth.sin()).atan2(sin_a);
Tilt {
    x_deg: tilt_x_rad.to_degrees() as f32,
    y_deg: tilt_y_rad.to_degrees() as f32,
}
```

Zero both when `theta.sin() < 0.02` (shaft nearly perpendicular — azimuth is noise). Pass our canvas view to `azimuthAngle(in:)` so tilt rotates with the view.

## View bridging

Winit iOS creates `UIWindow → WinitViewController → WinitView`. Winit's view overrides the touch methods and dispatches lossy `WindowEvent::Touch`.

**Approach: overlay `UIView` subclass via `objc2::define_class!`.**

1. Pull `UIView*` from `raw_window_handle` (`UiKitWindowHandle.ui_view`).
2. Construct `AJStylusView` with matching frame and autoresizing masks, `isUserInteractionEnabled = true`, `isMultipleTouchEnabled = true`.
3. Insert above WinitView (`insertSubview:aboveSubview:`).
4. Override `hitTest(_:with:)` to return `self` only for `.pencil` touches; return `nil` for `.direct`/`.indirect*` (UIKit falls through to WinitView below, so finger/trackpad still reach egui via winit).
5. Implement the four touch methods and `touchesEstimatedPropertiesUpdated:`.

Scaffolding reference: `winit/src/platform_impl/ios/view.rs` (the installed winit source). Copy the `define_class!` pattern, swap the bodies.

```rust
define_class!(
    #[unsafe(super(UIView))]
    #[name = "AJStylusView"]
    #[ivars = AjStylusViewIvars]
    pub(crate) struct AjStylusView;

    impl AjStylusView {
        #[unsafe(method(touchesBegan:withEvent:))]
        fn touches_began(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) { ... }

        #[unsafe(method(touchesMoved:withEvent:))]
        fn touches_moved(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) { ... }

        #[unsafe(method(touchesEnded:withEvent:))]
        fn touches_ended(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) { ... }

        #[unsafe(method(touchesCancelled:withEvent:))]
        fn touches_cancelled(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) { ... }

        #[unsafe(method(touchesEstimatedPropertiesUpdated:))]
        fn touches_estimated_updated(&self, touches: &NSSet<UITouch>) { ... }

        #[unsafe(method(hitTest:withEvent:))]
        fn hit_test(&self, point: CGPoint, event: Option<&UIEvent>) -> Option<Retained<UIView>> {
            // Return self only for .pencil touches. Otherwise nil.
        }
    }
);
```

## Proximity / hover (M1+ iPads, iPadOS 16.1+)

Attach a `UIHoverGestureRecognizer` with `allowedTouchTypes = [.pencil]`. Its `.began/.changed/.ended` states map to `ProximitySample { is_entering: true/false }` plus `StylusEvent::Sample { phase: Phase::Hover }` on continuous updates. Expose: location, altitudeAngle, azimuthAngle, zOffset (iOS 16.4+), rollAngle (17.5+).

## UIPencilInteraction — tap and squeeze

Separate from touches. Attach `UIPencilInteraction` to the view with a delegate implementing:
- `pencilInteractionDidTap:` — Pencil 2 double-tap.
- `pencilInteraction:didReceiveSqueeze:` (17.5+) — Pencil Pro squeeze with `.began/.changed/.ended/.cancelled` and `hoverPose` (location + z + azimuth + altitude + roll).

These don't have a `UITouch` — they're separate events. Add a new `StylusEvent::PencilInteraction { kind, hover_pose }` variant. Respect `UIPencilInteraction.preferredSqueezeAction` and `preferredTapAction`: if the user set these to run a Shortcut, the app does **not** get the event.

## Scribble

`UIScribbleInteraction` auto-enables on text-input-capable views. Our canvas isn't. Optionally attach `UIScribbleInteraction` with a delegate whose `scribbleInteraction:shouldBeginAtLocation:` returns `false` — belt-and-braces against false positives near text-adjacent chrome.

## Palm rejection

Apple's built-in rejection (for `.pencil`-preferring views) handles concurrent finger touches on Pencil-capable hardware. Layer on top:

- During an active pencil stroke, drop `.direct` touches in `touchesBegan:`.
- Keep `.indirect*` passing through to winit (trackpad while drawing is fine).

## Timestamps

`UITouch.timestamp` is monotonic seconds. Share the `PlatformTimestampAnchor` helper (extract from macOS `MacEpoch`) for first-touch anchoring.

## Coordinates

`preciseLocation(in: canvasView)` returns points. Multiply by `canvasView.contentScaleFactor` for physical pixels matching the adapter contract.

## Backgrounding

Register for `UIApplicationWillResignActiveNotification` and `UIApplicationDidEnterBackgroundNotification`. On fire, call `adapter.on_focus_lost()` (already exists — same path as macOS). UIKit also sends `touchesCancelled:` to active responders; redundant but harmless.

## Info.plist

- `CADisableMinimumFrameDuration = YES` — opts into 120 Hz ProMotion for `CADisplayLink`/`CAMetalDisplayLink`.

## Rust crates

- `objc2` 0.6+, `objc2-ui-kit` 0.3+ — full coverage of `UITouch`, `UIEvent`, `UIView` subclassing, `UIPencilInteraction`, `UIHoverGestureRecognizer`.
- `objc2-foundation` for `NSSet`, `NSNumber`, `NSNotificationCenter`.

## Adapter refactors triggered by this platform

- Extract `MacEpoch` → `PlatformTimestampAnchor`.
- Add `SampleClass::Predicted { predict_index }` to `aj-core`.
- Add `StylusEvent::PencilInteraction` (iOS Pencil tap/squeeze).
- Re-key `pending_estimated` from `PointerId` to `update_index` so out-of-order revisions resolve correctly.

## Minimum viable implementation — step list

1. Add `aj-stylus/src/ios_touch.rs` under `#[cfg(target_os = "ios")]`. Define `IosTouchRawSample`, `IosTouchProximitySample`, `IosPencilInteractionEvent`.
2. Adapter entry points `handle_ios_raw`, `handle_ios_proximity`, `handle_ios_estimated_update`, `handle_ios_pencil_interaction`.
3. Write `AjStylusView: UIView` via `define_class!`. Override `hitTest` to pass-through non-pencil.
4. In `touchesBegan/Moved/Ended/Cancelled`: iterate `coalescedTouches` (real) then `predictedTouches` (Predicted class). For each `.pencil` touch, build `IosTouchRawSample`.
5. `touchesEstimatedPropertiesUpdated:` → `StylusEvent::Revise` keyed by `update_index`.
6. Install `UIHoverGestureRecognizer` with `.pencil` filter; map state to proximity + Hover phase.
7. Install `UIPencilInteraction` with delegate; map tap / squeeze to `StylusEvent::PencilInteraction`.
8. Tilt math: §"Tilt math" above. Unit-test four cardinal orientations.
9. Notification observers: `UIApplicationWillResignActive`, `UIApplicationDidEnterBackground` → `adapter.on_focus_lost()`.
10. RAII: `IosStylusBackend` holds the view (retained), hover recognizer, pencil interaction, notification tokens. Drop removes them. Mirror `MacTabletBackend` shape.
11. Plumb backend install into `aj-app`'s iOS entry (after winit `Resumed`, acquire `ui_view` from raw handle).
12. Tests: `aj-stylus/tests/ios_adapter.rs` — drive `handle_ios_raw` and `handle_ios_estimated_update` with hand-built samples. Zero UIKit.

## Testing

- **Simulator**: no pressure, no tilt, no pencil. Good for plumbing tests (touch type, indirect).
- **Real device required**: iPad Pro (Pencil 2 for baseline; Pencil Pro M2/M4 for squeeze + roll + hover on M1+).
- **Instruments "Metal System Trace"** for latency checks; no built-in pencil event track — record your own JSONL dump via debug screen and replay through unit tests.

## References

- [UITouch](https://developer.apple.com/documentation/uikit/uitouch)
- [UIEvent.coalescedTouches](https://developer.apple.com/documentation/uikit/uievent/coalescedtouches(for:))
- [UIEvent.predictedTouches](https://developer.apple.com/documentation/uikit/uievent/predictedtouches(for:))
- [UIPencilInteraction](https://developer.apple.com/documentation/uikit/uipencilinteraction)
- [UIHoverGestureRecognizer](https://developer.apple.com/documentation/uikit/uihovergesturerecognizer)
- [objc2-ui-kit on docs.rs](https://docs.rs/objc2-ui-kit/)
- [WWDC24 10214 — Squeeze the most out of Apple Pencil](https://developer.apple.com/videos/play/wwdc2024/10214/)
