//! Cross-platform stylus / pen input and palm-rejection adapter.
//!
//! This crate owns the wire-level input protocol (`StylusEvent`, `Phase`) plus
//! a `StylusAdapter` that translates platform input into that protocol. Under
//! `feature = "winit"` (default in this workspace), a convenience shim accepts
//! winit events directly; without the feature, consumers feed the adapter's
//! primitive methods (`on_cursor_moved`, `on_mouse_button`, `on_touch`) from
//! whatever event source they use.
//!
//! The adapter is buffered: one input event can emit zero or more stylus
//! events (multi-touch, future coalesced-history APIs). Consumers call the
//! event methods and then `drain` in sequence.

mod adapter;
mod geom;
mod input;

#[cfg(feature = "kurbo")]
mod kurbo_interop;

#[cfg(feature = "winit")]
mod winit_shim;

pub use adapter::{ButtonState, MouseButton, TouchEvent, TouchPhase};
pub use geom::{Point, Size};
pub use input::{
    PointerId, Sample, SampleClass, SampleRevision, StylusButtons, Tilt, ToolCaps, ToolKind,
};

#[cfg(all(feature = "mac", target_os = "macos"))]
mod macos_tablet;

#[cfg(all(feature = "wayland", target_os = "linux"))]
mod linux_wayland_tablet;

#[cfg(all(feature = "x11", target_os = "linux"))]
mod linux_x11_tablet;

#[cfg(all(feature = "windows", target_os = "windows"))]
mod windows_tablet;

#[cfg(all(feature = "web", target_arch = "wasm32"))]
mod web_pointer;

#[cfg(all(feature = "ios", target_os = "ios"))]
mod ios_touch;

#[cfg(all(feature = "android", target_os = "android"))]
mod android_motion;

pub use adapter::StylusAdapter;

#[cfg(feature = "winit")]
pub use ::winit;
#[cfg(all(feature = "android", target_os = "android"))]
pub use android_motion::{AndroidMotionEventStub, handle_android_motion};
#[cfg(all(feature = "ios", target_os = "ios"))]
pub use ios_touch::{IosStylusBackend, IosStylusInstallError, install as install_ios};
#[cfg(all(feature = "wayland", target_os = "linux"))]
pub use linux_wayland_tablet::{WaylandTabletBackend, WaylandTabletInstallError};
#[cfg(all(feature = "x11", target_os = "linux"))]
pub use linux_x11_tablet::{X11TabletBackend, X11TabletInstallError};
#[cfg(all(feature = "mac", target_os = "macos"))]
pub use macos_tablet::{MacTabletBackend, MacTabletInstallError};
#[cfg(any(
    all(feature = "mac", target_os = "macos"),
    all(feature = "ios", target_os = "ios")
))]
pub use objc2::MainThreadMarker;
#[cfg(all(feature = "web", target_arch = "wasm32"))]
pub use web_pointer::{WebStylusAttachError, WebStylusBridge, attach as attach_web};
#[cfg(all(feature = "windows", target_os = "windows"))]
pub use windows_tablet::{WindowsTabletBackend, WindowsTabletInstallError};

/// Which transition a `StylusEvent::Sample` represents. Lives here rather than
/// on `Sample` so stored strokes (which are always mid-stroke moves) don't
/// carry a field that would be meaningless on persistence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
