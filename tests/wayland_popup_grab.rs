// Modified by enzUI contributors. See FORK.md.
//! Popup grabs and replacement order, against an in-process compositor that
//! records the client's popup requests in order. A grab is only allowed from
//! the topmost popup, so a replacement must not be created while the popup
//! it replaces still exists, and a grab needs a recent press.
#![cfg(all(feature = "wayland", target_os = "linux"))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wayland_protocols::xdg::shell::server::xdg_popup::{self, XdgPopup};
use wayland_protocols::xdg::shell::server::xdg_positioner::XdgPositioner;
use wayland_protocols::xdg::shell::server::xdg_surface::{self, XdgSurface};
use wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel;
use wayland_protocols::xdg::shell::server::xdg_wm_base::{self, XdgWmBase};
use wayland_server::backend::{ClientData, ClientId, DisconnectReason, ObjectId};
use wayland_server::protocol::wl_buffer::WlBuffer;
use wayland_server::protocol::wl_compositor::{self, WlCompositor};
use wayland_server::protocol::wl_pointer::{self, WlPointer};
use wayland_server::protocol::wl_region::WlRegion;
use wayland_server::protocol::wl_seat::{self, WlSeat};
use wayland_server::protocol::wl_shm::{self, WlShm};
use wayland_server::protocol::wl_shm_pool::{self, WlShmPool};
use wayland_server::protocol::wl_surface::{self, WlSurface};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, ListeningSocket, New,
    Resource,
};

use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::wayland::{
    EventLoopBuilderExtWayland, PopupAttributes, PopupConstraint, PopupEdge,
    WindowAttributesExtWayland,
};
use winit::window::{Window, WindowId};

/// What the compositor saw, in order. Popups are named by their order of
/// creation.
#[derive(Debug, Clone, PartialEq)]
enum Seen {
    GetPopup(usize),
    Grab(usize, u32),
    Destroy(usize),
}

#[derive(Default)]
struct Server {
    seen: Vec<Seen>,
    popups: Vec<ObjectId>,
    pointer: Option<WlPointer>,
    serial: u32,
    /// The serial of the press sent to the toplevel.
    press: Option<u32>,
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
global!(WlShm);
global!(XdgWmBase);

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
inert!(XdgToplevel, ());
inert!(XdgPositioner, ());
inert!(WlPointer, ());

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
            xdg_surface::Request::GetToplevel { id } => {
                Role::Toplevel(xdg_surface.clone(), data_init.init(id, ()))
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

/// Runs `script` on the toplevel's press and returns what the compositor saw
/// and the press serial.
fn run(name: &str, script: Script) -> (Vec<Seen>, u32) {
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
    let mut app = App { toplevel: None, script };
    event_loop.run_app(&mut app).unwrap();
    drop(app);
    // Let the compositor read what the client sent last.
    std::thread::sleep(Duration::from_millis(50));
    stop.store(true, Ordering::SeqCst);
    compositor.join().unwrap();
    let _ = std::fs::remove_dir_all(&directory);

    let server = server.lock().unwrap();
    (server.seen.clone(), server.press.expect("the press was sent"))
}

/// One event loop per process: every case runs in this one sequence.
#[test]
fn popups_grab_only_on_top_after_a_recent_press_and_replace_in_order() {
    let (seen, press) = run("replace", |event_loop, parent| {
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
    });
    let position = |entry: &Seen| {
        seen.iter().position(|seen| seen == entry).unwrap_or_else(|| panic!("{entry:?}: {seen:?}"))
    };
    let grabbed = |index: usize| seen.iter().any(|seen| matches!(seen, Seen::Grab(i, _) if *i == index));
    assert!(seen.contains(&Seen::Grab(0, press)), "the menu grabs: {seen:?}");
    assert!(!grabbed(1), "no grab beside a grabbing popup: {seen:?}");
    assert!(position(&Seen::Destroy(1)) < position(&Seen::GetPopup(2)), "{seen:?}");
    assert!(position(&Seen::Destroy(0)) < position(&Seen::GetPopup(2)), "{seen:?}");
    assert!(seen.contains(&Seen::Grab(2, press)), "the replacement grabs: {seen:?}");
    assert!(position(&Seen::Destroy(2)) < position(&Seen::GetPopup(3)), "{seen:?}");
    assert!(!grabbed(3), "a tooltip does not grab: {seen:?}");
    assert!(seen.contains(&Seen::GetPopup(4)), "{seen:?}");
    assert!(!grabbed(4), "a stale press opens no grab: {seen:?}");
    for index in 0..4 {
        let destroys = seen.iter().filter(|seen| **seen == Seen::Destroy(index)).count();
        assert_eq!(destroys, 1, "popup {index} is destroyed once: {seen:?}");
    }
}
