//! Wayland tablet-v2 seam: adapter-side types + `StylusAdapter` impl block
//! for the wayland handlers + `build_wayland_sample` helper.
//!
//! Wayland delivers fully-resolved samples at frame boundaries — no
//! Estimated/Revise cycle is necessary. The backend thread on its separate
//! `EventQueue` accumulates axis events between `frame` events and pushes
//! one `WaylandRawSample` per commit.

use std::time::Duration;

use crate::{Point, PointerId, Sample, StylusButtons, Tilt, ToolCaps, ToolKind};

use super::{
    OPTIMISTIC_PEN_CAPS, PenState, PlatformTimestampAnchor, StylusAdapter, alloc_pointer_id,
};
use crate::{Phase, StylusEvent};

#[cfg(all(feature = "wayland", any(target_os = "linux", test)))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaylandTabletPhase {
    Down,
    Move,
    Up,
}

/// One hardware sample, already frame-boundary-accumulated by the Wayland
/// backend. The `frame` event is the commit boundary in tablet-v2, so the
/// backend collapses all axis events between two `frame`s into a single
/// `WaylandRawSample`. Pressure arrives pre-normalized to `0..=1` so the
/// adapter stays free of Wayland axis-range magic numbers.
#[cfg(all(feature = "wayland", any(target_os = "linux", test)))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WaylandRawSample {
    pub position_physical_px: Point,
    /// Frame timestamp — the compositor's millisecond `frame.time` divided to
    /// seconds. Relative-ordering only, monotonic within a backend session.
    pub timestamp_secs: f64,
    pub pressure: f32,
    pub tilt: Option<Tilt>,
    pub twist_deg: Option<f32>,
    pub tangential_pressure: Option<f32>,
    pub button_mask: u32,
    pub device_id: u32,
    pub hardware_serial: Option<u64>,
    pub pointing_device_type: ToolKind,
    pub source_phase: WaylandTabletPhase,
}

#[cfg(all(feature = "wayland", any(target_os = "linux", test)))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WaylandProximitySample {
    pub device_id: u32,
    pub hardware_serial: Option<u64>,
    pub pointing_device_type: ToolKind,
    pub caps: ToolCaps,
    pub is_entering: bool,
}

impl StylusAdapter {
    #[cfg(all(feature = "wayland", any(target_os = "linux", test)))]
    pub(crate) fn handle_wayland_raw(&mut self, raw: WaylandRawSample) {
        let ts = PlatformTimestampAnchor::translate_or_anchor(
            &mut self.wayland_anchor,
            raw.timestamp_secs,
            self.epoch,
        );

        let (caps, tool, active_pid) = {
            let pen = self.pens.entry(raw.device_id).or_insert_with(|| PenState {
                active_pointer_id: None,
                caps: OPTIMISTIC_PEN_CAPS,
                tool: raw.pointing_device_type,
                unique_id: raw.hardware_serial,
                last_position: None,
            });
            pen.last_position = Some(raw.position_physical_px);
            (pen.caps, pen.tool, pen.active_pointer_id)
        };

        match raw.source_phase {
            WaylandTabletPhase::Down => {
                let pid = alloc_pointer_id(&mut self.next_pointer_id);
                if let Some(pen) = self.pens.get_mut(&raw.device_id) {
                    pen.active_pointer_id = Some(pid);
                }
                self.active_pen_pointer = Some(pid);
                let sample = build_wayland_sample(&raw, ts, pid, tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Down, caps });
            }
            WaylandTabletPhase::Move | WaylandTabletPhase::Up => {
                let Some(pid) = active_pid else {
                    return;
                };
                let phase = match raw.source_phase {
                    WaylandTabletPhase::Move => Phase::Move,
                    WaylandTabletPhase::Up => Phase::Up,
                    WaylandTabletPhase::Down => unreachable!(),
                };
                let sample = build_wayland_sample(&raw, ts, pid, tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase, caps });

                if matches!(raw.source_phase, WaylandTabletPhase::Up) {
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

    #[cfg(all(feature = "wayland", any(target_os = "linux", test)))]
    pub(crate) fn handle_wayland_proximity(&mut self, prox: WaylandProximitySample) {
        if prox.is_entering {
            let entry = self.pens.entry(prox.device_id).or_insert(PenState {
                active_pointer_id: None,
                caps: prox.caps,
                tool: prox.pointing_device_type,
                unique_id: prox.hardware_serial,
                last_position: None,
            });
            entry.caps = prox.caps;
            entry.tool = prox.pointing_device_type;
            entry.unique_id = prox.hardware_serial;
        } else if let Some(mut pen) = self.pens.remove(&prox.device_id)
            && let Some(pid) = pen.active_pointer_id.take()
        {
            // Proximity-out without a preceding Up — user lifted above hover
            // range mid-stroke. Synthesize Cancel so downstream tears the
            // stroke down cleanly.
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

#[cfg(all(feature = "wayland", any(target_os = "linux", test)))]
fn build_wayland_sample(
    raw: &WaylandRawSample,
    timestamp: Duration,
    pointer_id: PointerId,
    tool: ToolKind,
) -> Sample {
    // Wayland's per-button bitmask is Linux `BTN_*` codes collapsed by the
    // backend into bit 0 = primary barrel, bit 1 = secondary — mirror the
    // mac layout so downstream stays platform-agnostic.
    let mut buttons = StylusButtons::CONTACT;
    if raw.button_mask & 0x1 != 0 {
        buttons |= StylusButtons::BARREL;
    }
    if raw.button_mask & 0x2 != 0 {
        buttons |= StylusButtons::SECONDARY;
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
    use std::time::Duration;

    use crate::{Point, SampleClass, StylusButtons, ToolCaps, ToolKind};

    use super::super::OPTIMISTIC_PEN_CAPS;
    use super::super::tests_common::{adapter, drained, expect_sample};
    use super::*;
    use crate::{Phase, StylusEvent};

    fn wl_raw(
        device_id: u32,
        phase: WaylandTabletPhase,
        pos: (f64, f64),
        ts: f64,
        pressure: f32,
    ) -> WaylandRawSample {
        WaylandRawSample {
            position_physical_px: Point::new(pos.0, pos.1),
            timestamp_secs: ts,
            pressure,
            tilt: None,
            twist_deg: None,
            tangential_pressure: None,
            button_mask: 0,
            device_id,
            hardware_serial: Some(0xABCD_1234_5678_9ABC),
            pointing_device_type: ToolKind::Pen,
            source_phase: phase,
        }
    }

    fn wl_prox(
        device_id: u32,
        tool: ToolKind,
        caps: ToolCaps,
        is_entering: bool,
    ) -> WaylandProximitySample {
        WaylandProximitySample {
            device_id,
            hardware_serial: Some(0xABCD_1234_5678_9ABC),
            pointing_device_type: tool,
            caps,
            is_entering,
        }
    }

    #[test]
    fn wayland_down_emits_committed_sample_no_estimated() {
        let mut a = adapter();
        a.handle_wayland_proximity(wl_prox(1, ToolKind::Pen, ToolCaps::PRESSURE, true));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Down, (10.0, 20.0), 1.0, 0.5));

        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (sample, phase, caps) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Down);
        assert_eq!(sample.class, SampleClass::Committed);
        assert!(caps.contains(ToolCaps::PRESSURE));
        assert!(a.active_pen_pointer.is_some());
    }

    #[test]
    fn wayland_frame_sequence_emits_one_sample_per_frame() {
        let mut a = adapter();
        a.handle_wayland_proximity(wl_prox(1, ToolKind::Pen, ToolCaps::PRESSURE, true));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Down, (0.0, 0.0), 0.0, 0.1));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Move, (1.0, 1.0), 0.008, 0.3));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Move, (2.0, 2.0), 0.016, 0.5));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Up, (3.0, 3.0), 0.024, 0.0));

        let events = drained(&mut a);
        let phases: Vec<Phase> = events.iter().map(|e| expect_sample(e).1).collect();
        assert_eq!(phases, vec![Phase::Down, Phase::Move, Phase::Move, Phase::Up]);
    }

    #[test]
    fn wayland_pressure_clamps_to_unit_range() {
        let mut a = adapter();
        a.handle_wayland_proximity(wl_prox(1, ToolKind::Pen, ToolCaps::PRESSURE, true));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Down, (0.0, 0.0), 0.0, 1.7));

        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert!((sample.pressure - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn wayland_eraser_tool_flags_inverted() {
        let mut a = adapter();
        a.handle_wayland_proximity(wl_prox(2, ToolKind::Eraser, OPTIMISTIC_PEN_CAPS, true));
        let mut raw = wl_raw(2, WaylandTabletPhase::Down, (0.0, 0.0), 0.0, 0.4);
        raw.pointing_device_type = ToolKind::Eraser;
        a.handle_wayland_raw(raw);

        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert_eq!(sample.tool, ToolKind::Eraser);
        assert!(sample.buttons.contains(StylusButtons::INVERTED));
    }

    #[test]
    fn wayland_proximity_out_mid_stroke_synthesizes_cancel() {
        let mut a = adapter();
        a.handle_wayland_proximity(wl_prox(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Down, (0.0, 0.0), 0.0, 0.4));
        let _ = drained(&mut a);

        a.handle_wayland_proximity(wl_prox(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, false));

        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
        assert!(a.active_pen_pointer.is_none());
        assert!(!a.pens.contains_key(&1));
    }

    #[test]
    fn wayland_hardware_serial_lands_on_pen_unique_id() {
        let mut a = adapter();
        let serial = 0xDEAD_BEEF_CAFE_BABE_u64;
        let prox = WaylandProximitySample {
            device_id: 9,
            hardware_serial: Some(serial),
            pointing_device_type: ToolKind::Pen,
            caps: ToolCaps::PRESSURE,
            is_entering: true,
        };
        a.handle_wayland_proximity(prox);

        let pen = a.pens.get(&9).expect("pen state created by proximity");
        assert_eq!(pen.unique_id, Some(serial));
    }

    #[test]
    fn wayland_timestamps_monotonic_and_anchored() {
        let mut a = adapter();
        a.handle_wayland_proximity(wl_prox(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Down, (0.0, 0.0), 1000.0, 0.3));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Move, (1.0, 1.0), 1000.008, 0.4));
        a.handle_wayland_raw(wl_raw(1, WaylandTabletPhase::Up, (2.0, 2.0), 1000.016, 0.4));

        let events = drained(&mut a);
        let timestamps: Vec<Duration> = events
            .iter()
            .filter_map(|e| {
                if let StylusEvent::Sample { sample, .. } = e {
                    Some(sample.timestamp)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(timestamps.len(), 3);
        for pair in timestamps.windows(2) {
            assert!(pair[0] <= pair[1]);
        }
    }
}
