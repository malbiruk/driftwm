//! A synthetic [`InputBackend`] so scenarios can drive input through the real
//! [`DriftWm::process_input_event`](crate::state::DriftWm::process_input_event)
//! rather than the sub-handlers it dispatches to. Every sub-handler is generic
//! over the backend, so reaching one from a test needs an `InputBackend` impl
//! either way; entering at the top costs nothing extra and lets what runs ahead
//! of the dispatch match — idle-notify, DPMS wake, lock routing, tap taint —
//! run as well. Nothing asserts on those paths; they are exercised, not covered.
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
    KeyState, KeyboardKeyEvent, Keycode, PointerButtonEvent, PointerMotionAbsoluteEvent,
    PointerMotionEvent, TouchCancelEvent, TouchDownEvent, TouchEvent, TouchMotionEvent, TouchSlot,
    TouchUpEvent, UnusedEvent,
};
use smithay::utils::{Logical, Point};

use super::Fixture;

pub struct FakeInput;

/// Timestamps in milliseconds. Real backends hand out increasing times and the
/// middle-click buffer stores a press/release pair, so give every event its own
/// tick rather than a constant. The counter is process-wide and never reset —
/// no scenario reads an absolute time, only differences within one sequence.
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

    pub fn keyboard() -> Self {
        Self::new("fake-keyboard", &[DeviceCapability::Keyboard])
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

/// The `Event` half is the same two fields on every fake event, so it is
/// written once — above all the millisecond→microsecond conversion, which
/// smithay's provided `time_msec` divides straight back out.
macro_rules! impl_event {
    ($($ty:ty),+ $(,)?) => {$(
        impl Event<FakeInput> for $ty {
            fn time(&self) -> u64 {
                u64::from(self.time) * 1000
            }

            fn device(&self) -> FakeDevice {
                self.device.clone()
            }
        }
    )+};
}

/// smithay documents `x`/`y` as raw device space and the `_transformed` pair as
/// that range mapped into an output of the given size; its libinput backend
/// scales the device range onto it, its winit backend multiplies out a 0..1
/// fraction. The fake's raw space *is* the output's logical space, so all four
/// answer the same and the size argument is redundant — `assert_on_viewport`
/// holds scenarios to positions where that identity is honest. Only
/// `position_transformed` is read today (`input/mod.rs`, `input/touch.rs`).
macro_rules! impl_absolute_position {
    ($($ty:ty),+ $(,)?) => {$(
        impl AbsolutePositionEvent<FakeInput> for $ty {
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
    )+};
}

/// A key changing state. `keycode` is the evdev `KEY_*` code, the space real
/// backends report in.
pub struct FakeKeyEvent {
    device: FakeDevice,
    keycode: u32,
    state: KeyState,
    time: u32,
}

impl KeyboardKeyEvent<FakeInput> for FakeKeyEvent {
    fn key_code(&self) -> Keycode {
        // The evdev→xkb offset both real backends apply.
        (self.keycode + 8).into()
    }

    fn state(&self) -> KeyState {
        self.state
    }

    // Seat-wide count of held keys; nothing on the keyboard path reads it.
    fn count(&self) -> u32 {
        1
    }
}

pub struct FakeButtonEvent {
    device: FakeDevice,
    button: u32,
    state: ButtonState,
    time: u32,
}

impl PointerButtonEvent<FakeInput> for FakeButtonEvent {
    fn button_code(&self) -> u32 {
        self.button
    }

    fn state(&self) -> ButtonState {
        self.state
    }
}

/// Absolute pointer motion. `screen` is already in the output's logical space.
pub struct FakeAbsoluteEvent {
    device: FakeDevice,
    screen: Point<f64, Logical>,
    time: u32,
}

impl PointerMotionAbsoluteEvent<FakeInput> for FakeAbsoluteEvent {}

/// Relative pointer motion, e.g. from a mouse. `delta` is screen-pixel
/// movement; accelerated and unaccelerated are the same value here, since
/// nothing under test reads the unaccelerated pair.
pub struct FakeRelativeEvent {
    device: FakeDevice,
    delta: Point<f64, Logical>,
    time: u32,
}

impl PointerMotionEvent<FakeInput> for FakeRelativeEvent {
    fn delta_x(&self) -> f64 {
        self.delta.x
    }

    fn delta_y(&self) -> f64 {
        self.delta.y
    }

    fn delta_x_unaccel(&self) -> f64 {
        self.delta.x
    }

    fn delta_y_unaccel(&self) -> f64 {
        self.delta.y
    }
}

/// A finger landing, in the same screen-space convention as
/// [`FakeAbsoluteEvent`].
pub struct FakeTouchDownEvent {
    device: FakeDevice,
    screen: Point<f64, Logical>,
    slot: TouchSlot,
    time: u32,
}

impl TouchEvent<FakeInput> for FakeTouchDownEvent {
    fn slot(&self) -> TouchSlot {
        self.slot
    }
}

impl TouchDownEvent<FakeInput> for FakeTouchDownEvent {}

/// A finger already down moving, in the same screen-space convention as
/// [`FakeAbsoluteEvent`].
pub struct FakeTouchMotionEvent {
    device: FakeDevice,
    screen: Point<f64, Logical>,
    slot: TouchSlot,
    time: u32,
}

impl TouchEvent<FakeInput> for FakeTouchMotionEvent {
    fn slot(&self) -> TouchSlot {
        self.slot
    }
}

impl TouchMotionEvent<FakeInput> for FakeTouchMotionEvent {}

/// A finger lifting. A real touch-up reports only its slot — where the finger
/// was is the sequence's business, not the event's.
pub struct FakeTouchUpEvent {
    device: FakeDevice,
    slot: TouchSlot,
    time: u32,
}

impl TouchEvent<FakeInput> for FakeTouchUpEvent {
    fn slot(&self) -> TouchSlot {
        self.slot
    }
}

impl TouchUpEvent<FakeInput> for FakeTouchUpEvent {}

/// A hardware-level touch cancel — libinput sends one when it loses track of
/// the whole sequence. `on_touch_cancel` ignores the event entirely (it only
/// exists to run
/// [`DriftWm::cancel_touch_sequence`](crate::state::DriftWm::cancel_touch_sequence)),
/// so the slot carried here is never read.
pub struct FakeTouchCancelEvent {
    device: FakeDevice,
    slot: TouchSlot,
    time: u32,
}

impl TouchEvent<FakeInput> for FakeTouchCancelEvent {
    fn slot(&self) -> TouchSlot {
        self.slot
    }
}

impl TouchCancelEvent<FakeInput> for FakeTouchCancelEvent {}

impl_event!(
    FakeKeyEvent,
    FakeButtonEvent,
    FakeAbsoluteEvent,
    FakeRelativeEvent,
    FakeTouchDownEvent,
    FakeTouchMotionEvent,
    FakeTouchUpEvent,
    FakeTouchCancelEvent,
);
impl_absolute_position!(FakeAbsoluteEvent, FakeTouchDownEvent, FakeTouchMotionEvent);

impl InputBackend for FakeInput {
    type Device = FakeDevice;
    type KeyboardKeyEvent = FakeKeyEvent;
    type PointerButtonEvent = FakeButtonEvent;
    type PointerMotionAbsoluteEvent = FakeAbsoluteEvent;
    type PointerMotionEvent = FakeRelativeEvent;
    type TouchDownEvent = FakeTouchDownEvent;
    type TouchUpEvent = FakeTouchUpEvent;
    type TouchCancelEvent = FakeTouchCancelEvent;
    type TouchMotionEvent = FakeTouchMotionEvent;

    type PointerAxisEvent = UnusedEvent;
    type GestureSwipeBeginEvent = UnusedEvent;
    type GestureSwipeUpdateEvent = UnusedEvent;
    type GestureSwipeEndEvent = UnusedEvent;
    type GesturePinchBeginEvent = UnusedEvent;
    type GesturePinchUpdateEvent = UnusedEvent;
    type GesturePinchEndEvent = UnusedEvent;
    type GestureHoldBeginEvent = UnusedEvent;
    type GestureHoldEndEvent = UnusedEvent;
    type TouchFrameEvent = UnusedEvent;
    type TabletToolAxisEvent = UnusedEvent;
    type TabletToolProximityEvent = UnusedEvent;
    type TabletToolTipEvent = UnusedEvent;
    type TabletToolButtonEvent = UnusedEvent;
    type SwitchToggleEvent = UnusedEvent;
    type SpecialEvent = UnusedEvent;
}

/// smithay's libinput and winit backends both fold their device range into the
/// output's, so a position outside it is one no hardware could have reported and
/// the compositor is under no obligation to handle.
fn assert_on_viewport(f: &mut Fixture, screen: Point<f64, Logical>) {
    let size = f.state().get_viewport_size();
    debug_assert!(
        (0.0..=f64::from(size.w)).contains(&screen.x)
            && (0.0..=f64::from(size.h)).contains(&screen.y),
        "{screen:?} is off the {size:?} viewport — no device could report it"
    );
}

/// Where a physical device would have to report to land on canvas-space
/// `canvas`, given the active output's camera and zoom.
///
/// Touch resolves its output from the *device* instead
/// (`DriftWm::touch_output_for_device`), so with more than one output this can
/// answer for the wrong viewport; every scenario so far has one, where the two
/// agree. This is also the inverse of the mapping under test, so it can only
/// aim a scenario, never confirm the mapping — `input_dispatch` has a scenario
/// that checks that from hand-computed numbers.
fn screen_of(f: &mut Fixture, canvas: Point<f64, Logical>) -> Point<f64, Logical> {
    let camera = f.state().camera();
    let zoom = f.state().zoom();
    let screen = canvas_to_screen(CanvasPos(canvas), camera, zoom).0;
    assert_on_viewport(f, screen);
    screen
}

/// Report absolute motion at raw screen position `screen` — what a device
/// hands over, before any camera/zoom mapping.
pub fn pointer_to_screen(f: &mut Fixture, device: &FakeDevice, screen: Point<f64, Logical>) {
    assert_on_viewport(f, screen);
    f.state()
        .process_input_event::<FakeInput>(InputEvent::PointerMotionAbsolute {
            event: FakeAbsoluteEvent {
                device: device.clone(),
                screen,
                time: next_time(),
            },
        });
}

/// Move the pointer onto canvas-space `at`.
pub fn pointer_to(f: &mut Fixture, device: &FakeDevice, at: Point<f64, Logical>) {
    let screen = screen_of(f, at);
    pointer_to_screen(f, device, screen);
}

/// Report relative motion of `delta` — what a mouse hands over between the
/// last position and this one, unclamped and unmapped to any output.
pub fn pointer_relative_motion(f: &mut Fixture, device: &FakeDevice, delta: Point<f64, Logical>) {
    f.state()
        .process_input_event::<FakeInput>(InputEvent::PointerMotion {
            event: FakeRelativeEvent {
                device: device.clone(),
                delta,
                time: next_time(),
            },
        });
}

fn key(f: &mut Fixture, keycode: u32, state: KeyState) {
    f.state()
        .process_input_event::<FakeInput>(InputEvent::Keyboard {
            event: FakeKeyEvent {
                device: FakeDevice::keyboard(),
                keycode,
                state,
                time: next_time(),
            },
        });
}

/// Press the key with evdev code `keycode` and hold it down.
pub fn key_press(f: &mut Fixture, keycode: u32) {
    key(f, keycode, KeyState::Pressed);
}

pub fn key_release(f: &mut Fixture, keycode: u32) {
    key(f, keycode, KeyState::Released);
}

/// The event a button change arrives as, for scenarios that classify an event
/// rather than dispatch one.
pub fn button_event(device: &FakeDevice, button: u32, state: ButtonState) -> InputEvent<FakeInput> {
    InputEvent::PointerButton {
        event: FakeButtonEvent {
            device: device.clone(),
            button,
            state,
            time: next_time(),
        },
    }
}

fn button(f: &mut Fixture, device: &FakeDevice, button: u32, state: ButtonState) {
    let event = button_event(device, button, state);
    f.state().process_input_event::<FakeInput>(event);
}

/// Press `button` wherever the pointer already is — a real button event carries
/// no position of its own.
pub fn press(f: &mut Fixture, device: &FakeDevice, button_code: u32) {
    button(f, device, button_code, ButtonState::Pressed);
}

pub fn release(f: &mut Fixture, device: &FakeDevice, button_code: u32) {
    button(f, device, button_code, ButtonState::Released);
}

/// A whole click on canvas-space `at`: move there, press, release. The motion
/// comes from the same device as the buttons, as it would on hardware.
pub fn click(f: &mut Fixture, device: &FakeDevice, at: Point<f64, Logical>, button_code: u32) {
    pointer_to(f, device, at);
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

/// Move the finger holding `slot` to canvas-space `at`.
pub fn touch_motion(f: &mut Fixture, at: Point<f64, Logical>, slot: u32) {
    let screen = screen_of(f, at);
    f.state()
        .process_input_event::<FakeInput>(InputEvent::TouchMotion {
            event: FakeTouchMotionEvent {
                device: FakeDevice::touchscreen(),
                screen,
                slot: TouchSlot::from(Some(slot)),
                time: next_time(),
            },
        });
}

/// The event a finger lifting arrives as, for scenarios that classify an event
/// rather than dispatch one.
pub fn touch_up_event(slot: u32) -> InputEvent<FakeInput> {
    InputEvent::TouchUp {
        event: FakeTouchUpEvent {
            device: FakeDevice::touchscreen(),
            slot: TouchSlot::from(Some(slot)),
            time: next_time(),
        },
    }
}

/// Lift the finger holding `slot`. No frame event follows: `TouchFrameEvent` is
/// `UnusedEvent` here, so the fake structurally cannot send one.
pub fn touch_up(f: &mut Fixture, slot: u32) {
    let event = touch_up_event(slot);
    f.state().process_input_event::<FakeInput>(event);
}

/// A hardware touch cancel, covering the whole sequence rather than one slot.
pub fn touch_cancel(f: &mut Fixture) {
    f.state()
        .process_input_event::<FakeInput>(InputEvent::TouchCancel {
            event: FakeTouchCancelEvent {
                device: FakeDevice::touchscreen(),
                slot: TouchSlot::from(Some(0)),
                time: next_time(),
            },
        });
}
