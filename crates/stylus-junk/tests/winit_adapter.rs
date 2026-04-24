//! Adapter state-machine tests driven by canned winit events. Gated on
//! `feature = "winit"` so a `--no-default-features` build doesn't try to link
//! winit just for these tests.

#![cfg(feature = "winit")]

use stylus_junk::{Phase, PointerId, Sample, StylusAdapter, StylusEvent, ToolCaps, ToolKind};
use winit::dpi::PhysicalPosition;
use winit::event::{DeviceId, ElementState, Force, MouseButton, Touch, TouchPhase, WindowEvent};

fn device() -> DeviceId {
    DeviceId::dummy()
}

fn cursor_moved(x: f64, y: f64) -> WindowEvent {
    WindowEvent::CursorMoved { device_id: device(), position: PhysicalPosition::new(x, y) }
}

fn mouse_input(state: ElementState) -> WindowEvent {
    WindowEvent::MouseInput { device_id: device(), state, button: MouseButton::Left }
}

fn cursor_left() -> WindowEvent {
    WindowEvent::CursorLeft { device_id: device() }
}

fn touch_event(phase: TouchPhase, id: u64, x: f64, y: f64, force: Option<Force>) -> WindowEvent {
    WindowEvent::Touch(Touch {
        device_id: device(),
        phase,
        location: PhysicalPosition::new(x, y),
        force,
        id,
    })
}

fn drain(adapter: &mut StylusAdapter) -> Vec<StylusEvent> {
    adapter.drain().collect()
}

/// Destructure an event expected to be a `Sample` variant; panic otherwise.
/// Keeps per-test noise down now that `StylusEvent` is an enum.
fn expect_sample(ev: &StylusEvent) -> (&Sample, Phase, ToolCaps) {
    match ev {
        StylusEvent::Sample { sample, phase, caps } => (sample, *phase, *caps),
        other => panic!("expected StylusEvent::Sample, got {other:?}"),
    }
}

#[test]
fn mouse_press_move_release_emits_down_move_up() {
    let mut a = StylusAdapter::new();

    a.on_window_event(&cursor_moved(10.0, 20.0));
    assert!(drain(&mut a).is_empty(), "cursor-move without press emits nothing");

    a.on_window_event(&mouse_input(ElementState::Pressed));
    let events = drain(&mut a);
    assert_eq!(events.len(), 1);
    let (sample, phase, _) = expect_sample(&events[0]);
    assert_eq!(phase, Phase::Down);
    assert_eq!(sample.tool, ToolKind::Mouse);
    assert_eq!(sample.pointer_id, PointerId::MOUSE);

    a.on_window_event(&cursor_moved(15.0, 25.0));
    let events = drain(&mut a);
    assert_eq!(events.len(), 1);
    let (sample, phase, _) = expect_sample(&events[0]);
    assert_eq!(phase, Phase::Move);
    assert!((sample.position.x - 15.0).abs() < f64::EPSILON);

    a.on_window_event(&mouse_input(ElementState::Released));
    let events = drain(&mut a);
    assert_eq!(events.len(), 1);
    let (_, phase, _) = expect_sample(&events[0]);
    assert_eq!(phase, Phase::Up);
    assert!(!a.is_tracking_pointer());
}

#[test]
fn cursor_moved_without_press_emits_nothing() {
    let mut a = StylusAdapter::new();
    a.on_window_event(&cursor_moved(1.0, 2.0));
    a.on_window_event(&cursor_moved(3.0, 4.0));
    a.on_window_event(&cursor_moved(5.0, 6.0));
    assert!(drain(&mut a).is_empty());
    assert!(!a.is_tracking_pointer());
}

#[test]
fn cursor_left_mid_stroke_does_not_cancel() {
    let mut a = StylusAdapter::new();
    a.on_window_event(&cursor_moved(1.0, 2.0));
    a.on_window_event(&mouse_input(ElementState::Pressed));
    a.on_window_event(&cursor_moved(3.0, 4.0));
    let _ = drain(&mut a);

    a.on_window_event(&cursor_left());
    assert!(drain(&mut a).is_empty(), "cursor_left must not emit events");
    assert!(a.is_tracking_pointer(), "stroke continues after cursor leaves");

    a.on_window_event(&cursor_moved(-5.0, -5.0));
    let events = drain(&mut a);
    assert_eq!(events.len(), 1);
    let (_, phase, _) = expect_sample(&events[0]);
    assert_eq!(phase, Phase::Move);

    a.on_window_event(&mouse_input(ElementState::Released));
    let events = drain(&mut a);
    assert_eq!(events.len(), 1);
    let (_, phase, _) = expect_sample(&events[0]);
    assert_eq!(phase, Phase::Up);
    assert!(!a.is_tracking_pointer());
}

#[test]
fn cursor_left_without_stroke_is_silent() {
    let mut a = StylusAdapter::new();
    a.on_window_event(&cursor_moved(1.0, 2.0));
    a.on_window_event(&cursor_left());
    assert!(drain(&mut a).is_empty());
    assert!(!a.is_tracking_pointer());
}

#[test]
fn double_press_is_idempotent() {
    let mut a = StylusAdapter::new();
    a.on_window_event(&cursor_moved(1.0, 2.0));
    a.on_window_event(&mouse_input(ElementState::Pressed));
    a.on_window_event(&mouse_input(ElementState::Pressed));
    let events = drain(&mut a);
    assert_eq!(events.len(), 1, "second Pressed is dropped, not re-emitted");
    let (_, phase, _) = expect_sample(&events[0]);
    assert_eq!(phase, Phase::Down);
}

#[test]
fn release_without_press_emits_nothing() {
    let mut a = StylusAdapter::new();
    a.on_window_event(&mouse_input(ElementState::Released));
    assert!(drain(&mut a).is_empty());
}

#[test]
fn two_fingers_track_independent_pointer_ids() {
    let mut a = StylusAdapter::new();
    a.on_window_event(&touch_event(TouchPhase::Started, 7, 10.0, 10.0, None));
    a.on_window_event(&touch_event(TouchPhase::Started, 8, 20.0, 20.0, None));

    let events = drain(&mut a);
    assert_eq!(events.len(), 2);
    let (sample_7, phase_7, _) = expect_sample(&events[0]);
    let (sample_8, phase_8, _) = expect_sample(&events[1]);
    assert_eq!(phase_7, Phase::Down);
    assert_eq!(phase_8, Phase::Down);
    let pid_7 = sample_7.pointer_id;
    let pid_8 = sample_8.pointer_id;
    assert_ne!(pid_7, pid_8);
    assert_ne!(pid_7, PointerId::MOUSE);
    assert_ne!(pid_8, PointerId::MOUSE);

    a.on_window_event(&touch_event(TouchPhase::Moved, 7, 11.0, 11.0, None));
    let events = drain(&mut a);
    assert_eq!(events.len(), 1);
    let (sample, phase, _) = expect_sample(&events[0]);
    assert_eq!(sample.pointer_id, pid_7);
    assert_eq!(phase, Phase::Move);

    a.on_window_event(&touch_event(TouchPhase::Ended, 8, 20.0, 20.0, None));
    let events = drain(&mut a);
    assert_eq!(events.len(), 1);
    let (sample, phase, _) = expect_sample(&events[0]);
    assert_eq!(sample.pointer_id, pid_8);
    assert_eq!(phase, Phase::Up);
    assert!(a.is_tracking_pointer(), "finger 7 is still down");

    a.on_window_event(&touch_event(TouchPhase::Ended, 7, 11.0, 11.0, None));
    assert!(!a.is_tracking_pointer());
}

#[test]
fn touch_force_populates_pressure_and_caps() {
    let mut a = StylusAdapter::new();
    a.on_window_event(&touch_event(
        TouchPhase::Started,
        1,
        0.0,
        0.0,
        Some(Force::Normalized(0.25)),
    ));
    let events = drain(&mut a);
    assert_eq!(events.len(), 1);
    let (sample, _, caps) = expect_sample(&events[0]);
    assert!((sample.pressure - 0.25).abs() < 1e-4);
    assert!(caps.contains(ToolCaps::PRESSURE));
    assert_eq!(sample.tool, ToolKind::Finger);
}

#[test]
fn touch_without_force_defaults_pressure_one_and_no_caps() {
    let mut a = StylusAdapter::new();
    a.on_window_event(&touch_event(TouchPhase::Started, 1, 0.0, 0.0, None));
    let events = drain(&mut a);
    assert_eq!(events.len(), 1);
    let (sample, _, caps) = expect_sample(&events[0]);
    assert!((sample.pressure - 1.0).abs() < f32::EPSILON);
    assert!(!caps.contains(ToolCaps::PRESSURE));
}

#[test]
fn mouse_event_caps_are_empty() {
    let mut a = StylusAdapter::new();
    a.on_window_event(&cursor_moved(1.0, 2.0));
    a.on_window_event(&mouse_input(ElementState::Pressed));
    let events = drain(&mut a);
    assert_eq!(events.len(), 1);
    let (sample, _, caps) = expect_sample(&events[0]);
    assert_eq!(caps, ToolCaps::empty());
    assert!(sample.tilt.is_none());
}

#[test]
fn is_tracking_reflects_mouse_state() {
    let mut a = StylusAdapter::new();
    assert!(!a.is_tracking_pointer());
    a.on_window_event(&cursor_moved(1.0, 2.0));
    a.on_window_event(&mouse_input(ElementState::Pressed));
    assert!(a.is_tracking_pointer());
    a.on_window_event(&mouse_input(ElementState::Released));
    assert!(!a.is_tracking_pointer());
}

#[test]
fn timestamps_are_monotonic_within_a_stroke() {
    let mut a = StylusAdapter::new();
    a.on_window_event(&cursor_moved(1.0, 2.0));
    a.on_window_event(&mouse_input(ElementState::Pressed));
    std::thread::sleep(std::time::Duration::from_millis(2));
    a.on_window_event(&cursor_moved(3.0, 4.0));
    std::thread::sleep(std::time::Duration::from_millis(2));
    a.on_window_event(&mouse_input(ElementState::Released));

    let events = drain(&mut a);
    let timestamps: Vec<_> = events.iter().map(|e| expect_sample(e).0.timestamp).collect();
    for pair in timestamps.windows(2) {
        assert!(pair[0] <= pair[1], "timestamps must be non-decreasing: {timestamps:?}");
    }
}
