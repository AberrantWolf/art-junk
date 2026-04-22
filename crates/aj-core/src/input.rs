//! Input sample data types shared by the engine, renderer, and persistence layers.
//!
//! These types describe *stored* input: what a stroke is made of, independent of
//! where the data came from. The adapter crate (`aj-stylus`) owns the wire-level
//! protocol (phase, events, raw platform data) and produces `Sample`s that land
//! here after world-space conversion.
//!
//! Design notes:
//!
//! - Mandatory-with-defaults fields (`position`, `timestamp`, `pressure`, `tool`,
//!   `buttons`, `pointer_id`, `class`) are always present; missing platform data
//!   is filled with the documented default for the source.
//! - `Option<T>` fields (`tilt`, `twist_deg`, `tangential_pressure`, `distance`,
//!   `contact_size`) distinguish "platform doesn't report this" from "platform
//!   reports zero". UIs should consult a stroke's `ToolCaps` rather than
//!   sentinel-check these.
//! - `SampleClass::Estimated { update_index }` exists so iOS Pencil's late-
//!   arriving revisions can mutate earlier samples without a schema migration,
//!   even though no backend produces estimated samples in Milestone 1.

use std::time::Duration;

use bitflags::bitflags;
use kurbo::{Point, Size};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolKind {
    Unknown,
    Mouse,
    Finger,
    Pen,
    Eraser,
}

/// Stable identifier for one input source across the lifetime of a gesture. Mouse
/// is always `PointerId(0)`; fingers and pens are minted monotonically by the
/// adapter. Not serialized in committed strokes (it's adapter-session-scoped),
/// but lives on `Sample` so the app can route multi-touch events correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PointerId(pub u64);

impl PointerId {
    pub const MOUSE: Self = Self(0);
}

bitflags! {
    /// Which buttons were pressed on the input device at sample time.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct StylusButtons: u8 {
        const CONTACT   = 0b0000_0001;
        const BARREL    = 0b0000_0010;
        const SECONDARY = 0b0000_0100;
        const INVERTED  = 0b0000_1000;
    }

    /// What a pointer source can report. Published per-stroke (on `Stroke::caps`)
    /// and per-event (on `StylusEvent::caps` in aj-stylus) so UI can hide
    /// pressure-dependent controls before the first sample lands.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct ToolCaps: u16 {
        const PRESSURE             = 1 << 0;
        const TILT                 = 1 << 1;
        const TWIST                = 1 << 2;
        const TANGENTIAL_PRESSURE  = 1 << 3;
        const DISTANCE             = 1 << 4;
        const CONTACT_SIZE         = 1 << 5;
        const HOVER                = 1 << 6;
        const BARREL_BUTTON        = 1 << 7;
        const INVERT_DETECT        = 1 << 8;
        const COALESCED_HISTORY    = 1 << 9;
        const PREDICTION           = 1 << 10;
    }
}

/// Pen tilt expressed as degrees of lean along the window X and Y axes. This is
/// the canonical storage form; altitude/azimuth (used by iOS and Web L3) are
/// trivially derivable on demand.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tilt {
    pub x_deg: f32,
    pub y_deg: f32,
}

/// Whether a sample is final, predicted, or an estimate that may be revised.
/// Milestone 1 only emits `Committed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleClass {
    Committed,
    Predicted,
    Estimated { update_index: u64 },
}

/// A single input sample. See module-level docs for field semantics.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub position: Point,
    pub timestamp: Duration,
    pub pressure: f32,
    pub tool: ToolKind,
    pub buttons: StylusButtons,
    pub pointer_id: PointerId,
    pub class: SampleClass,

    pub tilt: Option<Tilt>,
    pub twist_deg: Option<f32>,
    pub tangential_pressure: Option<f32>,
    pub distance: Option<f32>,
    pub contact_size: Option<Size>,
}

impl Sample {
    /// Internal constructor that enforces the pressure-clamp invariant in one
    /// place. All public constructors route through this.
    fn new_internal(
        position: Point,
        timestamp: Duration,
        pressure: f32,
        tool: ToolKind,
        buttons: StylusButtons,
        pointer_id: PointerId,
    ) -> Self {
        Self {
            position,
            timestamp,
            pressure: pressure.clamp(0.0, 1.0),
            tool,
            buttons,
            pointer_id,
            class: SampleClass::Committed,
            tilt: None,
            twist_deg: None,
            tangential_pressure: None,
            distance: None,
            contact_size: None,
        }
    }

    /// Build a mouse sample. Pressure defaults to 0.5 since a mouse can't report
    /// it — a midpoint feels less wrong than 0 or 1 when strokes finally start
    /// varying width with pressure.
    #[must_use]
    pub fn mouse(position: Point, timestamp: Duration, pointer_id: PointerId) -> Self {
        Self::new_internal(
            position,
            timestamp,
            0.5,
            ToolKind::Mouse,
            StylusButtons::CONTACT,
            pointer_id,
        )
    }

    /// Build a finger-touch sample. `force` comes from winit's `Force::Normalized`
    /// or the calibrated conversion; `None` means the platform didn't report it
    /// and we fill 1.0 (full-contact finger).
    #[must_use]
    pub fn finger(
        position: Point,
        timestamp: Duration,
        pointer_id: PointerId,
        force: Option<f32>,
    ) -> Self {
        Self::new_internal(
            position,
            timestamp,
            force.unwrap_or(1.0),
            ToolKind::Finger,
            StylusButtons::CONTACT,
            pointer_id,
        )
    }

    /// Build a pen sample with `tool` = Pen or Eraser. Callers are expected to
    /// fill in `pressure`, `tilt`, `twist_deg`, and `tangential_pressure`
    /// after construction; this constructor just sets the tool-specific
    /// defaults that are shared across platform backends. Non-pen tool kinds
    /// are accepted and pass through unchanged — useful when `tool` was
    /// learned from a platform proximity event.
    #[must_use]
    pub fn new_pen(
        position: Point,
        timestamp: Duration,
        pointer_id: PointerId,
        tool: ToolKind,
    ) -> Self {
        Self::new_internal(position, timestamp, 0.5, tool, StylusButtons::CONTACT, pointer_id)
    }

    /// Placeholder sample used when synthesizing a `Cancel` for a lost stroke
    /// (focus loss, proximity-out without prior Up). Pressure is meaningless on
    /// a cancellation; we use 0 so downstream visualisations don't render
    /// "full pressure" at the tear-down point.
    #[must_use]
    pub fn new_pen_placeholder(
        position: Point,
        timestamp: Duration,
        pointer_id: PointerId,
        tool: ToolKind,
    ) -> Self {
        Self::new_internal(position, timestamp, 0.0, tool, StylusButtons::empty(), pointer_id)
    }
}

/// Shape of a brush as it affects stroke rendering. Milestone 1 ships the type
/// but the renderer ignores it; the circle-stamp milestone will read these.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrushParams {
    pub min_width: f32,
    pub max_width: f32,
    pub curve: PressureCurve,
}

impl Default for BrushParams {
    fn default() -> Self {
        Self { min_width: 0.5, max_width: 4.0, curve: PressureCurve::Linear }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureCurve {
    Linear,
}

/// Partial update applied to an `Estimated` sample. Sent by platforms that
/// deliver initial samples with incomplete data (notably macOS: the first
/// `LeftMouseDown` with tablet subtype fires before the pen fully settles, and
/// the real pressure arrives on the immediately-following `NSTabletPoint`).
/// `None` fields mean "don't change this field."
///
/// Applying a revision promotes the sample's `class` from `Estimated` to
/// `Committed`; future revisions for the same `update_index` are dropped.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SampleRevision {
    pub pressure: Option<f32>,
    pub tilt: Option<Tilt>,
    pub twist_deg: Option<f32>,
    pub tangential_pressure: Option<f32>,
}

impl SampleRevision {
    /// Apply this revision to a sample's fields. Clamps pressure to 0..=1 to
    /// preserve the invariant enforced by `Sample::new_internal`.
    pub fn apply_to(self, sample: &mut Sample) {
        if let Some(p) = self.pressure {
            sample.pressure = p.clamp(0.0, 1.0);
        }
        if let Some(t) = self.tilt {
            sample.tilt = Some(t);
        }
        if let Some(t) = self.twist_deg {
            sample.twist_deg = Some(t);
        }
        if let Some(t) = self.tangential_pressure {
            sample.tangential_pressure = Some(t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_sample_has_defaults() {
        let s = Sample::mouse(Point::new(1.0, 2.0), Duration::from_millis(10), PointerId::MOUSE);
        assert_eq!(s.tool, ToolKind::Mouse);
        assert!((s.pressure - 0.5).abs() < f32::EPSILON);
        assert_eq!(s.pointer_id, PointerId::MOUSE);
        assert_eq!(s.class, SampleClass::Committed);
        assert!(s.tilt.is_none());
    }

    #[test]
    fn finger_sample_uses_force_when_given() {
        let s = Sample::finger(Point::new(0.0, 0.0), Duration::ZERO, PointerId(7), Some(0.25));
        assert!((s.pressure - 0.25).abs() < f32::EPSILON);
        assert_eq!(s.tool, ToolKind::Finger);
    }

    #[test]
    fn finger_sample_defaults_to_one_without_force() {
        let s = Sample::finger(Point::new(0.0, 0.0), Duration::ZERO, PointerId(1), None);
        assert!((s.pressure - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn pressure_is_clamped() {
        let s = Sample::finger(Point::new(0.0, 0.0), Duration::ZERO, PointerId(1), Some(5.0));
        assert!((s.pressure - 1.0).abs() < f32::EPSILON);
        let s = Sample::finger(Point::new(0.0, 0.0), Duration::ZERO, PointerId(1), Some(-1.0));
        assert!(s.pressure.abs() < f32::EPSILON);
    }

    #[test]
    fn brush_params_default_is_sensible() {
        let b = BrushParams::default();
        assert!(b.min_width < b.max_width);
        assert_eq!(b.curve, PressureCurve::Linear);
    }
}
