// Modified by enzUI contributors. See FORK.md.
//! In-process Wayland protocol recorder shared by native window tests.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wayland_protocols::xdg::shell::server::xdg_popup::{self, XdgPopup};
use wayland_protocols::xdg::shell::server::xdg_positioner::{self, XdgPositioner};
use wayland_protocols::xdg::shell::server::xdg_surface::{self, XdgSurface};
use wayland_protocols::xdg::shell::server::xdg_toplevel::{self, XdgToplevel};
use wayland_protocols::xdg::shell::server::xdg_wm_base::{self, XdgWmBase};
use wayland_server::backend::{ClientData, ClientId, DisconnectReason, ObjectId};
use wayland_server::protocol::wl_buffer::WlBuffer;
use wayland_server::protocol::wl_compositor::{self, WlCompositor};
use wayland_server::protocol::wl_pointer::{self, WlPointer};
use wayland_server::protocol::wl_region::WlRegion;
use wayland_server::protocol::wl_seat::{self, WlSeat};
use wayland_server::protocol::wl_shm::{self, WlShm};
use wayland_server::protocol::wl_shm_pool::{self, WlShmPool};
use wayland_server::protocol::wl_subcompositor::{self, WlSubcompositor};
use wayland_server::protocol::wl_subsurface::WlSubsurface;
use wayland_server::protocol::wl_surface::{self, WlSurface};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, ListeningSocket, New,
    Resource,
};

use winit::application::ApplicationHandler;
use winit::event_loop::EventLoop;
use winit::platform::wayland::EventLoopBuilderExtWayland;

/// What the compositor saw, in order. Popups are named by their order of
/// creation.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Seen {
    GetPopup(usize),
    Grab(usize, u32),
    Destroy(usize),
    SubsurfaceCreate(u32),
    SubsurfaceCommit(u32, bool),
    SubsurfaceDestroy(u32),
    Fullscreen(bool),
}

#[derive(Clone, Default)]
pub(super) struct Server {
    subsurfaces: HashMap<u32, bool>,
    pub(super) seen: Vec<Seen>,
    popups: Vec<ObjectId>,
    pointer: Option<WlPointer>,
    serial: u32,
    /// The serial of the press sent to the toplevel.
    pub(super) press: Option<u32>,
    /// The toplevel's latest window geometry origin.
    pub(super) geometry: (i32, i32),
    /// The toplevel's `xdg_surface`.
    toplevel: Option<ObjectId>,
    /// The anchor rectangle of every positioner, in creation order.
    pub(super) anchors: Vec<(i32, i32)>,
}

impl Server {
    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }

    fn popup_index(&self, popup: &ObjectId) -> usize {
        self.popups.iter().position(|id| id == popup).expect("a known popup")
    }
}

struct NoData;
impl ClientData for NoData {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}

macro_rules! global {
    ($interface:ty) => {
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
global!(WlCompositor);
global!(XdgWmBase);
global!(WlSubcompositor);

impl GlobalDispatch<WlShm, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WlShm>,
        (): &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let shm = data_init.init(resource, ());
        shm.format(wl_shm::Format::Argb8888);
        shm.format(wl_shm::Format::Xrgb8888);
    }
}

impl GlobalDispatch<WlSeat, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WlSeat>,
        (): &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let seat = data_init.init(resource, ());
        seat.capabilities(wl_seat::Capability::Pointer);
        if seat.version() >= 2 {
            seat.name("seat0".to_owned());
        }
    }
}

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
impl Dispatch<XdgToplevel, XdgSurface> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        toplevel: &XdgToplevel,
        request: xdg_toplevel::Request,
        surface: &XdgSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let fullscreen = match request {
            xdg_toplevel::Request::SetFullscreen { .. } => true,
            xdg_toplevel::Request::UnsetFullscreen => false,
            _ => return,
        };
        state.seen.push(Seen::Fullscreen(fullscreen));
        let states = if fullscreen {
            (xdg_toplevel::State::Fullscreen as u32).to_ne_bytes().to_vec()
        } else {
            Vec::new()
        };
        let (width, height) = if fullscreen { (1280, 900) } else { (640, 480) };
        toplevel.configure(width, height, states);
        surface.configure(state.next_serial());
    }
}
inert!(WlPointer, ());
inert!(WlSubsurface, ());

/// Subsurfaces for client-drawn frames and shadows.
impl Dispatch<WlSubcompositor, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlSubcompositor,
        request: wl_subcompositor::Request,
        (): &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_subcompositor::Request::GetSubsurface { id, surface, .. } = request {
            let surface_id = surface.id().protocol_id();
            state.subsurfaces.insert(surface_id, false);
            state.seen.push(Seen::SubsurfaceCreate(surface_id));
            data_init.init(id, ());
        }
    }
}

impl Dispatch<WlSeat, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlSeat,
        request: wl_seat::Request,
        (): &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_seat::Request::GetPointer { id } = request {
            state.pointer = Some(data_init.init(id, ()));
        }
    }
}

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

/// A surface's role objects once it has them, to configure it on its first
/// commit.
enum Role {
    Toplevel(XdgSurface, XdgToplevel),
    Popup(XdgSurface, XdgPopup),
}
type SurfaceRole = Mutex<Option<Role>>;

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
        let surface_id = surface.id().protocol_id();
        if let Some(mapped) = state.subsurfaces.get_mut(&surface_id) {
            match &request {
                wl_surface::Request::Attach { buffer, .. } => *mapped = buffer.is_some(),
                wl_surface::Request::Commit => {
                    state.seen.push(Seen::SubsurfaceCommit(surface_id, *mapped))
                },
                wl_surface::Request::Destroy => {
                    state.seen.push(Seen::SubsurfaceDestroy(surface_id))
                },
                _ => {},
            }
        }
        if !matches!(request, wl_surface::Request::Commit) {
            return;
        }
        let Some(role) = role.lock().unwrap().take() else {
            return;
        };
        let serial = state.next_serial();
        match role {
            Role::Toplevel(xdg_surface, toplevel) => {
                toplevel.configure(640, 480, Vec::new());
                xdg_surface.configure(serial);
                // The user presses on the window: the serial a menu grabs with.
                if let Some(pointer) = state.pointer.clone() {
                    let enter = state.next_serial();
                    pointer.enter(enter, surface, 10.0, 10.0);
                    let press = state.next_serial();
                    pointer.button(press, 0, 0x110, wl_pointer::ButtonState::Pressed);
                    if pointer.version() >= 5 {
                        pointer.frame();
                    }
                    state.press = Some(press);
                }
            },
            Role::Popup(xdg_surface, popup) => {
                popup.configure(0, 0, 100, 100);
                xdg_surface.configure(serial);
            },
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
        match request {
            xdg_wm_base::Request::GetXdgSurface { id, surface } => {
                data_init.init(id, surface);
            },
            xdg_wm_base::Request::CreatePositioner { id } => {
                data_init.init(id, ());
            },
            _ => {},
        }
    }
}

impl Dispatch<XdgSurface, WlSurface> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        xdg_surface: &XdgSurface,
        request: xdg_surface::Request,
        surface: &WlSurface,
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let role = match request {
            xdg_surface::Request::SetWindowGeometry { x, y, .. } => {
                if state.toplevel.as_ref() == Some(&xdg_surface.id()) {
                    state.geometry = (x, y);
                }
                return;
            },
            xdg_surface::Request::GetToplevel { id } => {
                state.toplevel = Some(xdg_surface.id());
                Role::Toplevel(xdg_surface.clone(), data_init.init(id, xdg_surface.clone()))
            },
            xdg_surface::Request::GetPopup { id, .. } => {
                let popup = data_init.init(id, ());
                state.popups.push(popup.id());
                state.seen.push(Seen::GetPopup(state.popups.len() - 1));
                Role::Popup(xdg_surface.clone(), popup)
            },
            _ => return,
        };
        if let Some(slot) = surface.data::<SurfaceRole>() {
            *slot.lock().unwrap() = Some(role);
        }
    }
}

impl Dispatch<XdgPositioner, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &XdgPositioner,
        request: xdg_positioner::Request,
        (): &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let xdg_positioner::Request::SetAnchorRect { x, y, .. } = request {
            state.anchors.push((x, y));
        }
    }
}

impl Dispatch<XdgPopup, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        popup: &XdgPopup,
        request: xdg_popup::Request,
        (): &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let index = state.popup_index(&popup.id());
        match request {
            xdg_popup::Request::Grab { serial, .. } => state.seen.push(Seen::Grab(index, serial)),
            xdg_popup::Request::Destroy => state.seen.push(Seen::Destroy(index)),
            _ => {},
        }
    }
}

pub(super) fn run(name: &str, mut app: impl ApplicationHandler) -> Server {
    let directory =
        std::env::temp_dir().join(format!("winit-popup-grab-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let socket_path = directory.join("wayland-popup-test");
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
            handle.create_global::<Server, XdgWmBase, ()>(3, ());
            handle.create_global::<Server, WlSeat, ()>(7, ());
            handle.create_global::<Server, WlSubcompositor, ()>(1, ());
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

    std::env::set_var("WAYLAND_DISPLAY", &socket_path);
    std::env::set_var("XDG_RUNTIME_DIR", &directory);
    let event_loop = EventLoop::builder().with_wayland().with_any_thread(true).build().unwrap();
    event_loop.run_app(&mut app).unwrap();
    drop(app);
    // Let the compositor read what the client sent last.
    std::thread::sleep(Duration::from_millis(50));
    stop.store(true, Ordering::SeqCst);
    compositor.join().unwrap();
    let _ = std::fs::remove_dir_all(&directory);

    let server = server.lock().unwrap();
    server.clone()
}
