//! Android pen / stylus backend. Translates raw `MotionEvent`s (via the
//! `ndk` crate from the `android-activity` input iterator) into
//! `AndroidRawSample`s and feeds the adapter. Bypasses winit's Touch
//! dispatch because it drops tool type, tilt, orientation, button state,
//! and historical-sample data.
//!
//! Design decisions (see `.claude/skills/stylus-input/android.md`):
//!
//! - **Raw `MotionEvent`**, not winit's `WindowEvent::Touch`. Winit's
//!   translation drops everything stylus-distinguishing.
//! - **Historical drain first.** `MotionEvent.getHistoricalAxisValue(..)`
//!   is Android's coalesced-sample API; drained oldest-to-newest then
//!   the current sample, so 240 Hz S-Pen samples on a 120 Hz UI don't
//!   get flattened.
//! - **Palm rejection at the backend.** While any `TOOL_TYPE_STYLUS`
//!   pointer is down or hovering, drop `TOOL_TYPE_FINGER` pointers. The
//!   adapter's `active_pen_pointer` gate covers winit-routed mouse
//!   events; finger drops are backend-local.
//! - **`FLAG_CANCELED` support** (API 24+). A per-pointer Up can carry
//!   the cancel flag; the backend translates that into
//!   `AndroidSourcePhase::Cancel` so the adapter emits Phase::Cancel.
//! - **Tilt decomposition lives in the adapter.** The polar (tilt_rad,
//!   orientation_rad) pair from Android maps to component tilts via
//!   `android_tilt_to_xy_deg`, keeping the backend thin and the math
//!   testable in pure Rust.
//! - **Prediction (Jetpack `MotionEventPredictor`) deferred.** Lands
//!   behind a cargo feature in a follow-up pass; the adapter's
//!   `SampleClass::Predicted` path is already in place.
//!
//! The backend's entry point is intended to be called from aj-app's
//! input pump:
//!
//! ```ignore
//! for event in android_app.input_events_iter() {
//!     if let InputEvent::Motion(m) = event {
//!         stylus_junk::handle_android_motion(m, &adapter);
//!     }
//! }
//! ```
//!
//! Implementation body is staged for first-device iteration; the
//! adapter seam is fully designed and test-covered. See the TODO list
//! in `handle_android_motion` for the remaining work.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;

use crate::StylusAdapter;

/// Process one `MotionEvent`, emitting zero or more `AndroidRawSample`s
/// into the adapter.
///
/// Staged for on-device iteration:
/// 1. Classify `getToolType(pointerIndex)` → `ToolKind`.
/// 2. Decode `getActionMasked` / `getActionIndex` / `FLAG_CANCELED`
///    (on `ACTION_POINTER_UP`) into `AndroidSourcePhase`.
/// 3. Walk `getHistorySize()` historical samples (oldest first) then
///    the current sample; build `AndroidRawSample` per pointer per
///    historical-plus-current step, with `AMotionEvent_getEventTime`
///    divided to seconds.
/// 4. Palm rejection: while any stylus pointer is active, drop
///    `TOOL_TYPE_FINGER` pointers before emitting.
/// 5. On `ACTION_HOVER_ENTER` / `EXIT`, emit
///    `AndroidProximitySample` via `handle_android_proximity`.
///
/// The adapter seam (`handle_android_raw`, `handle_android_proximity`)
/// is ready; this file wires them into the real `MotionEvent` once a
/// real device is available to iterate on.
pub fn handle_android_motion(
    _motion: &AndroidMotionEventStub,
    _adapter: &Rc<RefCell<StylusAdapter>>,
) {
    // Body staged — the adapter seam is designed and tested;
    // `aj-app`'s Android entry pump will feed real `ndk::event::MotionEvent`
    // values through a version of this function that fills in the
    // numbered steps above.
}

/// Placeholder so this module compiles on targets where we don't yet
/// pull in `ndk`. Pre-alpha callers should substitute
/// `ndk::event::MotionEvent<'_>` at the real implementation stage.
pub struct AndroidMotionEventStub;
