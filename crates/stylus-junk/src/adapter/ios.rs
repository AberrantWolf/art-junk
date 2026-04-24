//! iOS overlay-UIView seam. iOS delivers `UITouch` events with an
//! Estimated/Revise cycle for axes that weren't yet settled at touch time
//! (pressure over BT, etc.). Unlike mac, multiple axes per stroke can be in
//! flight and `touchesEstimatedPropertiesUpdated:` can arrive out of order
//! — which is why `pending_estimated` is keyed by `update_index` rather
//! than `PointerId`.

use std::time::Duration;

use crate::{
    Point, PointerId, Sample, SampleClass, SampleRevision, StylusButtons, Tilt, ToolCaps, ToolKind,
};

use super::{OPTIMISTIC_PEN_CAPS, PlatformTimestampAnchor, StylusAdapter, alloc_pointer_id};
use crate::{Phase, StylusEvent};

#[cfg(all(feature = "ios", any(target_os = "ios", test)))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IosTouchPhase {
    Down,
    Move,
    Up,
    Cancel,
    /// Hover state from `UIHoverGestureRecognizer`. Emits `Phase::Hover`.
    Hover,
}

#[cfg(all(feature = "ios", any(target_os = "ios", test)))]
bitflags::bitflags! {
    /// Which axes are `.estimated` or `expecting update` on an iOS `UITouch`.
    /// Mirrors `UITouchProperties` but keeps the adapter free of UIKit types.
    /// Only the `ExpectsUpdate` bit gates whether the adapter inserts into
    /// `pending_estimated`; the `Estimated` bit is informational.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub(crate) struct IosEstimatedProperties: u8 {
        const FORCE         = 1 << 0;
        const AZIMUTH       = 1 << 1;
        const ALTITUDE      = 1 << 2;
        const LOCATION      = 1 << 3;
        const ROLL          = 1 << 4;
        /// At least one axis on this sample is estimated *and* the OS will
        /// later deliver a `touchesEstimatedPropertiesUpdated:` with the
        /// refinement.
        const EXPECTS_UPDATE = 1 << 5;
    }
}

/// Raw sample from the iOS overlay `UIView` backend. One
/// `IosTouchRawSample` per element of `UIEvent.coalescedTouches(for:)`
/// (all tagged `Committed`) plus one per element of
/// `UIEvent.predictedTouches(for:)` (tagged `Predicted`).
#[cfg(all(feature = "ios", any(target_os = "ios", test)))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct IosTouchRawSample {
    pub position_physical_px: Point,
    /// `UITouch.timestamp` — seconds, monotonic (shares clock with
    /// `CACurrentMediaTime()`).
    pub timestamp_secs: f64,
    /// Normalized `force / maximumPossibleForce`, pre-clamped to `0..=1`.
    pub pressure: f32,
    /// Apple's polar form, radians. `altitude=0` = flush, `π/2` = perpendicular.
    pub altitude_rad: f32,
    /// Radians, `-π..=π`. Direction the shaft points on the canvas plane.
    pub azimuth_rad: f32,
    /// Pencil Pro barrel-roll (iOS 17.5+). `None` on older Pencils.
    pub roll_rad: Option<f32>,
    /// Stable key from `UITouch.estimationUpdateIndex`; populated iff at
    /// least one axis is `.estimated`. Maps a later
    /// `touchesEstimatedPropertiesUpdated:` to this sample.
    pub estimation_update_index: Option<u64>,
    pub estimated_properties: IosEstimatedProperties,
    /// Per-touch opaque identity (hash of the `UITouch *` for the gesture's
    /// lifetime).
    pub touch_id: u64,
    pub pointing_device_type: ToolKind,
    pub source_phase: IosTouchPhase,
    /// `true` if this sample came from `predictedTouches(for:)` rather than
    /// `coalescedTouches(for:)`. Tags the emitted sample
    /// `SampleClass::Predicted`.
    pub predicted: bool,
}

#[cfg(all(feature = "ios", any(target_os = "ios", test)))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct IosTouchProximitySample {
    pub touch_id: u64,
    pub pointing_device_type: ToolKind,
    pub caps: ToolCaps,
    pub is_entering: bool,
}

#[cfg(all(feature = "ios", any(target_os = "ios", test)))]
#[allow(clippy::similar_names)]
pub(crate) fn ios_altitude_azimuth_to_tilt_xy_deg(altitude: f32, azimuth: f32) -> (f32, f32) {
    let theta = std::f32::consts::FRAC_PI_2 - altitude;
    let sin_theta = theta.sin();
    if sin_theta < 0.02 {
        return (0.0, 0.0);
    }
    let sin_alt = altitude.sin().max(f32::EPSILON);
    let tilt_x = (sin_theta * azimuth.cos()).atan2(sin_alt);
    let tilt_y = (sin_theta * azimuth.sin()).atan2(sin_alt);
    (tilt_x.to_degrees(), tilt_y.to_degrees())
}

impl StylusAdapter {
    #[cfg(all(feature = "ios", any(target_os = "ios", test)))]
    pub(crate) fn handle_ios_raw(&mut self, raw: IosTouchRawSample) {
        let ts = PlatformTimestampAnchor::translate_or_anchor(
            &mut self.ios_anchor,
            raw.timestamp_secs,
            self.epoch,
        );

        if matches!(raw.source_phase, IosTouchPhase::Hover) {
            let (tilt_x, tilt_y) =
                ios_altitude_azimuth_to_tilt_xy_deg(raw.altitude_rad, raw.azimuth_rad);
            let mut sample = Sample::new_pen(
                raw.position_physical_px,
                ts,
                PointerId::MOUSE,
                raw.pointing_device_type,
            );
            sample.pressure = 0.0;
            sample.tilt = Some(Tilt { x_deg: tilt_x, y_deg: tilt_y });
            sample.twist_deg = raw.roll_rad.map(f32::to_degrees);
            self.queue.push_back(StylusEvent::Sample {
                sample,
                phase: Phase::Hover,
                caps: OPTIMISTIC_PEN_CAPS,
            });
            return;
        }

        match raw.source_phase {
            IosTouchPhase::Down => {
                let pid = alloc_pointer_id(&mut self.next_pointer_id);
                self.ios_pointers.insert(raw.touch_id, pid);
                self.active_pen_pointer = Some(pid);
                let mut sample = build_ios_sample(&raw, ts, pid);
                if raw.estimated_properties.contains(IosEstimatedProperties::EXPECTS_UPDATE)
                    && let Some(update_index) = raw.estimation_update_index
                {
                    sample.class = SampleClass::Estimated { update_index };
                    self.pending_estimated.insert(update_index, pid);
                }
                self.queue.push_back(StylusEvent::Sample {
                    sample,
                    phase: Phase::Down,
                    caps: OPTIMISTIC_PEN_CAPS,
                });
            }
            IosTouchPhase::Move | IosTouchPhase::Up | IosTouchPhase::Cancel => {
                let Some(&pid) = self.ios_pointers.get(&raw.touch_id) else {
                    return;
                };
                let phase = match raw.source_phase {
                    IosTouchPhase::Move => Phase::Move,
                    IosTouchPhase::Up => Phase::Up,
                    IosTouchPhase::Cancel => Phase::Cancel,
                    _ => unreachable!(),
                };
                let mut sample = build_ios_sample(&raw, ts, pid);
                if raw.predicted {
                    sample.class = SampleClass::Predicted;
                } else if raw.estimated_properties.contains(IosEstimatedProperties::EXPECTS_UPDATE)
                    && let Some(update_index) = raw.estimation_update_index
                {
                    sample.class = SampleClass::Estimated { update_index };
                    self.pending_estimated.insert(update_index, pid);
                }
                self.queue.push_back(StylusEvent::Sample {
                    sample,
                    phase,
                    caps: OPTIMISTIC_PEN_CAPS,
                });

                if matches!(raw.source_phase, IosTouchPhase::Up | IosTouchPhase::Cancel) {
                    self.ios_pointers.remove(&raw.touch_id);
                    if self.active_pen_pointer == Some(pid) {
                        self.active_pen_pointer = None;
                    }
                }
            }
            IosTouchPhase::Hover => unreachable!("handled above"),
        }
    }

    /// Resolve a deferred estimate via `touchesEstimatedPropertiesUpdated:`.
    /// Keyed by `update_index` directly because iOS can deliver these out
    /// of order for Pencil-over-BT scenarios — unlike mac where the first
    /// Move closes the Down's estimate linearly.
    #[cfg(all(feature = "ios", any(target_os = "ios", test)))]
    pub(crate) fn handle_ios_estimated_update(
        &mut self,
        update_index: u64,
        revision: SampleRevision,
    ) {
        let Some(pid) = self.pending_estimated.remove(&update_index) else {
            return;
        };
        self.queue.push_back(StylusEvent::Revise { pointer_id: pid, update_index, revision });
    }

    #[cfg(all(feature = "ios", any(target_os = "ios", test)))]
    pub(crate) fn handle_ios_proximity(&mut self, prox: IosTouchProximitySample) {
        if !prox.is_entering
            && let Some(pid) = self.ios_pointers.remove(&prox.touch_id)
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

    #[cfg(all(feature = "ios", any(target_os = "ios", test)))]
    pub(crate) fn handle_ios_pencil_interaction(
        &mut self,
        kind: crate::PencilInteractionKind,
        hover_pose: Option<crate::HoverPose>,
    ) {
        self.queue.push_back(StylusEvent::PencilInteraction { kind, hover_pose });
    }
}

#[cfg(all(feature = "ios", any(target_os = "ios", test)))]
fn build_ios_sample(raw: &IosTouchRawSample, timestamp: Duration, pointer_id: PointerId) -> Sample {
    let (tilt_x, tilt_y) = ios_altitude_azimuth_to_tilt_xy_deg(raw.altitude_rad, raw.azimuth_rad);
    let mut sample =
        Sample::new_pen(raw.position_physical_px, timestamp, pointer_id, raw.pointing_device_type);
    sample.pressure = raw.pressure.clamp(0.0, 1.0);
    sample.tilt = Some(Tilt { x_deg: tilt_x, y_deg: tilt_y });
    sample.twist_deg = raw.roll_rad.map(f32::to_degrees);
    let mut buttons = StylusButtons::CONTACT;
    if matches!(raw.pointing_device_type, ToolKind::Eraser) {
        buttons |= StylusButtons::INVERTED;
    }
    sample.buttons = buttons;
    sample
}

#[cfg(test)]
mod tests {
    use crate::{Point, PointerId, SampleClass, SampleRevision, ToolKind};

    use super::super::OPTIMISTIC_PEN_CAPS;
    use super::super::tests_common::{adapter, drained, expect_sample};
    use super::*;
    use crate::{Phase, StylusEvent};

    fn ios_raw(
        touch_id: u64,
        phase: IosTouchPhase,
        pos: (f64, f64),
        ts: f64,
        pressure: f32,
    ) -> IosTouchRawSample {
        IosTouchRawSample {
            position_physical_px: Point::new(pos.0, pos.1),
            timestamp_secs: ts,
            pressure,
            altitude_rad: std::f32::consts::FRAC_PI_4,
            azimuth_rad: 0.0,
            roll_rad: None,
            estimation_update_index: None,
            estimated_properties: IosEstimatedProperties::empty(),
            touch_id,
            pointing_device_type: ToolKind::Pen,
            source_phase: phase,
            predicted: false,
        }
    }

    #[test]
    fn ios_down_expects_update_emits_estimated_with_update_index() {
        let mut a = adapter();
        let mut raw = ios_raw(42, IosTouchPhase::Down, (10.0, 20.0), 0.0, 0.3);
        raw.estimation_update_index = Some(100);
        raw.estimated_properties =
            IosEstimatedProperties::FORCE | IosEstimatedProperties::EXPECTS_UPDATE;

        a.handle_ios_raw(raw);
        let events = drained(&mut a);
        let (sample, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Down);
        assert!(matches!(sample.class, SampleClass::Estimated { update_index: 100 }));
        assert!(a.pending_estimated.contains_key(&100));
    }

    #[test]
    fn ios_estimated_update_emits_revise_by_update_index() {
        let mut a = adapter();
        let mut down = ios_raw(7, IosTouchPhase::Down, (0.0, 0.0), 0.0, 0.2);
        down.estimation_update_index = Some(55);
        down.estimated_properties = IosEstimatedProperties::EXPECTS_UPDATE;
        a.handle_ios_raw(down);
        let _ = drained(&mut a);

        let revision = SampleRevision { pressure: Some(0.7), ..SampleRevision::default() };
        a.handle_ios_estimated_update(55, revision);

        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StylusEvent::Revise { update_index, revision, .. } => {
                assert_eq!(*update_index, 55);
                assert!((revision.pressure.unwrap() - 0.7).abs() < f32::EPSILON);
            }
            other => panic!("expected Revise, got {other:?}"),
        }
        assert!(!a.pending_estimated.contains_key(&55));
    }

    #[test]
    fn ios_out_of_order_estimated_updates_resolve_correctly() {
        let mut a = adapter();
        let mut d1 = ios_raw(1, IosTouchPhase::Down, (0.0, 0.0), 0.0, 0.2);
        d1.estimation_update_index = Some(10);
        d1.estimated_properties = IosEstimatedProperties::EXPECTS_UPDATE;
        a.handle_ios_raw(d1);
        let mut m1 = ios_raw(1, IosTouchPhase::Move, (1.0, 1.0), 0.008, 0.3);
        m1.estimation_update_index = Some(11);
        m1.estimated_properties = IosEstimatedProperties::EXPECTS_UPDATE;
        a.handle_ios_raw(m1);
        let _ = drained(&mut a);

        a.handle_ios_estimated_update(
            11,
            SampleRevision { pressure: Some(0.45), ..SampleRevision::default() },
        );
        a.handle_ios_estimated_update(
            10,
            SampleRevision { pressure: Some(0.25), ..SampleRevision::default() },
        );
        let events = drained(&mut a);
        assert_eq!(events.len(), 2, "each update emits one Revise");
        assert!(a.pending_estimated.is_empty());
    }

    #[test]
    fn ios_predicted_samples_tagged_predicted() {
        let mut a = adapter();
        a.handle_ios_raw(ios_raw(1, IosTouchPhase::Down, (0.0, 0.0), 0.0, 0.4));
        let _ = drained(&mut a);

        let mut p = ios_raw(1, IosTouchPhase::Move, (2.0, 2.0), 0.016, 0.5);
        p.predicted = true;
        a.handle_ios_raw(p);

        let events = drained(&mut a);
        let (sample, _, _) = expect_sample(&events[0]);
        assert_eq!(sample.class, SampleClass::Predicted);
    }

    #[test]
    fn ios_tilt_decomposition_cardinals_and_zero() {
        let (x, y) = ios_altitude_azimuth_to_tilt_xy_deg(
            std::f32::consts::FRAC_PI_2,
            std::f32::consts::FRAC_PI_4,
        );
        assert!(x.abs() < 0.5 && y.abs() < 0.5, "near-perpendicular zero region");

        let (x, y) = ios_altitude_azimuth_to_tilt_xy_deg(std::f32::consts::FRAC_PI_4, 0.0);
        assert!(x > 30.0, "tilt toward +X should have positive x_deg, got {x}");
        assert!(y.abs() < 1.0, "no Y tilt component, got {y}");

        let (x, y) = ios_altitude_azimuth_to_tilt_xy_deg(
            std::f32::consts::FRAC_PI_4,
            std::f32::consts::FRAC_PI_2,
        );
        assert!(y > 30.0, "tilt toward +Y should have positive y_deg, got {y}");
        assert!(x.abs() < 1.0, "no X tilt component, got {x}");

        let (x, y) = ios_altitude_azimuth_to_tilt_xy_deg(0.0, 0.0);
        assert!((x - 90.0).abs() < 1.0, "flat toward +X: expect ~90°, got {x}");
        assert!(y.abs() < 1.0);
    }

    #[test]
    fn ios_full_stroke_down_move_up_cancel_phases() {
        let mut a = adapter();
        a.handle_ios_raw(ios_raw(1, IosTouchPhase::Down, (0.0, 0.0), 0.0, 0.3));
        a.handle_ios_raw(ios_raw(1, IosTouchPhase::Move, (1.0, 1.0), 0.008, 0.4));
        a.handle_ios_raw(ios_raw(1, IosTouchPhase::Up, (1.0, 1.0), 0.016, 0.0));
        let events = drained(&mut a);
        let phases: Vec<Phase> = events.iter().map(|e| expect_sample(e).1).collect();
        assert_eq!(phases, vec![Phase::Down, Phase::Move, Phase::Up]);
        assert!(a.active_pen_pointer.is_none());

        a.handle_ios_raw(ios_raw(2, IosTouchPhase::Down, (5.0, 5.0), 0.024, 0.3));
        a.handle_ios_raw(ios_raw(2, IosTouchPhase::Cancel, (5.0, 5.0), 0.030, 0.0));
        let events = drained(&mut a);
        let phases: Vec<Phase> = events.iter().map(|e| expect_sample(e).1).collect();
        assert_eq!(phases, vec![Phase::Down, Phase::Cancel]);
    }

    #[test]
    fn ios_hover_emits_hover_phase_on_mouse_pointer() {
        let mut a = adapter();
        let mut hover = ios_raw(99, IosTouchPhase::Hover, (5.0, 5.0), 0.0, 0.0);
        hover.pressure = 0.0;
        a.handle_ios_raw(hover);

        let events = drained(&mut a);
        let (sample, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Hover);
        assert_eq!(sample.pointer_id, PointerId::MOUSE);
    }

    #[test]
    fn ios_proximity_out_mid_stroke_synthesizes_cancel() {
        let mut a = adapter();
        a.handle_ios_raw(ios_raw(5, IosTouchPhase::Down, (0.0, 0.0), 0.0, 0.3));
        let _ = drained(&mut a);

        a.handle_ios_proximity(IosTouchProximitySample {
            touch_id: 5,
            pointing_device_type: ToolKind::Pen,
            caps: OPTIMISTIC_PEN_CAPS,
            is_entering: false,
        });
        let events = drained(&mut a);
        assert_eq!(events.len(), 1);
        let (_, phase, _) = expect_sample(&events[0]);
        assert_eq!(phase, Phase::Cancel);
        assert!(a.active_pen_pointer.is_none());
    }

    #[test]
    fn ios_pencil_interaction_emits_variant() {
        let mut a = adapter();
        a.handle_ios_pencil_interaction(crate::PencilInteractionKind::Tap, None);
        let mut events: Vec<StylusEvent> = a.drain().collect();
        assert_eq!(events.len(), 1);
        match events.remove(0) {
            StylusEvent::PencilInteraction { kind, .. } => {
                assert_eq!(kind, crate::PencilInteractionKind::Tap);
            }
            other => panic!("expected PencilInteraction, got {other:?}"),
        }
    }
}
