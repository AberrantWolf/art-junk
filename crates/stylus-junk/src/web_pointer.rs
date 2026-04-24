//! Web Pointer Events backend. Attaches listeners to the winit-created
//! `HtmlCanvasElement` that winit's Pointer-Events path doesn't surface —
//! pressure, tilt, twist, tangential, coalesced sub-events, and predicted
//! extrapolation. Coexists with winit's listeners (events dispatch to all
//! handlers attached to the same target).
//!
//! Design decisions (see `.claude/skills/stylus-input/web.md`):
//!
//! - **Own listeners over winit's.** Winit 0.30.x drops pressure / tilt /
//!   coalesced / predicted on the web target; re-wrapping isn't sufficient.
//! - **`touch-action: none` + `setPointerCapture`** on the canvas. First
//!   prevents the browser's pan/zoom gestures from stealing the stroke;
//!   second routes subsequent moves/ups to our canvas even if the pointer
//!   drifts into chrome.
//! - **Drain `getCoalescedEvents()` every `pointermove`.** Browsers align
//!   Pointer Events to frame cadence; a pen at 240–500 Hz drops samples
//!   unless we iterate the coalesced array for each delivery.
//! - **Feature-detect `getPredictedEvents()`.** Chrome/Edge yes, Safari
//!   recent, Firefox not yet. Emit predicted as `SampleClass::Predicted`;
//!   the engine draws them to the Placeholder layer only and discards on
//!   next real delivery.
//! - **Eraser via `buttons & 0x20`.** Pointer Events has no "eraser"
//!   `pointerType` — Surface Pen and Wacom on Windows both signal the
//!   flipped-end state through this bit.
//! - **Palm-rejection gate at the backend.** Live-pen `HashSet<i32>`; while
//!   non-empty, touch `pointermove`/`pointerdown` are dropped before they
//!   reach the adapter.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use crate::{Point, Tilt, ToolCaps};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{
    AddEventListenerOptions, Event, HtmlCanvasElement, HtmlElement, MouseEvent, PointerEvent,
};

use crate::StylusAdapter;
use crate::adapter::{WebPointerType, WebProximitySample, WebRawSample, WebSourcePhase};

#[derive(Debug, thiserror::Error)]
pub enum WebStylusAttachError {
    #[error("canvas style access failed: {0}")]
    Style(String),
    #[error("JS event-listener registration failed: {0}")]
    Register(String),
}

/// RAII bridge that keeps closures alive as long as the canvas listeners
/// are registered. Drop removes every listener and drops every closure,
/// which is required for dev-server hot reload (otherwise stale closures
/// keep firing against a stale adapter).
pub struct WebStylusBridge {
    canvas: HtmlCanvasElement,
    listeners: Vec<RegisteredListener>,
    /// Live-pen pointer-ids. Mutated by the closures themselves; shared
    /// via `Rc<RefCell<_>>` so palm-rejection state survives across
    /// individual event deliveries.
    _live_pens: Rc<RefCell<HashSet<i32>>>,
}

struct RegisteredListener {
    event: &'static str,
    closure: Closure<dyn FnMut(Event)>,
}

impl Drop for WebStylusBridge {
    fn drop(&mut self) {
        for l in &self.listeners {
            let _ = self
                .canvas
                .remove_event_listener_with_callback(l.event, l.closure.as_ref().unchecked_ref());
        }
    }
}

/// Attach Pointer Events listeners + canvas styling.
pub fn attach(
    canvas: &HtmlCanvasElement,
    adapter: Rc<RefCell<StylusAdapter>>,
) -> Result<WebStylusBridge, WebStylusAttachError> {
    apply_canvas_styles(canvas)?;

    let live_pens: Rc<RefCell<HashSet<i32>>> = Rc::new(RefCell::new(HashSet::new()));
    let mut listeners: Vec<RegisteredListener> = Vec::new();

    register(
        canvas,
        &mut listeners,
        "pointerdown",
        make_handler(adapter.clone(), live_pens.clone(), canvas.clone(), PointerAction::Down),
    )?;
    register(
        canvas,
        &mut listeners,
        "pointermove",
        make_handler(adapter.clone(), live_pens.clone(), canvas.clone(), PointerAction::Move),
    )?;
    register(
        canvas,
        &mut listeners,
        "pointerup",
        make_handler(adapter.clone(), live_pens.clone(), canvas.clone(), PointerAction::Up),
    )?;
    register(
        canvas,
        &mut listeners,
        "pointercancel",
        make_handler(adapter.clone(), live_pens.clone(), canvas.clone(), PointerAction::Cancel),
    )?;
    register(
        canvas,
        &mut listeners,
        "pointerleave",
        make_handler(adapter.clone(), live_pens.clone(), canvas.clone(), PointerAction::Leave),
    )?;
    register(
        canvas,
        &mut listeners,
        "lostpointercapture",
        make_handler(adapter.clone(), live_pens.clone(), canvas.clone(), PointerAction::Cancel),
    )?;
    register(
        canvas,
        &mut listeners,
        "pointerover",
        make_handler(adapter.clone(), live_pens.clone(), canvas.clone(), PointerAction::Over),
    )?;
    register(
        canvas,
        &mut listeners,
        "pointerout",
        make_handler(adapter, live_pens.clone(), canvas.clone(), PointerAction::Out),
    )?;

    Ok(WebStylusBridge { canvas: canvas.clone(), listeners, _live_pens: live_pens })
}

#[derive(Debug, Clone, Copy)]
enum PointerAction {
    Down,
    Move,
    Up,
    Cancel,
    Leave,
    Over,
    Out,
}

fn apply_canvas_styles(canvas: &HtmlCanvasElement) -> Result<(), WebStylusAttachError> {
    // HtmlCanvasElement → HtmlElement coercion for style access.
    let html: &HtmlElement = canvas.unchecked_ref();
    let style = html.style();
    for (prop, val) in [
        ("touch-action", "none"),
        ("user-select", "none"),
        ("-webkit-user-select", "none"),
        ("-webkit-touch-callout", "none"),
    ] {
        style.set_property(prop, val).map_err(|e| WebStylusAttachError::Style(format!("{e:?}")))?;
    }
    Ok(())
}

fn register(
    canvas: &HtmlCanvasElement,
    listeners: &mut Vec<RegisteredListener>,
    event: &'static str,
    closure: Closure<dyn FnMut(Event)>,
) -> Result<(), WebStylusAttachError> {
    // `passive: false` so `preventDefault()` actually cancels the browser's
    // gesture handling; `capture: true` so we see events before ancestor
    // elements do.
    let opts = AddEventListenerOptions::new();
    opts.set_passive(false);
    opts.set_capture(true);

    canvas
        .add_event_listener_with_callback_and_add_event_listener_options(
            event,
            closure.as_ref().unchecked_ref(),
            &opts,
        )
        .map_err(|e| WebStylusAttachError::Register(format!("{e:?}")))?;
    listeners.push(RegisteredListener { event, closure });
    Ok(())
}

fn make_handler(
    adapter: Rc<RefCell<StylusAdapter>>,
    live_pens: Rc<RefCell<HashSet<i32>>>,
    canvas: HtmlCanvasElement,
    action: PointerAction,
) -> Closure<dyn FnMut(Event)> {
    Closure::<dyn FnMut(Event)>::new(move |event: Event| {
        let Ok(pe) = event.dyn_into::<PointerEvent>() else {
            return;
        };
        dispatch(&adapter, &live_pens, &canvas, action, pe);
    })
}

fn dispatch(
    adapter: &Rc<RefCell<StylusAdapter>>,
    live_pens: &Rc<RefCell<HashSet<i32>>>,
    canvas: &HtmlCanvasElement,
    action: PointerAction,
    event: PointerEvent,
) {
    let ptype = classify_pointer_type(&event);
    let pid = event.pointer_id();

    // Palm-rejection gate: while any pen is live, drop touch pointers.
    if matches!(ptype, WebPointerType::Touch) && !live_pens.borrow().is_empty() {
        let _ = event.prevent_default();
        return;
    }

    match action {
        PointerAction::Down => {
            let _ = event.prevent_default();
            if matches!(ptype, WebPointerType::Pen) {
                live_pens.borrow_mut().insert(pid);
                // Capture the pointer so subsequent moves/ups come to us
                // even if the pointer drifts off-canvas.
                let _ = canvas.set_pointer_capture(pid);
            }
            let raw = build_raw(&event, canvas, ptype, WebSourcePhase::Down);
            deliver_raw(adapter, raw);
        }
        PointerAction::Move => {
            let _ = event.prevent_default();
            drain_coalesced(adapter, canvas, &event, ptype);
            drain_predicted(adapter, canvas, &event, ptype);
        }
        PointerAction::Up => {
            let _ = event.prevent_default();
            live_pens.borrow_mut().remove(&pid);
            let raw = build_raw(&event, canvas, ptype, WebSourcePhase::Up);
            deliver_raw(adapter, raw);
        }
        PointerAction::Cancel => {
            live_pens.borrow_mut().remove(&pid);
            let raw = build_raw(&event, canvas, ptype, WebSourcePhase::Cancel);
            deliver_raw(adapter, raw);
        }
        PointerAction::Leave => {
            // pointerleave — pointer left the element. If a stroke was
            // active, the capture usually keeps it alive; if pen was
            // removed from hover, we need to synthesize proximity-out so
            // the adapter cleans up if it was tracking.
            if matches!(ptype, WebPointerType::Pen) {
                live_pens.borrow_mut().remove(&pid);
                deliver_prox(
                    adapter,
                    WebProximitySample {
                        pointer_id: pid,
                        pointer_type: ptype,
                        caps: optimistic_caps(),
                        is_entering: false,
                    },
                );
            }
        }
        PointerAction::Over => {
            if matches!(ptype, WebPointerType::Pen) {
                deliver_prox(
                    adapter,
                    WebProximitySample {
                        pointer_id: pid,
                        pointer_type: ptype,
                        caps: optimistic_caps(),
                        is_entering: true,
                    },
                );
                // Pen over with pressure 0 — synthesize a Hover sample so
                // brush-preview UX works without initiating a stroke.
                if event.pressure() == 0.0 {
                    let raw = build_raw(&event, canvas, ptype, WebSourcePhase::Hover);
                    deliver_raw(adapter, raw);
                }
            }
        }
        PointerAction::Out => {
            if matches!(ptype, WebPointerType::Pen) {
                deliver_prox(
                    adapter,
                    WebProximitySample {
                        pointer_id: pid,
                        pointer_type: ptype,
                        caps: optimistic_caps(),
                        is_entering: false,
                    },
                );
            }
        }
    }
}

fn drain_coalesced(
    adapter: &Rc<RefCell<StylusAdapter>>,
    canvas: &HtmlCanvasElement,
    event: &PointerEvent,
    ptype: WebPointerType,
) {
    let list = event.get_coalesced_events();
    let len = list.length();
    if len == 0 {
        // Safari sometimes returns an empty array — fall back to the
        // dispatched event itself.
        let raw = build_raw(event, canvas, ptype, WebSourcePhase::Move);
        deliver_raw(adapter, raw);
        return;
    }
    for i in 0..len {
        let Some(sub) = list.get(i).dyn_into::<PointerEvent>().ok() else {
            continue;
        };
        let raw = build_raw(&sub, canvas, ptype, WebSourcePhase::Move);
        deliver_raw(adapter, raw);
    }
}

fn drain_predicted(
    adapter: &Rc<RefCell<StylusAdapter>>,
    canvas: &HtmlCanvasElement,
    event: &PointerEvent,
    ptype: WebPointerType,
) {
    // `getPredictedEvents` is Chrome/Edge (and recent Safari); Firefox
    // doesn't implement it. `web-sys` may or may not have a binding
    // depending on version; call via `js_sys::Reflect` so missing-method
    // degrades to an empty list instead of a panic.
    let key = JsValue::from_str("getPredictedEvents");
    let Ok(func) = js_sys::Reflect::get(event.as_ref(), &key) else {
        return;
    };
    let Some(func) = func.dyn_ref::<js_sys::Function>() else {
        return;
    };
    let Ok(result) = func.call0(event.as_ref()) else {
        return;
    };
    let Ok(list) = result.dyn_into::<js_sys::Array>() else {
        return;
    };
    let len = list.length();
    for i in 0..len {
        let Some(sub) = list.get(i).dyn_into::<PointerEvent>().ok() else {
            continue;
        };
        let raw = build_raw(&sub, canvas, ptype, WebSourcePhase::Predicted);
        deliver_raw(adapter, raw);
    }
}

fn classify_pointer_type(event: &PointerEvent) -> WebPointerType {
    match event.pointer_type().as_str() {
        "pen" => WebPointerType::Pen,
        "touch" => WebPointerType::Touch,
        "mouse" => WebPointerType::Mouse,
        _ => WebPointerType::Unknown,
    }
}

fn build_raw(
    event: &PointerEvent,
    canvas: &HtmlCanvasElement,
    ptype: WebPointerType,
    phase: WebSourcePhase,
) -> WebRawSample {
    let position = to_canvas_physical(event, canvas);
    // DOMHighResTimeStamp is ms since navigation start — monotonic per page.
    let timestamp_secs = event.time_stamp() / 1000.0;
    let pressure = event.pressure() as f32;
    let tilt = Some(Tilt { x_deg: event.tilt_x() as f32, y_deg: event.tilt_y() as f32 });
    let twist_deg = Some(event.twist() as f32);
    let tangential_pressure = Some(event.tangential_pressure() as f32);
    // `PointerEvent.buttons` bitmask: low bits are primary / secondary; the
    // eraser bit (0x20) gates tool classification in the adapter.
    let button_mask = u32::from(event.buttons());
    WebRawSample {
        position_physical_px: position,
        timestamp_secs,
        pressure,
        tilt,
        twist_deg,
        tangential_pressure,
        button_mask,
        pointer_id: event.pointer_id(),
        pointer_type: ptype,
        source_phase: phase,
    }
}

fn to_canvas_physical(event: &MouseEvent, canvas: &HtmlCanvasElement) -> Point {
    let rect = canvas.get_bounding_client_rect();
    // `devicePixelRatio` converts CSS px → physical px. visualViewport.scale
    // during pinch-zoom would shift the mapping; ignored for v1.
    let dpr = web_sys::window().map_or(1.0, |w| w.device_pixel_ratio());
    Point::new(
        (f64::from(event.client_x()) - rect.left()) * dpr,
        (f64::from(event.client_y()) - rect.top()) * dpr,
    )
}

fn optimistic_caps() -> ToolCaps {
    ToolCaps::PRESSURE
        | ToolCaps::TILT
        | ToolCaps::TWIST
        | ToolCaps::TANGENTIAL_PRESSURE
        | ToolCaps::HOVER
}

fn deliver_raw(adapter: &Rc<RefCell<StylusAdapter>>, raw: WebRawSample) {
    match adapter.try_borrow_mut() {
        Ok(mut a) => a.handle_web_raw(raw),
        Err(_) => log::warn!("web pointer: adapter borrowed elsewhere, sample dropped"),
    }
}

fn deliver_prox(adapter: &Rc<RefCell<StylusAdapter>>, prox: WebProximitySample) {
    match adapter.try_borrow_mut() {
        Ok(mut a) => a.handle_web_proximity(prox),
        Err(_) => log::warn!("web pointer: adapter borrowed elsewhere, proximity dropped"),
    }
}
