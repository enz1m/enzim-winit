// Modified by enzUI contributors. See FORK.md.
//! Window icons through `xdg_toplevel_icon_v1`.

use sctk::globals::GlobalData;
use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::client::protocol::wl_shm::Format;
use sctk::reexports::client::{delegate_dispatch, Connection, Dispatch, Proxy, QueueHandle};
use sctk::reexports::protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;
use sctk::reexports::protocols::xdg::toplevel_icon::v1::client::xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1;
use sctk::reexports::protocols::xdg::toplevel_icon::v1::client::xdg_toplevel_icon_v1::XdgToplevelIconV1;
use sctk::shm::slot::{Buffer, SlotPool};

use crate::icon::RgbaIcon;
use crate::platform_impl::wayland::state::WinitState;
use crate::platform_impl::wayland::types::cursor::write_premultiplied_argb;

#[derive(Clone)]
pub struct ToplevelIconManager {
    manager: XdgToplevelIconManagerV1,
    queue: QueueHandle<WinitState>,
}

impl ToplevelIconManager {
    pub fn bind(globals: &GlobalList, queue: &QueueHandle<WinitState>) -> Result<Self, BindError> {
        Ok(Self { manager: globals.bind(queue, 1..=1, GlobalData)?, queue: queue.clone() })
    }

    /// Shows `icon` for `toplevel`, or the compositor's default for `None`.
    /// The returned buffer holds the pixels and must live as long as the icon
    /// is shown.
    pub(crate) fn set(
        &self,
        toplevel: &XdgToplevel,
        icon: Option<&RgbaIcon>,
        pool: &mut SlotPool,
    ) -> Option<Buffer> {
        let Some(icon) = icon else {
            self.manager.set_icon(toplevel, None);
            return None;
        };
        let (side, square) = square_icon(icon);
        let side_i32 = i32::try_from(side).ok()?;
        let (buffer, canvas) =
            pool.create_buffer(side_i32, side_i32, 4 * side_i32, Format::Argb8888).ok()?;
        write_premultiplied_argb(canvas, &square);
        let object = self.manager.create_icon(&self.queue, GlobalData);
        object.add_buffer(buffer.wl_buffer(), 1);
        self.manager.set_icon(toplevel, Some(&object));
        // The icon applies once set; the object itself is no longer needed.
        object.destroy();
        Some(buffer)
    }
}

/// The icon centred on a transparent square, as the protocol requires square
/// buffers.
fn square_icon(icon: &RgbaIcon) -> (u32, Vec<u8>) {
    let side = icon.width.max(icon.height);
    if icon.width == icon.height {
        return (side, icon.rgba.clone());
    }
    let mut square = vec![0u8; (side * side * 4) as usize];
    let left = (side - icon.width) / 2;
    let top = (side - icon.height) / 2;
    for row in 0..icon.height {
        let from = (row * icon.width * 4) as usize;
        let to = (((top + row) * side + left) * 4) as usize;
        let len = (icon.width * 4) as usize;
        square[to..to + len].copy_from_slice(&icon.rgba[from..from + len]);
    }
    (side, square)
}

impl Dispatch<XdgToplevelIconManagerV1, GlobalData, WinitState> for ToplevelIconManager {
    fn event(
        _: &mut WinitState,
        _: &XdgToplevelIconManagerV1,
        _: <XdgToplevelIconManagerV1 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        // `icon_size` and `done` list preferred sizes; any square size works.
    }
}

impl Dispatch<XdgToplevelIconV1, GlobalData, WinitState> for ToplevelIconManager {
    fn event(
        _: &mut WinitState,
        _: &XdgToplevelIconV1,
        _: <XdgToplevelIconV1 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        unreachable!("xdg_toplevel_icon_v1 defines no events");
    }
}

delegate_dispatch!(WinitState: [XdgToplevelIconManagerV1: GlobalData] => ToplevelIconManager);
delegate_dispatch!(WinitState: [XdgToplevelIconV1: GlobalData] => ToplevelIconManager);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wide_icon_is_centred_on_a_square() {
        // 3x1: red, green, blue.
        let wide = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255];
        let icon = RgbaIcon::from_rgba(wide.clone(), 3, 1).unwrap();
        let (side, square) = square_icon(&icon);
        assert_eq!(side, 3);
        assert_eq!(&square[..12], &[0; 12], "the top row is padding");
        assert_eq!(&square[12..24], &wide[..], "the middle row is the icon");
        assert_eq!(&square[24..], &[0; 12], "the bottom row is padding");
        let tall = RgbaIcon::from_rgba(vec![9; 3 * 4], 1, 3).unwrap();
        let (side, square) = square_icon(&tall);
        assert_eq!(side, 3);
        let column: Vec<u8> = (0..3).map(|row| square[(row * 3 + 1) * 4]).collect();
        assert_eq!(column, [9, 9, 9], "the middle column carries the icon");
        assert_eq!(square[0], 0);
    }
}
