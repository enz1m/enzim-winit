// Modified by enzUI contributors. See FORK.md.
//! Popup grab serials and replacement order against a live protocol recorder.
#![cfg(all(feature = "wayland", target_os = "linux"))]

#[path = "common/wayland_compositor.rs"]
mod compositor;
use compositor::{run, Seen};
use std::time::Duration;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::platform::wayland::{
    PopupAttributes, PopupConstraint, PopupEdge, WindowAttributesExtWayland,
};
use winit::window::{Window, WindowId};

fn popup(parent: WindowId, grab: bool) -> winit::window::WindowAttributes {
    Window::default_attributes().with_popup(PopupAttributes {
        parent,
        anchor_rect: (10, 10, 1, 1),
        size: (100, 100),
        anchor: PopupEdge::TopLeft,
        gravity: PopupEdge::BottomRight,
        offset: (0, 0),
        constraint_adjustment: PopupConstraint::SLIDE_X,
        grab,
    })
}

/// What the client does once the press arrives.
type Script = fn(&ActiveEventLoop, WindowId);

struct App {
    toplevel: Option<Window>,
    script: Script,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.toplevel.is_none() {
            let window = event_loop.create_window(Window::default_attributes()).expect("a window");
            self.toplevel = Some(window);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let WindowEvent::MouseInput { state: ElementState::Pressed, .. } = event {
            (self.script)(event_loop, id);
            event_loop.exit();
        }
    }
}

/// One event loop per process: every case runs in this one sequence.
#[test]
fn popups_grab_only_on_top_after_a_recent_press_and_replace_in_order() {
    let snapshot = run(
        "replace",
        App {
            toplevel: None,
            script: |event_loop, parent| {
                // A menu opened by the press grabs.
                let menu = event_loop.create_window(popup(parent, true)).unwrap();
                // A second grabbing popup beside it would not be topmost: no grab.
                let sibling = event_loop.create_window(popup(parent, true)).unwrap();
                drop(sibling);
                // The menu moves: the replacement comes after the old one is gone.
                drop(menu);
                let moved = event_loop.create_window(popup(parent, true)).unwrap();
                // A tooltip replaces it, without a grab.
                drop(moved);
                let tooltip = event_loop.create_window(popup(parent, false)).unwrap();
                drop(tooltip);
                // A popup asking to grab long after the press does not.
                std::thread::sleep(Duration::from_millis(1200));
                let late = event_loop.create_window(popup(parent, true)).unwrap();
                std::mem::forget(late);
            },
        },
    );
    let (seen, press, geometry, anchors) = (
        snapshot.seen,
        snapshot.press.expect("the press was sent"),
        snapshot.geometry,
        snapshot.anchors,
    );
    let position = |entry: &Seen| {
        seen.iter().position(|seen| seen == entry).unwrap_or_else(|| panic!("{entry:?}: {seen:?}"))
    };
    let grabbed =
        |index: usize| seen.iter().any(|seen| matches!(seen, Seen::Grab(i, _) if *i == index));
    assert!(seen.contains(&Seen::Grab(0, press)), "the menu grabs: {seen:?}");
    assert!(!grabbed(1), "no grab beside a grabbing popup: {seen:?}");
    assert!(position(&Seen::Destroy(1)) < position(&Seen::GetPopup(2)), "{seen:?}");
    assert!(position(&Seen::Destroy(0)) < position(&Seen::GetPopup(2)), "{seen:?}");
    assert!(seen.contains(&Seen::Grab(2, press)), "the replacement grabs: {seen:?}");
    assert!(position(&Seen::Destroy(2)) < position(&Seen::GetPopup(3)), "{seen:?}");
    assert!(!grabbed(3), "a tooltip does not grab: {seen:?}");
    assert!(seen.contains(&Seen::GetPopup(4)), "{seen:?}");
    assert!(!grabbed(4), "a stale press opens no grab: {seen:?}");
    // Positioner coordinates are the parent's window geometry, which starts
    // at the frame winit draws: the surface anchor (10, 10) moves by it.
    assert!(geometry.1 < 0, "winit draws a titlebar here: {geometry:?}");
    assert_eq!(anchors[0], (10 - geometry.0, 10 - geometry.1), "{anchors:?}");
    for index in 0..4 {
        let destroys = seen.iter().filter(|seen| **seen == Seen::Destroy(index)).count();
        assert_eq!(destroys, 1, "popup {index} is destroyed once: {seen:?}");
    }
}
