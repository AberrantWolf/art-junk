//! Adapter state-machine tests driven by the primitive (winit-free) API.
//! Mirrors the coverage in `winit_adapter.rs` through `on_cursor_moved` /
//! `on_mouse_button` / `on_touch` so the same behaviour is exercised with
//! the `winit` feature disabled.

use stylus_junk::{
    ButtonState, MouseButton, Phase, Point, PointerId, StylusAdapter, StylusEvent, ToolCaps,
    ToolKind, TouchEvent, TouchPhase,
};

fn drain(a: &mut StylusAdapter) -> Vec<StylusEvent> {
    a.drain().collect()
}

fn expect_sample(ev: &StylusEvent) -> (Phase, ToolCaps, PointerId, ToolKind, Point) {
    match ev {
        StylusEvent::Sample { sample, phase, caps } => {
            (*phase, *caps, sample.pointer_id, sample.tool, sample.position)
        }
        other => panic!("expected StylusEvent::Sample, got {other:?}"),
    }
}

#[test]
fn mouse_down_move_up_emits_three_samples() {
    let mut a = StylusAdapter::new();
    a.on_mouse_button(MouseButton::Left, ButtonState::Pressed, Point::new(1.0, 2.0));
    a.on_cursor_moved(Point::new(3.0, 4.0));
    a.on_mouse_button(MouseButton::Left, ButtonState::Released, Point::new(5.0, 6.0));

    let events = drain(&mut a);
    assert_eq!(events.len(), 3);
    let (phase, _caps, pid, tool, pos) = expect_sample(&events[0]);
    assert_eq!(phase, Phase::Down);
    assert_eq!(pid, PointerId::MOUSE);
    assert_eq!(tool, ToolKind::Mouse);
    assert_eq!(pos, Point::new(1.0, 2.0));
    assert_eq!(expect_sample(&events[1]).0, Phase::Move);
    assert_eq!(expect_sample(&events[2]).0, Phase::Up);
}

#[test]
fn cursor_move_without_down_emits_nothing() {
    let mut a = StylusAdapter::new();
    a.on_cursor_moved(Point::new(1.0, 1.0));
    a.on_cursor_moved(Point::new(2.0, 2.0));
    assert!(drain(&mut a).is_empty());
}

#[test]
fn mouse_button_other_than_left_is_ignored() {
    let mut a = StylusAdapter::new();
    a.on_mouse_button(MouseButton::Right, ButtonState::Pressed, Point::new(1.0, 1.0));
    a.on_mouse_button(MouseButton::Middle, ButtonState::Pressed, Point::new(1.0, 1.0));
    a.on_mouse_button(MouseButton::Other(5), ButtonState::Pressed, Point::new(1.0, 1.0));
    assert!(drain(&mut a).is_empty());
}

#[test]
fn duplicate_press_is_ignored() {
    let mut a = StylusAdapter::new();
    a.on_mouse_button(MouseButton::Left, ButtonState::Pressed, Point::new(0.0, 0.0));
    a.on_mouse_button(MouseButton::Left, ButtonState::Pressed, Point::new(0.0, 0.0));
    assert_eq!(drain(&mut a).len(), 1);
}

#[test]
fn touch_started_moved_ended_allocates_one_pointer_id() {
    let mut a = StylusAdapter::new();
    a.on_touch(TouchEvent {
        id: 42,
        phase: TouchPhase::Started,
        position: Point::new(1.0, 1.0),
        force: None,
    });
    a.on_touch(TouchEvent {
        id: 42,
        phase: TouchPhase::Moved,
        position: Point::new(2.0, 2.0),
        force: None,
    });
    a.on_touch(TouchEvent {
        id: 42,
        phase: TouchPhase::Ended,
        position: Point::new(3.0, 3.0),
        force: None,
    });

    let events = drain(&mut a);
    assert_eq!(events.len(), 3);
    let p0 = expect_sample(&events[0]);
    let p1 = expect_sample(&events[1]);
    let p2 = expect_sample(&events[2]);
    assert_eq!(p0.0, Phase::Down);
    assert_eq!(p1.0, Phase::Move);
    assert_eq!(p2.0, Phase::Up);
    assert_eq!(p0.2, p1.2);
    assert_eq!(p1.2, p2.2);
    assert_ne!(p0.2, PointerId::MOUSE);
    assert_eq!(p0.3, ToolKind::Finger);
}

#[test]
fn touch_with_force_populates_pressure_caps() {
    let mut a = StylusAdapter::new();
    a.on_touch(TouchEvent {
        id: 1,
        phase: TouchPhase::Started,
        position: Point::new(0.0, 0.0),
        force: Some(0.5),
    });
    let events = drain(&mut a);
    assert_eq!(events.len(), 1);
    let (_, caps, _, _, _) = expect_sample(&events[0]);
    assert!(caps.contains(ToolCaps::PRESSURE));
}

#[test]
fn touch_cancelled_clears_without_emitting_up() {
    let mut a = StylusAdapter::new();
    a.on_touch(TouchEvent {
        id: 7,
        phase: TouchPhase::Started,
        position: Point::new(0.0, 0.0),
        force: None,
    });
    a.on_touch(TouchEvent {
        id: 7,
        phase: TouchPhase::Cancelled,
        position: Point::new(0.0, 0.0),
        force: None,
    });
    let events = drain(&mut a);
    assert_eq!(events.len(), 2);
    assert_eq!(expect_sample(&events[0]).0, Phase::Down);
    assert_eq!(expect_sample(&events[1]).0, Phase::Cancel);
}

#[test]
fn is_tracking_pointer_reflects_active_state() {
    let mut a = StylusAdapter::new();
    assert!(!a.is_tracking_pointer());
    a.on_mouse_button(MouseButton::Left, ButtonState::Pressed, Point::new(0.0, 0.0));
    assert!(a.is_tracking_pointer());
    a.on_mouse_button(MouseButton::Left, ButtonState::Released, Point::new(0.0, 0.0));
    assert!(!a.is_tracking_pointer());
}

#[test]
fn last_cursor_position_is_updated_by_cursor_moved() {
    let mut a = StylusAdapter::new();
    assert_eq!(a.last_cursor_position(), None);
    a.on_cursor_moved(Point::new(9.0, 10.0));
    assert_eq!(a.last_cursor_position(), Some(Point::new(9.0, 10.0)));
}
