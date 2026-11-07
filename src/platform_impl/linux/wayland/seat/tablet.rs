// Modified by enzUI contributors. See FORK.md.
//! Drawing tablets (`zwp_tablet_v2`): pens, erasers and pucks.
//!
//! Each tool's events accumulate until its `frame` and then arrive as one
//! [`WindowEvent::TabletTool`], followed by the pointer events the tool stands
//! in for: a compositor stops sending `wl_pointer` events for a tool once the
//! client binds the tablet seat, so without them widgets that only read the
//! mouse would not see the pen at all. The tool's cursor follows the window's
//! named cursor through `wp_cursor_shape_v1`.

use std::sync::{Arc, Mutex};

use ahash::AHashMap;
use cursor_icon::CursorIcon;
use sctk::compositor::SurfaceData;
use sctk::dispatch2::Dispatch2;
use sctk::globals::GlobalData;
use sctk::reexports::client::backend::{ObjectData, ObjectId};
use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::{Connection, Proxy, QueueHandle, WEnum};
use sctk::reexports::protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
    Shape, WpCursorShapeDeviceV1,
};
use sctk::reexports::protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;
use sctk::reexports::protocols::wp::tablet::zv2::client::zwp_tablet_manager_v2::ZwpTabletManagerV2;
use sctk::reexports::protocols::wp::tablet::zv2::client::zwp_tablet_pad_group_v2::{
    self, ZwpTabletPadGroupV2,
};
use sctk::reexports::protocols::wp::tablet::zv2::client::zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2;
use sctk::reexports::protocols::wp::tablet::zv2::client::zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2;
use sctk::reexports::protocols::wp::tablet::zv2::client::zwp_tablet_pad_v2::{
    self, ZwpTabletPadV2,
};
use sctk::reexports::protocols::wp::tablet::zv2::client::zwp_tablet_seat_v2::{
    self, ZwpTabletSeatV2,
};
use sctk::reexports::protocols::wp::tablet::zv2::client::zwp_tablet_tool_v2::{
    self, ZwpTabletToolV2,
};
use sctk::reexports::protocols::wp::tablet::zv2::client::zwp_tablet_v2::{self, ZwpTabletV2};

use crate::dpi::{LogicalPosition, PhysicalPosition};
use crate::event::{
    ElementState, MouseButton, TabletTool, TabletToolEvent, TabletToolKind, TabletToolPhase,
    WindowEvent,
};
use crate::platform_impl::wayland::state::WinitState;
use crate::platform_impl::wayland::{self, DeviceId, WindowId};

/// `BTN_STYLUS`, the lower button on the barrel.
const BTN_STYLUS: u32 = 0x14b;
/// `BTN_STYLUS2`, the upper button on the barrel.
const BTN_STYLUS2: u32 = 0x14c;

/// The bound tablet manager and the tablet seat of each `wl_seat`.
#[derive(Debug)]
pub struct TabletState {
    manager: ZwpTabletManagerV2,
    cursor_shapes: Option<WpCursorShapeManagerV1>,
    seats: AHashMap<ObjectId, ZwpTabletSeatV2>,
}

impl TabletState {
    pub fn new(globals: &GlobalList, queue: &QueueHandle<WinitState>) -> Result<Self, BindError> {
        let manager = globals.bind(queue, 1..=1, GlobalData)?;
        let cursor_shapes = globals.bind(queue, 1..=1, GlobalData).ok();
        Ok(Self { manager, cursor_shapes, seats: AHashMap::new() })
    }

    /// Starts listening to the tablets of `seat`.
    pub fn add_seat(&mut self, seat: &WlSeat, queue: &QueueHandle<WinitState>) {
        let data = SeatData { cursor_shapes: self.cursor_shapes.clone() };
        let tablet_seat = self.manager.get_tablet_seat(seat, queue, data);
        if let Some(old) = self.seats.insert(seat.id(), tablet_seat) {
            old.destroy();
        }
    }

    pub fn remove_seat(&mut self, seat: &WlSeat) {
        if let Some(tablet_seat) = self.seats.remove(&seat.id()) {
            tablet_seat.destroy();
        }
    }
}

/// A tool in proximity of a window, so the window can set its cursor.
#[derive(Debug, Clone)]
pub struct TabletCursor {
    pub tool: ZwpTabletToolV2,
    shape: Option<WpCursorShapeDeviceV1>,
    serial: u32,
}

impl TabletCursor {
    /// Shows `icon` while the tool is over the window.
    pub fn set_icon(&self, icon: CursorIcon) {
        // Without cursor shapes the compositor keeps its own tool cursor.
        if let Some(shape) = &self.shape {
            shape.set_shape(self.serial, cursor_shape(icon));
        }
    }

    /// Hides the cursor while the tool is over the window.
    pub fn hide(&self) {
        self.tool.set_cursor(self.serial, None, 0, 0);
    }
}

/// The user data of a tablet seat.
#[derive(Debug)]
pub struct SeatData {
    cursor_shapes: Option<WpCursorShapeManagerV1>,
}

/// The user data of a tool: what it is and the frame being gathered.
#[derive(Debug, Default)]
pub struct ToolData {
    inner: Mutex<ToolState>,
}

#[derive(Debug, Default)]
struct ToolState {
    kind: Option<TabletToolKind>,
    serial: u64,
    shape: Option<WpCursorShapeDeviceV1>,
    /// The window the tool is over, and the proximity serial.
    window: Option<(WindowId, u32)>,
    position: (f64, f64),
    pressure: f64,
    distance: f64,
    tilt: (f64, f64),
    rotation: f64,
    down: bool,
    /// What happened since the last frame, in order.
    pending: Vec<Pending>,
}

#[derive(Debug, Clone, Copy)]
enum Pending {
    ProximityIn,
    Down,
    Up,
    Button(u32, ElementState),
    ProximityOut,
}

fn device_id() -> crate::event::DeviceId {
    crate::event::DeviceId(crate::platform_impl::DeviceId::Wayland(DeviceId))
}

fn tool_kind(kind: zwp_tablet_tool_v2::Type) -> TabletToolKind {
    use zwp_tablet_tool_v2::Type;
    match kind {
        Type::Eraser => TabletToolKind::Eraser,
        Type::Brush => TabletToolKind::Brush,
        Type::Pencil => TabletToolKind::Pencil,
        Type::Airbrush => TabletToolKind::Airbrush,
        Type::Finger => TabletToolKind::Finger,
        Type::Mouse => TabletToolKind::Mouse,
        Type::Lens => TabletToolKind::Lens,
        _ => TabletToolKind::Pen,
    }
}

/// The pointer button a tool button stands in for, as GTK maps them.
fn emulated_button(button: u32) -> Option<MouseButton> {
    match button {
        BTN_STYLUS => Some(MouseButton::Middle),
        BTN_STYLUS2 => Some(MouseButton::Right),
        _ => None,
    }
}

impl Dispatch2<ZwpTabletManagerV2, WinitState> for GlobalData {
    fn event(
        &self,
        _: &mut WinitState,
        _: &ZwpTabletManagerV2,
        _: <ZwpTabletManagerV2 as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
    }
}

impl Dispatch2<ZwpTabletSeatV2, WinitState> for SeatData {
    fn event(
        &self,
        _: &mut WinitState,
        _: &ZwpTabletSeatV2,
        event: zwp_tablet_seat_v2::Event,
        _: &Connection,
        queue: &QueueHandle<WinitState>,
    ) {
        if let zwp_tablet_seat_v2::Event::ToolAdded { id } = event {
            let data = id.data::<ToolData>().expect("tool user data");
            if let Some(manager) = &self.cursor_shapes {
                data.inner.lock().unwrap().shape =
                    Some(manager.get_tablet_tool_v2(&id, queue, GlobalData));
            }
        }
    }

    fn event_created_child(opcode: u16, queue: &QueueHandle<WinitState>) -> Arc<dyn ObjectData> {
        // A tool's cursor shape device is made on `tool_added`, which carries
        // the new tool.
        match opcode {
            zwp_tablet_seat_v2::EVT_TABLET_ADDED_OPCODE => {
                child::<ZwpTabletV2, _>(queue, GlobalData)
            },
            zwp_tablet_seat_v2::EVT_TOOL_ADDED_OPCODE => {
                child::<ZwpTabletToolV2, _>(queue, ToolData::default())
            },
            zwp_tablet_seat_v2::EVT_PAD_ADDED_OPCODE => {
                child::<ZwpTabletPadV2, _>(queue, GlobalData)
            },
            _ => unreachable!("zwp_tablet_seat_v2 has no event {opcode} with a new object"),
        }
    }
}

/// The object data of a child created by an event.
fn child<I, U>(queue: &QueueHandle<WinitState>, data: U) -> Arc<dyn ObjectData>
where
    I: Proxy + 'static,
    U: Send + Sync + 'static,
    WinitState: sctk::reexports::client::Dispatch<I, U>,
{
    queue.make_data::<I, U>(data)
}

impl Dispatch2<ZwpTabletV2, WinitState> for GlobalData {
    fn event(
        &self,
        _: &mut WinitState,
        tablet: &ZwpTabletV2,
        event: zwp_tablet_v2::Event,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        if let zwp_tablet_v2::Event::Removed = event {
            tablet.destroy();
        }
    }
}

impl Dispatch2<ZwpTabletToolV2, WinitState> for ToolData {
    fn event(
        &self,
        state: &mut WinitState,
        tool: &ZwpTabletToolV2,
        event: zwp_tablet_tool_v2::Event,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        use zwp_tablet_tool_v2::Event;
        let mut inner = self.inner.lock().unwrap();
        match event {
            Event::Type { tool_type: WEnum::Value(kind) } => inner.kind = Some(tool_kind(kind)),
            Event::HardwareSerial { hardware_serial_hi, hardware_serial_lo } => {
                inner.serial =
                    (u64::from(hardware_serial_hi) << 32) | u64::from(hardware_serial_lo);
            },
            Event::ProximityIn { serial, surface, .. } => {
                // Over a decoration subsurface the tool is outside the window.
                let parent = surface
                    .data::<SurfaceData<()>>()
                    .and_then(|data| data.parent_surface().cloned());
                if parent.is_some() {
                    return;
                }
                inner.window = Some((wayland::make_wid(&surface), serial));
                inner.pending.push(Pending::ProximityIn);
            },
            Event::ProximityOut => inner.pending.push(Pending::ProximityOut),
            Event::Down { .. } => {
                inner.down = true;
                inner.pending.push(Pending::Down);
            },
            Event::Up => {
                inner.down = false;
                inner.pending.push(Pending::Up);
            },
            Event::Motion { x, y } => inner.position = (x, y),
            Event::Pressure { pressure } => inner.pressure = f64::from(pressure) / 65535.0,
            Event::Distance { distance } => inner.distance = f64::from(distance) / 65535.0,
            Event::Tilt { tilt_x, tilt_y } => inner.tilt = (tilt_x, tilt_y),
            Event::Rotation { degrees } => inner.rotation = degrees,
            Event::Button { button, state: WEnum::Value(button_state), .. } => {
                let pressed = button_state == zwp_tablet_tool_v2::ButtonState::Pressed;
                let element = if pressed { ElementState::Pressed } else { ElementState::Released };
                inner.pending.push(Pending::Button(button, element));
            },
            Event::Frame { .. } => {
                let frame = std::mem::take(&mut inner.pending);
                let window = inner.window;
                let tool_id = TabletTool {
                    kind: inner.kind.unwrap_or(TabletToolKind::Pen),
                    serial: inner.serial,
                };
                let snapshot = Snapshot {
                    tool: tool_id,
                    position: inner.position,
                    pressure: if inner.down { inner.pressure } else { 0.0 },
                    distance: inner.distance,
                    tilt: inner.tilt,
                    rotation: inner.rotation,
                };
                let shape = inner.shape.clone();
                if frame.iter().any(|pending| matches!(pending, Pending::ProximityOut)) {
                    inner.window = None;
                    inner.down = false;
                }
                drop(inner);
                if let Some((window_id, serial)) = window {
                    let cursor = TabletCursor { tool: tool.clone(), shape, serial };
                    emit_frame(state, window_id, cursor, snapshot, &frame);
                }
            },
            Event::Removed => {
                if let Some(shape) = inner.shape.take() {
                    shape.destroy();
                }
                if let Some((window_id, _)) = inner.window.take() {
                    if let Some(window) = state.windows.get_mut().get(&window_id) {
                        window.lock().unwrap().tablet_tool_left(tool);
                    }
                    state.events_sink.push_window_event(
                        WindowEvent::CursorLeft { device_id: device_id() },
                        window_id,
                    );
                }
                drop(inner);
                tool.destroy();
            },
            _ => {},
        }
    }
}

/// The tool's state at the end of a frame.
#[derive(Debug, Clone, Copy)]
struct Snapshot {
    tool: TabletTool,
    position: (f64, f64),
    pressure: f64,
    distance: f64,
    tilt: (f64, f64),
    rotation: f64,
}

/// Sends a frame's events to the window and keeps its tool cursors.
fn emit_frame(
    state: &mut WinitState,
    window_id: WindowId,
    cursor: TabletCursor,
    snapshot: Snapshot,
    frame: &[Pending],
) {
    let Some(window) = state.windows.get_mut().get(&window_id).cloned() else { return };
    let scale_factor = {
        let mut window = window.lock().unwrap();
        for pending in frame {
            match pending {
                Pending::ProximityIn => window.tablet_tool_entered(cursor.clone()),
                Pending::ProximityOut => window.tablet_tool_left(&cursor.tool),
                _ => {},
            }
        }
        window.scale_factor()
    };
    let position: PhysicalPosition<f64> =
        LogicalPosition::new(snapshot.position.0, snapshot.position.1).to_physical(scale_factor);
    for event in frame_events(device_id(), snapshot, position, frame) {
        state.events_sink.push_window_event(event, window_id);
    }
    state.dispatched_events = true;
}

/// A frame's tablet events, each followed by the pointer events it stands
/// for; a frame without other changes is one `Moved`.
fn frame_events(
    device_id: crate::event::DeviceId,
    snapshot: Snapshot,
    position: PhysicalPosition<f64>,
    frame: &[Pending],
) -> Vec<WindowEvent> {
    let tablet_event = |phase| {
        WindowEvent::TabletTool(TabletToolEvent {
            device_id,
            tool: snapshot.tool,
            phase,
            position,
            pressure: snapshot.pressure,
            distance: snapshot.distance,
            tilt: snapshot.tilt,
            rotation: snapshot.rotation,
        })
    };
    let moved = |events: &mut Vec<WindowEvent>| {
        events.push(tablet_event(TabletToolPhase::Moved));
        events.push(WindowEvent::CursorMoved { device_id, position });
    };
    let mut events = Vec::new();
    let mut at_position = false;
    for pending in frame {
        match *pending {
            Pending::ProximityIn => {
                events.push(tablet_event(TabletToolPhase::ProximityIn));
                events.push(WindowEvent::CursorEntered { device_id });
                events.push(WindowEvent::CursorMoved { device_id, position });
                at_position = true;
            },
            Pending::Down | Pending::Up => {
                if !at_position {
                    moved(&mut events);
                    at_position = true;
                }
                let (phase, element) = if matches!(pending, Pending::Down) {
                    (TabletToolPhase::Down, ElementState::Pressed)
                } else {
                    (TabletToolPhase::Up, ElementState::Released)
                };
                events.push(tablet_event(phase));
                events.push(WindowEvent::MouseInput {
                    device_id,
                    state: element,
                    button: MouseButton::Left,
                });
            },
            Pending::Button(button, element) => {
                events.push(tablet_event(TabletToolPhase::Button { button, state: element }));
                if let Some(mouse) = emulated_button(button) {
                    events.push(WindowEvent::MouseInput {
                        device_id,
                        state: element,
                        button: mouse,
                    });
                }
            },
            Pending::ProximityOut => {
                events.push(tablet_event(TabletToolPhase::ProximityOut));
                events.push(WindowEvent::CursorLeft { device_id });
                at_position = true;
            },
        }
    }
    if !at_position {
        moved(&mut events);
    }
    events
}

impl Dispatch2<ZwpTabletPadV2, WinitState> for GlobalData {
    fn event(
        &self,
        _: &mut WinitState,
        pad: &ZwpTabletPadV2,
        event: zwp_tablet_pad_v2::Event,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        if let zwp_tablet_pad_v2::Event::Removed = event {
            pad.destroy();
        }
    }

    fn event_created_child(opcode: u16, queue: &QueueHandle<WinitState>) -> Arc<dyn ObjectData> {
        match opcode {
            zwp_tablet_pad_v2::EVT_GROUP_OPCODE => {
                child::<ZwpTabletPadGroupV2, _>(queue, GlobalData)
            },
            _ => unreachable!("zwp_tablet_pad_v2 has no event {opcode} with a new object"),
        }
    }
}

impl Dispatch2<ZwpTabletPadGroupV2, WinitState> for GlobalData {
    fn event(
        &self,
        _: &mut WinitState,
        _: &ZwpTabletPadGroupV2,
        _: zwp_tablet_pad_group_v2::Event,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
    }

    fn event_created_child(opcode: u16, queue: &QueueHandle<WinitState>) -> Arc<dyn ObjectData> {
        match opcode {
            zwp_tablet_pad_group_v2::EVT_RING_OPCODE => {
                child::<ZwpTabletPadRingV2, _>(queue, GlobalData)
            },
            zwp_tablet_pad_group_v2::EVT_STRIP_OPCODE => {
                child::<ZwpTabletPadStripV2, _>(queue, GlobalData)
            },
            _ => unreachable!("zwp_tablet_pad_group_v2 has no event {opcode} with a new object"),
        }
    }
}

impl Dispatch2<ZwpTabletPadRingV2, WinitState> for GlobalData {
    fn event(
        &self,
        _: &mut WinitState,
        _: &ZwpTabletPadRingV2,
        _: <ZwpTabletPadRingV2 as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
    }
}

impl Dispatch2<ZwpTabletPadStripV2, WinitState> for GlobalData {
    fn event(
        &self,
        _: &mut WinitState,
        _: &ZwpTabletPadStripV2,
        _: <ZwpTabletPadStripV2 as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
    }
}

/// The cursor shape for a named cursor.
fn cursor_shape(icon: CursorIcon) -> Shape {
    match icon {
        CursorIcon::ContextMenu => Shape::ContextMenu,
        CursorIcon::Help => Shape::Help,
        CursorIcon::Pointer => Shape::Pointer,
        CursorIcon::Progress => Shape::Progress,
        CursorIcon::Wait => Shape::Wait,
        CursorIcon::Cell => Shape::Cell,
        CursorIcon::Crosshair => Shape::Crosshair,
        CursorIcon::Text => Shape::Text,
        CursorIcon::VerticalText => Shape::VerticalText,
        CursorIcon::Alias => Shape::Alias,
        CursorIcon::Copy => Shape::Copy,
        CursorIcon::Move => Shape::Move,
        CursorIcon::NoDrop => Shape::NoDrop,
        CursorIcon::NotAllowed => Shape::NotAllowed,
        CursorIcon::Grab => Shape::Grab,
        CursorIcon::Grabbing => Shape::Grabbing,
        CursorIcon::EResize => Shape::EResize,
        CursorIcon::NResize => Shape::NResize,
        CursorIcon::NeResize => Shape::NeResize,
        CursorIcon::NwResize => Shape::NwResize,
        CursorIcon::SResize => Shape::SResize,
        CursorIcon::SeResize => Shape::SeResize,
        CursorIcon::SwResize => Shape::SwResize,
        CursorIcon::WResize => Shape::WResize,
        CursorIcon::EwResize => Shape::EwResize,
        CursorIcon::NsResize => Shape::NsResize,
        CursorIcon::NeswResize => Shape::NeswResize,
        CursorIcon::NwseResize => Shape::NwseResize,
        CursorIcon::ColResize => Shape::ColResize,
        CursorIcon::RowResize => Shape::RowResize,
        CursorIcon::AllScroll => Shape::AllScroll,
        CursorIcon::ZoomIn => Shape::ZoomIn,
        CursorIcon::ZoomOut => Shape::ZoomOut,
        _ => Shape::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(pressure: f64) -> Snapshot {
        Snapshot {
            tool: TabletTool { kind: TabletToolKind::Pen, serial: 7 },
            position: (10.0, 20.0),
            pressure,
            distance: 0.0,
            tilt: (30.0, -15.0),
            rotation: 0.0,
        }
    }

    fn summary(events: &[WindowEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                WindowEvent::TabletTool(tool) => format!("tablet {:?}", tool.phase),
                WindowEvent::CursorEntered { .. } => "entered".into(),
                WindowEvent::CursorMoved { .. } => "moved".into(),
                WindowEvent::CursorLeft { .. } => "left".into(),
                WindowEvent::MouseInput { state, button, .. } => {
                    format!("{button:?} {state:?}")
                },
                other => format!("{other:?}"),
            })
            .collect()
    }

    fn events(pressure: f64, frame: &[Pending]) -> Vec<WindowEvent> {
        frame_events(device_id(), snapshot(pressure), PhysicalPosition::new(20.0, 40.0), frame)
    }

    #[test]
    fn a_stroke_drives_the_pointer() {
        let entered = events(0.0, &[Pending::ProximityIn]);
        assert_eq!(summary(&entered), ["tablet ProximityIn", "entered", "moved"]);
        let down = events(0.6, &[Pending::Down]);
        assert_eq!(summary(&down), ["tablet Moved", "moved", "tablet Down", "Left Pressed"]);
        let WindowEvent::TabletTool(stroke) = down[2] else { unreachable!() };
        assert_eq!((stroke.pressure, stroke.tilt), (0.6, (30.0, -15.0)));
        assert_eq!(stroke.position, PhysicalPosition::new(20.0, 40.0));
        assert_eq!(summary(&events(0.6, &[])), ["tablet Moved", "moved"]);
        let up = events(0.0, &[Pending::Up, Pending::ProximityOut]);
        assert_eq!(
            summary(&up),
            ["tablet Moved", "moved", "tablet Up", "Left Released", "tablet ProximityOut", "left"]
        );
    }

    #[test]
    fn barrel_buttons_press_middle_and_right() {
        let lower = events(0.0, &[Pending::Button(BTN_STYLUS, ElementState::Pressed)]);
        assert_eq!(
            summary(&lower),
            [
                "tablet Button { button: 331, state: Pressed }",
                "Middle Pressed",
                "tablet Moved",
                "moved"
            ]
        );
        let upper = events(0.0, &[Pending::Button(BTN_STYLUS2, ElementState::Released)]);
        assert_eq!(summary(&upper)[1], "Right Released");
        let other = events(0.0, &[Pending::Button(0x110, ElementState::Pressed)]);
        assert_eq!(summary(&other).len(), 3, "an unknown button stays a tablet event");
    }
}
