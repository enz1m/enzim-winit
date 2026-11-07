// Modified by enzUI contributors. See FORK.md.
//! Window placement restore through `xdg_session_manager_v1`, against an
//! in-process compositor that configures every toplevel and records what the
//! client asks of the session and when each surface first commits.
#![cfg(all(feature = "wayland", target_os = "linux"))]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wayland_protocols::xdg::shell::server::xdg_surface::{self, XdgSurface};
use wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel;
use wayland_protocols::xdg::shell::server::xdg_wm_base::{self, XdgWmBase};
use wayland_server::backend::{ClientData, ClientId, DisconnectReason, ObjectId};
use wayland_server::protocol::wl_buffer::WlBuffer;
use wayland_server::protocol::wl_compositor::{self, WlCompositor};
use wayland_server::protocol::wl_region::WlRegion;
use wayland_server::protocol::wl_shm::{self, WlShm};
use wayland_server::protocol::wl_shm_pool::{self, WlShmPool};
use wayland_server::protocol::wl_surface::{self, WlSurface};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, ListeningSocket, New,
    Resource,
};

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::wayland::{
    EventLoopBuilderExtWayland, WindowAttributesExtWayland, WindowExtWayland,
};
use winit::window::{Window, WindowId};

#[allow(dead_code, non_camel_case_types, non_upper_case_globals, unused_imports)]
#[allow(clippy::all)]
mod session {
    use wayland_protocols::xdg::shell::server::*;
    use wayland_server;
    use wayland_server::protocol::*;
    pub mod __interfaces {
        use wayland_protocols::xdg::shell::server::__interfaces::*;
        use wayland_server::backend as wayland_backend;
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("wayland-protocols/xdg-session-management-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_server_code!("wayland-protocols/xdg-session-management-v1.xml");
}
use session::xdg_session_manager_v1::{self, XdgSessionManagerV1};
use session::xdg_session_v1::{self, XdgSessionV1};
use session::xdg_toplevel_session_v1::XdgToplevelSessionV1;

/// What the compositor saw, in order.
#[derive(Debug, Clone, PartialEq)]
enum Seen {
    GetSession(u32, Option<String>),
    Restore(String, ObjectId),
    FirstCommit(ObjectId),
}

#[derive(Default)]
struct Server {
    seen: Vec<Seen>,
    committed: HashMap<ObjectId, ()>,
    serial: u32,
}

struct NoData;
impl ClientData for NoData {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}

macro_rules! global {
    ($interface:ty, $version:expr) => {
        impl GlobalDispatch<$interface, ()> for Server {
            fn bind(
                _: &mut Self,
                _: &DisplayHandle,
                _: &Client,
                resource: New<$interface>,
                (): &(),
                data_init: &mut DataInit<'_, Self>,
            ) {
                data_init.init(resource, ());
            }
        }
    };
}
global!(WlCompositor, 4);
global!(WlShm, 1);
global!(XdgWmBase, 1);
global!(XdgSessionManagerV1, 1);

macro_rules! inert {
    ($interface:ty, $data:ty) => {
        impl Dispatch<$interface, $data> for Server {
            fn request(
                _: &mut Self,
                _: &Client,
                _: &$interface,
                _: <$interface as Resource>::Request,
                _: &$data,
                _: &DisplayHandle,
                _: &mut DataInit<'_, Self>,
            ) {
            }
        }
    };
}
inert!(WlRegion, ());
inert!(WlBuffer, ());
inert!(XdgToplevel, ObjectId);
inert!(XdgToplevelSessionV1, ());

impl Dispatch<WlCompositor, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlCompositor,
        request: wl_compositor::Request,
        (): &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                data_init.init(id, Mutex::new(None));
            },
            wl_compositor::Request::CreateRegion { id } => {
                data_init.init(id, ());
            },
            _ => {},
        }
    }
}

/// A surface's `xdg_surface`, once it has one, to configure it.
type SurfaceRole = Mutex<Option<(XdgSurface, XdgToplevel)>>;

impl Dispatch<WlSurface, SurfaceRole> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        surface: &WlSurface,
        request: wl_surface::Request,
        role: &SurfaceRole,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let wl_surface::Request::Commit = request {
            if state.committed.insert(surface.id(), ()).is_none() {
                state.seen.push(Seen::FirstCommit(surface.id()));
                if let Some((xdg_surface, toplevel)) = role.lock().unwrap().as_ref() {
                    state.serial += 1;
                    toplevel.configure(640, 480, Vec::new());
                    xdg_surface.configure(state.serial);
                }
            }
        }
    }
}

impl Dispatch<WlShm, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlShm,
        request: wl_shm::Request,
        (): &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm::Request::CreatePool { id, .. } = request {
            data_init.init(id, ());
        }
    }
}

impl Dispatch<WlShmPool, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlShmPool,
        request: wl_shm_pool::Request,
        (): &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm_pool::Request::CreateBuffer { id, .. } = request {
            data_init.init(id, ());
        }
    }
}

impl Dispatch<XdgWmBase, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &XdgWmBase,
        request: xdg_wm_base::Request,
        (): &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let xdg_wm_base::Request::GetXdgSurface { id, surface } = request {
            data_init.init(id, surface);
        }
    }
}

impl Dispatch<XdgSurface, WlSurface> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        xdg_surface: &XdgSurface,
        request: xdg_surface::Request,
        surface: &WlSurface,
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let xdg_surface::Request::GetToplevel { id } = request {
            let toplevel = data_init.init(id, surface.id());
            if let Some(role) = surface.data::<SurfaceRole>() {
                *role.lock().unwrap() = Some((xdg_surface.clone(), toplevel));
            }
        }
    }
}

impl Dispatch<XdgSessionManagerV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &XdgSessionManagerV1,
        request: xdg_session_manager_v1::Request,
        (): &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let xdg_session_manager_v1::Request::GetSession { id, reason, session_id } = request {
            let reason = match reason {
                wayland_server::WEnum::Value(reason) => reason as u32,
                wayland_server::WEnum::Unknown(reason) => reason,
            };
            state.seen.push(Seen::GetSession(reason, session_id));
            // This compositor knows no earlier session: it makes a new one.
            data_init.init(id, ()).created("fresh-session".to_owned());
        }
    }
}

impl Dispatch<XdgSessionV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &XdgSessionV1,
        request: xdg_session_v1::Request,
        (): &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_session_v1::Request::RestoreToplevel { id, toplevel, name }
            | xdg_session_v1::Request::AddToplevel { id, toplevel, name } => {
                let surface = toplevel.data::<ObjectId>().cloned().expect("a toplevel");
                state.seen.push(Seen::Restore(name, surface));
                data_init.init(id, ());
            },
            _ => {},
        }
    }
}

struct App {
    windows: Vec<Window>,
    session_ids: Vec<Option<String>>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if !self.windows.is_empty() {
            return;
        }
        for name in ["main", "tools", "main"] {
            let attributes = Window::default_attributes()
                .with_title(name)
                .with_session(Some("earlier-session".to_owned()), name);
            let window = event_loop.create_window(attributes).expect("a window");
            self.session_ids.push(window.wayland_session_id());
            self.windows.push(window);
        }
        event_loop.exit();
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

#[test]
fn windows_restore_their_placement_before_their_first_commit() {
    let directory = std::env::temp_dir().join(format!("winit-session-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let socket_path = directory.join("wayland-session-test");
    let _ = std::fs::remove_file(&socket_path);
    let socket = ListeningSocket::bind_absolute(socket_path.clone()).expect("a socket");

    let server = Arc::new(Mutex::new(Server::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let compositor = {
        let server = Arc::clone(&server);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut display: Display<Server> = Display::new().unwrap();
            let mut handle = display.handle();
            handle.create_global::<Server, WlCompositor, ()>(4, ());
            handle.create_global::<Server, WlShm, ()>(1, ());
            handle.create_global::<Server, XdgWmBase, ()>(1, ());
            handle.create_global::<Server, XdgSessionManagerV1, ()>(1, ());
            while !stop.load(Ordering::SeqCst) {
                if let Ok(Some(stream)) = socket.accept() {
                    handle.insert_client(stream, Arc::new(NoData)).unwrap();
                }
                {
                    let mut state = server.lock().unwrap();
                    let _ = display.dispatch_clients(&mut state);
                }
                let _ = display.flush_clients();
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };

    // The client finds the compositor through the environment.
    std::env::set_var("WAYLAND_DISPLAY", &socket_path);
    std::env::set_var("XDG_RUNTIME_DIR", &directory);
    let event_loop = EventLoop::builder().with_wayland().with_any_thread(true).build().unwrap();
    let mut app = App { windows: Vec::new(), session_ids: Vec::new() };
    event_loop.run_app(&mut app).unwrap();

    assert_eq!(
        app.session_ids,
        [Some("fresh-session".to_owned()), Some("fresh-session".to_owned()), None],
        "the compositor's new id is reported; the duplicate name has no session"
    );
    drop(app);
    stop.store(true, Ordering::SeqCst);
    compositor.join().unwrap();

    let seen = server.lock().unwrap().seen.clone();
    let sessions: Vec<_> =
        seen.iter().filter(|seen| matches!(seen, Seen::GetSession(..))).collect();
    assert_eq!(
        sessions,
        [&Seen::GetSession(1, Some("earlier-session".to_owned()))],
        "one session for the process, opened for a launch with the stored id"
    );
    let restored: Vec<(String, usize, usize)> = seen
        .iter()
        .enumerate()
        .filter_map(|(at, entry)| match entry {
            Seen::Restore(name, surface) => Some((name.clone(), at, first_commit(&seen, surface))),
            _ => None,
        })
        .collect();
    assert_eq!(
        restored.iter().map(|(name, ..)| name.as_str()).collect::<Vec<_>>(),
        ["main", "tools"]
    );
    for (name, restore, commit) in restored {
        assert!(restore < commit, "{name} was restored after its first commit: {seen:?}");
    }
    let _ = std::fs::remove_dir_all(&directory);
}

fn first_commit(seen: &[Seen], surface: &ObjectId) -> usize {
    seen.iter()
        .position(|seen| *seen == Seen::FirstCommit(surface.clone()))
        .expect("the surface committed")
}
