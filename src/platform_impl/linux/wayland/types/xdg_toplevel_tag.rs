// Modified by enzUI contributors. See FORK.md.
use sctk::globals::GlobalData;
use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::client::{delegate_dispatch, Connection, Dispatch, Proxy, QueueHandle};
use sctk::reexports::protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;
use sctk::reexports::protocols::xdg::toplevel_tag::v1::client::xdg_toplevel_tag_manager_v1::XdgToplevelTagManagerV1;

use crate::platform_impl::wayland::state::WinitState;

#[derive(Clone)]
pub struct ToplevelTagManager {
    manager: XdgToplevelTagManagerV1,
}

impl ToplevelTagManager {
    pub fn bind(globals: &GlobalList, queue: &QueueHandle<WinitState>) -> Result<Self, BindError> {
        Ok(Self { manager: globals.bind(queue, 1..=1, GlobalData)? })
    }

    pub fn set(&self, toplevel: &XdgToplevel, tag: &str) {
        self.manager.set_toplevel_tag(toplevel, tag.to_owned());
    }
}

impl Dispatch<XdgToplevelTagManagerV1, GlobalData, WinitState> for ToplevelTagManager {
    fn event(
        _: &mut WinitState,
        _: &XdgToplevelTagManagerV1,
        _: <XdgToplevelTagManagerV1 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        unreachable!("xdg_toplevel_tag_manager_v1 defines no events");
    }
}

delegate_dispatch!(WinitState: [XdgToplevelTagManagerV1: GlobalData] => ToplevelTagManager);
