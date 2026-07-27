//! A synthetic [`InputBackend`] so scenarios can drive input through the real
//! [`DriftWm::process_input_event`](crate::state::DriftWm::process_input_event)
//! instead of calling the sub-handlers it dispatches to. Everything the entry
//! point does before the dispatch match — idle-notify, DPMS wake, lock routing,
//! tap taint — is then under test too, and the sub-handlers stay untouched by
//! test-only visibility.
//!
//! Only the event types a scenario actually drives are real here; the rest are
//! smithay's uninhabited `UnusedEvent`, which implements every event trait, so
//! an unbuilt event type costs nothing. Add one when a scenario needs it.

use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use driftwm::canvas::{CanvasPos, canvas_to_screen};
use smithay::backend::input::{
    AbsolutePositionEvent, ButtonState, Device, DeviceCapability, Event, InputBackend, InputEvent,
    PointerButtonEvent, PointerMotionAbsoluteEvent, TouchDownEvent, TouchEvent, TouchSlot,
    UnusedEvent,
};
use smithay::utils::{Logical, Point};

use super::Fixture;

pub struct FakeInput;

/// Timestamps in milliseconds. Real backends hand out increasing times and the
/// middle-click buffer stores a press/release pair, so give every event its own
/// tick rather than a constant.
fn next_time() -> u32 {
    static CLOCK: AtomicU32 = AtomicU32::new(1);
    CLOCK.fetch_add(1, Ordering::Relaxed)
}

/// A synthetic input device. The capability set is per-device because paths like
/// the 3-finger-tap middle-click buffer branch on
/// [`DeviceCapability::Gesture`], and only a fake can hold both answers.
#[derive(Clone, PartialEq, Eq)]
pub struct FakeDevice {
    name: String,
    capabilities: Vec<DeviceCapability>,
}

impl FakeDevice {
    fn new(name: &str, capabilities: &[DeviceCapability]) -> Self {
        Self {
            name: name.to_string(),
            capabilities: capabilities.to_vec(),
        }
    }

    /// A plain mouse — no gesture capability, so nothing may delay its clicks.
    pub fn mouse() -> Self {
        Self::new("fake-mouse", &[DeviceCapability::Pointer])
    }

    /// A touchpad — libinput reports `Gesture` alongside `Pointer` on one, which
    /// is what gates the buffering of a 3-finger tap's middle click.
    pub fn touchpad() -> Self {
        Self::new(
            "fake-touchpad",
            &[DeviceCapability::Pointer, DeviceCapability::Gesture],
        )
    }

    pub fn touchscreen() -> Self {
        Self::new("fake-touchscreen", &[DeviceCapability::Touch])
    }
}

// `Device` requires `Hash`; the id is a device's identity, and here that is the
// name, which each constructor pairs with one fixed capability set.
impl Hash for FakeDevice {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl Device for FakeDevice {
    fn id(&self) -> String {
        self.name.clone()
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn has_capability(&self, capability: DeviceCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    fn usb_id(&self) -> Option<(u32, u32)> {
        None
    }

    fn syspath(&self) -> Option<PathBuf> {
        None
    }
}

pub struct FakeButtonEvent {
    device: FakeDevice,
    button: u32,
    state: ButtonState,
    time: u32,
}

impl Event<FakeInput> for FakeButtonEvent {
    fn time(&self) -> u64 {
        u64::from(self.time) * 1000
    }

    fn device(&self) -> FakeDevice {
        self.device.clone()
    }
}

impl PointerButtonEvent<FakeInput> for FakeButtonEvent {
    fn button_code(&self) -> u32 {
        self.button
    }

    fn state(&self) -> ButtonState {
        self.state
    }
}

/// Absolute pointer motion. `screen` is already in the output's logical space —
/// the fake's raw space *is* that space, so the `_transformed` accessors don't
/// rescale.
pub struct FakeAbsoluteEvent {
    device: FakeDevice,
    screen: Point<f64, Logical>,
    time: u32,
}

impl Event<FakeInput> for FakeAbsoluteEvent {
    fn time(&self) -> u64 {
        u64::from(self.time) * 1000
    }

    fn device(&self) -> FakeDevice {
        self.device.clone()
    }
}

impl AbsolutePositionEvent<FakeInput> for FakeAbsoluteEvent {
    fn x(&self) -> f64 {
        self.screen.x
    }

    fn y(&self) -> f64 {
        self.screen.y
    }

    fn x_transformed(&self, _width: i32) -> f64 {
        self.screen.x
    }

    fn y_transformed(&self, _height: i32) -> f64 {
        self.screen.y
    }
}

impl PointerMotionAbsoluteEvent<FakeInput> for FakeAbsoluteEvent {}

/// A finger landing, in the same screen-space convention as
/// [`FakeAbsoluteEvent`].
pub struct FakeTouchDownEvent {
    device: FakeDevice,
    screen: Point<f64, Logical>,
    slot: TouchSlot,
    time: u32,
}

impl Event<FakeInput> for FakeTouchDownEvent {
    fn time(&self) -> u64 {
        u64::from(self.time) * 1000
    }

    fn device(&self) -> FakeDevice {
        self.device.clone()
    }
}

impl AbsolutePositionEvent<FakeInput> for FakeTouchDownEvent {
    fn x(&self) -> f64 {
        self.screen.x
    }

    fn y(&self) -> f64 {
        self.screen.y
    }

    fn x_transformed(&self, _width: i32) -> f64 {
        self.screen.x
    }

    fn y_transformed(&self, _height: i32) -> f64 {
        self.screen.y
    }
}

impl TouchEvent<FakeInput> for FakeTouchDownEvent {
    fn slot(&self) -> TouchSlot {
        self.slot
    }
}

impl TouchDownEvent<FakeInput> for FakeTouchDownEvent {}

impl InputBackend for FakeInput {
    type Device = FakeDevice;
    type PointerButtonEvent = FakeButtonEvent;
    type PointerMotionAbsoluteEvent = FakeAbsoluteEvent;
    type TouchDownEvent = FakeTouchDownEvent;

    type KeyboardKeyEvent = UnusedEvent;
    type PointerAxisEvent = UnusedEvent;
    type PointerMotionEvent = UnusedEvent;
    type GestureSwipeBeginEvent = UnusedEvent;
    type GestureSwipeUpdateEvent = UnusedEvent;
    type GestureSwipeEndEvent = UnusedEvent;
    type GesturePinchBeginEvent = UnusedEvent;
    type GesturePinchUpdateEvent = UnusedEvent;
    type GesturePinchEndEvent = UnusedEvent;
    type GestureHoldBeginEvent = UnusedEvent;
    type GestureHoldEndEvent = UnusedEvent;
    type TouchUpEvent = UnusedEvent;
    type TouchMotionEvent = UnusedEvent;
    type TouchCancelEvent = UnusedEvent;
    type TouchFrameEvent = UnusedEvent;
    type TabletToolAxisEvent = UnusedEvent;
    type TabletToolProximityEvent = UnusedEvent;
    type TabletToolTipEvent = UnusedEvent;
    type TabletToolButtonEvent = UnusedEvent;
    type SwitchToggleEvent = UnusedEvent;
    type SpecialEvent = UnusedEvent;
}

/// Where a physical device would have to report to land on canvas-space
/// `canvas`, given the active output's current camera and zoom.
fn screen_of(f: &mut Fixture, canvas: Point<f64, Logical>) -> Point<f64, Logical> {
    let camera = f.state().camera();
    let zoom = f.state().zoom();
    canvas_to_screen(CanvasPos(canvas), camera, zoom).0
}

/// Move the pointer onto canvas-space `at`. The device is fixed because
/// absolute motion never consults it.
pub fn pointer_to(f: &mut Fixture, at: Point<f64, Logical>) {
    let screen = screen_of(f, at);
    f.state()
        .process_input_event::<FakeInput>(InputEvent::PointerMotionAbsolute {
            event: FakeAbsoluteEvent {
                device: FakeDevice::mouse(),
                screen,
                time: next_time(),
            },
        });
}

fn button(f: &mut Fixture, device: &FakeDevice, button: u32, state: ButtonState) {
    f.state()
        .process_input_event::<FakeInput>(InputEvent::PointerButton {
            event: FakeButtonEvent {
                device: device.clone(),
                button,
                state,
                time: next_time(),
            },
        });
}

/// Press `button` wherever the pointer already is — a real button event carries
/// no position of its own.
pub fn press(f: &mut Fixture, device: &FakeDevice, button_code: u32) {
    button(f, device, button_code, ButtonState::Pressed);
}

pub fn release(f: &mut Fixture, device: &FakeDevice, button_code: u32) {
    button(f, device, button_code, ButtonState::Released);
}

/// A whole click on canvas-space `at`: move there, press, release.
pub fn click(f: &mut Fixture, device: &FakeDevice, at: Point<f64, Logical>, button_code: u32) {
    pointer_to(f, at);
    press(f, device, button_code);
    release(f, device, button_code);
}

/// Put one finger down on canvas-space `at`.
pub fn touch_down(f: &mut Fixture, at: Point<f64, Logical>, slot: u32) {
    let screen = screen_of(f, at);
    f.state()
        .process_input_event::<FakeInput>(InputEvent::TouchDown {
            event: FakeTouchDownEvent {
                device: FakeDevice::touchscreen(),
                screen,
                slot: TouchSlot::from(Some(slot)),
                time: next_time(),
            },
        });
}
