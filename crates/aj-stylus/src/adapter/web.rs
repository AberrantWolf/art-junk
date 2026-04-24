//! Web Pointer Events seam. Pointer Events deliver fully-resolved samples;
//! no Estimated/Revise cycle needed. `Predicted` phases emit
//! `SampleClass::Predicted` so the engine can draw them to the Placeholder
//! layer only and discard on next real delivery. Touch pointers are gated
//! at the backend by a live-pen set; finger-while-pen palm rejection
//! happens before the adapter sees the touch sample.

use std::time::Duration;

use aj_core::{PointerId, Sample, SampleClass, StylusButtons, Tilt, ToolCaps, ToolKind};
use kurbo::Point;

use super::{OPTIMISTIC_PEN_CAPS, PlatformTimestampAnchor, StylusAdapter, alloc_pointer_id};
use crate::{Phase, StylusEvent};

/// Per-gesture state for one Pointer Events pointer. Owning the tool kind
/// here (rather than re-classifying from every raw sample) keeps the stroke
/// stable if the browser momentarily reports a different `buttons` bitmask
/// mid-stroke — observed with Wacom + Chromium when the side switch toggles
/// during a drag.
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) struct WebPointerState {
    pub(crate) adapter_pointer_id: PointerId,
    pub(crate) tool: ToolKind,
    pub(crate) caps: ToolCaps,
    pub(crate) last_position: Option<Point>,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebPointerType {
    Pen,
    Touch,
    Mouse,
    Unknown,
}

/// Phase for a Web raw sample. `Hover` is synthesized by the backend when a
/// `pen` pointer moves with `pressure === 0` (no tip contact). `Predicted`
/// is for events drained from `getPredictedEvents()` — they render to the
/// Placeholder layer only and discard on the next real delivery.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebSourcePhase {
    Down,
    Move,
    Up,
    Cancel,
    Hover,
    Predicted,
}

/// Raw sample supplied by the Web Pointer Events backend. Timestamp is
/// `event.timeStamp / 1000.0` (`DOMHighResTimeStamp` is ms since navigation
/// start; monotonic). Position is physical px canvas-relative, pre-converted
/// by the backend from `(client_x - rect.left) * devicePixelRatio`.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WebRawSample {
    pub position_physical_px: Point,
    pub timestamp_secs: f64,
    pub pressure: f32,
    pub tilt: Option<Tilt>,
    pub twist_deg: Option<f32>,
    pub tangential_pressure: Option<f32>,
    pub button_mask: u32,
    /// JS `PointerEvent.pointerId` — stable for the gesture only, not across.
    pub pointer_id: i32,
    pub pointer_type: WebPointerType,
    pub source_phase: WebSourcePhase,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WebProximitySample {
    pub pointer_id: i32,
    pub pointer_type: WebPointerType,
    pub caps: ToolCaps,
    pub is_entering: bool,
}

impl StylusAdapter {
    #[cfg(any(target_arch = "wasm32", test))]
    pub(crate) fn handle_web_raw(&mut self, raw: WebRawSample) {
        let ts = PlatformTimestampAnchor::translate_or_anchor(
            &mut self.web_anchor,
            raw.timestamp_secs,
            self.epoch,
        );

        let tool = classify_web_tool(raw.pointer_type, raw.button_mask);

        // Hover: no stroke state; attach to MOUSE pointer id.
        if matches!(raw.source_phase, WebSourcePhase::Hover) {
            if matches!(raw.pointer_type, WebPointerType::Pen) {
                let caps =
                    self.web_pointers.get(&raw.pointer_id).map_or(OPTIMISTIC_PEN_CAPS, |s| s.caps);
                let sample = build_web_sample(&raw, ts, PointerId::MOUSE, tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Hover, caps });
            }
            return;
        }

        match raw.source_phase {
            WebSourcePhase::Down => {
                let pid = alloc_pointer_id(&mut self.next_pointer_id);
                self.web_pointers.insert(
                    raw.pointer_id,
                    WebPointerState {
                        adapter_pointer_id: pid,
                        tool,
                        caps: OPTIMISTIC_PEN_CAPS,
                        last_position: Some(raw.position_physical_px),
                    },
                );
                if matches!(raw.pointer_type, WebPointerType::Pen) {
                    self.active_pen_pointer = Some(pid);
                }
                let caps = OPTIMISTIC_PEN_CAPS;
                let sample = build_web_sample(&raw, ts, pid, tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Down, caps });
            }
            WebSourcePhase::Move | WebSourcePhase::Predicted => {
                let Some(state) = self.web_pointers.get_mut(&raw.pointer_id) else {
                    return;
                };
                state.last_position = Some(raw.position_physical_px);
                let pid = state.adapter_pointer_id;
                let pinned_tool = state.tool;
                let caps = state.caps;
                let mut sample = build_web_sample(&raw, ts, pid, pinned_tool);
                if matches!(raw.source_phase, WebSourcePhase::Predicted) {
                    sample.class = SampleClass::Predicted;
                }
                self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Move, caps });
            }
            WebSourcePhase::Up | WebSourcePhase::Cancel => {
                let Some(state) = self.web_pointers.remove(&raw.pointer_id) else {
                    return;
                };
                let pid = state.adapter_pointer_id;
                let caps = state.caps;
                let phase = match raw.source_phase {
                    WebSourcePhase::Up => Phase::Up,
                    WebSourcePhase::Cancel => Phase::Cancel,
                    _ => unreachable!(),
                };
                let sample = build_web_sample(&raw, ts, pid, state.tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase, caps });
                if self.active_pen_pointer == Some(pid) {
                    self.active_pen_pointer = None;
                }
            }
            WebSourcePhase::Hover => unreachable!("handled above"),
        }
    }

    #[cfg(any(target_arch = "wasm32", test))]
    pub(crate) fn handle_web_proximity(&mut self, prox: WebProximitySample) {
        if prox.is_entering {
            if let Some(state) = self.web_pointers.get_mut(&prox.pointer_id) {
                state.caps = prox.caps;
            }
        } else if let Some(state) = self.web_pointers.remove(&prox.pointer_id) {
            let ts = self.current_duration();
            let pid = state.adapter_pointer_id;
            let mut sample = Sample::new_pen_placeholder(
                state.last_position.unwrap_or(Point::ZERO),
                ts,
                pid,
                state.tool,
            );
            let caps = state.caps;
            sample.tool = state.tool;
            self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Cancel, caps });
            let _ = self.take_pending_for_pointer(pid);
            if self.active_pen_pointer == Some(pid) {
                self.active_pen_pointer = None;
            }
        }
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn classify_web_tool(pointer_type: WebPointerType, button_mask: u32) -> ToolKind {
    // Eraser is signalled by `buttons & 0x20` — Pointer Events has no
    // dedicated pointer_type for eraser, unlike iOS / Android.
    match (pointer_type, button_mask & 0x20 != 0) {
        (WebPointerType::Pen, true) => ToolKind::Eraser,
        (WebPointerType::Pen, false) => ToolKind::Pen,
        (WebPointerType::Mouse, _) => ToolKind::Mouse,
        (WebPointerType::Touch, _) => ToolKind::Finger,
        (WebPointerType::Unknown, _) => ToolKind::Unknown,
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn build_web_sample(
    raw: &WebRawSample,
    timestamp: Duration,
    pointer_id: PointerId,
    tool: ToolKind,
) -> Sample {
    // Pointer Events `buttons` bits: 0x1 primary (tip), 0x2 secondary/barrel,
    // 0x20 eraser. Map barrel → StylusButtons::BARREL; eraser already
    // handled above via tool classification.
    let mut buttons = StylusButtons::CONTACT;
    if raw.button_mask & 0x2 != 0 {
        buttons |= StylusButtons::BARREL;
    }
    if matches!(tool, ToolKind::Eraser) {
        buttons |= StylusButtons::INVERTED;
    }
    let mut sample = Sample::new_pen(raw.position_physical_px, timestamp, pointer_id, tool);
    sample.pressure = raw.pressure.clamp(0.0, 1.0);
    sample.tilt = raw.tilt;
    sample.twist_deg = raw.twist_deg;
    sample.tangential_pressure = raw.tangential_pressure;
    sample.buttons = buttons;
    sample
}

#[cfg(test)]
mod tests {
    use aj_core::{PointerId, SampleClass, StylusButtons, Tilt, ToolKind};
    use kurbo::Point;

    use super::super::OPTIMISTIC_PEN_CAPS;
    use super::super::tests_common::{adapter, drained, expect_sample};
    use super::*;
    use crate::Phase;

    fn web_raw(
        pointer_id: i32,
        phase: WebSourcePhase,
        pos: (f64, f64),
        ts: f64,
        pressure: f32,
        pointer_type: WebPointerType,
    ) -> WebRawSample {
        WebRawSample {
            position_physical_px: Point::new(pos.0, pos.1),
            timestamp_secs: ts,
            pressure,
            tilt: Some(Tilt { x_deg: 0.0, y_deg: 0.0 }),
            twist_deg: Some(0.0),
            tangential_pressure: None,
            button_mask: 0,
            pointer_id,
            pointer_type,
            source_phase: phase,
        }
    }

    #[test]
    fn web_down_move_up_emits_committed_samples() {
        let mut a = adapter();
        a.handle_web_raw(web_raw(
            7,
            WebSourcePhase::Down,
            (0.0, 0.0),
            0.0,
            0.4,
            WebPointerType::Pen,
        ));
        a.handle_web_raw(web_raw(
            7,
            WebSourcePhase::Move,
            (1.0, 1.0),
            0.008,
            0.5,
            WebPointerType::Pen,
        ));
        a.handle_web_raw(web_raw(
            7,
            WebSourcePhase::Up,
            (1.0, 1.0),
            0.016,
            0.0,
            WebPointerType::Pen,
        ));

        let events = drained(&mut a);
        let phases: Vec<Phase> = events.iter().map(|e| expect_sample(e).1).collect();
        assert_eq!(phases, vec![Phase::Down, Phase::Move, Phase::Up]);
        for ev in &events {
            let (s, _, _) = expect_sample(ev);
            assert_eq!(s.class, SampleClass::Committed);
        }
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn web_predicted_samples_tagged_predicted_class() {
        let mut a = adapter();
        a.handle_web_raw(web_raw(
            1,
            WebSourcePhase::Down,
            (0.0, 0.0),
            0.0,
            0.3,
            WebPointerType::Pen,
        ));
        let _ = drained(&mut a);

        a.handle_web_raw(web_raw(
            1,
            WebSourcePhase::Predicted,
            (2.0, 2.0),
            0.016,
            0.4,
            WebPointerType::Pen,
        ));
        let events = drained(&mut a);
        let (sample, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Move);
        assert_eq!(sample.class, SampleClass::Predicted);
    }

    #[test]
    fn web_eraser_via_buttons_bit_0x20() {
        let mut a = adapter();
        let mut raw = web_raw(1, WebSourcePhase::Down, (0.0, 0.0), 0.0, 0.4, WebPointerType::Pen);
        raw.button_mask = 0x20;
        a.handle_web_raw(raw);

        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert_eq!(sample.tool, ToolKind::Eraser);
        assert!(sample.buttons.contains(StylusButtons::INVERTED));
    }

    #[test]
    fn web_pen_hover_emits_hover_phase() {
        let mut a = adapter();
        a.handle_web_raw(web_raw(
            1,
            WebSourcePhase::Hover,
            (5.0, 5.0),
            0.0,
            0.0,
            WebPointerType::Pen,
        ));

        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (sample, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Hover);
        assert_eq!(sample.pointer_id, PointerId::MOUSE);
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn web_pointercancel_synthesizes_cancel() {
        let mut a = adapter();
        a.handle_web_raw(web_raw(
            1,
            WebSourcePhase::Down,
            (0.0, 0.0),
            0.0,
            0.3,
            WebPointerType::Pen,
        ));
        let _ = drained(&mut a);

        a.handle_web_raw(web_raw(
            1,
            WebSourcePhase::Cancel,
            (1.0, 1.0),
            0.016,
            0.0,
            WebPointerType::Pen,
        ));
        let events = drained(&mut a);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn web_proximity_out_mid_stroke_cancels() {
        let mut a = adapter();
        a.handle_web_raw(web_raw(
            1,
            WebSourcePhase::Down,
            (0.0, 0.0),
            0.0,
            0.3,
            WebPointerType::Pen,
        ));
        let _ = drained(&mut a);

        a.handle_web_proximity(WebProximitySample {
            pointer_id: 1,
            pointer_type: WebPointerType::Pen,
            caps: OPTIMISTIC_PEN_CAPS,
            is_entering: false,
        });
        let events = drained(&mut a);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
    }

    #[test]
    fn web_mouse_pointer_maps_to_mouse_tool() {
        let mut a = adapter();
        a.handle_web_raw(web_raw(
            0,
            WebSourcePhase::Down,
            (0.0, 0.0),
            0.0,
            0.5,
            WebPointerType::Mouse,
        ));
        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert_eq!(sample.tool, ToolKind::Mouse);
    }

    #[test]
    fn web_move_without_prior_down_is_dropped() {
        let mut a = adapter();
        a.handle_web_raw(web_raw(
            1,
            WebSourcePhase::Move,
            (1.0, 1.0),
            0.0,
            0.5,
            WebPointerType::Pen,
        ));
        assert!(drained(&mut a).is_empty());
    }

    #[test]
    fn web_touch_and_unknown_pointer_types_map_correctly() {
        let mut a = adapter();
        a.handle_web_raw(web_raw(
            2,
            WebSourcePhase::Down,
            (0.0, 0.0),
            0.0,
            0.5,
            WebPointerType::Touch,
        ));
        let events = drained(&mut a);
        assert_eq!(expect_sample(&events[0]).0.tool, ToolKind::Finger);

        let mut b = adapter();
        b.handle_web_raw(web_raw(
            3,
            WebSourcePhase::Down,
            (0.0, 0.0),
            0.0,
            0.5,
            WebPointerType::Unknown,
        ));
        let events = drained(&mut b);
        assert_eq!(expect_sample(&events[0]).0.tool, ToolKind::Unknown);
    }
}
