// Modified by enzUI contributors. See FORK.md.
//! Handling of xdg activation, which is used for user attention requests.

use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, Weak};

use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{delegate_dispatch, Connection, Dispatch, Proxy, QueueHandle};
use sctk::reexports::protocols::xdg::activation::v1::client::xdg_activation_token_v1::{
    Event as ActivationTokenEvent, XdgActivationTokenV1,
};
use sctk::reexports::protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;

use sctk::globals::GlobalData;

use crate::event_loop::AsyncRequestSerial;
use crate::platform_impl::wayland::state::WinitState;
use crate::platform_impl::WindowId;
use crate::window::ActivationToken;

const MAX_PENDING_ACTIVATION_REQUESTS: usize = 32;

pub(crate) struct ActivationRequests<Key, Token> {
    pending: Vec<(Key, Token)>,
}

impl<Key: PartialEq, Token> ActivationRequests<Key, Token> {
    pub(crate) fn insert(&mut self, key: Key, token: Token) -> Result<(), Token> {
        if self.pending.len() >= MAX_PENDING_ACTIVATION_REQUESTS
            || self.pending.iter().any(|(candidate, _)| candidate == &key)
        {
            Err(token)
        } else {
            self.pending.push((key, token));
            Ok(())
        }
    }

    fn take(&mut self, key: &Key) -> Option<Token> {
        let index = self.pending.iter().position(|(candidate, _)| candidate == key)?;
        Some(self.pending.swap_remove(index).1)
    }

    pub(crate) fn complete(&mut self, key: &Key) -> Option<Token> {
        self.take(key)
    }

    pub(crate) fn cancel(&mut self, key: &Key) -> Option<Token> {
        self.take(key)
    }

    pub(crate) fn take_all(&mut self) -> Vec<Token> {
        self.pending.drain(..).map(|(_, token)| token).collect()
    }
}

impl<Key, Token> Default for ActivationRequests<Key, Token> {
    fn default() -> Self {
        Self { pending: Vec::new() }
    }
}

pub(crate) type PendingActivationRequests =
    Mutex<ActivationRequests<AsyncRequestSerial, XdgActivationTokenV1>>;

pub struct XdgActivationState {
    xdg_activation: XdgActivationV1,
}

impl XdgActivationState {
    pub fn bind(
        globals: &GlobalList,
        queue_handle: &QueueHandle<WinitState>,
    ) -> Result<Self, BindError> {
        let xdg_activation = globals.bind(queue_handle, 1..=1, GlobalData)?;
        Ok(Self { xdg_activation })
    }

    pub fn global(&self) -> &XdgActivationV1 {
        &self.xdg_activation
    }
}

impl Dispatch<XdgActivationV1, GlobalData, WinitState> for XdgActivationState {
    fn event(
        _state: &mut WinitState,
        _proxy: &XdgActivationV1,
        _event: <XdgActivationV1 as Proxy>::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &QueueHandle<WinitState>,
    ) {
    }
}

impl Dispatch<XdgActivationTokenV1, XdgActivationTokenData, WinitState> for XdgActivationState {
    fn event(
        state: &mut WinitState,
        proxy: &XdgActivationTokenV1,
        event: <XdgActivationTokenV1 as Proxy>::Event,
        data: &XdgActivationTokenData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        let token = match event {
            ActivationTokenEvent::Done { token } => token,
            _ => return,
        };

        let global = state
            .xdg_activation
            .as_ref()
            .expect("got xdg_activation event without global.")
            .global();

        match data {
            XdgActivationTokenData::Attention((surface, fence)) => {
                global.activate(token, surface);
                // Mark that no request attention is in process.
                if let Some(attention_requested) = fence.upgrade() {
                    attention_requested.store(false, std::sync::atomic::Ordering::Relaxed);
                }
            },
            XdgActivationTokenData::Obtain { window_id, serial, pending } => {
                let Some(pending) = pending.upgrade() else {
                    return;
                };
                let Some(request) = pending.lock().unwrap().complete(serial) else {
                    return;
                };
                request.destroy();
                state.events_sink.push_window_event(
                    crate::event::WindowEvent::ActivationTokenDone {
                        serial: *serial,
                        token: ActivationToken::from_raw(token),
                    },
                    *window_id,
                );
                return;
            },
        }

        proxy.destroy();
    }
}

/// The data associated with the activation request.
pub enum XdgActivationTokenData {
    /// Request user attention for the given surface.
    Attention((WlSurface, Weak<AtomicBool>)),
    /// Get a token to be passed outside of the winit.
    Obtain {
        window_id: WindowId,
        serial: AsyncRequestSerial,
        pending: Weak<PendingActivationRequests>,
    },
}

#[cfg(test)]
mod activation_native_tests {
    use super::ActivationRequests;

    #[test]
    fn withheld_completion_and_cancel_are_bounded_and_idempotent() {
        let mut requests = ActivationRequests::default();
        assert_eq!(requests.insert(0_u64, 0), Ok(()));
        assert_eq!(requests.insert(0, 99), Err(99));
        for key in 1_u64..32 {
            assert_eq!(requests.insert(key, key), Ok(()));
        }
        assert_eq!(requests.insert(32, 32), Err(32));
        for key in 0_u64..32 {
            assert_eq!(requests.cancel(&key), Some(key));
            assert_eq!(requests.complete(&key), None);
        }
        assert!(requests.take_all().is_empty());

        let mut requests = ActivationRequests::default();
        assert_eq!(requests.insert(33_u64, "completed-first"), Ok(()));
        assert_eq!(requests.complete(&33), Some("completed-first"));
        assert_eq!(requests.cancel(&33), None);

        assert_eq!(requests.insert(34, "closed"), Ok(()));
        assert_eq!(requests.take_all(), ["closed"]);
        assert!(requests.take_all().is_empty());
    }
}

delegate_dispatch!(WinitState: [ XdgActivationV1: GlobalData] => XdgActivationState);
delegate_dispatch!(WinitState: [ XdgActivationTokenV1: XdgActivationTokenData] => XdgActivationState);
