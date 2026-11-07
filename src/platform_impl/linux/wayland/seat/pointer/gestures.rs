// Modified by enzUI contributors. See FORK.md.
//! Touchpad gestures (`zwp_pointer_gestures_v1`): pinch, swipe and hold.
//!
//! A pinch arrives as [`WindowEvent::PinchGesture`] (the scale change since
//! the last update) and [`WindowEvent::RotationGesture`], a swipe as
//! [`WindowEvent::PanGesture`]. A hold, fingers resting on the touchpad, is
//! reported as a zero [`WindowEvent::MouseWheel`] that starts a new scroll,
//! which is how a client stops kinetic scrolling.

use std::sync::Mutex;

use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::client::protocol::wl_pointer::WlPointer;
use sctk::reexports::client::{delegate_dispatch, Connection, Dispatch, Proxy, QueueHandle};
use sctk::reexports::protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gesture_hold_v1::{
    self, ZwpPointerGestureHoldV1,
};
use sctk::reexports::protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gesture_pinch_v1::{
    self, ZwpPointerGesturePinchV1,
};
use sctk::reexports::protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gesture_swipe_v1::{
    self, ZwpPointerGestureSwipeV1,
};
use sctk::reexports::protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gestures_v1::ZwpPointerGesturesV1;

use crate::dpi::{LogicalPosition, PhysicalPosition};
use crate::event::{MouseScrollDelta, TouchPhase, WindowEvent};
use crate::platform_impl::wayland::state::WinitState;
use crate::platform_impl::wayland::{self, DeviceId, WindowId};

/// The bound `zwp_pointer_gestures_v1` global.
pub struct PointerGesturesState {
    manager: ZwpPointerGesturesV1,
}

/// The user data of the bound global.
pub struct GesturesGlobal;

impl PointerGesturesState {
    pub fn new(globals: &GlobalList, queue: &QueueHandle<WinitState>) -> Result<Self, BindError> {
        Ok(Self { manager: globals.bind(queue, 1..=3, GesturesGlobal)? })
    }

    /// The gesture objects of one pointer.
    pub fn gestures(&self, pointer: &WlPointer, queue: &QueueHandle<WinitState>) -> SeatGestures {
        let hold = (self.manager.version() >= 3)
            .then(|| self.manager.get_hold_gesture(pointer, queue, GestureData::default()));
        SeatGestures {
            pinch: self.manager.get_pinch_gesture(pointer, queue, GestureData::default()),
            swipe: self.manager.get_swipe_gesture(pointer, queue, GestureData::default()),
            hold,
        }
    }
}

/// The gesture objects of a seat's pointer, destroyed on drop.
#[derive(Debug)]
pub struct SeatGestures {
    pinch: ZwpPointerGesturePinchV1,
    swipe: ZwpPointerGestureSwipeV1,
    hold: Option<ZwpPointerGestureHoldV1>,
}

impl Drop for SeatGestures {
    fn drop(&mut self) {
        self.pinch.destroy();
        self.swipe.destroy();
        if let Some(hold) = self.hold.take() {
            hold.destroy();
        }
    }
}

/// The window a gesture began on, and the pinch scale reached so far.
#[derive(Debug)]
pub struct GestureData {
    inner: Mutex<(Option<WindowId>, f64)>,
}

impl Default for GestureData {
    fn default() -> Self {
        Self { inner: Mutex::new((None, 1.0)) }
    }
}

fn device_id() -> crate::event::DeviceId {
    crate::event::DeviceId(crate::platform_impl::DeviceId::Wayland(DeviceId))
}

fn scale_factor(state: &WinitState, window_id: WindowId) -> f64 {
    state
        .windows
        .borrow()
        .get(&window_id)
        .map(|window| window.lock().unwrap().scale_factor())
        .unwrap_or(1.0)
}

impl Dispatch<ZwpPointerGesturesV1, GesturesGlobal, WinitState> for PointerGesturesState {
    fn event(
        _: &mut WinitState,
        _: &ZwpPointerGesturesV1,
        _: <ZwpPointerGesturesV1 as Proxy>::Event,
        _: &GesturesGlobal,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
    }
}

impl Dispatch<ZwpPointerGesturePinchV1, GestureData, WinitState> for PointerGesturesState {
    fn event(
        state: &mut WinitState,
        _: &ZwpPointerGesturePinchV1,
        event: zwp_pointer_gesture_pinch_v1::Event,
        data: &GestureData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        let mut inner = data.inner.lock().unwrap();
        let (window_id, pinch, rotation, phase) = match event {
            zwp_pointer_gesture_pinch_v1::Event::Begin { surface, .. } => {
                let window_id = wayland::make_wid(&surface);
                *inner = (Some(window_id), 1.0);
                (window_id, 0.0, 0.0, TouchPhase::Started)
            },
            zwp_pointer_gesture_pinch_v1::Event::Update { scale, rotation, .. } => {
                let Some(window_id) = inner.0 else { return };
                let previous = inner.1;
                inner.1 = scale;
                let change = if previous > 0.0 { scale / previous - 1.0 } else { 0.0 };
                (window_id, change, rotation as f32, TouchPhase::Moved)
            },
            zwp_pointer_gesture_pinch_v1::Event::End { cancelled, .. } => {
                let Some(window_id) = inner.0.take() else { return };
                let phase = if cancelled != 0 { TouchPhase::Cancelled } else { TouchPhase::Ended };
                (window_id, 0.0, 0.0, phase)
            },
            _ => return,
        };
        drop(inner);
        state.events_sink.push_window_event(
            WindowEvent::PinchGesture { device_id: device_id(), delta: pinch, phase },
            window_id,
        );
        state.events_sink.push_window_event(
            WindowEvent::RotationGesture { device_id: device_id(), delta: rotation, phase },
            window_id,
        );
        state.dispatched_events = true;
    }
}

impl Dispatch<ZwpPointerGestureSwipeV1, GestureData, WinitState> for PointerGesturesState {
    fn event(
        state: &mut WinitState,
        _: &ZwpPointerGestureSwipeV1,
        event: zwp_pointer_gesture_swipe_v1::Event,
        data: &GestureData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        let mut inner = data.inner.lock().unwrap();
        let (window_id, delta, phase) = match event {
            zwp_pointer_gesture_swipe_v1::Event::Begin { surface, .. } => {
                let window_id = wayland::make_wid(&surface);
                *inner = (Some(window_id), 1.0);
                (window_id, (0.0, 0.0), TouchPhase::Started)
            },
            zwp_pointer_gesture_swipe_v1::Event::Update { dx, dy, .. } => {
                let Some(window_id) = inner.0 else { return };
                (window_id, (dx, dy), TouchPhase::Moved)
            },
            zwp_pointer_gesture_swipe_v1::Event::End { cancelled, .. } => {
                let Some(window_id) = inner.0.take() else { return };
                let phase = if cancelled != 0 { TouchPhase::Cancelled } else { TouchPhase::Ended };
                (window_id, (0.0, 0.0), phase)
            },
            _ => return,
        };
        drop(inner);
        let scale = scale_factor(state, window_id);
        let delta: PhysicalPosition<f32> =
            LogicalPosition::new(delta.0, delta.1).to_physical(scale);
        state.events_sink.push_window_event(
            WindowEvent::PanGesture { device_id: device_id(), delta, phase },
            window_id,
        );
        state.dispatched_events = true;
    }
}

impl Dispatch<ZwpPointerGestureHoldV1, GestureData, WinitState> for PointerGesturesState {
    fn event(
        state: &mut WinitState,
        _: &ZwpPointerGestureHoldV1,
        event: zwp_pointer_gesture_hold_v1::Event,
        _: &GestureData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        if let zwp_pointer_gesture_hold_v1::Event::Begin { surface, .. } = event {
            state.events_sink.push_window_event(
                WindowEvent::MouseWheel {
                    device_id: device_id(),
                    delta: MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 0.0)),
                    phase: TouchPhase::Started,
                },
                wayland::make_wid(&surface),
            );
            state.dispatched_events = true;
        }
    }
}

delegate_dispatch!(WinitState: [ZwpPointerGesturesV1: GesturesGlobal] => PointerGesturesState);
delegate_dispatch!(WinitState: [ZwpPointerGesturePinchV1: GestureData] => PointerGesturesState);
delegate_dispatch!(WinitState: [ZwpPointerGestureSwipeV1: GestureData] => PointerGesturesState);
delegate_dispatch!(WinitState: [ZwpPointerGestureHoldV1: GestureData] => PointerGesturesState);
