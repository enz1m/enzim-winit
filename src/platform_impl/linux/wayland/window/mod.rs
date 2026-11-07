// Modified by enzUI contributors. See FORK.md.
//! The Wayland window.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sctk::reexports::client::protocol::wl_display::WlDisplay;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Proxy, QueueHandle};

use sctk::compositor::{CompositorState, Region, SurfaceData};
use sctk::reexports::protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;
use sctk::reexports::protocols::xdg::shell::client::xdg_positioner;
use sctk::shell::xdg::popup::Popup;
use sctk::shell::xdg::window::WindowDecorations;
use sctk::shell::xdg::{XdgPositioner, XdgShell};

use tracing::warn;

use crate::dpi::{LogicalSize, PhysicalPosition, PhysicalSize, Position, Size};
use crate::error::{ExternalError, NotSupportedError, OsError as RootOsError};
use crate::event::{Ime, WindowEvent};
use crate::event_loop::AsyncRequestSerial;
use crate::platform_impl::{
    Fullscreen, MonitorHandle as PlatformMonitorHandle, OsError, PlatformIcon,
};
use crate::window::{
    Cursor, CursorGrabMode, ImePurpose, ResizeDirection, Theme, UserAttentionType,
    WindowAttributes, WindowButtons, WindowLevel,
};

use super::event_loop::sink::EventSink;
use super::output::MonitorHandle;
use super::state::WinitState;
use super::types::xdg_activation::{PendingActivationRequests, XdgActivationTokenData};
use super::types::xdg_toplevel_tag::ToplevelTagManager;
use super::{ActiveEventLoop, WaylandError, WindowId};

pub(crate) mod state;

pub use state::{ShellSurface, WindowState};

const ACTIVATION_SERIAL_LIFETIME: Duration = Duration::from_secs(1);

/// The Wayland window.
pub struct Window {
    /// Reference to the underlying SCTK toplevel or popup.
    window: ShellSurface,

    /// Window id.
    window_id: WindowId,

    /// The state of the window.
    window_state: Arc<Mutex<WindowState>>,

    /// Compositor to handle WlRegion stuff.
    compositor: Arc<CompositorState>,

    /// The wayland display used solely for raw window handle.
    #[allow(dead_code)]
    display: WlDisplay,

    /// Xdg activation to request user attention.
    xdg_activation: Option<XdgActivationV1>,
    toplevel_tag_manager: Option<ToplevelTagManager>,

    /// The state of the requested attention from the `xdg_activation`.
    attention_requested: Arc<AtomicBool>,

    activation_requests: Arc<PendingActivationRequests>,

    /// Handle to the main queue to perform requests.
    queue_handle: QueueHandle<WinitState>,

    /// Window requests to the event loop.
    window_requests: Arc<WindowRequests>,

    /// Observed monitors.
    monitors: Arc<Mutex<Vec<MonitorHandle>>>,

    /// Source to wake-up the event-loop for window requests.
    event_loop_awakener: calloop::ping::Ping,

    /// The event sink to deliver synthetic events.
    window_events_sink: Arc<Mutex<EventSink>>,
}

impl Window {
    pub(crate) fn new(
        event_loop_window_target: &ActiveEventLoop,
        attributes: WindowAttributes,
    ) -> Result<Self, RootOsError> {
        let queue_handle = event_loop_window_target.queue_handle.clone();
        let mut state = event_loop_window_target.state.borrow_mut();

        let monitors = state.monitors.clone();

        let surface = state.compositor_state.create_surface(&queue_handle);
        let compositor = state.compositor_state.clone();
        let xdg_activation =
            state.xdg_activation.as_ref().map(|activation_state| activation_state.global().clone());
        let display = event_loop_window_target.connection.display();
        let toplevel_tag_manager = state.toplevel_tag_manager.clone();

        let size: Size = attributes.inner_size.unwrap_or(LogicalSize::new(800., 600.).into());

        // We prefer server side decorations, however to not have decorations we ask for client
        // side decorations instead.
        let default_decorations = if attributes.decorations {
            WindowDecorations::RequestServer
        } else {
            WindowDecorations::RequestClient
        };

        let popup = attributes.platform_specific.popup;
        let window = match popup {
            None => ShellSurface::Toplevel(state.xdg_shell.create_window(
                surface.clone(),
                default_decorations,
                &queue_handle,
            )),
            Some(popup) => {
                let parent = state.windows.get_mut().get(&popup.parent.0).map(|parent| {
                    let parent = parent.lock().unwrap();
                    let grab = popup.grab.then(|| parent_serial(&parent)).flatten();
                    (parent.window.xdg_surface().clone(), grab)
                });
                let Some((parent, grab)) = parent else {
                    return Err(os_error!(OsError::Misc(
                        "the popup's parent window does not exist"
                    )));
                };
                let positioner = positioner(&state.xdg_shell, &popup)
                    .map_err(|_| os_error!(OsError::Misc("the compositor has no xdg_wm_base")))?;
                let popup = Popup::from_surface(
                    Some(&parent),
                    &positioner,
                    &queue_handle,
                    surface.clone(),
                    &state.xdg_shell,
                )
                .map_err(|_| os_error!(OsError::Misc("the compositor has no xdg_wm_base")))?;
                if let Some((serial, seat)) = grab {
                    popup.xdg_popup().grab(&seat, serial);
                }
                ShellSurface::Popup(popup)
            },
        };
        let is_popup = popup.is_some();

        let mut window_state = WindowState::new(
            event_loop_window_target.connection.clone(),
            &event_loop_window_target.queue_handle,
            &state,
            size,
            window.clone(),
            attributes.preferred_theme,
        );

        // Set transparency hint.
        window_state.set_transparent(attributes.transparent);

        window_state.set_blur(attributes.blur);

        // Set the decorations hint; a popup has none.
        window_state.set_decorate(attributes.decorations && !is_popup);

        // Set the app_id.
        if let (Some(name), Some(toplevel)) =
            (attributes.platform_specific.name.map(|name| name.general), window.toplevel())
        {
            toplevel.set_app_id(name);
        }

        // Set the window title.
        window_state.set_title(attributes.title);

        // Set the min and max sizes. We must set the hints upon creating a window, so
        // we use the default `1.` scaling...
        let min_size = attributes.min_inner_size.map(|size| size.to_logical(1.));
        let max_size = attributes.max_inner_size.map(|size| size.to_logical(1.));
        window_state.set_min_inner_size(min_size);
        window_state.set_max_inner_size(max_size);

        // Non-resizable implies that the min and max sizes are set to the same value.
        window_state.set_resizable(attributes.resizable);

        // Set startup mode.
        match attributes.fullscreen.map(Into::into) {
            Some(Fullscreen::Exclusive(_)) => {
                warn!("`Fullscreen::Exclusive` is ignored on Wayland");
            },
            #[cfg_attr(not(x11_platform), allow(clippy::bind_instead_of_map))]
            Some(Fullscreen::Borderless(monitor)) => {
                let output = monitor.and_then(|monitor| match monitor {
                    PlatformMonitorHandle::Wayland(monitor) => Some(monitor.proxy),
                    #[cfg(x11_platform)]
                    PlatformMonitorHandle::X(_) => None,
                });

                if let Some(toplevel) = window.toplevel() {
                    toplevel.set_fullscreen(output.as_ref());
                }
            },
            _ if attributes.maximized => window.set_maximized(),
            _ => (),
        };

        match attributes.cursor {
            Cursor::Icon(icon) => window_state.set_cursor(icon),
            Cursor::Custom(cursor) => window_state.set_custom_cursor(cursor),
        }

        // Apply resize increments.
        if let Some(increments) = attributes.resize_increments {
            let increments = increments.to_logical(window_state.scale_factor());
            window_state.set_resize_increments(Some(increments));
        }

        // Activate the window when the token is passed.
        if let (Some(xdg_activation), Some(token)) =
            (xdg_activation.as_ref(), attributes.platform_specific.activation_token)
        {
            xdg_activation.activate(token.token, &surface);
        }

        // XXX Do initial commit.
        window.commit();

        // Add the window and window requests into the state.
        let window_state = Arc::new(Mutex::new(window_state));
        let window_id = super::make_wid(&surface);
        state.windows.get_mut().insert(window_id, window_state.clone());

        let window_requests = WindowRequests {
            redraw_requested: AtomicBool::new(true),
            closed: AtomicBool::new(false),
        };
        let window_requests = Arc::new(window_requests);
        state.window_requests.get_mut().insert(window_id, window_requests.clone());

        // Setup the event sync to insert `WindowEvents` right from the window.
        let window_events_sink = state.window_events_sink.clone();

        let mut wayland_source = event_loop_window_target.wayland_dispatcher.as_source_mut();
        let event_queue = wayland_source.queue();

        // Do a roundtrip.
        event_queue.roundtrip(&mut state).map_err(|error| {
            os_error!(OsError::WaylandError(Arc::new(WaylandError::Dispatch(error))))
        })?;

        // XXX Wait for the initial configure to arrive.
        while !window_state.lock().unwrap().is_configured() {
            event_queue.blocking_dispatch(&mut state).map_err(|error| {
                os_error!(OsError::WaylandError(Arc::new(WaylandError::Dispatch(error))))
            })?;
        }

        // Wake-up event loop, so it'll send initial redraw requested.
        let event_loop_awakener = event_loop_window_target.event_loop_awakener.clone();
        event_loop_awakener.ping();

        Ok(Self {
            window,
            display,
            monitors,
            window_id,
            compositor,
            window_state,
            queue_handle,
            xdg_activation,
            toplevel_tag_manager,
            attention_requested: Arc::new(AtomicBool::new(false)),
            activation_requests: Arc::new(PendingActivationRequests::default()),
            event_loop_awakener,
            window_requests,
            window_events_sink,
        })
    }

    pub(crate) fn xdg_toplevel(&self) -> Option<NonNull<c_void>> {
        NonNull::new(self.window.xdg_toplevel()?.id().as_ptr().cast())
    }
}

impl Window {
    #[inline]
    pub fn id(&self) -> WindowId {
        self.window_id
    }

    #[inline]
    pub fn set_title(&self, title: impl ToString) {
        let new_title = title.to_string();
        self.window_state.lock().unwrap().set_title(new_title);
    }

    #[inline]
    pub fn set_visible(&self, _visible: bool) {
        // Not possible on Wayland.
    }

    #[inline]
    pub fn is_visible(&self) -> Option<bool> {
        None
    }

    #[inline]
    pub fn outer_position(&self) -> Result<PhysicalPosition<i32>, NotSupportedError> {
        Err(NotSupportedError::new())
    }

    #[inline]
    pub fn inner_position(&self) -> Result<PhysicalPosition<i32>, NotSupportedError> {
        Err(NotSupportedError::new())
    }

    #[inline]
    pub fn set_outer_position(&self, _: Position) {
        // Not possible on Wayland.
    }

    #[inline]
    pub fn inner_size(&self) -> PhysicalSize<u32> {
        let window_state = self.window_state.lock().unwrap();
        let scale_factor = window_state.scale_factor();
        super::logical_to_physical_rounded(window_state.inner_size(), scale_factor)
    }

    #[inline]
    pub fn request_redraw(&self) {
        // NOTE: try to not wake up the loop when the event was already scheduled and not yet
        // processed by the loop, because if at this point the value was `true` it could only
        // mean that the loop still haven't dispatched the value to the client and will do
        // eventually, resetting it to `false`.
        if self
            .window_requests
            .redraw_requested
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            self.event_loop_awakener.ping();
        }
    }

    #[inline]
    pub fn pre_present_notify(&self) {
        self.window_state.lock().unwrap().request_frame_callback();
    }

    #[inline]
    pub fn outer_size(&self) -> PhysicalSize<u32> {
        let window_state = self.window_state.lock().unwrap();
        let scale_factor = window_state.scale_factor();
        super::logical_to_physical_rounded(window_state.outer_size(), scale_factor)
    }

    #[inline]
    pub fn request_inner_size(&self, size: Size) -> Option<PhysicalSize<u32>> {
        let mut window_state = self.window_state.lock().unwrap();
        let new_size = window_state.request_inner_size(size);
        self.request_redraw();
        Some(new_size)
    }

    /// Set the minimum inner size for the window.
    #[inline]
    pub fn set_min_inner_size(&self, min_size: Option<Size>) {
        let scale_factor = self.scale_factor();
        let min_size = min_size.map(|size| size.to_logical(scale_factor));
        self.window_state.lock().unwrap().set_min_inner_size(min_size);
        // NOTE: Requires commit to be applied.
        self.request_redraw();
    }

    /// Set the maximum inner size for the window.
    #[inline]
    pub fn set_max_inner_size(&self, max_size: Option<Size>) {
        let scale_factor = self.scale_factor();
        let max_size = max_size.map(|size| size.to_logical(scale_factor));
        self.window_state.lock().unwrap().set_max_inner_size(max_size);
        // NOTE: Requires commit to be applied.
        self.request_redraw();
    }

    #[inline]
    pub fn resize_increments(&self) -> Option<PhysicalSize<u32>> {
        let window_state = self.window_state.lock().unwrap();
        let scale_factor = window_state.scale_factor();
        window_state
            .resize_increments()
            .map(|size| super::logical_to_physical_rounded(size, scale_factor))
    }

    #[inline]
    pub fn set_resize_increments(&self, increments: Option<Size>) {
        let mut window_state = self.window_state.lock().unwrap();
        let scale_factor = window_state.scale_factor();
        let increments = increments.map(|size| size.to_logical(scale_factor));
        window_state.set_resize_increments(increments);
    }

    #[inline]
    pub fn set_transparent(&self, transparent: bool) {
        self.window_state.lock().unwrap().set_transparent(transparent);
    }

    #[inline]
    pub fn has_focus(&self) -> bool {
        self.window_state.lock().unwrap().has_focus()
    }

    #[inline]
    pub fn is_minimized(&self) -> Option<bool> {
        // XXX clients don't know whether they are minimized or not.
        None
    }

    #[inline]
    pub fn show_window_menu(&self, position: Position) {
        let scale_factor = self.scale_factor();
        let position = position.to_logical(scale_factor);
        self.window_state.lock().unwrap().show_window_menu(position);
    }

    #[inline]
    pub fn drag_resize_window(&self, direction: ResizeDirection) -> Result<(), ExternalError> {
        self.window_state.lock().unwrap().drag_resize_window(direction)
    }

    #[inline]
    pub fn set_resizable(&self, resizable: bool) {
        if self.window_state.lock().unwrap().set_resizable(resizable) {
            // NOTE: Requires commit to be applied.
            self.request_redraw();
        }
    }

    #[inline]
    pub fn is_resizable(&self) -> bool {
        self.window_state.lock().unwrap().resizable()
    }

    #[inline]
    pub fn set_enabled_buttons(&self, _buttons: WindowButtons) {
        // TODO(kchibisov) v5 of the xdg_shell allows that.
    }

    #[inline]
    pub fn enabled_buttons(&self) -> WindowButtons {
        // TODO(kchibisov) v5 of the xdg_shell allows that.
        WindowButtons::all()
    }

    #[inline]
    pub fn scale_factor(&self) -> f64 {
        self.window_state.lock().unwrap().scale_factor()
    }

    #[inline]
    pub fn set_blur(&self, blur: bool) {
        self.window_state.lock().unwrap().set_blur(blur);
    }

    #[inline]
    pub fn set_decorations(&self, decorate: bool) {
        self.window_state.lock().unwrap().set_decorate(decorate)
    }

    #[inline]
    pub fn is_decorated(&self) -> bool {
        self.window_state.lock().unwrap().is_decorated()
    }

    #[inline]
    pub fn set_window_level(&self, _level: WindowLevel) {}

    #[inline]
    pub(crate) fn set_window_icon(&self, _window_icon: Option<PlatformIcon>) {}

    #[inline]
    pub fn set_minimized(&self, minimized: bool) {
        // You can't unminimize the window on Wayland.
        if !minimized {
            warn!("Unminimizing is ignored on Wayland.");
            return;
        }

        self.window.set_minimized();
    }

    #[inline]
    pub fn is_maximized(&self) -> bool {
        self.window_state
            .lock()
            .unwrap()
            .last_configure
            .as_ref()
            .map(|last_configure| last_configure.is_maximized())
            .unwrap_or_default()
    }

    #[inline]
    pub fn set_maximized(&self, maximized: bool) {
        if maximized {
            self.window.set_maximized()
        } else {
            self.window.unset_maximized()
        }
    }

    #[inline]
    pub(crate) fn fullscreen(&self) -> Option<Fullscreen> {
        let is_fullscreen = self
            .window_state
            .lock()
            .unwrap()
            .last_configure
            .as_ref()
            .map(|last_configure| last_configure.is_fullscreen())
            .unwrap_or_default();

        if is_fullscreen {
            let current_monitor = self.current_monitor().map(PlatformMonitorHandle::Wayland);
            Some(Fullscreen::Borderless(current_monitor))
        } else {
            None
        }
    }

    #[inline]
    pub(crate) fn set_fullscreen(&self, fullscreen: Option<Fullscreen>) {
        match fullscreen {
            Some(Fullscreen::Exclusive(_)) => {
                warn!("`Fullscreen::Exclusive` is ignored on Wayland");
            },
            #[cfg_attr(not(x11_platform), allow(clippy::bind_instead_of_map))]
            Some(Fullscreen::Borderless(monitor)) => {
                let output = monitor.and_then(|monitor| match monitor {
                    PlatformMonitorHandle::Wayland(monitor) => Some(monitor.proxy),
                    #[cfg(x11_platform)]
                    PlatformMonitorHandle::X(_) => None,
                });

                if let Some(toplevel) = self.window.toplevel() {
                    toplevel.set_fullscreen(output.as_ref());
                }
            },
            None => {
                if let Some(toplevel) = self.window.toplevel() {
                    toplevel.unset_fullscreen();
                }
            },
        }
    }

    #[inline]
    pub fn set_cursor(&self, cursor: Cursor) {
        let window_state = &mut self.window_state.lock().unwrap();

        match cursor {
            Cursor::Icon(icon) => window_state.set_cursor(icon),
            Cursor::Custom(cursor) => window_state.set_custom_cursor(cursor),
        }
    }

    #[inline]
    pub fn set_cursor_visible(&self, visible: bool) {
        self.window_state.lock().unwrap().set_cursor_visible(visible);
    }

    pub fn request_user_attention(&self, request_type: Option<UserAttentionType>) {
        let xdg_activation = match self.xdg_activation.as_ref() {
            Some(xdg_activation) => xdg_activation,
            None => {
                warn!("`request_user_attention` isn't supported");
                return;
            },
        };

        // Urgency is only removed by the compositor and there's no need to raise urgency when it
        // was already raised.
        if request_type.is_none() || self.attention_requested.load(Ordering::Relaxed) {
            return;
        }

        self.attention_requested.store(true, Ordering::Relaxed);
        let surface = self.surface().clone();
        let data = XdgActivationTokenData::Attention((
            surface.clone(),
            Arc::downgrade(&self.attention_requested),
        ));
        let xdg_activation_token = xdg_activation.get_activation_token(&self.queue_handle, data);
        xdg_activation_token.set_surface(&surface);
        xdg_activation_token.commit();
    }

    pub fn request_activation_token(&self) -> Result<AsyncRequestSerial, NotSupportedError> {
        let xdg_activation = match self.xdg_activation.as_ref() {
            Some(xdg_activation) => xdg_activation,
            None => return Err(NotSupportedError::new()),
        };

        let serial = AsyncRequestSerial::get();

        let data = XdgActivationTokenData::Obtain {
            window_id: self.window_id,
            serial,
            pending: Arc::downgrade(&self.activation_requests),
        };
        let xdg_activation_token = xdg_activation.get_activation_token(&self.queue_handle, data);
        let admission =
            self.activation_requests.lock().unwrap().insert(serial, xdg_activation_token.clone());
        if let Err(request) = admission {
            request.destroy();
            return Err(NotSupportedError::new());
        }

        {
            // Keep seat removal ordered with serial attachment through commit.
            let mut window_state = self.window_state.lock().unwrap();
            xdg_activation_token.set_surface(self.surface());
            if let Some((input_serial, seat)) =
                window_state.take_activation_serial(ACTIVATION_SERIAL_LIFETIME)
            {
                xdg_activation_token.set_serial(input_serial, &seat);
            }
            xdg_activation_token.commit();
        }

        Ok(serial)
    }

    pub fn cancel_activation_token_request(&self, serial: AsyncRequestSerial) -> bool {
        let Some(request) = self.activation_requests.lock().unwrap().cancel(&serial) else {
            return false;
        };
        request.destroy();
        true
    }

    pub fn activate_with_token(
        &self,
        token: crate::window::ActivationToken,
    ) -> Result<(), NotSupportedError> {
        let xdg_activation = self.xdg_activation.as_ref().ok_or_else(NotSupportedError::new)?;
        xdg_activation.activate(token.into_raw(), self.surface());
        Ok(())
    }

    #[inline]
    pub fn set_cursor_grab(&self, mode: CursorGrabMode) -> Result<(), ExternalError> {
        self.window_state.lock().unwrap().set_cursor_grab(mode)
    }

    #[inline]
    pub fn set_cursor_position(&self, position: Position) -> Result<(), ExternalError> {
        let scale_factor = self.scale_factor();
        let position = position.to_logical(scale_factor);
        self.window_state
            .lock()
            .unwrap()
            .set_cursor_position(position)
            // Request redraw on success, since the state is double buffered.
            .map(|_| self.request_redraw())
    }

    #[inline]
    pub fn drag_window(&self) -> Result<(), ExternalError> {
        self.window_state.lock().unwrap().drag_window()
    }

    #[inline]
    pub fn set_cursor_hittest(&self, hittest: bool) -> Result<(), ExternalError> {
        let surface = self.window.wl_surface();

        if hittest {
            surface.set_input_region(None);
            Ok(())
        } else {
            let region = Region::new(&*self.compositor).map_err(|_| {
                ExternalError::Os(os_error!(OsError::Misc("failed to set input region.")))
            })?;
            region.add(0, 0, 0, 0);
            surface.set_input_region(Some(region.wl_region()));
            Ok(())
        }
    }

    #[inline]
    pub fn set_ime_cursor_area(&self, position: Position, size: Size) {
        let window_state = self.window_state.lock().unwrap();
        if window_state.ime_allowed() {
            let scale_factor = window_state.scale_factor();
            let position = position.to_logical(scale_factor);
            let size = size.to_logical(scale_factor);
            window_state.set_ime_cursor_area(position, size);
        }
    }

    #[inline]
    pub fn set_ime_allowed(&self, allowed: bool) {
        let mut window_state = self.window_state.lock().unwrap();

        if window_state.ime_allowed() != allowed && window_state.set_ime_allowed(allowed) {
            let event = WindowEvent::Ime(if allowed { Ime::Enabled } else { Ime::Disabled });
            self.window_events_sink.lock().unwrap().push_window_event(event, self.window_id);
            self.event_loop_awakener.ping();
        }
    }

    #[inline]
    pub fn set_ime_purpose(&self, purpose: ImePurpose) {
        self.window_state.lock().unwrap().set_ime_purpose(purpose);
    }

    #[inline]
    pub fn focus_window(&self) {}

    #[inline]
    pub fn surface(&self) -> &WlSurface {
        self.window.wl_surface()
    }

    /// The connection this window's surface was created on.
    #[inline]
    pub fn connection(&self) -> Connection {
        self.window_state.lock().unwrap().connection.clone()
    }

    /// Identity of this window's `wl_surface`.
    #[inline]
    pub fn surface_incarnation(&self) -> u64 {
        self.window_state.lock().unwrap().surface_incarnation()
    }

    /// The objects an XDG portal parent export needs: this window's own
    /// connection, its surface, and that surface's identity.
    ///
    /// The surface is created once, in [`Window::new`], and no path replaces
    /// it, so these three describe one protocol object for as long as the
    /// window lives.
    pub(crate) fn wayland_parent_objects(&self) -> (Connection, WlSurface, u64) {
        let state = self.window_state.lock().unwrap();
        (
            state.connection.clone(),
            self.window.wl_surface().clone(),
            state.surface_incarnation(),
        )
    }

    pub(crate) fn set_toplevel_tag(&self, tag: &str) -> bool {
        if tag.is_empty()
            || tag.len() > 128
            || !tag.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
        {
            return false;
        }
        let Some(manager) = &self.toplevel_tag_manager else {
            return false;
        };
        let Some(toplevel) = self.window.xdg_toplevel() else {
            return false;
        };
        manager.set(toplevel, tag);
        self.event_loop_awakener.ping();
        true
    }

    #[inline]
    pub fn current_monitor(&self) -> Option<MonitorHandle> {
        let data = self.window.wl_surface().data::<SurfaceData<()>>()?;
        data.outputs().next().map(MonitorHandle::new)
    }

    #[inline]
    pub fn available_monitors(&self) -> Vec<MonitorHandle> {
        self.monitors.lock().unwrap().clone()
    }

    #[inline]
    pub fn primary_monitor(&self) -> Option<MonitorHandle> {
        // XXX there's no such concept on Wayland.
        None
    }

    #[cfg(feature = "rwh_04")]
    #[inline]
    pub fn raw_window_handle_rwh_04(&self) -> rwh_04::RawWindowHandle {
        let mut window_handle = rwh_04::WaylandHandle::empty();
        window_handle.surface = self.window.wl_surface().id().as_ptr() as *mut _;
        window_handle.display = self.display.id().as_ptr() as *mut _;
        rwh_04::RawWindowHandle::Wayland(window_handle)
    }

    #[cfg(feature = "rwh_05")]
    #[inline]
    pub fn raw_window_handle_rwh_05(&self) -> rwh_05::RawWindowHandle {
        let mut window_handle = rwh_05::WaylandWindowHandle::empty();
        window_handle.surface = self.window.wl_surface().id().as_ptr() as *mut _;
        rwh_05::RawWindowHandle::Wayland(window_handle)
    }

    #[cfg(feature = "rwh_05")]
    #[inline]
    pub fn raw_display_handle_rwh_05(&self) -> rwh_05::RawDisplayHandle {
        let mut display_handle = rwh_05::WaylandDisplayHandle::empty();
        display_handle.display = self.display.id().as_ptr() as *mut _;
        rwh_05::RawDisplayHandle::Wayland(display_handle)
    }

    #[cfg(feature = "rwh_06")]
    #[inline]
    pub fn raw_window_handle_rwh_06(&self) -> Result<rwh_06::RawWindowHandle, rwh_06::HandleError> {
        Ok(rwh_06::WaylandWindowHandle::new({
            let ptr = self.window.wl_surface().id().as_ptr();
            std::ptr::NonNull::new(ptr as *mut _).expect("wl_surface will never be null")
        })
        .into())
    }

    #[cfg(feature = "rwh_06")]
    #[inline]
    pub fn raw_display_handle_rwh_06(
        &self,
    ) -> Result<rwh_06::RawDisplayHandle, rwh_06::HandleError> {
        Ok(rwh_06::WaylandDisplayHandle::new({
            let ptr = self.display.id().as_ptr();
            std::ptr::NonNull::new(ptr as *mut _).expect("wl_proxy should never be null")
        })
        .into())
    }

    #[inline]
    pub fn set_theme(&self, theme: Option<Theme>) {
        self.window_state.lock().unwrap().set_theme(theme)
    }

    #[inline]
    pub fn theme(&self) -> Option<Theme> {
        self.window_state.lock().unwrap().theme()
    }

    pub fn set_content_protected(&self, _protected: bool) {}

    #[inline]
    pub fn title(&self) -> String {
        self.window_state.lock().unwrap().title().to_owned()
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let requests = self.activation_requests.lock().unwrap().take_all();
        for request in requests {
            request.destroy();
        }
        self.window_requests.closed.store(true, Ordering::Relaxed);
        self.event_loop_awakener.ping();
    }
}

/// The request from the window to the event loop.
#[derive(Debug)]
pub struct WindowRequests {
    /// The window was closed.
    pub closed: AtomicBool,

    /// Redraw Requested.
    pub redraw_requested: AtomicBool,
}

impl WindowRequests {
    pub fn take_closed(&self) -> bool {
        self.closed.swap(false, Ordering::Relaxed)
    }

    pub fn take_redraw_requested(&self) -> bool {
        self.redraw_requested.swap(false, Ordering::Relaxed)
    }
}

impl TryFrom<&str> for Theme {
    type Error = ();

    /// ```
    /// use winit::window::Theme;
    ///
    /// assert_eq!("dark".try_into(), Ok(Theme::Dark));
    /// assert_eq!("lIghT".try_into(), Ok(Theme::Light));
    /// ```
    fn try_from(theme: &str) -> Result<Self, Self::Error> {
        if theme.eq_ignore_ascii_case("dark") {
            Ok(Self::Dark)
        } else if theme.eq_ignore_ascii_case("light") {
            Ok(Self::Light)
        } else {
            Err(())
        }
    }
}

/// The latest press on the parent, for an explicit popup grab.
fn parent_serial(
    parent: &WindowState,
) -> Option<(u32, sctk::reexports::client::protocol::wl_seat::WlSeat)> {
    parent.latest_input_serial()
}

/// An `xdg_positioner` for `popup`.
fn positioner(
    xdg_shell: &XdgShell,
    popup: &crate::platform::wayland::PopupAttributes,
) -> Result<XdgPositioner, sctk::error::GlobalError> {
    use crate::platform::wayland::PopupEdge;
    let positioner = XdgPositioner::new(xdg_shell)?;
    let (x, y, width, height) = popup.anchor_rect;
    positioner.set_size(popup.size.0.max(1), popup.size.1.max(1));
    positioner.set_anchor_rect(x, y, width.max(1), height.max(1));
    let anchor = |edge: PopupEdge| match edge {
        PopupEdge::None => xdg_positioner::Anchor::None,
        PopupEdge::Top => xdg_positioner::Anchor::Top,
        PopupEdge::Bottom => xdg_positioner::Anchor::Bottom,
        PopupEdge::Left => xdg_positioner::Anchor::Left,
        PopupEdge::Right => xdg_positioner::Anchor::Right,
        PopupEdge::TopLeft => xdg_positioner::Anchor::TopLeft,
        PopupEdge::BottomLeft => xdg_positioner::Anchor::BottomLeft,
        PopupEdge::TopRight => xdg_positioner::Anchor::TopRight,
        PopupEdge::BottomRight => xdg_positioner::Anchor::BottomRight,
    };
    let gravity = |edge: PopupEdge| match edge {
        PopupEdge::None => xdg_positioner::Gravity::None,
        PopupEdge::Top => xdg_positioner::Gravity::Top,
        PopupEdge::Bottom => xdg_positioner::Gravity::Bottom,
        PopupEdge::Left => xdg_positioner::Gravity::Left,
        PopupEdge::Right => xdg_positioner::Gravity::Right,
        PopupEdge::TopLeft => xdg_positioner::Gravity::TopLeft,
        PopupEdge::BottomLeft => xdg_positioner::Gravity::BottomLeft,
        PopupEdge::TopRight => xdg_positioner::Gravity::TopRight,
        PopupEdge::BottomRight => xdg_positioner::Gravity::BottomRight,
    };
    positioner.set_anchor(anchor(popup.anchor));
    positioner.set_gravity(gravity(popup.gravity));
    positioner.set_offset(popup.offset.0, popup.offset.1);
    positioner.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::from_bits_truncate(
        popup.constraint_adjustment.0,
    ));
    Ok(positioner)
}
