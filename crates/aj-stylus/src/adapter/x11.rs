//! X11 / `XInput2` tablet seam. X11 delivers fully-resolved samples (no
//! Estimated/Revise cycle — the backend merges axis deltas with its
//! per-device cache) and no reliable timestamp, so `handle_x11_raw` uses
//! `Instant::now()`-relative timing and emits `SampleClass::Committed`
//! directly.

use std::time::Duration;

use aj_core::{PointerId, Sample, StylusButtons, Tilt, ToolCaps, ToolKind};
use kurbo::Point;

use super::{OPTIMISTIC_PEN_CAPS, PenState, StylusAdapter, alloc_pointer_id};
use crate::{Phase, StylusEvent};

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum X11TabletPhase {
    Down,
    Move,
    Up,
}

/// Raw sample supplied by the X11 / `XInput2` backend. The backend has already
/// merged per-device axis deltas with its last-known cache — X11 delivers only
/// changed valuators per `XI_Motion`, and the full-sample reconstruction
/// belongs in the backend, not the adapter. No `timestamp_secs` because
/// `XIDeviceEvent.time` is ms since X server start and not monotonic; the
/// adapter stamps with `Instant::now()` on receipt.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct X11RawSample {
    pub position_physical_px: Point,
    pub pressure: f32,
    pub tilt: Tilt,
    pub twist_deg: f32,
    pub tangential_pressure: f32,
    pub button_mask: u32,
    pub device_id: u32,
    pub pointing_device_type: ToolKind,
    pub source_phase: X11TabletPhase,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct X11ProximitySample {
    pub device_id: u32,
    pub unique_id: Option<u64>,
    pub pointing_device_type: ToolKind,
    pub caps: ToolCaps,
    pub is_entering: bool,
}

impl StylusAdapter {
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn handle_x11_raw(&mut self, raw: X11RawSample) {
        let ts = self.current_duration();

        let (caps, tool, active_pid) = {
            let pen = self.pens.entry(raw.device_id).or_insert_with(|| PenState {
                active_pointer_id: None,
                caps: OPTIMISTIC_PEN_CAPS,
                tool: raw.pointing_device_type,
                unique_id: None,
                last_position: None,
            });
            pen.last_position = Some(raw.position_physical_px);
            (pen.caps, pen.tool, pen.active_pointer_id)
        };

        match raw.source_phase {
            X11TabletPhase::Down => {
                let pid = alloc_pointer_id(&mut self.next_pointer_id);
                if let Some(pen) = self.pens.get_mut(&raw.device_id) {
                    pen.active_pointer_id = Some(pid);
                }
                self.active_pen_pointer = Some(pid);
                let sample = build_x11_pen_sample(&raw, ts, pid, tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Down, caps });
            }
            X11TabletPhase::Move | X11TabletPhase::Up => {
                let Some(pid) = active_pid else {
                    return;
                };
                let phase = match raw.source_phase {
                    X11TabletPhase::Move => Phase::Move,
                    X11TabletPhase::Up => Phase::Up,
                    X11TabletPhase::Down => unreachable!(),
                };
                let sample = build_x11_pen_sample(&raw, ts, pid, tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase, caps });

                if matches!(raw.source_phase, X11TabletPhase::Up) {
                    if let Some(pen) = self.pens.get_mut(&raw.device_id) {
                        pen.active_pointer_id = None;
                    }
                    if self.active_pen_pointer == Some(pid) {
                        self.active_pen_pointer = None;
                    }
                }
            }
        }
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn handle_x11_proximity(&mut self, prox: X11ProximitySample) {
        if prox.is_entering {
            let entry = self.pens.entry(prox.device_id).or_insert(PenState {
                active_pointer_id: None,
                caps: prox.caps,
                tool: prox.pointing_device_type,
                unique_id: prox.unique_id,
                last_position: None,
            });
            entry.caps = prox.caps;
            entry.tool = prox.pointing_device_type;
            entry.unique_id = prox.unique_id;
        } else if let Some(mut pen) = self.pens.remove(&prox.device_id)
            && let Some(pid) = pen.active_pointer_id.take()
        {
            let ts = self.current_duration();
            let mut sample = Sample::new_pen_placeholder(
                pen.last_position.unwrap_or(Point::ZERO),
                ts,
                pid,
                pen.tool,
            );
            let caps = pen.caps;
            sample.tool = pen.tool;
            self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Cancel, caps });
            let _ = self.take_pending_for_pointer(pid);
            if self.active_pen_pointer == Some(pid) {
                self.active_pen_pointer = None;
            }
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn build_x11_pen_sample(
    raw: &X11RawSample,
    timestamp: Duration,
    pointer_id: PointerId,
    tool: ToolKind,
) -> Sample {
    let mut buttons = StylusButtons::CONTACT;
    if raw.button_mask & 0x2 != 0 {
        buttons |= StylusButtons::BARREL;
    }
    if raw.button_mask & 0x4 != 0 {
        buttons |= StylusButtons::SECONDARY;
    }
    if matches!(tool, ToolKind::Eraser) {
        buttons |= StylusButtons::INVERTED;
    }
    let mut sample = Sample::new_pen(raw.position_physical_px, timestamp, pointer_id, tool);
    sample.pressure = raw.pressure.clamp(0.0, 1.0);
    sample.tilt = Some(raw.tilt);
    sample.twist_deg = Some(raw.twist_deg);
    sample.tangential_pressure = Some(raw.tangential_pressure);
    sample.buttons = buttons;
    sample
}

#[cfg(test)]
mod tests {
    use aj_core::{SampleClass, StylusButtons, Tilt, ToolCaps, ToolKind};
    use kurbo::Point;

    use super::super::tests_common::{adapter, drained, expect_sample};
    use super::*;
    use crate::Phase;

    fn x11_raw(
        device_id: u32,
        phase: X11TabletPhase,
        pos: (f64, f64),
        pressure: f32,
    ) -> X11RawSample {
        X11RawSample {
            position_physical_px: Point::new(pos.0, pos.1),
            pressure,
            tilt: Tilt { x_deg: 0.0, y_deg: 0.0 },
            twist_deg: 0.0,
            tangential_pressure: 0.0,
            button_mask: 0,
            device_id,
            pointing_device_type: ToolKind::Pen,
            source_phase: phase,
        }
    }

    fn x11_proximity(device_id: u32, tool: ToolKind, is_entering: bool) -> X11ProximitySample {
        X11ProximitySample {
            device_id,
            unique_id: Some(0xBEEF),
            pointing_device_type: tool,
            caps: ToolCaps::PRESSURE | ToolCaps::TILT,
            is_entering,
        }
    }

    #[test]
    fn x11_down_move_up_emits_committed_samples() {
        let mut a = adapter();
        a.handle_x11_proximity(x11_proximity(1, ToolKind::Pen, true));
        a.handle_x11_raw(x11_raw(1, X11TabletPhase::Down, (10.0, 20.0), 0.3));
        a.handle_x11_raw(x11_raw(1, X11TabletPhase::Move, (11.0, 21.0), 0.5));
        a.handle_x11_raw(x11_raw(1, X11TabletPhase::Up, (11.0, 21.0), 0.5));

        let events = drained(&mut a);
        assert_eq!(events.len(), 3);
        let phases: Vec<Phase> = events.iter().map(|e| expect_sample(e).1).collect();
        assert_eq!(phases, vec![Phase::Down, Phase::Move, Phase::Up]);
        for ev in &events {
            let (sample, _, _) = expect_sample(ev);
            assert_eq!(sample.class, SampleClass::Committed);
        }
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn x11_proximity_out_with_active_stroke_cancels() {
        let mut a = adapter();
        a.handle_x11_proximity(x11_proximity(2, ToolKind::Pen, true));
        a.handle_x11_raw(x11_raw(2, X11TabletPhase::Down, (0.0, 0.0), 0.4));
        let _ = drained(&mut a);

        a.handle_x11_proximity(x11_proximity(2, ToolKind::Pen, false));
        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn x11_eraser_tool_yields_inverted_buttons() {
        let mut a = adapter();
        a.handle_x11_proximity(x11_proximity(3, ToolKind::Eraser, true));
        let mut r = x11_raw(3, X11TabletPhase::Down, (5.0, 5.0), 0.2);
        r.pointing_device_type = ToolKind::Eraser;
        a.handle_x11_raw(r);

        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert_eq!(sample.tool, ToolKind::Eraser);
        assert!(sample.buttons.contains(StylusButtons::INVERTED));
    }

    #[test]
    fn x11_move_without_prior_down_is_dropped() {
        let mut a = adapter();
        a.handle_x11_proximity(x11_proximity(4, ToolKind::Pen, true));
        a.handle_x11_raw(x11_raw(4, X11TabletPhase::Move, (1.0, 1.0), 0.5));
        assert!(drained(&mut a).is_empty());
    }
}
