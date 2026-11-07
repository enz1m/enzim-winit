// Modified by enzUI contributors. See FORK.md.
//! # Wayland
//!
//! **Note:** Windows don't appear on Wayland until you draw/present to them.
//!
//! By default, Winit loads system libraries using `dlopen`. This can be
//! disabled by disabling the `"wayland-dlopen"` cargo feature.
//!
//! ## Client-side decorations
//!
//! Winit provides client-side decorations by default, but the behaviour can
//! be controlled with the following feature flags:
//!
//! * `wayland-csd-adwaita` (default).
//! * `wayland-csd-adwaita-crossfont`.
//! * `wayland-csd-adwaita-notitle`.

use std::ffi::c_void;
use std::ptr::NonNull;

use crate::event_loop::{ActiveEventLoop, EventLoop, EventLoopBuilder};
use crate::monitor::MonitorHandle;
use crate::window::{Window, WindowAttributes};

pub use crate::window::Theme;

/// Additional methods on [`ActiveEventLoop`] that are specific to Wayland.
pub trait ActiveEventLoopExtWayland {
    /// True if the [`ActiveEventLoop`] uses Wayland.
    fn is_wayland(&self) -> bool;
}

impl ActiveEventLoopExtWayland for ActiveEventLoop {
    #[inline]
    fn is_wayland(&self) -> bool {
        self.p.is_wayland()
    }
}

/// Additional methods on [`EventLoop`] that are specific to Wayland.
pub trait EventLoopExtWayland {
    /// True if the [`EventLoop`] uses Wayland.
    fn is_wayland(&self) -> bool;
}

impl<T: 'static> EventLoopExtWayland for EventLoop<T> {
    #[inline]
    fn is_wayland(&self) -> bool {
        self.event_loop.is_wayland()
    }
}

/// Additional methods on [`EventLoopBuilder`] that are specific to Wayland.
pub trait EventLoopBuilderExtWayland {
    /// Force using Wayland.
    fn with_wayland(&mut self) -> &mut Self;

    /// Whether to allow the event loop to be created off of the main thread.
    ///
    /// By default, the window is only allowed to be created on the main
    /// thread, to make platform compatibility easier.
    fn with_any_thread(&mut self, any_thread: bool) -> &mut Self;
}

impl<T> EventLoopBuilderExtWayland for EventLoopBuilder<T> {
    #[inline]
    fn with_wayland(&mut self) -> &mut Self {
        self.platform_specific.forced_backend = Some(crate::platform_impl::Backend::Wayland);
        self
    }

    #[inline]
    fn with_any_thread(&mut self, any_thread: bool) -> &mut Self {
        self.platform_specific.any_thread = any_thread;
        self
    }
}

/// The objects an XDG portal parent export needs from a Wayland window: the
/// connection the window's surface lives on, that surface, and the surface's
/// identity.
///
/// The identity is minted where the surface is created, so it names one
/// protocol object and cannot be satisfied by a later surface that reuses the
/// same numeric object id.
pub type WaylandParentObjects = (
    crate::platform_impl::wayland::Connection,
    crate::platform_impl::wayland::WlSurface,
    u64,
);

/// Additional methods on [`Window`] that are specific to Wayland.
///
/// [`Window`]: crate::window::Window
pub trait WindowExtWayland {
    /// Returns `xdg_toplevel` of the window or [`None`] if the window is X11 window.
    fn xdg_toplevel(&self) -> Option<NonNull<c_void>>;

    /// Queues an opaque `xdg_toplevel_tag` on this window's own toplevel.
    ///
    /// Returns false for X11, an unavailable protocol, or tags outside 1–128
    /// ASCII alphanumerics and `._:-`. The compositor processes the request
    /// asynchronously; no roundtrip or additional connection is opened.
    fn set_toplevel_tag(&self, tag: &str) -> bool;

    /// The window's own connection, surface and surface identity, for an XDG
    /// portal parent export; `None` for a window that is not Wayland.
    ///
    /// These are the window's existing proxies, cloned. Nothing is rebuilt
    /// from a raw pointer and no second connection is opened, so requests made
    /// with them are requests on the window's own objects. The window remains
    /// their owner: destroying it destroys the surface, and this method is the
    /// only way to reach them, so no caller can outlive that.
    fn wayland_parent_objects(&self) -> Option<WaylandParentObjects>;
}

impl WindowExtWayland for Window {
    fn set_toplevel_tag(&self, tag: &str) -> bool {
        self.window.set_toplevel_tag(tag)
    }

    #[inline]
    fn xdg_toplevel(&self) -> Option<NonNull<c_void>> {
        #[allow(clippy::single_match)]
        match &self.window {
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_) => None,
            #[cfg(wayland_platform)]
            crate::platform_impl::Window::Wayland(window) => window.xdg_toplevel(),
        }
    }

    fn wayland_parent_objects(&self) -> Option<WaylandParentObjects> {
        self.window.wayland_parent_objects()
    }
}

/// Additional methods on [`WindowAttributes`] that are specific to Wayland.
pub trait WindowAttributesExtWayland {
    /// Build window with the given name.
    ///
    /// The `general` name sets an application ID, which should match the `.desktop`
    /// file distributed with your program. The `instance` is a `no-op`.
    ///
    /// For details about application ID conventions, see the
    /// [Desktop Entry Spec](https://specifications.freedesktop.org/desktop-entry-spec/desktop-entry-spec-latest.html#desktop-file-id)
    fn with_name(self, general: impl Into<String>, instance: impl Into<String>) -> Self;

    /// Build the window as an `xdg_popup` of another window instead of a
    /// toplevel: a menu, list or tooltip that may extend past its parent's
    /// edges and that the compositor keeps on screen.
    ///
    /// The popup is [`PopupAttributes::size`] large and carries no
    /// decorations or title. Pointer events arrive with the popup's own
    /// [`WindowId`](crate::window::WindowId) in its own coordinates;
    /// [`WindowEvent::Moved`](crate::event::WindowEvent::Moved) reports where
    /// the compositor placed it relative to the parent, after any constraint
    /// adjustment, and
    /// [`WindowEvent::CloseRequested`](crate::event::WindowEvent::CloseRequested)
    /// means the compositor dismissed it. Ignored on X11.
    fn with_popup(self, popup: PopupAttributes) -> Self;

    /// Build the window as a dialog of `parent`: `xdg_toplevel.set_parent`,
    /// so the compositor keeps it above the parent and treats them as one
    /// application window, and, when the compositor offers
    /// `xdg_wm_dialog_v1`, a dialog marked modal or not. A modal dialog
    /// blocks the parent for the compositor.
    fn with_transient_parent(self, parent: crate::window::WindowId, modal: bool) -> Self;
}

impl WindowAttributesExtWayland for WindowAttributes {
    #[inline]
    fn with_name(mut self, general: impl Into<String>, instance: impl Into<String>) -> Self {
        self.platform_specific.name =
            Some(crate::platform_impl::ApplicationName::new(general.into(), instance.into()));
        self
    }

    #[inline]
    fn with_popup(mut self, popup: PopupAttributes) -> Self {
        self.platform_specific.popup = Some(popup);
        self
    }

    #[inline]
    fn with_transient_parent(mut self, parent: crate::window::WindowId, modal: bool) -> Self {
        self.platform_specific.transient_parent = Some((parent, modal));
        self
    }
}

/// Where an `xdg_popup` goes, in its parent's logical coordinates.
///
/// The positioner rule: take `anchor_rect`, pick the point `anchor` names on
/// it, add `offset`, and grow the popup away from that point in the
/// direction of `gravity`. When that would put it off screen, the compositor
/// applies `constraint_adjustment`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PopupAttributes {
    /// The window this popup belongs to.
    pub parent: crate::window::WindowId,
    /// The rectangle the popup is placed against: x, y, width, height.
    pub anchor_rect: (i32, i32, i32, i32),
    /// The popup's size: width, height.
    pub size: (i32, i32),
    /// The point on `anchor_rect` the popup is placed against.
    pub anchor: PopupEdge,
    /// The direction the popup grows from that point.
    pub gravity: PopupEdge,
    /// Added to the anchor point.
    pub offset: (i32, i32),
    /// What the compositor may do when the popup does not fit on screen.
    pub constraint_adjustment: PopupConstraint,
    /// Take an explicit grab with the parent's latest input serial: the popup
    /// gets keyboard focus and is dismissed by a click anywhere else.
    pub grab: bool,
}

/// An edge or corner of a rectangle, for [`PopupAttributes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PopupEdge {
    /// The centre.
    #[default]
    None,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    BottomLeft,
    TopRight,
    BottomRight,
}

/// Ways the compositor may adjust a popup that does not fit on screen, as
/// `xdg_positioner.constraint_adjustment` bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PopupConstraint(pub u32);

impl PopupConstraint {
    /// Keep the popup where asked, even partly off screen.
    pub const NONE: Self = Self(0);
    /// Slide horizontally until it fits.
    pub const SLIDE_X: Self = Self(1);
    /// Slide vertically until it fits.
    pub const SLIDE_Y: Self = Self(2);
    /// Mirror the anchor and gravity horizontally.
    pub const FLIP_X: Self = Self(4);
    /// Mirror the anchor and gravity vertically.
    pub const FLIP_Y: Self = Self(8);
    /// Shrink horizontally.
    pub const RESIZE_X: Self = Self(16);
    /// Shrink vertically.
    pub const RESIZE_Y: Self = Self(32);

    /// Both adjustments.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Additional methods on `MonitorHandle` that are specific to Wayland.
pub trait MonitorHandleExtWayland {
    /// Returns the inner identifier of the monitor.
    fn native_id(&self) -> u32;
}

impl MonitorHandleExtWayland for MonitorHandle {
    #[inline]
    fn native_id(&self) -> u32 {
        self.inner.native_identifier()
    }
}
