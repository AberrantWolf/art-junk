//! Android `MotionEvent` seam. Android delivers fully-resolved axis samples
//! per `MotionEvent`. No Estimated/Revise cycle — the backend walks
//! `getHistoricalAxisValue` plus the current sample, emitting
//! `AndroidRawSample`s in time order. Tilt decomposition happens in the
//! adapter (platform-agnostic math) so the backend stays thin.

use std::time::Duration;

use super::{OPTIMISTIC_PEN_CAPS, PlatformTimestampAnchor, StylusAdapter, alloc_pointer_id};
use crate::{
    Phase, Point, PointerId, Sample, StylusButtons, StylusEvent, Tilt, ToolCaps, ToolKind,
};

#[cfg(all(feature = "android", any(target_os = "android", test)))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AndroidSourcePhase {
    Down,
    Move,
    Up,
    Cancel,
    Hover,
}

/// Raw sample from the Android `MotionEvent` backend. Positions are
/// already in physical pixels view-local (backend multiplied by
/// `getPointerCoords` density / converts via `getRawX`). Pressure is
/// pre-clamped to `0..=1`. Tilt and orientation remain in radians
/// matching Android's native units; the adapter decomposes them via
/// `android_tilt_to_xy_deg` so component tilt is available on the
/// emitted `Sample`.
#[cfg(all(feature = "android", any(target_os = "android", test)))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AndroidRawSample {
    pub position_physical_px: Point,
    /// `AMotionEvent_getEventTime` result expressed in seconds. Native
    /// nanoseconds are divided by 1e9 by the backend so the adapter
    /// stays in its f64-seconds timeline.
    pub timestamp_secs: f64,
    pub pressure: f32,
    /// `AXIS_TILT` in radians. `0` = perpendicular, `π/2` = flat.
    pub tilt_rad: f32,
    /// `AXIS_ORIENTATION` in radians, `-π..=π`. Azimuthal direction of
    /// tilt.
    pub orientation_rad: f32,
    pub distance: Option<f32>,
    /// `AMotionEvent_getButtonState`. Bits map to `StylusButtons` in the
    /// adapter.
    pub button_state: u32,
    /// Android pointer id (distinct per finger / stylus in a multi-touch
    /// gesture). Stable only within the current gesture.
    pub pointer_id: i32,
    pub pointing_device_type: ToolKind,
    pub source_phase: AndroidSourcePhase,
}

#[cfg(all(feature = "android", any(target_os = "android", test)))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AndroidProximitySample {
    pub pointer_id: i32,
    pub pointing_device_type: ToolKind,
    pub caps: ToolCaps,
    pub is_entering: bool,
}

/// Decompose Android's `AXIS_TILT` + `AXIS_ORIENTATION` polar form into
/// component tilts on the view's X and Y axes. Returns degrees, each
/// bounded by `±90`. Per the skill doc, the tilt→x/y mapping uses
/// `sin/cos(orientation)` weighting of the magnitude.
#[cfg(all(feature = "android", any(target_os = "android", test)))]
pub(crate) fn android_tilt_to_xy_deg(tilt_rad: f32, orientation_rad: f32) -> (f32, f32) {
    let tilt_x = tilt_rad * orientation_rad.sin();
    let tilt_y = tilt_rad * orientation_rad.cos();
    (tilt_x.to_degrees(), tilt_y.to_degrees())
}

impl StylusAdapter {
    #[cfg(all(feature = "android", any(target_os = "android", test)))]
    pub(crate) fn handle_android_raw(&mut self, raw: AndroidRawSample) {
        let ts = PlatformTimestampAnchor::translate_or_anchor(
            &mut self.android_anchor,
            raw.timestamp_secs,
            self.epoch,
        );

        if matches!(raw.source_phase, AndroidSourcePhase::Hover) {
            let sample = build_android_sample(&raw, ts, PointerId::MOUSE);
            self.queue.push_back(StylusEvent::Sample {
                sample,
                phase: Phase::Hover,
                caps: OPTIMISTIC_PEN_CAPS,
            });
            return;
        }

        match raw.source_phase {
            AndroidSourcePhase::Down => {
                let pid = alloc_pointer_id(&mut self.next_pointer_id);
                self.android_pointers.insert(raw.pointer_id, pid);
                if matches!(raw.pointing_device_type, ToolKind::Pen | ToolKind::Eraser) {
                    self.active_pen_pointer = Some(pid);
                }
                let sample = build_android_sample(&raw, ts, pid);
                self.queue.push_back(StylusEvent::Sample {
                    sample,
                    phase: Phase::Down,
                    caps: OPTIMISTIC_PEN_CAPS,
                });
            }
            AndroidSourcePhase::Move | AndroidSourcePhase::Up | AndroidSourcePhase::Cancel => {
                let Some(&pid) = self.android_pointers.get(&raw.pointer_id) else {
                    return;
                };
                let phase = match raw.source_phase {
                    AndroidSourcePhase::Move => Phase::Move,
                    AndroidSourcePhase::Up => Phase::Up,
                    AndroidSourcePhase::Cancel => Phase::Cancel,
                    _ => unreachable!(),
                };
                let sample = build_android_sample(&raw, ts, pid);
                self.queue.push_back(StylusEvent::Sample {
                    sample,
                    phase,
                    caps: OPTIMISTIC_PEN_CAPS,
                });

                if matches!(raw.source_phase, AndroidSourcePhase::Up | AndroidSourcePhase::Cancel) {
                    self.android_pointers.remove(&raw.pointer_id);
                    if self.active_pen_pointer == Some(pid) {
                        self.active_pen_pointer = None;
                    }
                }
            }
            AndroidSourcePhase::Hover => unreachable!("handled above"),
        }
    }

    #[cfg(all(feature = "android", any(target_os = "android", test)))]
    pub(crate) fn handle_android_proximity(&mut self, prox: AndroidProximitySample) {
        if !prox.is_entering
            && let Some(pid) = self.android_pointers.remove(&prox.pointer_id)
        {
            let ts = self.current_duration();
            let sample =
                Sample::new_pen_placeholder(Point::ZERO, ts, pid, prox.pointing_device_type);
            self.queue.push_back(StylusEvent::Sample {
                sample,
                phase: Phase::Cancel,
                caps: prox.caps,
            });
            let _ = self.take_pending_for_pointer(pid);
            if self.active_pen_pointer == Some(pid) {
                self.active_pen_pointer = None;
            }
        }
    }
}

#[cfg(all(feature = "android", any(target_os = "android", test)))]
fn build_android_sample(
    raw: &AndroidRawSample,
    timestamp: Duration,
    pointer_id: PointerId,
) -> Sample {
    let (tilt_x, tilt_y) = android_tilt_to_xy_deg(raw.tilt_rad, raw.orientation_rad);
    let mut sample =
        Sample::new_pen(raw.position_physical_px, timestamp, pointer_id, raw.pointing_device_type);
    sample.pressure = raw.pressure.clamp(0.0, 1.0);
    sample.tilt = Some(Tilt { x_deg: tilt_x, y_deg: tilt_y });
    sample.distance = raw.distance;
    // `BUTTON_STYLUS_PRIMARY` = 0x20, `BUTTON_STYLUS_SECONDARY` = 0x40
    // per the NDK input reference.
    let mut buttons = StylusButtons::CONTACT;
    if raw.button_state & 0x20 != 0 {
        buttons |= StylusButtons::BARREL;
    }
    if raw.button_state & 0x40 != 0 {
        buttons |= StylusButtons::SECONDARY;
    }
    if matches!(raw.pointing_device_type, ToolKind::Eraser) {
        buttons |= StylusButtons::INVERTED;
    }
    sample.buttons = buttons;
    sample
}

#[cfg(test)]
mod tests {
    use crate::{Point, SampleClass, StylusButtons, ToolKind};

    use super::super::OPTIMISTIC_PEN_CAPS;
    use super::super::tests_common::{adapter, drained, expect_sample};
    use super::*;
    use crate::Phase;

    fn android_raw(
        pointer_id: i32,
        phase: AndroidSourcePhase,
        pos: (f64, f64),
        ts: f64,
        pressure: f32,
        tool: ToolKind,
    ) -> AndroidRawSample {
        AndroidRawSample {
            position_physical_px: Point::new(pos.0, pos.1),
            timestamp_secs: ts,
            pressure,
            tilt_rad: 0.0,
            orientation_rad: 0.0,
            distance: None,
            button_state: 0,
            pointer_id,
            pointing_device_type: tool,
            source_phase: phase,
        }
    }

    #[test]
    fn android_down_move_up_emits_committed_samples() {
        let mut a = adapter();
        a.handle_android_raw(android_raw(
            0,
            AndroidSourcePhase::Down,
            (0.0, 0.0),
            0.0,
            0.4,
            ToolKind::Pen,
        ));
        a.handle_android_raw(android_raw(
            0,
            AndroidSourcePhase::Move,
            (1.0, 1.0),
            0.008,
            0.5,
            ToolKind::Pen,
        ));
        a.handle_android_raw(android_raw(
            0,
            AndroidSourcePhase::Up,
            (1.0, 1.0),
            0.016,
            0.0,
            ToolKind::Pen,
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
    fn android_eraser_tool_flags_inverted() {
        let mut a = adapter();
        a.handle_android_raw(android_raw(
            0,
            AndroidSourcePhase::Down,
            (0.0, 0.0),
            0.0,
            0.3,
            ToolKind::Eraser,
        ));
        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert_eq!(sample.tool, ToolKind::Eraser);
        assert!(sample.buttons.contains(StylusButtons::INVERTED));
    }

    #[test]
    fn android_flag_canceled_synthesizes_cancel() {
        let mut a = adapter();
        a.handle_android_raw(android_raw(
            0,
            AndroidSourcePhase::Down,
            (0.0, 0.0),
            0.0,
            0.3,
            ToolKind::Pen,
        ));
        a.handle_android_raw(android_raw(
            0,
            AndroidSourcePhase::Cancel,
            (0.0, 0.0),
            0.016,
            0.0,
            ToolKind::Pen,
        ));
        let events = drained(&mut a);
        let phases: Vec<Phase> = events.iter().map(|e| expect_sample(e).1).collect();
        assert_eq!(phases, vec![Phase::Down, Phase::Cancel]);
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn android_hover_emits_hover_phase() {
        let mut a = adapter();
        a.handle_android_raw(android_raw(
            0,
            AndroidSourcePhase::Hover,
            (5.0, 5.0),
            0.0,
            0.0,
            ToolKind::Pen,
        ));
        let events = drained(&mut a);
        let (sample, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Hover);
        assert_eq!(sample.pointer_id, PointerId::MOUSE);
    }

    #[test]
    fn android_tilt_decomposition_cardinals_and_zero() {
        let (x, y) = android_tilt_to_xy_deg(0.0, std::f32::consts::FRAC_PI_2);
        assert!(x.abs() < 0.01 && y.abs() < 0.01);

        let (x, _y) =
            android_tilt_to_xy_deg(std::f32::consts::FRAC_PI_4, std::f32::consts::FRAC_PI_2);
        assert!(x > 40.0, "tilt toward +X: expected positive x component, got {x}");

        let (_x, y) = android_tilt_to_xy_deg(std::f32::consts::FRAC_PI_4, 0.0);
        assert!(y > 40.0, "tilt toward +Y: expected positive y component, got {y}");

        let (x, _y) =
            android_tilt_to_xy_deg(std::f32::consts::FRAC_PI_4, -std::f32::consts::FRAC_PI_2);
        assert!(x < -40.0, "tilt toward -X: expected negative x component, got {x}");
    }

    #[test]
    fn android_proximity_out_mid_stroke_synthesizes_cancel() {
        let mut a = adapter();
        a.handle_android_raw(android_raw(
            3,
            AndroidSourcePhase::Down,
            (0.0, 0.0),
            0.0,
            0.3,
            ToolKind::Pen,
        ));
        let _ = drained(&mut a);

        a.handle_android_proximity(AndroidProximitySample {
            pointer_id: 3,
            pointing_device_type: ToolKind::Pen,
            caps: OPTIMISTIC_PEN_CAPS,
            is_entering: false,
        });
        let events = drained(&mut a);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
        assert!(a.active_pen_pointer.is_none());
    }
}
