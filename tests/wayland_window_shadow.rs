// Modified by enzUI contributors. See FORK.md.
//! Window shadows survive fullscreen and theme changes without replacing live surfaces.
#![cfg(all(feature = "wayland", target_os = "linux"))]

#[path = "common/wayland_compositor.rs"]
mod compositor;
use compositor::{run, Seen};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::platform::wayland::{
    ShadowLayer, WindowAttributesExtWayland, WindowExtWayland, WindowShadow,
};
use winit::window::{Fullscreen, Window, WindowId};

fn shadow() -> WindowShadow {
    let layer =
        ShadowLayer { color: [0.0, 0.0, 0.0, 0.25], offset: (0.0, 2.0), blur: 8.0, spread: 0.0 };
    WindowShadow {
        corner_radius: 12.0,
        active: vec![layer],
        inactive: vec![layer],
        resize_border: 8.0,
    }
}

struct App {
    window: Option<Window>,
    phase: u8,
    deadline: Instant,
}
impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.window = Some(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_decorations(false)
                        .with_window_shadow(Some(shadow())),
                )
                .expect("a self-framed window"),
        );
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let window = self.window.as_ref().expect("window");
        if id != window.id() {
            return;
        }
        match (self.phase, event) {
            (0, WindowEvent::MouseInput { state: ElementState::Pressed, .. }) => {
                self.phase = 1;
                window.set_fullscreen(Some(Fullscreen::Borderless(None)));
            },
            (1, WindowEvent::Resized(_)) if window.fullscreen().is_some() => {
                self.phase = 2;
                window.set_fullscreen(None);
            },
            (2, WindowEvent::Resized(_)) if window.fullscreen().is_none() => {
                window.set_window_shadow(None);
                window.set_window_shadow(Some(shadow()));
                self.phase = 3;
                event_loop.exit();
            },
            _ => {},
        }
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        assert!(
            Instant::now() < self.deadline,
            "shadow lifecycle did not complete, phase {}",
            self.phase
        );
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.deadline));
    }
}

#[test]
fn shadows_unmap_for_fullscreen_and_theme_changes_then_reuse_their_surfaces() {
    let seen = run(
        "shadows",
        App { window: None, phase: 0, deadline: Instant::now() + Duration::from_secs(5) },
    )
    .seen;
    let entering =
        seen.iter().position(|event| *event == Seen::Fullscreen(true)).expect("fullscreen request");
    let mut surfaces = Vec::new();
    for event in &seen[..entering] {
        if let Seen::SubsurfaceCommit(id, true) = event {
            if !surfaces.contains(id) {
                surfaces.push(*id);
            }
        }
    }
    assert_eq!(surfaces.len(), 4, "four painted shadow strips before fullscreen: {seen:?}");
    let recreated: Vec<_> = seen[entering..]
        .iter()
        .filter_map(|event| if let Seen::SubsurfaceCreate(id) = event { Some(*id) } else { None })
        .collect();
    assert!(recreated.is_empty(), "state changes retain the shadow roles: {recreated:?}");
    let leaving =
        seen.iter().position(|event| *event == Seen::Fullscreen(false)).expect("restore request");
    assert!(
        !seen[entering..leaving]
            .iter()
            .any(|event| { matches!(event, Seen::SubsurfaceDestroy(id) if surfaces.contains(id)) }),
        "fullscreen does not destroy live shadow surfaces: {seen:?}"
    );
    assert!(
        seen.contains(&Seen::Fullscreen(true)) && seen.contains(&Seen::Fullscreen(false)),
        "fullscreen roundtrip: {seen:?}"
    );
    for id in surfaces {
        let mut states = Vec::new();
        for event in &seen {
            if let Seen::SubsurfaceCommit(surface, mapped) = event {
                if *surface == id && states.last() != Some(mapped) {
                    states.push(*mapped);
                }
            }
        }
        assert_eq!(
            states,
            [true, false, true, false, true],
            "strip {id} maps, hides for fullscreen, restores, hides for theme, restores: {seen:?}"
        );
    }
}
