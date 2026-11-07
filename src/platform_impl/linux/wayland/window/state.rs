// Modified by enzUI contributors. See FORK.md.
//! The state of the window, which is shared with the event-loop.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use ahash::HashSet;
use tracing::{info, warn};

use sctk::reexports::client::backend::ObjectId;
use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Proxy, QueueHandle};
use sctk::reexports::csd_frame::{
    DecorationsFrame, FrameAction, FrameClick, ResizeEdge, WindowState as XdgWindowState,
};
use sctk::reexports::protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::WpFractionalScaleV1;
use sctk::reexports::protocols::wp::tablet::zv2::client::zwp_tablet_tool_v2::ZwpTabletToolV2;
use sctk::reexports::protocols::wp::text_input::zv3::client::zwp_text_input_v3::ZwpTextInputV3;
use sctk::reexports::protocols::wp::viewporter::client::wp_viewport::WpViewport;
use sctk::reexports::protocols::xdg::shell::client::xdg_toplevel::ResizeEdge as XdgResizeEdge;

use sctk::compositor::{CompositorState, FrameCallbackData, Region, SurfaceData};
use sctk::seat::pointer::{PointerData, ThemedPointer};
use sctk::shell::xdg::window::{DecorationMode, Window, WindowConfigure};
use sctk::shell::xdg::XdgSurface;
use sctk::shell::WaylandSurface;
use sctk::shm::slot::SlotPool;
use sctk::shm::Shm;
use sctk::subcompositor::SubcompositorState;
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur::OrgKdeKwinBlur;

use crate::cursor::CustomCursor as RootCustomCursor;
use crate::dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize, Size};
use crate::error::{ExternalError, NotSupportedError};
use crate::platform_impl::wayland::logical_to_physical_rounded;
use crate::platform_impl::wayland::types::cursor::{CustomCursor, SelectedCursor};
use crate::platform_impl::wayland::types::kwin_blur::KWinBlurManager;
use crate::platform_impl::wayland::types::xdg_toplevel_icon::ToplevelIconManager;
use crate::platform_impl::{PlatformCustomCursor, WindowId};
use crate::window::{CursorGrabMode, CursorIcon, ImePurpose, ResizeDirection, Theme};

use crate::platform_impl::wayland::seat::{
    PointerConstraintsState, TabletCursor, WinitPointerData, WinitPointerDataExt, ZwpTextInputV3Ext,
};
use crate::platform_impl::wayland::state::{WindowCompositorUpdate, WinitState};

#[cfg(feature = "sctk-adwaita")]
pub type WinitFrame = sctk_adwaita::AdwaitaFrame<WinitState>;
#[cfg(not(feature = "sctk-adwaita"))]
pub type WinitFrame = sctk::shell::xdg::fallback_frame::FallbackFrame<WinitState>;

// Minimum window inner size.
const MIN_WINDOW_SIZE: LogicalSize<u32> = LogicalSize::new(2, 1);

/// Mints one identity per `wl_surface` this backend creates.
static NEXT_SURFACE_INCARNATION: AtomicU64 = AtomicU64::new(1);

struct ActivationSerial<T> {
    pending: Option<(u32, T, Instant)>,
}

impl<T> Default for ActivationSerial<T> {
    fn default() -> Self {
        Self { pending: None }
    }
}

impl<T: Clone> ActivationSerial<T> {
    fn record(&mut self, serial: u32, seat: T, recorded_at: Instant) {
        self.pending = Some((serial, seat, recorded_at));
    }

    fn take(&mut self, max_age: Duration, now: Instant) -> Option<(u32, T)> {
        let (serial, seat, recorded_at) = self.pending.take()?;
        now.checked_duration_since(recorded_at)
            .is_some_and(|age| age <= max_age)
            .then_some((serial, seat))
    }

    fn invalidate_if(&mut self, predicate: impl FnOnce(&T) -> bool) {
        if self.pending.as_ref().is_some_and(|(_, seat, _)| predicate(seat)) {
            self.pending = None;
        }
    }
}

/// The state of the window which is being updated from the [`WinitState`].
pub struct WindowState {
    /// The connection to Wayland server.
    pub connection: Connection,

    /// Identity of the `wl_surface` this state was built around.
    ///
    /// Minted where the surface is created, so it names that protocol object
    /// rather than the window or a reusable object id. A surface the server
    /// destroyed cannot be confused with a later one that reuses its numeric
    /// id, which is what an exported xdg-foreign handle must be able to tell
    /// apart while a portal request is outstanding.
    surface_incarnation: u64,

    // A shared pool where to allocate custom cursors.
    custom_cursor_pool: Arc<Mutex<SlotPool>>,

    /// The pixels of the window icon, kept while it is shown.
    icon_buffer: Option<sctk::shm::slot::Buffer>,

    /// The last received configure.
    pub last_configure: Option<WindowConfigure>,

    /// The pointers observed on the window.
    pub pointers: Vec<Weak<ThemedPointer<WinitPointerData>>>,

    /// The tablet tools in proximity of the window.
    tablet_tools: Vec<TabletCursor>,

    activation_serial: ActivationSerial<WlSeat>,
    selected_cursor: SelectedCursor,

    /// Whether the cursor is visible.
    pub cursor_visible: bool,

    /// Pointer constraints to lock/confine pointer.
    pub pointer_constraints: Option<Arc<PointerConstraintsState>>,

    /// Queue handle.
    pub queue_handle: QueueHandle<WinitState>,

    /// Theme variant.
    theme: Option<Theme>,

    /// The current window title.
    title: String,

    /// Whether the frame is resizable.
    resizable: bool,

    // NOTE: we can't use simple counter, since it's racy when seat getting destroyed and new
    // is created, since add/removed stuff could be delivered a bit out of order.
    /// Seats that has keyboard focus on that window.
    seat_focus: HashSet<ObjectId>,

    /// The scale factor of the window.
    scale_factor: f64,

    /// Whether the window is transparent.
    transparent: bool,

    /// The state of the compositor to create WlRegions.
    compositor: Arc<CompositorState>,

    /// The current cursor grabbing mode.
    cursor_grab_mode: GrabState,

    /// Whether the IME input is allowed for that window.
    ime_allowed: bool,

    /// The current IME purpose.
    ime_purpose: ImePurpose,

    /// The text inputs observed on the window.
    text_inputs: Vec<ZwpTextInputV3>,

    /// The inner size of the window, as in without client side decorations.
    size: LogicalSize<u32>,

    /// Whether the CSD fail to create, so we don't try to create them on each iteration.
    csd_fails: bool,

    /// Whether we should decorate the frame.
    decorate: bool,

    /// Min size.
    min_inner_size: LogicalSize<u32>,
    max_inner_size: Option<LogicalSize<u32>>,
    resize_increments: Option<LogicalSize<u32>>,

    /// The size of the window when no states were applied to it. The primary use for it
    /// is to fallback to original window size, before it was maximized, if the compositor
    /// sends `None` for the new size in the configure.
    stateless_size: LogicalSize<u32>,

    /// Initial window size provided by the user. Removed on the first
    /// configure.
    initial_size: Option<Size>,

    /// Frame callbacks requested from the compositor and not yet done.
    frame_callbacks_in_flight: u8,

    /// Chooses how many frame callbacks may be in flight before redraws are throttled.
    frame_pacing: FramePacing,

    viewport: Option<WpViewport>,
    fractional_scale: Option<WpFractionalScaleV1>,
    blur: Option<OrgKdeKwinBlur>,
    blur_manager: Option<KWinBlurManager>,

    /// The resize edge under a pointer on the shadow's resize band.
    shadow_pointer: Option<XdgResizeEdge>,

    /// Whether the client side decorations have pending move operations.
    ///
    /// The value is the serial of the event triggered moved.
    has_pending_move: Option<u32>,

    /// A popup received its first configure.
    popup_configured: bool,

    /// For a popup: its parent window and whether it took an input grab.
    pub popup_of: Option<(super::WindowId, bool)>,

    /// A popup's `xdg_popup` was destroyed ahead of the event loop's cleanup.
    pub popup_destroyed: bool,

    /// Where the window geometry starts on the surface: above and left of the
    /// content by the frame winit draws, `(0, 0)` without one. Popup
    /// positions are relative to the geometry.
    pub geometry_origin: (i32, i32),

    /// The application draws the decorations when the compositor does not.
    pub app_drawn_decorations: bool,

    /// The shadow cast around a window whose application draws its frame.
    pub window_shadow: Option<crate::platform::wayland::WindowShadow>,

    /// The subsurfaces carrying that shadow, made on first use.
    shadow_surfaces: Option<super::shadow::ShadowSurfaces>,

    /// The pool the shadow's buffers come from, until the surfaces take it.
    shadow_pool: Option<SlotPool>,

    subcompositor: Option<Arc<SubcompositorState>>,

    /// The decoration state last reported to the application.
    reported_decorations: Option<crate::event::WindowEvent>,

    /// The underlying SCTK surface: a toplevel window or a popup.
    pub window: ShellSurface,

    // NOTE: The spec says that destroying parent(`window` in our case), will unmap the
    // subsurfaces. Thus to achieve atomic unmap of the client, drop the decorations
    // frame after the `window` is dropped. To achieve that we rely on rust's struct
    // field drop order guarantees.
    /// The window frame, which is created from the configure request.
    frame: Option<WinitFrame>,
}

impl WindowState {
    /// Create new window state.
    pub fn new(
        connection: Connection,
        queue_handle: &QueueHandle<WinitState>,
        winit_state: &WinitState,
        initial_size: Size,
        window: ShellSurface,
        theme: Option<Theme>,
    ) -> Self {
        let compositor = winit_state.compositor_state.clone();
        let pointer_constraints = winit_state.pointer_constraints.clone();
        let viewport = winit_state
            .viewporter_state
            .as_ref()
            .map(|state| state.get_viewport(window.wl_surface(), queue_handle));
        let fractional_scale = winit_state
            .fractional_scaling_manager
            .as_ref()
            .map(|fsm| fsm.fractional_scaling(window.wl_surface(), queue_handle));

        Self {
            blur: None,
            blur_manager: winit_state.kwin_blur_manager.clone(),
            compositor,
            connection,
            surface_incarnation: NEXT_SURFACE_INCARNATION.fetch_add(1, Ordering::Relaxed),
            csd_fails: false,
            popup_configured: false,
            popup_of: None,
            popup_destroyed: false,
            geometry_origin: (0, 0),
            app_drawn_decorations: false,
            window_shadow: None,
            shadow_surfaces: None,
            shadow_pool: SlotPool::new(1, &winit_state.shm).ok(),
            subcompositor: winit_state.subcompositor_state.clone(),
            reported_decorations: None,
            cursor_grab_mode: GrabState::new(),
            selected_cursor: Default::default(),
            cursor_visible: true,
            decorate: true,
            fractional_scale,
            frame: None,
            frame_callbacks_in_flight: 0,
            frame_pacing: FramePacing::from_env(),
            seat_focus: Default::default(),
            has_pending_move: None,
            shadow_pointer: None,
            ime_allowed: false,
            ime_purpose: ImePurpose::Normal,
            last_configure: None,
            max_inner_size: None,
            min_inner_size: MIN_WINDOW_SIZE,
            resize_increments: None,
            pointer_constraints,
            pointers: Default::default(),
            tablet_tools: Vec::new(),
            activation_serial: ActivationSerial::default(),
            queue_handle: queue_handle.clone(),
            resizable: true,
            scale_factor: 1.,
            custom_cursor_pool: winit_state.custom_cursor_pool.clone(),
            icon_buffer: None,
            size: initial_size.to_logical(1.),
            stateless_size: initial_size.to_logical(1.),
            initial_size: Some(initial_size),
            text_inputs: Vec::new(),
            theme,
            title: String::default(),
            transparent: false,
            viewport,
            window,
        }
    }

    /// Apply closure on the given pointer.
    fn apply_on_pointer<
        F: FnMut(&ThemedPointer<WinitPointerData>, &PointerData<WinitPointerData>),
    >(
        &self,
        mut callback: F,
    ) {
        self.pointers.iter().filter_map(Weak::upgrade).for_each(|pointer| {
            let data = pointer.pointer().winit_data();
            callback(pointer.as_ref(), data);
        })
    }

    /// Get the current state of the frame callback.
    pub fn frame_callback_state(&self) -> FrameCallbackState {
        if self.frame_callbacks_in_flight >= self.frame_pacing.limit() {
            FrameCallbackState::Requested
        } else if self.frame_callbacks_in_flight == 0 {
            FrameCallbackState::None
        } else {
            FrameCallbackState::Received
        }
    }

    /// A frame callback completed at the compositor time `time_ms`.
    pub fn frame_callback_received(
        &mut self,
        time_ms: u32,
        redraw_pending: bool,
        refresh_millihertz: Option<u32>,
    ) {
        self.frame_callbacks_in_flight = self.frame_callbacks_in_flight.saturating_sub(1);
        self.frame_pacing.frame_done(time_ms, redraw_pending, refresh_millihertz);
    }

    /// Reset the frame callbacks state.
    ///
    /// Callbacks still in flight keep throttling until the compositor completes them.
    pub fn frame_callback_reset(&mut self) {}

    /// Request a frame callback unless the in-flight limit is reached.
    pub fn request_frame_callback(&mut self) {
        if self.frame_callbacks_in_flight < self.frame_pacing.limit() {
            self.frame_callbacks_in_flight += 1;
            let surface = self.window.wl_surface();
            surface.frame(&self.queue_handle, FrameCallbackData(surface.clone()));
        }
    }

    pub fn configure(
        &mut self,
        configure: WindowConfigure,
        shm: &Shm,
        subcompositor: &Option<Arc<SubcompositorState>>,
    ) -> bool {
        // NOTE: when using fractional scaling or wl_compositor@v6 the scaling
        // should be delivered before the first configure, thus apply it to
        // properly scale the physical sizes provided by the users.
        if let Some(initial_size) = self.initial_size.take() {
            self.size = initial_size.to_logical(self.scale_factor());
            self.stateless_size = self.size;
        }

        if let (Some(subcompositor), Some(toplevel)) = (
            subcompositor.as_ref().filter(|_| {
                configure.decoration_mode == DecorationMode::Client
                    && self.frame.is_none()
                    && !self.csd_fails
                    && !self.app_drawn_decorations
            }),
            self.window.toplevel(),
        ) {
            match WinitFrame::new(
                toplevel,
                shm,
                #[cfg(feature = "sctk-adwaita")]
                self.compositor.clone(),
                subcompositor.clone(),
                self.queue_handle.clone(),
                #[cfg(feature = "sctk-adwaita")]
                into_sctk_adwaita_config(self.theme),
            ) {
                Ok(mut frame) => {
                    frame.set_title(&self.title);
                    frame.set_scaling_factor(self.scale_factor);
                    // Hide the frame if we were asked to not decorate.
                    frame.set_hidden(!self.decorate);
                    self.frame = Some(frame);
                },
                Err(err) => {
                    warn!("Failed to create client side decorations frame: {err}");
                    self.csd_fails = true;
                },
            }
        } else if configure.decoration_mode == DecorationMode::Server {
            // Drop the frame for server side decorations to save resources.
            self.frame = None;
        }

        let stateless = Self::is_stateless(&configure);

        let (mut new_size, constrain) = if let Some(frame) = self.frame.as_mut() {
            // Configure the window states.
            frame.update_state(configure.state);

            match configure.new_size {
                (Some(width), Some(height)) => {
                    let (width, height) = frame.subtract_borders(width, height);
                    let width = width.map(|w| w.get()).unwrap_or(1);
                    let height = height.map(|h| h.get()).unwrap_or(1);
                    ((width, height).into(), false)
                },
                (..) if stateless => (self.stateless_size, true),
                _ => (self.size, true),
            }
        } else {
            match configure.new_size {
                (Some(width), Some(height)) => ((width.get(), height.get()).into(), false),
                _ if stateless => (self.stateless_size, true),
                _ => (self.size, true),
            }
        };

        // Apply configure bounds only when compositor let the user decide what size to pick.
        if constrain {
            let bounds = self.inner_size_bounds(&configure);
            new_size.width =
                bounds.0.map(|bound_w| new_size.width.min(bound_w.get())).unwrap_or(new_size.width);
            new_size.height = bounds
                .1
                .map(|bound_h| new_size.height.min(bound_h.get()))
                .unwrap_or(new_size.height);
        }

        // Apply size increments.
        //
        // We conditionally apply increments to avoid conflicts with the compositor's layout rules:
        // 1. If the window is floating (constrain == true), we snap to increments to ensure the
        //    app's grid alignment.
        // 2. If the user is interactively resizing (is_resizing), we snap the size to provide
        //    feedback.
        //
        // However, we MUST NOT snap if the compositor enforces a specific size (constrain == false,
        // or states like Maximized/Tiled). Snapping in these cases (e.g. corner tiling) would
        // shrink the window below the allocated area, creating visible gaps between valid
        // windows or screen edges.
        if (constrain || configure.is_resizing())
            && !configure.is_maximized()
            && !configure.is_fullscreen()
            && !configure.is_tiled()
        {
            if let Some(increments) = self.resize_increments {
                // We use min size as a base size for the increments, similar to how X11 does it.
                //
                // This ensures that we can always reach the min size and the increments are
                // calculated from it.
                let (delta_width, delta_height) = (
                    new_size.width.saturating_sub(self.min_inner_size.width),
                    new_size.height.saturating_sub(self.min_inner_size.height),
                );

                let width =
                    self.min_inner_size.width + (delta_width / increments.width) * increments.width;
                let height = self.min_inner_size.height
                    + (delta_height / increments.height) * increments.height;

                new_size = (width, height).into();
            }
        }

        let new_state = configure.state;
        let old_state = self.last_configure.as_ref().map(|configure| configure.state);

        let state_change_requires_resize = old_state
            .map(|old_state| {
                !old_state
                    .symmetric_difference(new_state)
                    .difference(XdgWindowState::ACTIVATED | XdgWindowState::SUSPENDED)
                    .is_empty()
            })
            // NOTE: `None` is present for the initial configure, thus we must always resize.
            .unwrap_or(true);

        let activation_changed = old_state.map_or(true, |old_state| {
            old_state.contains(XdgWindowState::ACTIVATED)
                != new_state.contains(XdgWindowState::ACTIVATED)
        });

        // NOTE: Set the configure before doing a resize, since we query it during it.
        self.last_configure = Some(configure);

        if state_change_requires_resize || new_size != self.inner_size() {
            self.resize(new_size);
            true
        } else {
            if activation_changed {
                self.update_shadow(false);
            }
            false
        }
    }

    /// Compute the bounds for the inner size of the surface.
    fn inner_size_bounds(
        &self,
        configure: &WindowConfigure,
    ) -> (Option<NonZeroU32>, Option<NonZeroU32>) {
        let configure_bounds = match configure.suggested_bounds {
            Some((width, height)) => (NonZeroU32::new(width), NonZeroU32::new(height)),
            None => (None, None),
        };

        if let Some(frame) = self.frame.as_ref() {
            let (width, height) = frame.subtract_borders(
                configure_bounds.0.unwrap_or(NonZeroU32::new(1).unwrap()),
                configure_bounds.1.unwrap_or(NonZeroU32::new(1).unwrap()),
            );
            (configure_bounds.0.and(width), configure_bounds.1.and(height))
        } else {
            configure_bounds
        }
    }

    #[inline]
    fn is_stateless(configure: &WindowConfigure) -> bool {
        !(configure.is_maximized() || configure.is_fullscreen() || configure.is_tiled())
    }

    /// Start interacting drag resize.
    pub fn drag_resize_window(&self, direction: ResizeDirection) -> Result<(), ExternalError> {
        let Some(xdg_toplevel) = self.window.xdg_toplevel() else {
            return Ok(());
        };

        // TODO(kchibisov) handle touch serials.
        self.apply_on_pointer(|_, data| {
            if let Some(serial) = data.latest_button_serial() {
                let seat = data.seat();
                xdg_toplevel.resize(seat, serial, direction.into());
            }
        });

        Ok(())
    }

    /// Start the window drag.
    pub fn drag_window(&self) -> Result<(), ExternalError> {
        let Some(xdg_toplevel) = self.window.xdg_toplevel() else {
            return Ok(());
        };
        // TODO(kchibisov) handle touch serials.
        self.apply_on_pointer(|_, data| {
            if let Some(serial) = data.latest_button_serial() {
                let seat = data.seat();
                xdg_toplevel._move(seat, serial);
            }
        });

        Ok(())
    }

    /// Tells whether the window should be closed.
    #[allow(clippy::too_many_arguments)]
    pub fn frame_click(
        &mut self,
        click: FrameClick,
        pressed: bool,
        seat: &WlSeat,
        serial: u32,
        timestamp: Duration,
        window_id: WindowId,
        updates: &mut Vec<WindowCompositorUpdate>,
    ) -> Option<bool> {
        if let Some(edge) = self.shadow_pointer.take() {
            if pressed && click == FrameClick::Normal && self.resizable {
                self.window.resize(seat, serial, edge);
            }
            self.shadow_pointer = Some(edge);
            return Some(false);
        }
        match self.frame.as_mut()?.on_click(timestamp, click, pressed)? {
            FrameAction::Minimize => self.window.set_minimized(),
            FrameAction::Maximize => self.window.set_maximized(),
            FrameAction::UnMaximize => self.window.unset_maximized(),
            FrameAction::Close => WinitState::queue_close(updates, window_id),
            FrameAction::Move => self.has_pending_move = Some(serial),
            FrameAction::Resize(edge) => {
                let edge = match edge {
                    ResizeEdge::None => XdgResizeEdge::None,
                    ResizeEdge::Top => XdgResizeEdge::Top,
                    ResizeEdge::Bottom => XdgResizeEdge::Bottom,
                    ResizeEdge::Left => XdgResizeEdge::Left,
                    ResizeEdge::TopLeft => XdgResizeEdge::TopLeft,
                    ResizeEdge::BottomLeft => XdgResizeEdge::BottomLeft,
                    ResizeEdge::Right => XdgResizeEdge::Right,
                    ResizeEdge::TopRight => XdgResizeEdge::TopRight,
                    ResizeEdge::BottomRight => XdgResizeEdge::BottomRight,
                    _ => return None,
                };
                self.window.resize(seat, serial, edge);
            },
            FrameAction::ShowMenu(x, y) => self.window.show_window_menu(seat, serial, (x, y)),
            _ => (),
        };

        Some(false)
    }

    pub fn frame_point_left(&mut self) {
        self.shadow_pointer = None;
        if let Some(frame) = self.frame.as_mut() {
            frame.click_point_left();
        }
    }

    // Move the point over decorations.
    pub fn frame_point_moved(
        &mut self,
        seat: &WlSeat,
        surface: &WlSurface,
        timestamp: Duration,
        x: f64,
        y: f64,
    ) -> Option<CursorIcon> {
        // The shadow's resize band.
        if let Some(edge) = self
            .shadow_surfaces
            .as_ref()
            .filter(|shadow| shadow.owns(surface))
            .and_then(|shadow| shadow.resize_edge(surface, x, y))
        {
            self.shadow_pointer = Some(edge);
            return self.resizable.then(|| resize_cursor(edge));
        }
        self.shadow_pointer = None;

        // Take the serial if we had any, so it doesn't stick around.
        let serial = self.has_pending_move.take();

        if let Some(frame) = self.frame.as_mut() {
            let cursor = frame.click_point_moved(timestamp, &surface.id(), x, y);
            // If we have a cursor change, that means that cursor is over the decorations,
            // so try to apply move.
            if let Some(serial) = cursor.is_some().then_some(serial).flatten() {
                self.window.move_(seat, serial);
                None
            } else {
                cursor
            }
        } else {
            None
        }
    }

    /// Get the stored resizable state.
    #[inline]
    pub fn resizable(&self) -> bool {
        self.resizable
    }

    /// Set the resizable state on the window.
    ///
    /// Returns `true` when the state was applied.
    #[inline]
    pub fn set_resizable(&mut self, resizable: bool) -> bool {
        if self.resizable == resizable {
            return false;
        }

        self.resizable = resizable;
        if resizable {
            // Restore min/max sizes of the window.
            self.reload_min_max_hints();
        } else {
            self.set_min_inner_size(Some(self.size));
            self.set_max_inner_size(Some(self.size));
        }

        // Reload the state on the frame as well.
        if let Some(frame) = self.frame.as_mut() {
            frame.set_resizable(resizable);
        }

        true
    }

    /// Whether the window is focused by any seat.
    #[inline]
    pub fn has_focus(&self) -> bool {
        !self.seat_focus.is_empty()
    }

    /// Whether the IME is allowed.
    #[inline]
    pub fn ime_allowed(&self) -> bool {
        self.ime_allowed
    }

    /// Get the size of the window.
    #[inline]
    pub fn inner_size(&self) -> LogicalSize<u32> {
        self.size
    }

    /// Whether the window received initial configure event from the compositor.
    #[inline]
    pub fn is_configured(&self) -> bool {
        self.last_configure.is_some() || self.popup_configured
    }

    #[inline]
    pub fn is_decorated(&mut self) -> bool {
        let csd = self
            .last_configure
            .as_ref()
            .map(|configure| configure.decoration_mode == DecorationMode::Client)
            .unwrap_or(false);
        if let Some(frame) = csd.then_some(self.frame.as_ref()).flatten() {
            !frame.is_hidden()
        } else {
            // Server side decorations.
            true
        }
    }

    /// Get the outer size of the window.
    #[inline]
    pub fn outer_size(&self) -> LogicalSize<u32> {
        self.frame
            .as_ref()
            .map(|frame| frame.add_borders(self.size.width, self.size.height).into())
            .unwrap_or(self.size)
    }

    pub fn record_activation_serial(&mut self, serial: u32, seat: WlSeat) {
        self.activation_serial.record(serial, seat, Instant::now());
    }

    /// The decoration state of the last configure, when it differs from the
    /// one last reported.
    pub fn decorations_change(&mut self) -> Option<crate::event::WindowEvent> {
        let configure = self.last_configure.as_ref()?;
        let event = crate::event::WindowEvent::DecorationsChanged {
            server_side: configure.decoration_mode == DecorationMode::Server,
            tiled: crate::event::TiledEdges {
                top: configure.is_tiled_top(),
                bottom: configure.is_tiled_bottom(),
                left: configure.is_tiled_left(),
                right: configure.is_tiled_right(),
            },
            maximized: configure.is_maximized(),
            fullscreen: configure.is_fullscreen(),
        };
        if self.reported_decorations.as_ref() == Some(&event) {
            return None;
        }
        self.reported_decorations = Some(event.clone());
        Some(event)
    }

    /// The latest press or key on this window, if it is at most `max_age`
    /// old, without consuming it.
    pub fn recent_input_serial(&self, max_age: Duration) -> Option<(u32, WlSeat)> {
        let (serial, seat, recorded_at) = self.activation_serial.pending.as_ref()?;
        Instant::now()
            .checked_duration_since(*recorded_at)
            .is_some_and(|age| age <= max_age)
            .then(|| (*serial, seat.clone()))
    }

    /// Destroys this popup's `xdg_popup` now, unmapping it, rather than when
    /// the event loop next drops the window: a popup that replaces it may not
    /// be created while this one still holds a grab.
    pub fn destroy_popup_now(&mut self) {
        if self.popup_destroyed {
            return;
        }
        if let ShellSurface::Popup(popup) = &self.window {
            popup.xdg_popup().destroy();
            self.popup_destroyed = true;
        }
    }

    /// Applies an `xdg_popup` configure: the size the compositor gave the
    /// popup. Returns whether the size changed (always true the first time).
    pub fn configure_popup(&mut self, width: i32, height: i32) -> bool {
        if let Some(initial_size) = self.initial_size.take() {
            self.size = initial_size.to_logical(self.scale_factor());
        }
        let first = !self.popup_configured;
        self.popup_configured = true;
        let size = LogicalSize::new(width.max(1) as u32, height.max(1) as u32);
        let resized = first || size != self.size;
        self.resize(size);
        resized
    }

    pub fn take_activation_serial(&mut self, max_age: Duration) -> Option<(u32, WlSeat)> {
        self.activation_serial.take(max_age, Instant::now())
    }

    pub fn invalidate_activation_serial_for_seat(&mut self, seat: &ObjectId) {
        self.activation_serial.invalidate_if(|candidate| candidate.id() == seat.clone());
    }

    /// Register pointer on the top-level.
    pub fn pointer_entered(&mut self, added: Weak<ThemedPointer<WinitPointerData>>) {
        self.pointers.push(added);
        self.reload_cursor_style();

        let mode = self.cursor_grab_mode.user_grab_mode;
        let _ = self.set_cursor_grab_inner(mode);
    }

    /// Pointer has left the top-level.
    pub fn pointer_left(&mut self, removed: Weak<ThemedPointer<WinitPointerData>>) {
        let mut new_pointers = Vec::new();
        for pointer in self.pointers.drain(..) {
            if let Some(pointer) = pointer.upgrade() {
                if pointer.pointer() != removed.upgrade().unwrap().pointer() {
                    new_pointers.push(Arc::downgrade(&pointer));
                }
            }
        }

        self.pointers = new_pointers;
    }

    /// Refresh the decorations frame if it's present returning whether the client should redraw.
    pub fn refresh_frame(&mut self) -> bool {
        if let Some(frame) = self.frame.as_mut() {
            if !frame.is_hidden() && frame.is_dirty() {
                return frame.draw();
            }
        }

        false
    }

    /// Reload the cursor style on the given window.
    pub fn reload_cursor_style(&mut self) {
        if self.cursor_visible {
            match &self.selected_cursor {
                SelectedCursor::Named(icon) => self.set_cursor(*icon),
                SelectedCursor::Custom(cursor) => self.apply_custom_cursor(cursor),
            }
        } else {
            self.set_cursor_visible(self.cursor_visible);
        }
    }

    /// Reissue the transparency hint to the compositor.
    pub fn reload_transparency_hint(&self) {
        let surface = self.window.wl_surface();

        if self.transparent {
            surface.set_opaque_region(None);
        } else if let Ok(region) = Region::new(&*self.compositor) {
            region.add(0, 0, i32::MAX, i32::MAX);
            surface.set_opaque_region(Some(region.wl_region()));
        } else {
            warn!("Failed to mark window opaque.");
        }
    }

    /// Try to resize the window when the user can do so.
    pub fn request_inner_size(&mut self, inner_size: Size) -> PhysicalSize<u32> {
        if self.last_configure.as_ref().map(Self::is_stateless).unwrap_or(true) {
            self.resize(inner_size.to_logical(self.scale_factor()))
        }

        logical_to_physical_rounded(self.inner_size(), self.scale_factor())
    }

    /// Resize the window to the new inner size.
    fn resize(&mut self, inner_size: LogicalSize<u32>) {
        self.size = inner_size;

        // Update the stateless size.
        if Some(true) == self.last_configure.as_ref().map(Self::is_stateless) {
            self.stateless_size = inner_size;
        }

        // Update the inner frame.
        let ((x, y), outer_size) = if let Some(frame) = self.frame.as_mut() {
            // Resize only visible frame.
            if !frame.is_hidden() {
                frame.resize(
                    NonZeroU32::new(self.size.width).unwrap(),
                    NonZeroU32::new(self.size.height).unwrap(),
                );
            }

            (frame.location(), frame.add_borders(self.size.width, self.size.height).into())
        } else {
            ((0, 0), self.size)
        };

        // Reload the hint.
        self.reload_transparency_hint();

        // Set the window geometry.
        self.geometry_origin = (x, y);
        self.window.xdg_surface().set_window_geometry(
            x,
            y,
            outer_size.width as i32,
            outer_size.height as i32,
        );

        // Update the target viewport, this is used if and only if fractional scaling is in use.
        if let Some(viewport) = self.viewport.as_ref() {
            // Set inner size without the borders.
            viewport.set_destination(self.size.width as _, self.size.height as _);
        }

        self.update_shadow(true);
    }

    /// Get the scale factor of the window.
    #[inline]
    pub fn scale_factor(&self) -> f64 {
        self.scale_factor
    }

    /// Identity of the `wl_surface` this state owns.
    pub fn surface_incarnation(&self) -> u64 {
        self.surface_incarnation
    }

    /// Set the cursor icon.
    pub fn set_cursor(&mut self, cursor_icon: CursorIcon) {
        self.selected_cursor = SelectedCursor::Named(cursor_icon);

        if !self.cursor_visible {
            return;
        }

        self.apply_on_pointer(|pointer, _| {
            if pointer.set_cursor(&self.connection, cursor_icon).is_err() {
                warn!("Failed to set cursor to {:?}", cursor_icon);
            }
        });
        for tool in &self.tablet_tools {
            tool.set_icon(cursor_icon);
        }
    }

    /// A tablet tool came over the window; it shows the window's cursor.
    pub fn tablet_tool_entered(&mut self, tool: TabletCursor) {
        if !self.cursor_visible {
            tool.hide();
        } else if let SelectedCursor::Named(icon) = &self.selected_cursor {
            tool.set_icon(*icon);
        }
        self.tablet_tools.retain(|known| known.tool != tool.tool);
        self.tablet_tools.push(tool);
    }

    /// A tablet tool left the window.
    pub fn tablet_tool_left(&mut self, tool: &ZwpTabletToolV2) {
        self.tablet_tools.retain(|known| &known.tool != tool);
    }

    /// Shows `icon` as the window's icon, or the compositor's default.
    pub(crate) fn set_icon(
        &mut self,
        manager: &ToplevelIconManager,
        toplevel: &sctk::reexports::protocols::xdg::shell::client::xdg_toplevel::XdgToplevel,
        icon: Option<&crate::icon::RgbaIcon>,
    ) {
        let mut pool = self.custom_cursor_pool.lock().unwrap();
        self.icon_buffer = manager.set(toplevel, icon, &mut pool);
    }

    /// Set the custom cursor icon.
    pub(crate) fn set_custom_cursor(&mut self, cursor: RootCustomCursor) {
        let cursor = match cursor {
            RootCustomCursor { inner: PlatformCustomCursor::Wayland(cursor) } => cursor.0,
            #[cfg(x11_platform)]
            RootCustomCursor { inner: PlatformCustomCursor::X(_) } => {
                tracing::error!("passed a X11 cursor to Wayland backend");
                return;
            },
        };

        let cursor = {
            let mut pool = self.custom_cursor_pool.lock().unwrap();
            CustomCursor::new(&mut pool, &cursor)
        };

        if self.cursor_visible {
            self.apply_custom_cursor(&cursor);
        }

        self.selected_cursor = SelectedCursor::Custom(cursor);
    }

    /// Set the resize increments of the window.
    pub fn set_resize_increments(&mut self, increments: Option<LogicalSize<u32>>) {
        self.resize_increments = increments;
        // NOTE: We don't update the window size here, because it will be done on the next resize
        // or configure event.
    }

    /// Get the resize increments of the window.
    pub fn resize_increments(&self) -> Option<LogicalSize<u32>> {
        self.resize_increments
    }

    fn apply_custom_cursor(&self, cursor: &CustomCursor) {
        self.apply_on_pointer(|pointer, data| {
            let surface = pointer.surface();

            let scale = if let Some(viewport) = data.data().viewport() {
                let scale = self.scale_factor();
                let size = PhysicalSize::new(cursor.w, cursor.h).to_logical(scale);
                viewport.set_destination(size.width, size.height);
                scale
            } else {
                let scale = surface.data::<SurfaceData<()>>().unwrap().scale_factor();
                surface.set_buffer_scale(scale);
                scale as f64
            };

            surface.attach(Some(cursor.buffer.wl_buffer()), 0, 0);
            if surface.version() >= 4 {
                surface.damage_buffer(0, 0, cursor.w, cursor.h);
            } else {
                let size = PhysicalSize::new(cursor.w, cursor.h).to_logical(scale);
                surface.damage(0, 0, size.width, size.height);
            }
            surface.commit();

            let serial = pointer
                .pointer()
                .data::<PointerData<WinitPointerData>>()
                .and_then(|data| data.latest_enter_serial())
                .unwrap();

            let hotspot =
                PhysicalPosition::new(cursor.hotspot_x, cursor.hotspot_y).to_logical(scale);
            pointer.pointer().set_cursor(serial, Some(surface), hotspot.x, hotspot.y);
        });
    }

    /// Set maximum inner window size.
    pub fn set_min_inner_size(&mut self, size: Option<LogicalSize<u32>>) {
        // Ensure that the window has the right minimum size.
        let mut size = size.unwrap_or(MIN_WINDOW_SIZE);
        size.width = size.width.max(MIN_WINDOW_SIZE.width);
        size.height = size.height.max(MIN_WINDOW_SIZE.height);

        // Add the borders.
        let size = self
            .frame
            .as_ref()
            .map(|frame| frame.add_borders(size.width, size.height).into())
            .unwrap_or(size);

        self.min_inner_size = size;
        self.window.set_min_size(Some(size.into()));
    }

    /// Set maximum inner window size.
    pub fn set_max_inner_size(&mut self, size: Option<LogicalSize<u32>>) {
        let size = size.map(|size| {
            self.frame
                .as_ref()
                .map(|frame| frame.add_borders(size.width, size.height).into())
                .unwrap_or(size)
        });

        self.max_inner_size = size;
        self.window.set_max_size(size.map(Into::into));
    }

    /// Set the CSD theme.
    pub fn set_theme(&mut self, theme: Option<Theme>) {
        self.theme = theme;
        #[cfg(feature = "sctk-adwaita")]
        if let Some(frame) = self.frame.as_mut() {
            frame.set_config(into_sctk_adwaita_config(theme))
        }
    }

    /// The current theme for CSD decorations.
    #[inline]
    pub fn theme(&self) -> Option<Theme> {
        self.theme
    }

    /// Set the cursor grabbing state on the top-level.
    pub fn set_cursor_grab(&mut self, mode: CursorGrabMode) -> Result<(), ExternalError> {
        if self.cursor_grab_mode.user_grab_mode == mode {
            return Ok(());
        }

        self.set_cursor_grab_inner(mode)?;
        // Update user grab on success.
        self.cursor_grab_mode.user_grab_mode = mode;
        Ok(())
    }

    /// Reload the hints for minimum and maximum sizes.
    pub fn reload_min_max_hints(&mut self) {
        self.set_min_inner_size(Some(self.min_inner_size));
        self.set_max_inner_size(self.max_inner_size);
    }

    /// Set the grabbing state on the surface.
    fn set_cursor_grab_inner(&mut self, mode: CursorGrabMode) -> Result<(), ExternalError> {
        let pointer_constraints = match self.pointer_constraints.as_ref() {
            Some(pointer_constraints) => pointer_constraints,
            None if mode == CursorGrabMode::None => return Ok(()),
            None => return Err(ExternalError::NotSupported(NotSupportedError::new())),
        };

        let mut unset_old = false;
        match self.cursor_grab_mode.current_grab_mode {
            CursorGrabMode::None => unset_old = true,
            CursorGrabMode::Confined => self.apply_on_pointer(|_, data| {
                data.data().unconfine_pointer();
                unset_old = true;
            }),
            CursorGrabMode::Locked => {
                self.apply_on_pointer(|_, data| {
                    data.data().unlock_pointer();
                    unset_old = true;
                });
            },
        }

        // In case we haven't unset the old mode, it means that we don't have a cursor above
        // the window, thus just wait for it to re-appear.
        if !unset_old {
            return Ok(());
        }

        let mut set_mode = false;
        let surface = self.window.wl_surface();
        match mode {
            CursorGrabMode::Locked => self.apply_on_pointer(|pointer, data| {
                let pointer = pointer.pointer();
                data.data().lock_pointer(pointer_constraints, surface, pointer, &self.queue_handle);
                set_mode = true;
            }),
            CursorGrabMode::Confined => self.apply_on_pointer(|pointer, data| {
                let pointer = pointer.pointer();
                data.data().confine_pointer(
                    pointer_constraints,
                    surface,
                    pointer,
                    &self.queue_handle,
                );
                set_mode = true;
            }),
            CursorGrabMode::None => {
                // Current lock/confine was already removed.
                set_mode = true;
            },
        }

        // Replace the current grab mode after we've ensure that it got updated.
        if set_mode {
            self.cursor_grab_mode.current_grab_mode = mode;
        }

        Ok(())
    }

    pub fn show_window_menu(&self, position: LogicalPosition<u32>) {
        // TODO(kchibisov) handle touch serials.
        self.apply_on_pointer(|_, data| {
            if let Some(serial) = data.latest_button_serial() {
                let seat = data.seat();
                self.window.show_window_menu(seat, serial, position.into());
            }
        });
    }

    /// Set the position of the cursor.
    pub fn set_cursor_position(&self, position: LogicalPosition<f64>) -> Result<(), ExternalError> {
        if self.pointer_constraints.is_none() {
            return Err(ExternalError::NotSupported(NotSupportedError::new()));
        }

        // Position can be set only for locked cursor.
        if self.cursor_grab_mode.current_grab_mode != CursorGrabMode::Locked {
            return Err(ExternalError::Os(os_error!(crate::platform_impl::OsError::Misc(
                "cursor position can be set only for locked cursor."
            ))));
        }

        self.apply_on_pointer(|_, data| {
            data.data().set_locked_cursor_position(position.x, position.y);
        });

        Ok(())
    }

    /// Set the visibility state of the cursor.
    pub fn set_cursor_visible(&mut self, cursor_visible: bool) {
        self.cursor_visible = cursor_visible;

        if self.cursor_visible {
            match &self.selected_cursor {
                SelectedCursor::Named(icon) => self.set_cursor(*icon),
                SelectedCursor::Custom(cursor) => self.apply_custom_cursor(cursor),
            }
        } else {
            for pointer in self.pointers.iter().filter_map(|pointer| pointer.upgrade()) {
                if let Some(latest_enter_serial) =
                    pointer.pointer().winit_data().latest_enter_serial()
                {
                    pointer.pointer().set_cursor(latest_enter_serial, None, 0, 0);
                }
            }
            for tool in &self.tablet_tools {
                tool.hide();
            }
        }
    }

    /// Whether show or hide client side decorations.
    #[inline]
    pub fn set_decorate(&mut self, decorate: bool) {
        if decorate == self.decorate {
            return;
        }

        self.decorate = decorate;

        match self.last_configure.as_ref().map(|configure| configure.decoration_mode) {
            Some(DecorationMode::Server) if !self.decorate => {
                // To disable decorations we should request client and hide the frame.
                self.window.request_decoration_mode(Some(DecorationMode::Client))
            },
            _ if self.decorate => self.window.request_decoration_mode(Some(DecorationMode::Server)),
            _ => (),
        }

        if let Some(frame) = self.frame.as_mut() {
            frame.set_hidden(!decorate);
            // Force the resize.
            self.resize(self.size);
        }
    }

    /// Add seat focus for the window.
    #[inline]
    pub fn add_seat_focus(&mut self, seat: ObjectId) {
        self.seat_focus.insert(seat);
    }

    /// Remove seat focus from the window.
    #[inline]
    pub fn remove_seat_focus(&mut self, seat: &ObjectId) {
        self.seat_focus.remove(seat);
    }

    /// Returns `true` if the requested state was applied.
    pub fn set_ime_allowed(&mut self, allowed: bool) -> bool {
        self.ime_allowed = allowed;

        let mut applied = false;
        for text_input in &self.text_inputs {
            applied = true;
            if allowed {
                text_input.enable();
                text_input.set_content_type_by_purpose(self.ime_purpose);
            } else {
                text_input.disable();
            }
            text_input.commit();
        }

        applied
    }

    /// Set the IME position.
    pub fn set_ime_cursor_area(&self, position: LogicalPosition<u32>, size: LogicalSize<u32>) {
        // FIXME: This won't fly unless user will have a way to request IME window per seat, since
        // the ime windows will be overlapping, but winit doesn't expose API to specify for
        // which seat we're setting IME position.
        let (x, y) = (position.x as i32, position.y as i32);
        let (width, height) = (size.width as i32, size.height as i32);
        for text_input in self.text_inputs.iter() {
            text_input.set_cursor_rectangle(x, y, width, height);
            text_input.commit();
        }
    }

    /// Set the IME purpose.
    pub fn set_ime_purpose(&mut self, purpose: ImePurpose) {
        self.ime_purpose = purpose;

        for text_input in &self.text_inputs {
            text_input.set_content_type_by_purpose(purpose);
            text_input.commit();
        }
    }

    /// Get the IME purpose.
    pub fn ime_purpose(&self) -> ImePurpose {
        self.ime_purpose
    }

    /// Set the scale factor for the given window.
    #[inline]
    pub fn set_scale_factor(&mut self, scale_factor: f64) {
        self.scale_factor = scale_factor;

        // NOTE: When fractional scaling is not used update the buffer scale.
        if self.fractional_scale.is_none() {
            let _ = self.window.set_buffer_scale(self.scale_factor as _);
        }

        if let Some(frame) = self.frame.as_mut() {
            frame.set_scaling_factor(scale_factor);
        }
        self.update_shadow(true);
    }

    /// Casts `shadow` around the window, or none (enzWinit).
    pub fn set_window_shadow(&mut self, shadow: Option<crate::platform::wayland::WindowShadow>) {
        if self.window_shadow == shadow {
            return;
        }
        self.window_shadow = shadow;
        self.update_shadow(false);
    }

    /// Paints the shadow for the window's current size, activation and
    /// tiling. Only a toplevel whose application draws its frame casts one;
    /// the compositor's frame and winit's own carry their own.
    fn update_shadow(&mut self, sync: bool) {
        let casts = self.window.xdg_toplevel().is_some()
            && self.frame.as_ref().map_or(true, |frame| frame.is_hidden())
            && self
                .last_configure
                .as_ref()
                .is_some_and(|configure| configure.decoration_mode == DecorationMode::Client);
        let (Some(shadow), true) = (self.window_shadow.as_ref(), casts) else {
            if let Some(surfaces) = self.shadow_surfaces.as_ref() {
                surfaces.hide(sync);
            }
            self.shadow_pointer = None;
            return;
        };
        let Some(subcompositor) = self.subcompositor.clone() else {
            return;
        };
        let configure = self.last_configure.as_ref().expect("checked above");
        let edges = if configure.is_maximized() || configure.is_fullscreen() {
            super::shadow::Edges::NONE
        } else {
            super::shadow::Edges {
                left: !configure.is_tiled_left(),
                top: !configure.is_tiled_top(),
                right: !configure.is_tiled_right(),
                bottom: !configure.is_tiled_bottom(),
            }
        };
        let active = configure.is_activated();
        if self.shadow_surfaces.is_none() {
            let Some(pool) = self.shadow_pool.take() else {
                return;
            };
            self.shadow_surfaces = Some(super::shadow::ShadowSurfaces::new(pool));
        }
        let scale = self.scale_factor.ceil().max(1.0) as u32;
        let size = (self.size.width, self.size.height);
        let shadow = shadow.clone();
        if let Some(surfaces) = self.shadow_surfaces.as_mut() {
            surfaces.update(
                self.window.wl_surface(),
                &subcompositor,
                &self.compositor,
                &self.queue_handle,
                &shadow,
                active,
                edges,
                size,
                scale,
                sync,
            );
        }
    }

    /// Make window background blurred
    #[inline]
    pub fn set_blur(&mut self, blurred: bool) {
        if blurred && self.blur.is_none() {
            if let Some(blur_manager) = self.blur_manager.as_ref() {
                let blur = blur_manager.blur(self.window.wl_surface(), &self.queue_handle);
                blur.commit();
                self.blur = Some(blur);
            } else {
                info!("Blur manager unavailable, unable to change blur")
            }
        } else if !blurred && self.blur.is_some() {
            self.blur_manager.as_ref().unwrap().unset(self.window.wl_surface());
            self.blur.take().unwrap().release();
        }
    }

    /// Set the window title to a new value.
    ///
    /// This will automatically truncate the title to something meaningful.
    pub fn set_title(&mut self, mut title: String) {
        // Truncate the title to at most 1024 bytes, so that it does not blow up the protocol
        // messages
        if title.len() > 1024 {
            let mut new_len = 1024;
            while !title.is_char_boundary(new_len) {
                new_len -= 1;
            }
            title.truncate(new_len);
        }

        // Update the CSD title.
        if let Some(frame) = self.frame.as_mut() {
            frame.set_title(&title);
        }

        self.window.set_title(&title);
        self.title = title;
    }

    /// Mark the window as transparent.
    #[inline]
    pub fn set_transparent(&mut self, transparent: bool) {
        self.transparent = transparent;
        self.reload_transparency_hint();
    }

    /// Register text input on the top-level.
    #[inline]
    pub fn text_input_entered(&mut self, text_input: &ZwpTextInputV3) {
        if !self.text_inputs.iter().any(|t| t == text_input) {
            self.text_inputs.push(text_input.clone());
        }
    }

    /// The text input left the top-level.
    #[inline]
    pub fn text_input_left(&mut self, text_input: &ZwpTextInputV3) {
        if let Some(position) = self.text_inputs.iter().position(|t| t == text_input) {
            self.text_inputs.remove(position);
        }
    }

    /// Get the cached title.
    #[inline]
    pub fn title(&self) -> &str {
        &self.title
    }
}

impl Drop for WindowState {
    fn drop(&mut self) {
        if let Some(blur) = self.blur.take() {
            blur.release();
        }

        if let Some(fs) = self.fractional_scale.take() {
            fs.destroy();
        }

        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }

        // NOTE: the wl_surface used by the window is being cleaned up when
        // dropping SCTK `Window`.
    }
}

/// The state of the cursor grabs.
#[derive(Clone, Copy)]
struct GrabState {
    /// The grab mode requested by the user.
    user_grab_mode: CursorGrabMode,

    /// The current grab mode.
    current_grab_mode: CursorGrabMode,
}

impl GrabState {
    fn new() -> Self {
        Self { user_grab_mode: CursorGrabMode::None, current_grab_mode: CursorGrabMode::None }
    }
}

/// Frame-callback pipelining for continuously redrawing windows.
///
/// One callback in flight renders each frame after the previous one was shown. When a
/// continuously redrawing window keeps missing refresh cycles, a second callback lets the
/// next frame render while the previous one waits for the compositor, giving each frame
/// two refresh periods at the cost of one period of latency. An idle gap restores one.
/// `WINIT_WAYLAND_FRAMES_IN_FLIGHT=1` or `2` fixes the limit instead.
#[derive(Debug, Default)]
struct FramePacing {
    forced_limit: Option<u8>,
    pipelined: bool,
    late_frames: u8,
    last_done_ms: Option<u32>,
}

impl FramePacing {
    /// Consecutive late frames that enable pipelining.
    const LATE_FRAMES: u8 = 3;
    /// A gap without frames this long ends the continuous redraw.
    const IDLE_MS: u32 = 100;

    fn from_env() -> Self {
        let forced_limit = std::env::var("WINIT_WAYLAND_FRAMES_IN_FLIGHT")
            .ok()
            .and_then(|value| value.parse::<u8>().ok())
            .map(|value| value.clamp(1, 2));
        Self { forced_limit, ..Self::default() }
    }

    fn limit(&self) -> u8 {
        self.forced_limit.unwrap_or(if self.pipelined { 2 } else { 1 })
    }

    fn frame_done(&mut self, time_ms: u32, redraw_pending: bool, refresh_millihertz: Option<u32>) {
        let interval_ms = self.last_done_ms.map(|last| time_ms.wrapping_sub(last));
        self.last_done_ms = Some(time_ms);
        let Some(interval_ms) = interval_ms else {
            return;
        };
        if interval_ms > Self::IDLE_MS {
            self.pipelined = false;
            self.late_frames = 0;
            return;
        }
        if self.pipelined {
            return;
        }
        // A frame is late when its callback came at least one and a half refresh periods after
        // the previous one although the window already wanted to redraw.
        let late = redraw_pending
            && refresh_millihertz
                .filter(|refresh| *refresh > 0)
                .is_some_and(|refresh| u64::from(interval_ms) * u64::from(refresh) >= 1_500_000);
        self.late_frames = if late { self.late_frames + 1 } else { 0 };
        if self.late_frames >= Self::LATE_FRAMES {
            self.pipelined = true;
        }
    }
}

/// The state of the frame callback.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameCallbackState {
    /// No frame callback was requested.
    #[default]
    None,
    /// The frame callback was requested, but not yet arrived, the redraw events are throttled.
    Requested,
    /// The callback was marked as done, and user could receive redraw requested
    Received,
}

impl From<ResizeDirection> for XdgResizeEdge {
    fn from(value: ResizeDirection) -> Self {
        match value {
            ResizeDirection::North => XdgResizeEdge::Top,
            ResizeDirection::West => XdgResizeEdge::Left,
            ResizeDirection::NorthWest => XdgResizeEdge::TopLeft,
            ResizeDirection::NorthEast => XdgResizeEdge::TopRight,
            ResizeDirection::East => XdgResizeEdge::Right,
            ResizeDirection::SouthWest => XdgResizeEdge::BottomLeft,
            ResizeDirection::SouthEast => XdgResizeEdge::BottomRight,
            ResizeDirection::South => XdgResizeEdge::Bottom,
        }
    }
}

#[cfg(test)]
mod activation_native_tests {
    use super::ActivationSerial;
    use std::time::{Duration, Instant};

    #[test]
    fn interaction_serial_is_latest_generation_one_shot_and_expiring() {
        let now = Instant::now();
        let mut serial = ActivationSerial::default();
        assert!(serial.take(Duration::from_secs(1), now).is_none());

        serial.record(7, "old-seat", now);
        serial.record(9, "current-seat", now);
        assert_eq!(serial.take(Duration::from_secs(1), now), Some((9, "current-seat")));
        assert!(serial.take(Duration::from_secs(1), now).is_none());

        serial.record(11, "stale-seat", now);
        assert!(serial.take(Duration::from_millis(5), now + Duration::from_millis(6)).is_none());

        serial.record(13, "removed-seat", now);
        serial.invalidate_if(|seat| *seat == "other-seat");
        assert_eq!(serial.take(Duration::from_secs(1), now), Some((13, "removed-seat")));
        serial.record(15, "removed-seat", now);
        serial.invalidate_if(|seat| *seat == "removed-seat");
        assert!(serial.take(Duration::from_secs(1), now).is_none());
    }
}

// NOTE: Rust doesn't allow `From<Option<Theme>>`.
#[cfg(feature = "sctk-adwaita")]
fn into_sctk_adwaita_config(theme: Option<Theme>) -> sctk_adwaita::FrameConfig {
    match theme {
        Some(Theme::Light) => sctk_adwaita::FrameConfig::light(),
        Some(Theme::Dark) => sctk_adwaita::FrameConfig::dark(),
        None => sctk_adwaita::FrameConfig::auto(),
    }
}

#[cfg(test)]
mod frame_pacing_tests {
    use super::FramePacing;

    const HZ_144: Option<u32> = Some(143_999);
    const HZ_60: Option<u32> = Some(59_950);

    fn feed(
        pacing: &mut FramePacing,
        intervals_ms: &[u32],
        redraw_pending: bool,
        refresh: Option<u32>,
    ) {
        let mut time = pacing.last_done_ms.unwrap_or_else(|| {
            pacing.frame_done(1_000, redraw_pending, refresh);
            1_000
        });
        for interval in intervals_ms {
            time = time.wrapping_add(*interval);
            pacing.frame_done(time, redraw_pending, refresh);
        }
    }

    #[test]
    fn repeated_missed_refreshes_while_redrawing_enable_pipelining() {
        let mut pacing = FramePacing::default();
        feed(&mut pacing, &[14, 21], true, HZ_144);
        assert_eq!(pacing.limit(), 1);
        feed(&mut pacing, &[14], true, HZ_144);
        assert_eq!(pacing.limit(), 2);
    }

    #[test]
    fn frames_that_fit_the_refresh_keep_one_callback() {
        let mut pacing = FramePacing::default();
        feed(&mut pacing, &[17, 16, 17, 17, 16, 17], true, HZ_60);
        assert_eq!(pacing.limit(), 1);
        // 21 ms is late at 144 Hz but not at 60 Hz.
        feed(&mut pacing, &[21, 21, 21], true, HZ_60);
        assert_eq!(pacing.limit(), 1);
    }

    #[test]
    fn a_frame_on_time_resets_the_late_count() {
        let mut pacing = FramePacing::default();
        feed(&mut pacing, &[14, 14, 7, 14, 14], true, HZ_144);
        assert_eq!(pacing.limit(), 1);
    }

    #[test]
    fn slow_one_off_redraws_and_unknown_refresh_never_pipeline() {
        let mut pacing = FramePacing::default();
        feed(&mut pacing, &[40, 40, 40, 40], false, HZ_144);
        assert_eq!(pacing.limit(), 1);
        feed(&mut pacing, &[40, 40, 40, 40], true, None);
        assert_eq!(pacing.limit(), 1);
    }

    #[test]
    fn an_idle_gap_restores_one_callback() {
        let mut pacing = FramePacing::default();
        feed(&mut pacing, &[14, 14, 14], true, HZ_144);
        assert_eq!(pacing.limit(), 2);
        feed(&mut pacing, &[7, 7, 7], true, HZ_144);
        assert_eq!(pacing.limit(), 2);
        feed(&mut pacing, &[101], false, HZ_144);
        assert_eq!(pacing.limit(), 1);
    }

    #[test]
    fn compositor_clock_wraparound_is_a_short_interval() {
        let mut pacing = FramePacing { last_done_ms: Some(u32::MAX - 3), ..FramePacing::default() };
        feed(&mut pacing, &[14, 14, 14], true, HZ_144);
        assert_eq!(pacing.limit(), 2);
    }

    #[test]
    fn a_forced_limit_overrides_the_policy() {
        let mut pacing = FramePacing { forced_limit: Some(1), ..FramePacing::default() };
        feed(&mut pacing, &[14, 14, 14], true, HZ_144);
        assert_eq!(pacing.limit(), 1);
        let pacing = FramePacing { forced_limit: Some(2), ..FramePacing::default() };
        assert_eq!(pacing.limit(), 2);
    }
}

/// The shell role of a window's surface: an `xdg_toplevel` or an `xdg_popup`.
///
/// Toplevel requests (title, size limits, maximizing, decorations, the window
/// menu, interactive move and resize) do nothing on a popup.
#[derive(Debug, Clone)]
pub enum ShellSurface {
    Toplevel(Window),
    Popup(sctk::shell::xdg::popup::Popup),
}

impl ShellSurface {
    pub fn wl_surface(&self) -> &WlSurface {
        match self {
            Self::Toplevel(window) => window.wl_surface(),
            Self::Popup(popup) => popup.wl_surface(),
        }
    }

    pub fn xdg_surface(
        &self,
    ) -> &sctk::reexports::protocols::xdg::shell::client::xdg_surface::XdgSurface {
        match self {
            Self::Toplevel(window) => window.xdg_surface(),
            Self::Popup(popup) => popup.xdg_surface(),
        }
    }

    pub fn toplevel(&self) -> Option<&Window> {
        match self {
            Self::Toplevel(window) => Some(window),
            Self::Popup(_) => None,
        }
    }

    pub fn xdg_toplevel(
        &self,
    ) -> Option<&sctk::reexports::protocols::xdg::shell::client::xdg_toplevel::XdgToplevel> {
        self.toplevel().map(Window::xdg_toplevel)
    }

    pub fn set_buffer_scale(&self, scale: u32) -> Result<(), sctk::shell::Unsupported> {
        match self {
            Self::Toplevel(window) => window.set_buffer_scale(scale),
            Self::Popup(popup) => popup.set_buffer_scale(scale),
        }
    }

    pub fn commit(&self) {
        self.wl_surface().commit();
    }

    pub fn set_minimized(&self) {
        if let Some(window) = self.toplevel() {
            window.set_minimized();
        }
    }

    pub fn set_maximized(&self) {
        if let Some(window) = self.toplevel() {
            window.set_maximized();
        }
    }

    pub fn unset_maximized(&self) {
        if let Some(window) = self.toplevel() {
            window.unset_maximized();
        }
    }

    pub fn resize(&self, seat: &WlSeat, serial: u32, edge: XdgResizeEdge) {
        if let Some(window) = self.toplevel() {
            window.resize(seat, serial, edge);
        }
    }

    pub fn move_(&self, seat: &WlSeat, serial: u32) {
        if let Some(window) = self.toplevel() {
            window.move_(seat, serial);
        }
    }

    pub fn show_window_menu(&self, seat: &WlSeat, serial: u32, position: (i32, i32)) {
        if let Some(window) = self.toplevel() {
            window.show_window_menu(seat, serial, position);
        }
    }

    pub fn set_min_size(&self, size: Option<(u32, u32)>) {
        if let Some(window) = self.toplevel() {
            window.set_min_size(size);
        }
    }

    pub fn set_max_size(&self, size: Option<(u32, u32)>) {
        if let Some(window) = self.toplevel() {
            window.set_max_size(size);
        }
    }

    pub fn request_decoration_mode(&self, mode: Option<DecorationMode>) {
        if let Some(window) = self.toplevel() {
            window.request_decoration_mode(mode);
        }
    }

    pub fn set_title(&self, title: &str) {
        if let Some(window) = self.toplevel() {
            window.set_title(title);
        }
    }
}

/// The pointer shape over a resize band.
fn resize_cursor(edge: XdgResizeEdge) -> CursorIcon {
    match edge {
        XdgResizeEdge::Top => CursorIcon::NResize,
        XdgResizeEdge::Bottom => CursorIcon::SResize,
        XdgResizeEdge::Left => CursorIcon::WResize,
        XdgResizeEdge::Right => CursorIcon::EResize,
        XdgResizeEdge::TopLeft => CursorIcon::NwResize,
        XdgResizeEdge::TopRight => CursorIcon::NeResize,
        XdgResizeEdge::BottomLeft => CursorIcon::SwResize,
        XdgResizeEdge::BottomRight => CursorIcon::SeResize,
        _ => CursorIcon::Default,
    }
}
