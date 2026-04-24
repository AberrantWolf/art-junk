//! Windows Pointer Input API seam. Windows delivers fully-resolved pointer
//! samples with `penMask` bits indicating which axes are valid; the backend
//! pre-filters by mask and supplies `None` for missing axes. No
//! Estimated/Revise cycle — `WM_POINTERDOWN` carries pressure on the first
//! sample (unlike macOS `NSEvent`). Hover samples ride the same entry point
//! with `source_phase: Hover`.

use std::time::Duration;

use aj_core::{PointerId, Sample, StylusButtons, Tilt, ToolCaps, ToolKind};
use kurbo::Point;

use super::{
    OPTIMISTIC_PEN_CAPS, PenState, PlatformTimestampAnchor, StylusAdapter, alloc_pointer_id,
};
use crate::{Phase, StylusEvent};

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowsPointerPhase {
    Down,
    Move,
    Up,
    Cancel,
    /// Pointer `INRANGE && !INCONTACT` — pen hovering over the sensing
    /// volume without tip contact. Emits `Phase::Hover` without starting
    /// or continuing a stroke.
    Hover,
}

/// Raw sample supplied by the Windows Pointer Input API backend. Pressure
/// already normalized to `0..=1` (the raw `POINTER_PEN_INFO.pressure` is
/// `0..=1024`), tilt already in degrees (`-90..=90`), rotation already in
/// degrees (`0..=359`). `tangential_pressure` is always `None` — the Windows
/// Pointer API has no corresponding field.
#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WindowsRawSample {
    pub position_physical_px: Point,
    /// `POINTER_INFO.PerformanceCount` converted to seconds by dividing by
    /// the cached `QueryPerformanceFrequency`. Monotonic.
    pub timestamp_secs: f64,
    pub pressure: f32,
    pub tilt: Option<Tilt>,
    pub twist_deg: Option<f32>,
    pub button_mask: u32,
    /// Hashed `POINTER_INFO.sourceDevice` (`HANDLE`) — stable per physical
    /// digitizer for its plug-in lifetime. Not a cross-session identifier.
    pub device_id: u32,
    pub pointing_device_type: ToolKind,
    pub source_phase: WindowsPointerPhase,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WindowsProximitySample {
    pub device_id: u32,
    pub pointing_device_type: ToolKind,
    pub caps: ToolCaps,
    pub is_entering: bool,
}

impl StylusAdapter {
    #[cfg(any(target_os = "windows", test))]
    pub(crate) fn handle_windows_raw(&mut self, raw: WindowsRawSample) {
        let ts = PlatformTimestampAnchor::translate_or_anchor(
            &mut self.windows_anchor,
            raw.timestamp_secs,
            self.epoch,
        );

        // Hover does not allocate a stroke-owning PointerId; emit a Hover
        // sample tied to `PointerId::MOUSE` so the app can drive cursor /
        // brush-preview UI without disturbing mid-stroke state.
        if matches!(raw.source_phase, WindowsPointerPhase::Hover) {
            let sample = build_windows_sample(&raw, ts, PointerId::MOUSE, raw.pointing_device_type);
            let caps = self.pens.get(&raw.device_id).map_or(OPTIMISTIC_PEN_CAPS, |p| p.caps);
            self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Hover, caps });
            return;
        }

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
            WindowsPointerPhase::Down => {
                let pid = alloc_pointer_id(&mut self.next_pointer_id);
                if let Some(pen) = self.pens.get_mut(&raw.device_id) {
                    pen.active_pointer_id = Some(pid);
                }
                self.active_pen_pointer = Some(pid);
                let sample = build_windows_sample(&raw, ts, pid, tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Down, caps });
            }
            WindowsPointerPhase::Move | WindowsPointerPhase::Up | WindowsPointerPhase::Cancel => {
                let Some(pid) = active_pid else {
                    return;
                };
                let phase = match raw.source_phase {
                    WindowsPointerPhase::Move => Phase::Move,
                    WindowsPointerPhase::Up => Phase::Up,
                    WindowsPointerPhase::Cancel => Phase::Cancel,
                    WindowsPointerPhase::Down | WindowsPointerPhase::Hover => unreachable!(),
                };
                let sample = build_windows_sample(&raw, ts, pid, tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase, caps });

                if matches!(raw.source_phase, WindowsPointerPhase::Up | WindowsPointerPhase::Cancel)
                {
                    if let Some(pen) = self.pens.get_mut(&raw.device_id) {
                        pen.active_pointer_id = None;
                    }
                    if self.active_pen_pointer == Some(pid) {
                        self.active_pen_pointer = None;
                    }
                }
            }
            WindowsPointerPhase::Hover => unreachable!("handled above"),
        }
    }

    #[cfg(any(target_os = "windows", test))]
    pub(crate) fn handle_windows_proximity(&mut self, prox: WindowsProximitySample) {
        if prox.is_entering {
            let entry = self.pens.entry(prox.device_id).or_insert(PenState {
                active_pointer_id: None,
                caps: prox.caps,
                tool: prox.pointing_device_type,
                unique_id: None,
                last_position: None,
            });
            entry.caps = prox.caps;
            entry.tool = prox.pointing_device_type;
        } else if let Some(mut pen) = self.pens.remove(&prox.device_id)
            && let Some(pid) = pen.active_pointer_id.take()
        {
            // Pen left proximity with a stroke still active — the WM_POINTERUP
            // never arrived. Synthesize Cancel to match the mac proximity-out
            // contract.
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

#[cfg(any(target_os = "windows", test))]
fn build_windows_sample(
    raw: &WindowsRawSample,
    timestamp: Duration,
    pointer_id: PointerId,
    tool: ToolKind,
) -> Sample {
    // `PEN_FLAG_BARREL` and `PEN_FLAG_INVERTED` are pre-folded by the backend
    // into button_mask bits: 0x1 barrel, 0x2 secondary, 0x4 inverted. Mirror
    // the mac layout so callers stay platform-agnostic.
    let mut buttons = StylusButtons::CONTACT;
    if raw.button_mask & 0x1 != 0 {
        buttons |= StylusButtons::BARREL;
    }
    if raw.button_mask & 0x2 != 0 {
        buttons |= StylusButtons::SECONDARY;
    }
    if raw.button_mask & 0x4 != 0 || matches!(tool, ToolKind::Eraser) {
        buttons |= StylusButtons::INVERTED;
    }
    let mut sample = Sample::new_pen(raw.position_physical_px, timestamp, pointer_id, tool);
    sample.pressure = raw.pressure.clamp(0.0, 1.0);
    sample.tilt = raw.tilt;
    sample.twist_deg = raw.twist_deg;
    sample.tangential_pressure = None;
    sample.buttons = buttons;
    sample
}

#[cfg(test)]
mod tests {
    use aj_core::{PointerId, SampleClass, StylusButtons, Tilt, ToolCaps, ToolKind};
    use kurbo::Point;

    use super::super::OPTIMISTIC_PEN_CAPS;
    use super::super::tests_common::{adapter, drained, expect_sample};
    use super::*;
    use crate::Phase;

    fn win_raw(
        device_id: u32,
        phase: WindowsPointerPhase,
        pos: (f64, f64),
        ts: f64,
        pressure: f32,
    ) -> WindowsRawSample {
        WindowsRawSample {
            position_physical_px: Point::new(pos.0, pos.1),
            timestamp_secs: ts,
            pressure,
            tilt: Some(Tilt { x_deg: 0.0, y_deg: 0.0 }),
            twist_deg: Some(0.0),
            button_mask: 0,
            device_id,
            pointing_device_type: ToolKind::Pen,
            source_phase: phase,
        }
    }

    fn win_prox(
        device_id: u32,
        tool: ToolKind,
        caps: ToolCaps,
        is_entering: bool,
    ) -> WindowsProximitySample {
        WindowsProximitySample { device_id, pointing_device_type: tool, caps, is_entering }
    }

    #[test]
    fn windows_down_move_up_emits_committed_samples() {
        let mut a = adapter();
        a.handle_windows_proximity(win_prox(1, ToolKind::Pen, ToolCaps::PRESSURE, true));
        a.handle_windows_raw(win_raw(1, WindowsPointerPhase::Down, (10.0, 20.0), 100.0, 0.3));
        a.handle_windows_raw(win_raw(1, WindowsPointerPhase::Move, (11.0, 21.0), 100.008, 0.5));
        a.handle_windows_raw(win_raw(1, WindowsPointerPhase::Up, (11.0, 21.0), 100.016, 0.5));

        let events = drained(&mut a);
        let phases: Vec<Phase> = events.iter().map(|e| expect_sample(e).1).collect();
        assert_eq!(phases, vec![Phase::Down, Phase::Move, Phase::Up]);
        for ev in &events {
            let (sample, _, _) = expect_sample(ev);
            assert_eq!(sample.class, SampleClass::Committed);
        }
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn windows_hover_emits_hover_phase_without_stroke() {
        let mut a = adapter();
        a.handle_windows_proximity(win_prox(1, ToolKind::Pen, ToolCaps::PRESSURE, true));
        a.handle_windows_raw(win_raw(1, WindowsPointerPhase::Hover, (5.0, 5.0), 0.0, 0.0));

        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (sample, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Hover);
        assert_eq!(sample.pointer_id, PointerId::MOUSE);
        assert!(a.active_pen_pointer.is_none(), "hover must not start a stroke");
    }

    #[test]
    fn windows_capture_lost_emits_cancel() {
        let mut a = adapter();
        a.handle_windows_proximity(win_prox(1, ToolKind::Pen, ToolCaps::PRESSURE, true));
        a.handle_windows_raw(win_raw(1, WindowsPointerPhase::Down, (0.0, 0.0), 0.0, 0.4));
        let _ = drained(&mut a);

        a.handle_windows_raw(win_raw(1, WindowsPointerPhase::Cancel, (1.0, 1.0), 0.016, 0.4));

        let events = drained(&mut a);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn windows_eraser_tool_flags_inverted() {
        let mut a = adapter();
        a.handle_windows_proximity(win_prox(1, ToolKind::Eraser, OPTIMISTIC_PEN_CAPS, true));
        let mut raw = win_raw(1, WindowsPointerPhase::Down, (0.0, 0.0), 0.0, 0.4);
        raw.pointing_device_type = ToolKind::Eraser;
        a.handle_windows_raw(raw);

        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert_eq!(sample.tool, ToolKind::Eraser);
        assert!(sample.buttons.contains(StylusButtons::INVERTED));
    }

    #[test]
    fn windows_tangential_pressure_always_none() {
        let mut a = adapter();
        a.handle_windows_proximity(win_prox(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_windows_raw(win_raw(1, WindowsPointerPhase::Down, (0.0, 0.0), 0.0, 0.5));
        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert!(sample.tangential_pressure.is_none());
    }

    #[test]
    fn windows_proximity_out_with_active_stroke_cancels() {
        let mut a = adapter();
        a.handle_windows_proximity(win_prox(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_windows_raw(win_raw(1, WindowsPointerPhase::Down, (0.0, 0.0), 0.0, 0.4));
        let _ = drained(&mut a);

        a.handle_windows_proximity(win_prox(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, false));
        let events = drained(&mut a);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn windows_move_without_prior_down_is_dropped() {
        let mut a = adapter();
        a.handle_windows_proximity(win_prox(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_windows_raw(win_raw(1, WindowsPointerPhase::Move, (1.0, 1.0), 0.0, 0.5));
        assert!(drained(&mut a).is_empty());
    }
}
