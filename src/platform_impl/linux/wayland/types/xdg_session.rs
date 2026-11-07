// Modified by enzUI contributors. See FORK.md.
//! Window placement restored across launches through
//! `xdg_session_manager_v1`.
//!
//! One session per process: the first window that names a session opens it
//! (`get_session`, reason `launch`), and every window that names a toplevel
//! asks for its old placement with `restore_toplevel` before its first
//! commit; a name the session does not know is simply added. The compositor
//! answers `created` with a new session id, or `restored` for the id given.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

#[allow(dead_code, non_camel_case_types, non_upper_case_globals, unused_imports, missing_docs)]
#[allow(clippy::all)]
mod protocol {
    use sctk::reexports::client as wayland_client;
    use sctk::reexports::client::protocol::*;
    use sctk::reexports::protocols::xdg::shell::client::*;
    pub mod __interfaces {
        use sctk::reexports::client::backend as wayland_backend;
        use sctk::reexports::client::protocol::__interfaces::*;
        use sctk::reexports::protocols::xdg::shell::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("wayland-protocols/xdg-session-management-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("wayland-protocols/xdg-session-management-v1.xml");
}

use protocol::xdg_session_manager_v1::{Reason, XdgSessionManagerV1};
use protocol::xdg_session_v1::{self, XdgSessionV1};
use protocol::xdg_toplevel_session_v1::XdgToplevelSessionV1;
use sctk::globals::GlobalData;
use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::client::{delegate_dispatch, Connection, Dispatch, Proxy, QueueHandle};
use sctk::reexports::protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;
use tracing::warn;

use crate::platform_impl::wayland::state::WinitState;

/// The bound session manager and the process's session, once opened.
pub struct SessionManager {
    manager: XdgSessionManagerV1,
    session: Option<(XdgSessionV1, Arc<SessionData>)>,
}

/// What the compositor said about the session, and the toplevel names in use.
#[derive(Debug, Default)]
pub struct SessionData {
    id: Mutex<Option<String>>,
    names: Mutex<HashSet<String>>,
}

impl SessionData {
    /// The id to restore this session with next time.
    pub fn id(&self) -> Option<String> {
        self.id.lock().unwrap().clone()
    }
}

/// A window's place in the session; leaving it keeps the stored placement.
#[derive(Debug)]
pub struct ToplevelSession {
    object: XdgToplevelSessionV1,
    name: String,
    data: Arc<SessionData>,
}

impl ToplevelSession {
    pub fn session(&self) -> &Arc<SessionData> {
        &self.data
    }
}

impl Drop for ToplevelSession {
    fn drop(&mut self) {
        self.object.destroy();
        self.data.names.lock().unwrap().remove(&self.name);
    }
}

impl SessionManager {
    pub fn bind(globals: &GlobalList, queue: &QueueHandle<WinitState>) -> Result<Self, BindError> {
        Ok(Self { manager: globals.bind(queue, 1..=1, GlobalData)?, session: None })
    }

    /// Asks for `toplevel`'s placement under `name` in the session `session_id`
    /// (a new one for `None`); call before the toplevel's first commit.
    /// `None` when the name is already taken by a live window, which would be a
    /// protocol error.
    pub fn restore(
        &mut self,
        session_id: Option<&str>,
        toplevel: &XdgToplevel,
        name: &str,
        queue: &QueueHandle<WinitState>,
    ) -> Option<ToplevelSession> {
        let (session, data) = self.session.get_or_insert_with(|| {
            let data = Arc::new(SessionData {
                id: Mutex::new(session_id.map(str::to_owned)),
                names: Mutex::default(),
            });
            let session = self.manager.get_session(
                Reason::Launch,
                session_id.map(str::to_owned),
                queue,
                Arc::clone(&data),
            );
            (session, data)
        });
        if session_id.is_some() && data.id().as_deref() != session_id {
            warn!("this process already has a Wayland session; {session_id:?} is not restored");
        }
        if name.is_empty() || !data.names.lock().unwrap().insert(name.to_owned()) {
            warn!("the toplevel name {name:?} is empty or already in use in the session");
            return None;
        }
        let object = session.restore_toplevel(toplevel, name.to_owned(), queue, GlobalData);
        Some(ToplevelSession { object, name: name.to_owned(), data: Arc::clone(data) })
    }
}

impl Dispatch<XdgSessionManagerV1, GlobalData, WinitState> for SessionManager {
    fn event(
        _: &mut WinitState,
        _: &XdgSessionManagerV1,
        _: <XdgSessionManagerV1 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        unreachable!("xdg_session_manager_v1 defines no events");
    }
}

impl Dispatch<XdgSessionV1, Arc<SessionData>, WinitState> for SessionManager {
    fn event(
        _: &mut WinitState,
        _: &XdgSessionV1,
        event: xdg_session_v1::Event,
        data: &Arc<SessionData>,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        match event {
            xdg_session_v1::Event::Created { session_id } => {
                *data.id.lock().unwrap() = Some(session_id);
            },
            // Taken over by another instance: this one stores nothing more.
            xdg_session_v1::Event::Replaced => *data.id.lock().unwrap() = None,
            _ => {},
        }
    }
}

impl Dispatch<XdgToplevelSessionV1, GlobalData, WinitState> for SessionManager {
    fn event(
        _: &mut WinitState,
        _: &XdgToplevelSessionV1,
        _: <XdgToplevelSessionV1 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        // `restored` precedes the first configure, which carries the placement.
    }
}

delegate_dispatch!(WinitState: [XdgSessionManagerV1: GlobalData] => SessionManager);
delegate_dispatch!(WinitState: [XdgSessionV1: Arc<SessionData>] => SessionManager);
delegate_dispatch!(WinitState: [XdgToplevelSessionV1: GlobalData] => SessionManager);
