//! iOS pen / stylus backend. Overlays a `UIView` subclass above winit's view,
//! intercepts `UITouch` events with `.pencil` type, and feeds the adapter
//! via `handle_ios_raw` / `handle_ios_estimated_update` /
//! `handle_ios_proximity` / `handle_ios_pencil_interaction`. Finger and
//! trackpad touches pass through via `hitTest(_:with:) -> nil` so winit
//! still receives them and egui chrome works.
//!
//! Design decisions (see `.claude/skills/stylus-input/ios.md`):
//!
//! - **Overlay UIView** over winit's, rather than swizzling or forking.
//!   Install via `insertSubview:aboveSubview:`, remove via `removeFromSuperview`.
//! - **`hitTest` routes by touch type.** Returns `self` only for `.pencil`;
//!   `nil` for `.direct`/`.indirect`/`.indirectPointer` so events fall
//!   through to the WinitView below and egui sees finger/trackpad input.
//! - **Iterate coalesced + predicted every `touchesMoved:`.** Pencil 240 Hz
//!   samples are collapsed to frame cadence without coalesced; predicted
//!   gives one-frame-ahead extrapolation that the adapter tags
//!   `SampleClass::Predicted`.
//! - **`touchesEstimatedPropertiesUpdated:`** is the refinement path —
//!   indexed by `UITouch.estimationUpdateIndex`, which maps to our
//!   `update_index`. Revisions can arrive out of order (BT latency), so
//!   the adapter's `pending_estimated` is keyed by update_index, not
//!   pointer.
//! - **Focus loss** via `UIApplicationWillResignActiveNotification` calls
//!   `adapter.on_focus_lost()` to synthesize Cancel for any live stroke.
//!
//! Note: the `define_class!` body is not yet wired to the real UIKit
//! event surface — the adapter seam is fully designed and test-covered,
//! but the overlay-install code path is staged for pre-alpha iPad
//! iteration. `install()` currently acts as a no-op placeholder that
//! retains the adapter handle and lets the app plug in without build
//! breakage. Real sample flow lands with the first on-device validation
//! pass.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;

use crate::StylusAdapter;

#[derive(Debug, thiserror::Error)]
pub enum IosStylusInstallError {
    #[error("UIView handle is null")]
    NullView,
}

/// RAII guard. On drop the overlay view is removed and all retained
/// references (gesture recognizer, pencil interaction, notification
/// observers) are released. Must drop on the main thread.
pub struct IosStylusBackend {
    // Held solely to keep the adapter borrow alive for the lifetime of
    // the overlay view. Closures inside the (pending) `define_class!`
    // body will capture `Rc::clone`s of this.
    _adapter: Rc<RefCell<StylusAdapter>>,
}

/// Entry point. `ui_view` is the pointer obtained from
/// `raw-window-handle`'s `UiKitWindowHandle.ui_view`. `MainThreadMarker`
/// proof is required because UIView subclassing / notification
/// registration is main-thread-only.
///
/// Implementation intended, iteratively landed against a real iPad:
///
/// 1. `objc2::define_class!` an `AJStylusView: UIView` with ivars
///    carrying `Rc<RefCell<StylusAdapter>>`, overriding:
///    - `hitTest:withEvent:` → return `self` for `.pencil` touches,
///      `nil` otherwise.
///    - `touchesBegan/Moved/Ended/Cancelled:withEvent:` → iterate
///      `coalescedTouches(for:)` then `predictedTouches(for:)`, build
///      `IosTouchRawSample` per sub-touch, dispatch via `handle_ios_raw`.
///    - `touchesEstimatedPropertiesUpdated:` → iterate the `NSSet`,
///      build `SampleRevision` per touch, dispatch via
///      `handle_ios_estimated_update`.
/// 2. Attach `UIHoverGestureRecognizer` with `allowedTouchTypes =
///    [.pencil]`; state handler dispatches `handle_ios_proximity` +
///    `IosTouchPhase::Hover` samples.
/// 3. Attach `UIPencilInteraction` with a delegate calling
///    `handle_ios_pencil_interaction` on tap / squeeze (17.5+).
/// 4. Register `NotificationCenter` observers for
///    `UIApplicationWillResignActiveNotification` and
///    `UIApplicationDidEnterBackgroundNotification`; on fire call
///    `adapter.on_focus_lost()`.
///
/// Reference implementation to mirror: the installed winit source at
/// `winit/src/platform_impl/ios/view.rs` demonstrates the
/// `define_class!` pattern against the same objc2 toolchain we use.
pub fn install(
    adapter: Rc<RefCell<StylusAdapter>>,
    ui_view: *mut core::ffi::c_void,
    _mtm: objc2::MainThreadMarker,
) -> Result<IosStylusBackend, IosStylusInstallError> {
    if ui_view.is_null() {
        return Err(IosStylusInstallError::NullView);
    }
    // Placeholder: defer the overlay implementation to on-device iteration.
    // The adapter handle is retained so the App can store the guard and
    // drop semantics are correct. The seam at `handle_ios_*` is ready
    // to receive samples as soon as the view body is filled in.
    Ok(IosStylusBackend { _adapter: adapter })
}
