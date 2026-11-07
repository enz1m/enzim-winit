// Modified by enzUI contributors. See FORK.md.
//! `xdg_wm_dialog_v1`: marks a toplevel as a dialog of its parent and its
//! modality.

use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::client::{delegate_dispatch, Connection, Dispatch, Proxy, QueueHandle};
use sctk::reexports::protocols::xdg::dialog::v1::client::xdg_dialog_v1::XdgDialogV1;
use sctk::reexports::protocols::xdg::dialog::v1::client::xdg_wm_dialog_v1::XdgWmDialogV1;
use sctk::reexports::protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;

use crate::platform_impl::wayland::state::WinitState;

#[derive(Clone)]
pub struct DialogManager {
    manager: XdgWmDialogV1,
}

impl DialogManager {
    pub fn bind(globals: &GlobalList, queue: &QueueHandle<WinitState>) -> Result<Self, BindError> {
        Ok(Self { manager: globals.bind(queue, 1..=1, DialogGlobal)? })
    }

    /// Marks `toplevel` as a dialog, modal or not, until the result drops.
    pub fn dialog(
        &self,
        toplevel: &XdgToplevel,
        modal: bool,
        queue: &QueueHandle<WinitState>,
    ) -> Dialog {
        let dialog = self.manager.get_xdg_dialog(toplevel, queue, ());
        if modal {
            dialog.set_modal();
        }
        Dialog(dialog)
    }
}

/// An `xdg_dialog_v1`, destroyed on drop.
pub struct Dialog(XdgDialogV1);

impl Drop for Dialog {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

/// The user data of the bound `xdg_wm_dialog_v1`.
pub struct DialogGlobal;

impl Dispatch<XdgWmDialogV1, DialogGlobal, WinitState> for DialogManager {
    fn event(
        _: &mut WinitState,
        _: &XdgWmDialogV1,
        _: <XdgWmDialogV1 as Proxy>::Event,
        _: &DialogGlobal,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        unreachable!("xdg_wm_dialog_v1 defines no events");
    }
}

impl Dispatch<XdgDialogV1, (), WinitState> for DialogManager {
    fn event(
        _: &mut WinitState,
        _: &XdgDialogV1,
        _: <XdgDialogV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        unreachable!("xdg_dialog_v1 defines no events");
    }
}

delegate_dispatch!(WinitState: [XdgWmDialogV1: DialogGlobal] => DialogManager);
delegate_dispatch!(WinitState: [XdgDialogV1: ()] => DialogManager);
