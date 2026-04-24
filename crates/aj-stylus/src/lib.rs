//! Cross-platform stylus input abstraction for art-junk.
//!
//! This crate owns the wire-level input protocol (`StylusEvent`, `Phase`) and a
//! `StylusAdapter` that translates winit events into that protocol. Stored sample
//! data (`Sample`, `ToolCaps`, `PointerId`, …) lives in `aj-core` so committed
//! strokes don't carry adapter-session types.
//!
//! The adapter is buffered: one winit event can emit zero or more stylus events
//! (multi-touch, future coalesced-history APIs). Consumers call `on_window_event`
//! and then `drain` in sequence.

mod adapter;

#[cfg(target_os = "macos")]
mod macos_tablet;

#[cfg(target_os = "linux")]
mod linux_wayland_tablet;

#[cfg(target_os = "linux")]
mod linux_x11_tablet;

pub use adapter::StylusAdapter;
#[cfg(target_os = "linux")]
pub use linux_wayland_tablet::{WaylandTabletBackend, WaylandTabletInstallError};
#[cfg(target_os = "linux")]
pub use linux_x11_tablet::{X11TabletBackend, X11TabletInstallError};
#[cfg(target_os = "macos")]
pub use macos_tablet::{MacTabletBackend, MacTabletInstallError};
#[cfg(target_os = "macos")]
pub use objc2::MainThreadMarker;

use aj_core::{PointerId, Sample, SampleRevision, ToolCaps};
use kurbo::Point;

/// Which transition a `StylusEvent::Sample` represents. Lives here rather than
/// on `Sample` so stored strokes (which are always mid-stroke moves) don't
/// carry a field that would be meaningless on persistence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Hover,
    Down,
    Move,
    Up,
    Cancel,
}

/// Off-contact pose reported by iPadOS Apple Pencil hover and Pencil Pro squeeze.
/// All angles in radians, matching Apple's native units. `z_offset` / `roll_rad`
/// are `Option` because they are gated on newer hardware or iOS versions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HoverPose {
    pub position: Point,
    pub z_offset: Option<f32>,
    pub altitude_rad: f32,
    pub azimuth_rad: f32,
    pub roll_rad: Option<f32>,
}

/// Pencil-side interaction events that don't ride on a `UITouch` — Apple
/// Pencil 2 double-tap and Pencil Pro squeeze. iOS-only today; variant kept
/// platform-agnostic so future stylus hardware with side gestures can reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PencilInteractionKind {
    Tap,
    SqueezeBegan,
    SqueezeChanged,
    SqueezeEnded,
    SqueezeCancelled,
}

/// Adapter output. The `sample` field on the `Sample` variant is in screen-space
/// physical pixels — the app is responsible for viewport conversion before
/// passing it to the engine.
///
/// Marked `#[non_exhaustive]` so future variants (e.g. bulk revisions,
/// hover with proximity distance) can land without breaking exhaustive matches
/// in the app.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum StylusEvent {
    Sample {
        sample: Sample,
        phase: Phase,
        caps: ToolCaps,
    },
    /// Revises an earlier sample that was emitted with
    /// `SampleClass::Estimated { update_index }`. The engine looks up the
    /// matching sample in the active stroke (or most-recently-committed stroke
    /// as a race rescue) and applies the fields.
    Revise {
        pointer_id: PointerId,
        update_index: u64,
        revision: SampleRevision,
    },
    /// Side-gesture event from a Pencil (tap, squeeze). Not tied to a
    /// `StylusEvent::Sample` — Pencil delivers these independently of
    /// touch streams.
    PencilInteraction {
        kind: PencilInteractionKind,
        hover_pose: Option<HoverPose>,
    },
}
