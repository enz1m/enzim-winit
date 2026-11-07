// Modified by enzUI contributors. See FORK.md.
//! Window startup notification to handle window raising.
//!
//! The [`ActivationToken`] is essential to ensure that your newly
//! created window will obtain the focus, otherwise the user could
//! be requered to click on the window.
//!
//! Such token is usually delivered via the environment variable and
//! could be read from it with the [`EventLoopExtStartupNotify::read_token_from_env`].
//!
//! Such token must also be reset after reading it from your environment with
//! [`reset_activation_token_env`] otherwise child processes could inherit it.
//!
//! When starting a new child process with a newly obtained [`ActivationToken`] from
//! [`WindowExtStartupNotify::request_activation_token`] the [`set_activation_token_env`]
//! must be used to propagate it to the child
//!
//! To ensure the delivery of such token by other processes to you, the user should
//! set `StartupNotify=true` inside the `.desktop` file of their application.
//!
//! The specification could be found [`here`].
//!
//! [`here`]: https://specifications.freedesktop.org/startup-notification-spec/startup-notification-latest.txt

use std::env;

use crate::error::NotSupportedError;
use crate::event_loop::{ActiveEventLoop, AsyncRequestSerial};
use crate::window::{ActivationToken, Window, WindowAttributes};

/// The variable which is used mostly on X11.
const X11_VAR: &str = "DESKTOP_STARTUP_ID";

/// The variable which is used mostly on Wayland.
const WAYLAND_VAR: &str = "XDG_ACTIVATION_TOKEN";

pub trait EventLoopExtStartupNotify {
    /// Read the token from the environment.
    ///
    /// It's recommended **to unset** this environment variable for child processes.
    fn read_token_from_env(&self) -> Option<ActivationToken>;
}

pub trait WindowExtStartupNotify {
    /// Requests a new activation token.
    ///
    /// The result is delivered as a window event. On Wayland, the request uses
    /// the most recent unconsumed input serial for this surface when available.
    fn request_activation_token(&self) -> Result<AsyncRequestSerial, NotSupportedError>;

    /// Cancels an outstanding token request.
    ///
    /// Returns whether this call owned and destroyed the request.
    fn cancel_activation_token_request(&self, serial: AsyncRequestSerial) -> bool;

    /// Activates this window with a token received from another client.
    ///
    /// Success only means the request was submitted; compositor policy decides
    /// whether the surface is activated.
    fn activate_with_token(&self, token: ActivationToken) -> Result<(), NotSupportedError>;
}

pub trait WindowAttributesExtStartupNotify {
    /// Use this [`ActivationToken`] during window creation.
    ///
    /// Not using such a token upon a window could make your window not gaining
    /// focus until the user clicks on the window.
    fn with_activation_token(self, token: ActivationToken) -> Self;
}

impl EventLoopExtStartupNotify for ActiveEventLoop {
    fn read_token_from_env(&self) -> Option<ActivationToken> {
        match self.p {
            #[cfg(wayland_platform)]
            crate::platform_impl::ActiveEventLoop::Wayland(_) => env::var(WAYLAND_VAR),
            #[cfg(x11_platform)]
            crate::platform_impl::ActiveEventLoop::X(_) => env::var(X11_VAR),
        }
        .ok()
        .map(ActivationToken::from_raw)
    }
}

impl WindowExtStartupNotify for Window {
    fn request_activation_token(&self) -> Result<AsyncRequestSerial, NotSupportedError> {
        self.window.request_activation_token()
    }

    fn cancel_activation_token_request(&self, serial: AsyncRequestSerial) -> bool {
        self.window.cancel_activation_token_request(serial)
    }

    fn activate_with_token(&self, token: ActivationToken) -> Result<(), NotSupportedError> {
        self.window.activate_with_token(token)
    }
}

impl WindowAttributesExtStartupNotify for WindowAttributes {
    fn with_activation_token(mut self, token: ActivationToken) -> Self {
        self.platform_specific.activation_token = Some(token);
        self
    }
}

/// Remove the activation environment variables from the current process.
///
/// This is wise to do before running child processes,
/// which may not to support the activation token.
pub fn reset_activation_token_env() {
    env::remove_var(X11_VAR);
    env::remove_var(WAYLAND_VAR);
}

/// Set environment variables responsible for activation token.
///
/// This could be used before running daemon processes.
pub fn set_activation_token_env(token: ActivationToken) {
    env::set_var(X11_VAR, &token.token);
    env::set_var(WAYLAND_VAR, token.token);
}

#[cfg(test)]
mod activation_native_tests {
    use super::{
        ActivationToken, NotSupportedError, Window, WindowAttributes,
        WindowAttributesExtStartupNotify, WindowExtStartupNotify,
    };
    use crate::event_loop::AsyncRequestSerial;

    // `check` resolves only while `T` is not `Copy`: a `Copy` type satisfies
    // both impls and `Marker` stops being inferable.
    trait AmbiguousIfCopy<Marker> {
        fn check() {}
    }
    impl<T: ?Sized> AmbiguousIfCopy<()> for T {}
    impl<T: ?Sized + Copy> AmbiguousIfCopy<u8> for T {}

    #[test]
    fn request_generation_and_existing_surface_token_are_owned_once() {
        let first = AsyncRequestSerial::get();
        let second = AsyncRequestSerial::get();
        assert_ne!(first, second);

        // `WindowAttributes` is `Clone` and carries an optional token, so a
        // `!Clone` token is impossible here; iced's `core::window`
        // `ActivationToken` holds that stronger guarantee.
        let _: fn(&WindowAttributes) -> WindowAttributes = Clone::clone;
        let _: fn(&ActivationToken) -> ActivationToken = Clone::clone;
        let _ = <ActivationToken as AmbiguousIfCopy<_>>::check;

        let _: fn(&Window, ActivationToken) -> Result<(), NotSupportedError> =
            <Window as WindowExtStartupNotify>::activate_with_token;
        let _: fn(WindowAttributes, ActivationToken) -> WindowAttributes =
            <WindowAttributes as WindowAttributesExtStartupNotify>::with_activation_token;
        let _: fn(ActivationToken) -> String = ActivationToken::into_raw;

        let mut handoff = Some(ActivationToken::from_raw("one-shot".into()));
        assert_eq!(handoff.take().unwrap().into_raw(), "one-shot");
        assert!(handoff.take().is_none());
    }
}
