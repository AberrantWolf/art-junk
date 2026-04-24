//! macOS tablet seam: `NSEvent`-driven backend types + `StylusAdapter` impl
//! block for the mac handlers + the mac-specific `build_pen_sample` helper.
//!
//! Three `pub(crate)` entry points let the real `NSEvent` backend and tests
//! drive the same adapter code. The NSEvent-specific translation (objc2
//! calls, coordinate flip, capability-mask decoding) lives in
//! `crate::macos_tablet`; this module only sees already-translated raw
//! samples.

use std::time::Duration;

use crate::{
    Point, PointerId, Sample, SampleClass, SampleRevision, StylusButtons, Tilt, ToolCaps, ToolKind,
};

use super::{
    OPTIMISTIC_PEN_CAPS, PenState, PlatformTimestampAnchor, StylusAdapter, alloc_pointer_id,
};
use crate::{Phase, StylusEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MacTabletOrigin {
    /// Rode on a regular mouse event (`LeftMouseDown/Up/Dragged`) with
    /// `subtype == .tabletPoint`.
    MouseSubtype,
    /// Rode on a native `NSEventTypeTabletPoint` (the exceptional path —
    /// fires between stylus-down and first drag, or during multi-tool use).
    NativeTabletPoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MacTabletPhase {
    Down,
    Move,
    Up,
}

/// Raw sample supplied by the macOS `NSEvent` backend (or a test harness).
/// The adapter owns timestamp translation, pointer-id allocation, and revision
/// emission — so this type is the stable seam between backend and adapter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MacTabletRawSample {
    pub position_physical_px: Point,
    /// `NSEvent.timestamp` — seconds since system boot, monotonic.
    pub timestamp_secs: f64,
    pub pressure: f32,
    pub tilt: Tilt,
    pub twist_deg: f32,
    pub tangential_pressure: f32,
    pub button_mask: u32,
    pub device_id: u32,
    pub pointing_device_type: ToolKind,
    pub origin: MacTabletOrigin,
    pub source_phase: MacTabletPhase,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MacTabletProximitySample {
    pub device_id: u32,
    pub unique_id: Option<u64>,
    pub pointing_device_type: ToolKind,
    /// Capabilities already translated from `NSEvent`'s `capabilityMask` bits
    /// into `ToolCaps`. The `NSEvent` bit-to-`ToolCaps` mapping lives in the
    /// macOS backend so the adapter stays platform-agnostic.
    pub caps: ToolCaps,
    pub is_entering: bool,
}

impl StylusAdapter {
    pub(crate) fn handle_mac_raw(&mut self, raw: MacTabletRawSample) {
        let ts = self.translate_mac_timestamp(raw.timestamp_secs);

        // Snapshot pen state in a narrow scope so the `&mut self.pens` borrow
        // is released before we call helpers like `take_pending_for_pointer`
        // that need `&mut self`.
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
            MacTabletPhase::Down => {
                let pid = alloc_pointer_id(&mut self.next_pointer_id);
                if let Some(pen) = self.pens.get_mut(&raw.device_id) {
                    pen.active_pointer_id = Some(pid);
                }
                let update_index = self.next_update_index;
                self.next_update_index = self.next_update_index.wrapping_add(1);

                let mut sample = build_pen_sample(&raw, ts, pid, tool);
                sample.class = SampleClass::Estimated { update_index };
                self.pending_estimated.insert(update_index, pid);
                self.active_pen_pointer = Some(pid);
                self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Down, caps });
            }
            MacTabletPhase::Move | MacTabletPhase::Up => {
                let Some(pid) = active_pid else {
                    // Move or Up without a preceding Down for this pen; drop.
                    // Can happen if the adapter was constructed after the
                    // pen was already pressed (very unlikely in practice).
                    return;
                };

                // If a revision is pending, refine the earlier Estimated sample
                // with whatever fresher axis data this event carries. Mac emits
                // at most one pending estimate per stroke; the iteration stays
                // correct for platforms that emit several.
                for update_index in self.take_pending_for_pointer(pid) {
                    let revision = SampleRevision {
                        pressure: Some(raw.pressure),
                        tilt: Some(raw.tilt),
                        twist_deg: Some(raw.twist_deg),
                        tangential_pressure: Some(raw.tangential_pressure),
                    };
                    self.queue.push_back(StylusEvent::Revise {
                        pointer_id: pid,
                        update_index,
                        revision,
                    });
                }

                // Native `NSTabletPoint` events are supplemental — Apple's docs
                // describe them firing *between* a mouse-down and the first
                // drag, and during multi-tool scenarios. In practice some
                // drivers (including Wacom) interleave them with
                // `LeftMouseDragged` events at the same physical instant,
                // which produces duplicate samples and visible zig-zags at
                // integer-pixel boundaries. Treat them as revise-only: they
                // refine a pending Estimated Down but don't emit new Move/Up
                // samples. The mouse-subtype path is authoritative for
                // position flow.
                if matches!(raw.origin, MacTabletOrigin::NativeTabletPoint) {
                    return;
                }

                let phase = match raw.source_phase {
                    MacTabletPhase::Move => Phase::Move,
                    MacTabletPhase::Up => Phase::Up,
                    MacTabletPhase::Down => unreachable!(),
                };
                let sample = build_pen_sample(&raw, ts, pid, tool);
                self.queue.push_back(StylusEvent::Sample { sample, phase, caps });

                if matches!(raw.source_phase, MacTabletPhase::Up) {
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

    pub(crate) fn handle_mac_proximity(&mut self, prox: MacTabletProximitySample) {
        if prox.is_entering {
            // Refresh or insert — preserves active_pointer_id / last_position
            // if we already have state for this physical stylus.
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
            // Proximity-out with still-active stroke — Up was lost somehow.
            // Synthesize a Cancel so the app can tear the stroke down cleanly.
            let ts = self.current_duration();
            let mut sample = Sample::new_pen_placeholder(
                pen.last_position.unwrap_or(Point::ZERO),
                ts,
                pid,
                pen.tool,
            );
            // Keep caps=pen.caps on the cancel so downstream sees consistent
            // data even as the stroke tears down.
            let caps = pen.caps;
            sample.tool = pen.tool;
            self.queue.push_back(StylusEvent::Sample { sample, phase: Phase::Cancel, caps });
            let _ = self.take_pending_for_pointer(pid);
            if self.active_pen_pointer == Some(pid) {
                self.active_pen_pointer = None;
            }
        }
    }

    fn translate_mac_timestamp(&mut self, nsevent_secs: f64) -> Duration {
        PlatformTimestampAnchor::translate_or_anchor(&mut self.mac_anchor, nsevent_secs, self.epoch)
    }
}

fn build_pen_sample(
    raw: &MacTabletRawSample,
    timestamp: Duration,
    pointer_id: PointerId,
    tool: ToolKind,
) -> Sample {
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
    sample.tilt = Some(raw.tilt);
    sample.twist_deg = Some(raw.twist_deg);
    sample.tangential_pressure = Some(raw.tangential_pressure);
    sample.buttons = buttons;
    sample
}

#[cfg(test)]
mod tests {
    use crate::{Point, SampleClass, StylusButtons, ToolCaps, ToolKind};

    use super::super::tests_common::{adapter, drained, expect_sample};
    use super::*;
    use crate::{Phase, StylusEvent};

    #[cfg(feature = "winit")]
    use winit::dpi::PhysicalPosition;
    #[cfg(feature = "winit")]
    use winit::event::{DeviceId, MouseButton, Touch, TouchPhase, WindowEvent};

    fn raw(
        device_id: u32,
        phase: MacTabletPhase,
        pos: (f64, f64),
        ts: f64,
        pressure: f32,
    ) -> MacTabletRawSample {
        raw_with_origin(device_id, phase, pos, ts, pressure, MacTabletOrigin::MouseSubtype)
    }

    fn raw_with_origin(
        device_id: u32,
        phase: MacTabletPhase,
        pos: (f64, f64),
        ts: f64,
        pressure: f32,
        origin: MacTabletOrigin,
    ) -> MacTabletRawSample {
        MacTabletRawSample {
            position_physical_px: Point::new(pos.0, pos.1),
            timestamp_secs: ts,
            pressure,
            tilt: Tilt { x_deg: 0.0, y_deg: 0.0 },
            twist_deg: 0.0,
            tangential_pressure: 0.0,
            button_mask: 0,
            device_id,
            pointing_device_type: ToolKind::Pen,
            origin,
            source_phase: phase,
        }
    }

    fn proximity(
        device_id: u32,
        tool: ToolKind,
        caps: ToolCaps,
        is_entering: bool,
    ) -> MacTabletProximitySample {
        MacTabletProximitySample {
            device_id,
            unique_id: Some(42),
            pointing_device_type: tool,
            caps,
            is_entering,
        }
    }

    #[test]
    fn pen_down_emits_estimated_sample_with_update_index() {
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, ToolCaps::PRESSURE, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (100.0, 200.0), 0.0, 0.1));

        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (sample, phase, caps) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Down);
        assert_eq!(sample.tool, ToolKind::Pen);
        assert!(matches!(sample.class, SampleClass::Estimated { .. }));
        assert!(caps.contains(ToolCaps::PRESSURE));
        assert!(a.active_pen_pointer.is_some());
    }

    #[test]
    fn pen_follow_up_sample_emits_revise_then_move() {
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (10.0, 10.0), 0.0, 0.0));
        let _ = drained(&mut a);

        a.handle_mac_raw(raw(1, MacTabletPhase::Move, (11.0, 11.0), 0.01, 0.8));
        let events = drained(&mut a);
        assert_eq!(events.len(), 2, "revise + move");

        match &events[0] {
            StylusEvent::Revise { revision, .. } => {
                assert!((revision.pressure.unwrap() - 0.8).abs() < f32::EPSILON);
            }
            other => panic!("expected Revise first, got {other:?}"),
        }
        let (sample, phase, _) = expect_sample(&events[1]);
        assert_eq!(phase, Phase::Move);
        assert_eq!(sample.class, SampleClass::Committed);
    }

    #[test]
    fn pen_subsequent_moves_emit_committed_only() {
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (0.0, 0.0), 0.0, 0.2));
        a.handle_mac_raw(raw(1, MacTabletPhase::Move, (1.0, 1.0), 0.01, 0.5));
        let _ = drained(&mut a);

        a.handle_mac_raw(raw(1, MacTabletPhase::Move, (2.0, 2.0), 0.02, 0.6));
        let events = drained(&mut a);
        assert_eq!(events.len(), 1, "no revise after pending cleared");
        let (sample, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Move);
        assert_eq!(sample.class, SampleClass::Committed);
    }

    #[test]
    fn pen_up_clears_active_pen_and_pending_estimated() {
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (0.0, 0.0), 0.0, 0.2));
        let _ = drained(&mut a);

        a.handle_mac_raw(raw(1, MacTabletPhase::Up, (0.0, 0.0), 0.05, 0.7));
        let _ = drained(&mut a);

        assert!(a.active_pen_pointer.is_none());
        assert!(a.pending_estimated.is_empty());
    }

    #[test]
    fn first_sample_without_prior_proximity_uses_optimistic_caps() {
        let mut a = adapter();
        // Skip proximity — simulate app launched with pen already hovering.
        a.handle_mac_raw(raw(5, MacTabletPhase::Down, (0.0, 0.0), 0.0, 0.3));

        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (_, _, caps) = expect_sample(&events[0]);
        assert!(caps.contains(ToolCaps::PRESSURE));
        assert!(caps.contains(ToolCaps::TILT));
        assert!(caps.contains(ToolCaps::TWIST));
        assert!(caps.contains(ToolCaps::TANGENTIAL_PRESSURE));
    }

    #[test]
    fn proximity_refreshes_caps_on_subsequent_strokes() {
        let mut a = adapter();
        a.handle_mac_raw(raw(7, MacTabletPhase::Down, (0.0, 0.0), 0.0, 0.3));
        a.handle_mac_raw(raw(7, MacTabletPhase::Up, (0.0, 0.0), 0.01, 0.3));
        let _ = drained(&mut a);

        // Proximity arrives with a stricter (no-TILT) cap set.
        a.handle_mac_proximity(proximity(7, ToolKind::Pen, ToolCaps::PRESSURE, true));
        a.handle_mac_raw(raw(7, MacTabletPhase::Down, (0.0, 0.0), 0.02, 0.4));
        let events = drained(&mut a);
        let (_, _, caps) = expect_sample(&events[0]);
        assert!(caps.contains(ToolCaps::PRESSURE));
        assert!(!caps.contains(ToolCaps::TILT), "second stroke uses real caps");
    }

    #[test]
    fn pen_eraser_flip_via_proximity_changes_tool_kind() {
        let mut a = adapter();
        a.handle_mac_proximity(proximity(3, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_proximity(proximity(3, ToolKind::Eraser, OPTIMISTIC_PEN_CAPS, true));

        a.handle_mac_raw(raw(3, MacTabletPhase::Down, (0.0, 0.0), 0.0, 0.5));
        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert_eq!(sample.tool, ToolKind::Eraser);
        assert!(sample.buttons.contains(StylusButtons::INVERTED));
    }

    #[cfg(feature = "winit")]
    #[test]
    fn active_pen_suppresses_winit_mouse_events() {
        use winit::event::{ElementState, MouseButton};
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (0.0, 0.0), 0.0, 0.3));
        let _ = drained(&mut a);

        // Synthesize a winit MouseInput(Pressed) that *would* arrive from the
        // same physical pen press (tablet driver sends both).
        a.on_window_event(&WindowEvent::MouseInput {
            device_id: DeviceId::dummy(),
            state: ElementState::Pressed,
            button: MouseButton::Left,
        });
        a.on_window_event(&WindowEvent::CursorMoved {
            device_id: DeviceId::dummy(),
            position: PhysicalPosition::new(1.0, 1.0),
        });
        assert!(drained(&mut a).is_empty(), "mouse events must be suppressed during pen stroke");
    }

    #[test]
    fn focus_loss_cancels_active_pen_stroke() {
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (10.0, 10.0), 0.0, 0.3));
        let _ = drained(&mut a);

        a.on_focus_lost();

        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
        assert!(a.active_pen_pointer.is_none());
        assert!(a.pending_estimated.is_empty());
    }

    #[cfg(feature = "winit")]
    #[test]
    fn focus_loss_cancels_mouse_and_touches() {
        let mut a = adapter();
        a.on_window_event(&WindowEvent::CursorMoved {
            device_id: DeviceId::dummy(),
            position: PhysicalPosition::new(5.0, 5.0),
        });
        a.on_window_event(&WindowEvent::MouseInput {
            device_id: DeviceId::dummy(),
            state: winit::event::ElementState::Pressed,
            button: MouseButton::Left,
        });
        a.on_window_event(&WindowEvent::Touch(Touch {
            device_id: DeviceId::dummy(),
            phase: TouchPhase::Started,
            location: PhysicalPosition::new(1.0, 1.0),
            force: None,
            id: 7,
        }));
        let _ = drained(&mut a);

        a.on_focus_lost();

        let events = drained(&mut a);
        let phases: Vec<Phase> = events.iter().map(|e| expect_sample(e).1).collect();
        assert!(phases.iter().all(|p| *p == Phase::Cancel));
        assert_eq!(phases.len(), 2, "one cancel per active pointer");
        assert!(!a.mouse_down);
        assert!(a.touches.is_empty());
    }

    #[test]
    fn proximity_out_with_active_pen_synthesizes_cancel() {
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (0.0, 0.0), 0.0, 0.3));
        let _ = drained(&mut a);

        // Pen leaves proximity while stroke is still active (Up was lost).
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, false));

        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
        assert!(a.active_pen_pointer.is_none());
        assert!(!a.pens.contains_key(&1), "pen removed on proximity-out");
    }

    #[test]
    fn mac_timestamps_are_monotonic_and_aligned_to_adapter_epoch() {
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (0.0, 0.0), 100.0, 0.3));
        a.handle_mac_raw(raw(1, MacTabletPhase::Move, (1.0, 1.0), 100.016, 0.4));
        a.handle_mac_raw(raw(1, MacTabletPhase::Up, (2.0, 2.0), 100.032, 0.4));

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

    #[test]
    fn native_tabletpoint_moves_do_not_emit_extra_sample() {
        // Some drivers (Wacom) interleave NSTabletPoint events with
        // LeftMouseDragged events at the same physical instant; treating
        // both as sample sources produces a visible zig-zag / pixel-jagged
        // stroke. Native TabletPoint should refine a pending Estimated
        // sample but never emit a new Move Sample.
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (10.0, 10.0), 0.0, 0.0));
        let _ = drained(&mut a); // drain Down

        // Native TabletPoint Move: should emit a Revise (pending from Down),
        // but no new Sample.
        a.handle_mac_raw(raw_with_origin(
            1,
            MacTabletPhase::Move,
            (10.5, 10.5),
            0.005,
            0.7,
            MacTabletOrigin::NativeTabletPoint,
        ));
        let events = drained(&mut a);
        assert_eq!(events.len(), 1, "native TabletPoint must emit only a Revise");
        match &events[0] {
            StylusEvent::Revise { revision, .. } => {
                assert!((revision.pressure.unwrap() - 0.7).abs() < f32::EPSILON);
            }
            other => panic!("expected Revise, got {other:?}"),
        }

        // Mouse-subtype Move arriving immediately after must emit a Move
        // Sample (the authoritative position source), no duplicate Revise.
        a.handle_mac_raw(raw(1, MacTabletPhase::Move, (11.0, 11.0), 0.008, 0.75));
        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Move);
    }

    #[test]
    fn native_tabletpoint_without_pending_revise_emits_nothing() {
        // After the pending Estimated has been resolved, a stray native
        // TabletPoint during the stroke should not emit anything — it's
        // neither a new position nor a revision.
        let mut a = adapter();
        a.handle_mac_proximity(proximity(1, ToolKind::Pen, OPTIMISTIC_PEN_CAPS, true));
        a.handle_mac_raw(raw(1, MacTabletPhase::Down, (0.0, 0.0), 0.0, 0.2));
        a.handle_mac_raw(raw(1, MacTabletPhase::Move, (1.0, 1.0), 0.01, 0.5));
        let _ = drained(&mut a);

        a.handle_mac_raw(raw_with_origin(
            1,
            MacTabletPhase::Move,
            (1.5, 1.5),
            0.012,
            0.6,
            MacTabletOrigin::NativeTabletPoint,
        ));
        assert!(drained(&mut a).is_empty(), "interleaved native TabletPoint must be silent");
    }
}
