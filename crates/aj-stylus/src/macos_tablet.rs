//! macOS tablet-input backend. Installs an `NSEvent` local monitor and an
//! `NSApplicationDidResignActive` notification observer, translates every
//! tablet-subtype mouse event, native `NSTabletPoint`, and `NSTabletProximity`
//! event into `MacTabletRawSample` / `MacTabletProximitySample`, and pushes
//! them into the shared `StylusAdapter`.
//!
//! Design decisions (see `art-junk/architecture/dataflow.md` and the M2 plan):
//!
//! - We **coexist with winit**, returning each event pointer unchanged from
//!   the monitor so winit (and therefore egui) processes it normally. The
//!   adapter's `active_pen_pointer` flag suppresses the duplicate mouse
//!   `StylusEvent` that winit's subsequent dispatch would otherwise produce.
//! - Per Apple docs the local monitor runs before `-[NSApplication sendEvent:]`
//!   dispatches to the target view, so we see events *before* winit does.
//! - The monitor handler is always invoked on the main thread; we share state
//!   through `Rc<RefCell<StylusAdapter>>`. Borrow-conflict under reentrancy is
//!   guarded with `try_borrow_mut` + a warn log so a dropped sample is
//!   preferable to a panic in a drawing app.
//! - Installation requires a `MainThreadMarker`; drop runs on whatever thread
//!   the owning `App` is dropped on (winit forces the main thread, so this is
//!   safe in practice).

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;

use aj_core::{Tilt, ToolCaps, ToolKind};
use block2::RcBlock;
use kurbo::Point;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{NSEvent, NSEventMask, NSEventSubtype, NSEventType, NSPointingDeviceType};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSString};

use crate::StylusAdapter;
use crate::adapter::{
    MacTabletOrigin, MacTabletPhase, MacTabletProximitySample, MacTabletRawSample,
};

/// `IOKit` tablet-capability bits carried on `NSEvent.capabilityMask()`.
/// Apple's SDK exposes these via `<IOKit/hidsystem/IOLLEvent.h>`, not
/// `AppKit`, so we define the mask constants we consume here.
const NX_TABLET_CAPABILITY_TILT_X: u64 = 1 << 7;
const NX_TABLET_CAPABILITY_TILT_Y: u64 = 1 << 8;
const NX_TABLET_CAPABILITY_PRESSURE: u64 = 1 << 10;
const NX_TABLET_CAPABILITY_TANGENTIAL_PRESSURE: u64 = 1 << 11;
const NX_TABLET_CAPABILITY_ROTATION: u64 = 1 << 13;
const NX_TABLET_CAPABILITY_BUTTONS: u64 = 1 << 6;

#[derive(Debug, thiserror::Error)]
pub enum MacTabletInstallError {
    #[error("NSEvent::addLocalMonitorForEventsMatchingMask_handler returned nil")]
    MonitorInstallFailed,
}

/// RAII guard that keeps the `NSEvent` monitor and focus-loss observer
/// alive. Drop removes both — installation and teardown must both occur on
/// the main thread (Apple docs + winit invariants).
pub struct MacTabletBackend {
    monitor_token: Option<Retained<AnyObject>>,
    observer_token: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl MacTabletBackend {
    /// Install the `NSEvent` monitor and focus-loss observer, feeding the
    /// given adapter. Held-alive `Rc<RefCell<_>>` clones are captured into
    /// the handler closures; the returned guard's Drop unregisters both.
    pub fn install(
        adapter: Rc<RefCell<StylusAdapter>>,
        mtm: MainThreadMarker,
    ) -> Result<Self, MacTabletInstallError> {
        // Opt out of mouse event coalescing. macOS's default is to merge
        // consecutive mouse-moved/dragged events when the app doesn't pull
        // them from the queue fast enough, dropping the pen's native 200 Hz
        // rate down to display refresh (~60 Hz). For a drawing app this
        // produces visibly sparse samples and straight-line chords across
        // curves. Disable and pay the per-event dispatch cost in exchange
        // for smooth strokes.
        NSEvent::setMouseCoalescingEnabled(false);
        let monitor_token = install_event_monitor(adapter.clone(), mtm)?;
        let observer_token = install_focus_loss_observer(adapter);
        Ok(Self { monitor_token: Some(monitor_token), observer_token: Some(observer_token) })
    }
}

impl Drop for MacTabletBackend {
    fn drop(&mut self) {
        if let Some(token) = self.monitor_token.take() {
            // SAFETY: `removeMonitor:` retains its argument internally and
            // only requires main-thread invocation. `MacTabletBackend` is
            // only constructed via `install` (which requires a
            // `MainThreadMarker`) and is `!Send` by virtue of holding
            // `Retained<AnyObject>`, so Drop must run on the main thread.
            unsafe {
                NSEvent::removeMonitor(&token);
            }
        }
        if let Some(token) = self.observer_token.take() {
            // SAFETY: `removeObserver:` is main-thread safe with the same
            // invariants as above. `ProtocolObject: AsRef<AnyObject>` gives
            // us the required argument type.
            let any_obj: &AnyObject = (*token).as_ref();
            unsafe {
                NSNotificationCenter::defaultCenter().removeObserver(any_obj);
            }
        }
    }
}

fn install_event_monitor(
    adapter: Rc<RefCell<StylusAdapter>>,
    mtm: MainThreadMarker,
) -> Result<Retained<AnyObject>, MacTabletInstallError> {
    // Narrow event mask — we listen for exactly the events carrying tablet
    // data. Omitting `NSEventMaskAny` avoids pulling gesture/scroll/periodic
    // events that the handler would otherwise have to filter.
    let mask = NSEventMask::LeftMouseDown
        | NSEventMask::LeftMouseUp
        | NSEventMask::LeftMouseDragged
        | NSEventMask::RightMouseDown
        | NSEventMask::RightMouseUp
        | NSEventMask::RightMouseDragged
        | NSEventMask::OtherMouseDown
        | NSEventMask::OtherMouseUp
        | NSEventMask::OtherMouseDragged
        | NSEventMask::MouseMoved
        | NSEventMask::TabletPoint
        | NSEventMask::TabletProximity;

    let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: The NSEvent local monitor guarantees the pointer is a valid
        // NSEvent for the duration of this call. We do not retain it past the
        // call; return unchanged so AppKit continues normal dispatch.
        let event_ref = unsafe { event.as_ref() };
        dispatch_event(event_ref, mtm, &adapter);
        event.as_ptr()
    });

    // SAFETY: `addLocalMonitorForEventsMatchingMask_handler` is documented as
    // main-thread-only; the `MainThreadMarker` proves we hold that invariant.
    // The block is retained by AppKit for the lifetime of the monitor.
    let token = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &handler) };
    token.ok_or(MacTabletInstallError::MonitorInstallFailed)
}

fn install_focus_loss_observer(
    adapter: Rc<RefCell<StylusAdapter>>,
) -> Retained<ProtocolObject<dyn NSObjectProtocol>> {
    let block =
        RcBlock::new(move |_notif: NonNull<NSNotification>| match adapter.try_borrow_mut() {
            Ok(mut a) => a.on_focus_lost(),
            Err(_) => {
                log::warn!("focus-loss observer: adapter borrowed elsewhere, cancel skipped");
            }
        });

    // SAFETY: `addObserverForName_object_queue_usingBlock` returns a new
    // NSObject identifier we must keep alive to receive notifications; we
    // store it in the guard and release in Drop.
    let center = NSNotificationCenter::defaultCenter();
    let name = ns_string("NSApplicationDidResignActiveNotification");
    unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(&name),
            None, // any sender
            None, // post synchronously on the main thread (no NSOperationQueue)
            &block,
        )
    }
}

fn dispatch_event(event: &NSEvent, mtm: MainThreadMarker, adapter: &Rc<RefCell<StylusAdapter>>) {
    let ty = event.r#type();
    let subtype = event.subtype();

    match ty {
        NSEventType::TabletProximity => {
            let prox = build_proximity(event);
            deliver(adapter, |a| a.handle_mac_proximity(prox));
        }
        NSEventType::TabletPoint => {
            if let Some(raw) = build_raw_sample(event, mtm, MacTabletOrigin::NativeTabletPoint) {
                deliver(adapter, move |a| a.handle_mac_raw(raw));
            }
        }
        NSEventType::LeftMouseDown
        | NSEventType::LeftMouseUp
        | NSEventType::LeftMouseDragged
        | NSEventType::RightMouseDown
        | NSEventType::RightMouseUp
        | NSEventType::RightMouseDragged
        | NSEventType::OtherMouseDown
        | NSEventType::OtherMouseUp
        | NSEventType::OtherMouseDragged
        | NSEventType::MouseMoved
            if subtype == NSEventSubtype::TabletPoint =>
        {
            if let Some(raw) = build_raw_sample(event, mtm, MacTabletOrigin::MouseSubtype) {
                deliver(adapter, move |a| a.handle_mac_raw(raw));
            }
        }
        NSEventType::LeftMouseDown
        | NSEventType::LeftMouseUp
        | NSEventType::LeftMouseDragged
        | NSEventType::RightMouseDown
        | NSEventType::RightMouseUp
        | NSEventType::RightMouseDragged
        | NSEventType::OtherMouseDown
        | NSEventType::OtherMouseUp
        | NSEventType::OtherMouseDragged
        | NSEventType::MouseMoved
            if subtype == NSEventSubtype::TabletProximity =>
        {
            // Proximity-carrying mouse events can appear on some drivers.
            let prox = build_proximity(event);
            deliver(adapter, |a| a.handle_mac_proximity(prox));
        }
        _ => {
            // Non-tablet event — winit handles it normally via the pass-through.
        }
    }
}

fn deliver<F>(adapter: &Rc<RefCell<StylusAdapter>>, f: F)
where
    F: FnOnce(&mut StylusAdapter),
{
    match adapter.try_borrow_mut() {
        Ok(mut a) => f(&mut a),
        Err(_) => {
            log::warn!("NSEvent monitor: adapter borrowed elsewhere, sample dropped");
        }
    }
}

fn build_proximity(event: &NSEvent) -> MacTabletProximitySample {
    let device_id = event.deviceID();
    let unique_id = event.uniqueID();
    let pointing = event.pointingDeviceType();
    let cap_mask = event.capabilityMask();
    let is_entering = event.isEnteringProximity();

    let tool = map_pointing_device_type(pointing);
    let caps = map_capability_mask(cap_mask as u64);

    MacTabletProximitySample {
        device_id: u32_from_usize(device_id),
        unique_id: if unique_id == 0 { None } else { Some(unique_id) },
        pointing_device_type: tool,
        caps,
        is_entering,
    }
}

fn build_raw_sample(
    event: &NSEvent,
    mtm: MainThreadMarker,
    origin: MacTabletOrigin,
) -> Option<MacTabletRawSample> {
    let ty = event.r#type();
    let position = window_to_physical_pixels(event, mtm)?;
    let timestamp_secs = event.timestamp();
    let pressure = event.pressure();
    let tilt = event.tilt();
    let rotation = event.rotation();
    let tangential = event.tangentialPressure();
    let buttons = event.buttonMask();
    let device_id = event.deviceID();
    let pointing = event.pointingDeviceType();

    // AppKit's `tilt.{x,y}` is a -1..1 ratio against full tilt, which the
    // driver maps from the pen's altitude. Mapping to degrees as `ratio * 90`
    // is the conventional choice — a cursor-puck would report 0 anyway.
    // Values are bounded [-1, 1] so f64→f32 truncation is meaningless.
    #[allow(clippy::cast_possible_truncation)]
    let tilt = Tilt { x_deg: (tilt.x as f32) * 90.0, y_deg: (tilt.y as f32) * 90.0 };

    let source_phase = match ty {
        NSEventType::LeftMouseDown | NSEventType::RightMouseDown | NSEventType::OtherMouseDown => {
            MacTabletPhase::Down
        }
        NSEventType::LeftMouseUp | NSEventType::RightMouseUp | NSEventType::OtherMouseUp => {
            MacTabletPhase::Up
        }
        NSEventType::LeftMouseDragged
        | NSEventType::RightMouseDragged
        | NSEventType::OtherMouseDragged
        | NSEventType::MouseMoved
        | NSEventType::TabletPoint => MacTabletPhase::Move,
        _ => return None,
    };

    Some(MacTabletRawSample {
        position_physical_px: position,
        timestamp_secs,
        pressure,
        tilt,
        twist_deg: rotation,
        tangential_pressure: tangential,
        button_mask: u32_from_usize(buttons.0),
        device_id: u32_from_usize(device_id),
        pointing_device_type: map_pointing_device_type(pointing),
        origin,
        source_phase,
    })
}

fn window_to_physical_pixels(event: &NSEvent, mtm: MainThreadMarker) -> Option<Point> {
    let window = event.window(mtm)?;
    let content_view = window.contentView()?;
    let window_point = event.locationInWindow();
    // `WinitView.isFlipped == YES`, so `convertPoint_fromView(..., None)`
    // performs the window→view→top-left-origin flip.
    let view_point = content_view.convertPoint_fromView(window_point, None);
    let scale = window.backingScaleFactor();
    Some(Point::new(view_point.x * scale, view_point.y * scale))
}

fn map_pointing_device_type(pt: NSPointingDeviceType) -> ToolKind {
    match pt {
        NSPointingDeviceType::Pen => ToolKind::Pen,
        NSPointingDeviceType::Eraser => ToolKind::Eraser,
        NSPointingDeviceType::Cursor => ToolKind::Mouse,
        _ => ToolKind::Unknown,
    }
}

fn map_capability_mask(mask: u64) -> ToolCaps {
    let mut caps = ToolCaps::empty();
    if mask & NX_TABLET_CAPABILITY_PRESSURE != 0 {
        caps |= ToolCaps::PRESSURE;
    }
    if mask & (NX_TABLET_CAPABILITY_TILT_X | NX_TABLET_CAPABILITY_TILT_Y) != 0 {
        caps |= ToolCaps::TILT;
    }
    if mask & NX_TABLET_CAPABILITY_ROTATION != 0 {
        caps |= ToolCaps::TWIST;
    }
    if mask & NX_TABLET_CAPABILITY_TANGENTIAL_PRESSURE != 0 {
        caps |= ToolCaps::TANGENTIAL_PRESSURE;
    }
    if mask & NX_TABLET_CAPABILITY_BUTTONS != 0 {
        caps |= ToolCaps::BARREL_BUTTON;
    }
    caps
}

fn u32_from_usize(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

fn ns_string(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}
