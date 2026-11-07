// Modified by enzUI contributors. See FORK.md.
//! Native diagnostic for distinct toplevel tags on two windows of one process.

#[cfg(all(target_os = "linux", feature = "wayland"))]
mod native {
    use std::collections::HashMap;
    use std::num::NonZeroU32;
    use std::sync::Arc;
    use winit::application::ApplicationHandler;
    use winit::dpi::LogicalSize;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, EventLoop};
    use winit::platform::wayland::{EventLoopBuilderExtWayland, WindowExtWayland};
    use winit::window::{Window, WindowId};

    struct PaintedWindow {
        window: Arc<Window>,
        surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
        color: u32,
    }

    #[derive(Default)]
    struct App {
        windows: HashMap<WindowId, PaintedWindow>,
    }

    impl ApplicationHandler for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if !self.windows.is_empty() {
                return;
            }
            let supported = std::env::var("ENZUI_EXPECT_TOPLEVEL_TAG").unwrap() == "1";
            for (title, tag, color) in [
                ("Tag origin A", "enzui-tag-a", 0x204060),
                ("Tag origin B", "enzui-tag-b", 0x604020),
            ] {
                let window = Arc::new(
                    event_loop
                        .create_window(
                            Window::default_attributes()
                                .with_title(title)
                                .with_inner_size(LogicalSize::new(480.0, 300.0)),
                        )
                        .unwrap(),
                );
                assert_eq!(window.set_toplevel_tag(tag), supported);
                assert!(!window.set_toplevel_tag("invalid tag\n"));
                let context = softbuffer::Context::new(window.clone()).unwrap();
                let surface = softbuffer::Surface::new(&context, window.clone()).unwrap();
                window.request_redraw();
                self.windows.insert(window.id(), PaintedWindow { window, surface, color });
                println!("queued {tag} pid={} supported={supported}", std::process::id());
            }
        }

        fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
            match event {
                WindowEvent::CloseRequested => event_loop.exit(),
                WindowEvent::RedrawRequested => {
                    let window = self.windows.get_mut(&id).unwrap();
                    let size = window.window.inner_size();
                    if let (Some(width), Some(height)) =
                        (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                    {
                        window.surface.resize(width, height).unwrap();
                        let mut buffer = window.surface.buffer_mut().unwrap();
                        buffer.fill(window.color);
                        buffer.present().unwrap();
                    }
                },
                _ => {},
            }
        }
    }

    pub fn run() {
        let event_loop = EventLoop::builder().with_wayland().build().unwrap();
        event_loop.run_app(&mut App::default()).unwrap();
    }
}

fn main() {
    #[cfg(all(target_os = "linux", feature = "wayland"))]
    native::run();
    #[cfg(not(all(target_os = "linux", feature = "wayland")))]
    {
        eprintln!("This diagnostic requires Linux and the Wayland feature");
        std::process::exit(2);
    }
}
