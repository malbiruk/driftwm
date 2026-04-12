//! FocusTarget newtype — the SeatHandler focus type for keyboard, pointer, and touch.
//!
//! Required because `PopupGrab` needs `KeyboardFocus: From<PopupKind>`, and we
//! can't impl `From<PopupKind> for WlSurface` (orphan rule). All input-target
//! Keyboard methods route through `X11Surface` when applicable (for ICCCM
//! `SetInputFocus` + `WM_TAKE_FOCUS`); pointer/touch delegate to `WlSurface`.

use std::borrow::Cow;

use smithay::{
    backend::input::KeyState,
    desktop::PopupKind,
    input::{
        Seat,
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
        pointer::{
            AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent,
            GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
            GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
            MotionEvent, PointerTarget, RelativeMotionEvent,
        },
        touch::{
            DownEvent as TouchDownEvent, MotionEvent as TouchMotionEvent, TouchTarget,
            UpEvent as TouchUpEvent,
        },
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Serial},
    wayland::seat::WaylandFocus,
    xwayland::X11Surface,
};

use crate::state::DriftWm;

// --- FocusTarget ---
// Newtype over WlSurface for use as SeatHandler focus types.
// Required because PopupGrab needs `KeyboardFocus: From<PopupKind>`,
// and we can't impl `From<PopupKind> for WlSurface` (orphan rule).

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusTarget(pub WlSurface);

impl From<PopupKind> for FocusTarget {
    fn from(popup: PopupKind) -> Self {
        FocusTarget(popup.wl_surface().clone())
    }
}

impl IsAlive for FocusTarget {
    fn alive(&self) -> bool {
        self.0.alive()
    }
}

impl WaylandFocus for FocusTarget {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        Some(Cow::Borrowed(&self.0))
    }
}

// Delegate all KeyboardTarget methods to the inner WlSurface using
// fully-qualified syntax to avoid clash with WlSurface::enter() protocol method.
impl KeyboardTarget<DriftWm> for FocusTarget {
    fn enter(
        &self,
        seat: &Seat<DriftWm>,
        data: &mut DriftWm,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        if let Some(x11) = data.find_x11_surface_by_wl(&self.0) {
            <X11Surface as KeyboardTarget<DriftWm>>::enter(&x11, seat, data, keys, serial);
        } else {
            <WlSurface as KeyboardTarget<DriftWm>>::enter(&self.0, seat, data, keys, serial);
        }
    }

    fn leave(&self, seat: &Seat<DriftWm>, data: &mut DriftWm, serial: Serial) {
        if let Some(x11) = data.find_x11_surface_by_wl(&self.0) {
            <X11Surface as KeyboardTarget<DriftWm>>::leave(&x11, seat, data, serial);
        } else {
            <WlSurface as KeyboardTarget<DriftWm>>::leave(&self.0, seat, data, serial);
        }
    }

    fn key(
        &self,
        seat: &Seat<DriftWm>,
        data: &mut DriftWm,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        if let Some(x11) = data.find_x11_surface_by_wl(&self.0) {
            <X11Surface as KeyboardTarget<DriftWm>>::key(&x11, seat, data, key, state, serial, time);
        } else {
            <WlSurface as KeyboardTarget<DriftWm>>::key(&self.0, seat, data, key, state, serial, time);
        }
    }

    fn modifiers(
        &self,
        seat: &Seat<DriftWm>,
        data: &mut DriftWm,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        if let Some(x11) = data.find_x11_surface_by_wl(&self.0) {
            <X11Surface as KeyboardTarget<DriftWm>>::modifiers(&x11, seat, data, modifiers, serial);
        } else {
            <WlSurface as KeyboardTarget<DriftWm>>::modifiers(&self.0, seat, data, modifiers, serial);
        }
    }
}

impl PointerTarget<DriftWm> for FocusTarget {
    fn enter(&self, seat: &Seat<DriftWm>, data: &mut DriftWm, event: &MotionEvent) {
        <WlSurface as PointerTarget<DriftWm>>::enter(&self.0, seat, data, event);
    }

    fn motion(&self, seat: &Seat<DriftWm>, data: &mut DriftWm, event: &MotionEvent) {
        <WlSurface as PointerTarget<DriftWm>>::motion(&self.0, seat, data, event);
    }

    fn relative_motion(
        &self,
        seat: &Seat<DriftWm>,
        data: &mut DriftWm,
        event: &RelativeMotionEvent,
    ) {
        <WlSurface as PointerTarget<DriftWm>>::relative_motion(&self.0, seat, data, event);
    }

    fn button(&self, seat: &Seat<DriftWm>, data: &mut DriftWm, event: &ButtonEvent) {
        <WlSurface as PointerTarget<DriftWm>>::button(&self.0, seat, data, event);
    }

    fn axis(&self, seat: &Seat<DriftWm>, data: &mut DriftWm, frame: AxisFrame) {
        <WlSurface as PointerTarget<DriftWm>>::axis(&self.0, seat, data, frame);
    }

    fn frame(&self, seat: &Seat<DriftWm>, data: &mut DriftWm) {
        <WlSurface as PointerTarget<DriftWm>>::frame(&self.0, seat, data);
    }

    fn gesture_swipe_begin(
        &self,
        seat: &Seat<DriftWm>,
        data: &mut DriftWm,
        event: &GestureSwipeBeginEvent,
    ) {
        <WlSurface as PointerTarget<DriftWm>>::gesture_swipe_begin(&self.0, seat, data, event);
    }

    fn gesture_swipe_update(
        &self,
        seat: &Seat<DriftWm>,
        data: &mut DriftWm,
        event: &GestureSwipeUpdateEvent,
    ) {
        <WlSurface as PointerTarget<DriftWm>>::gesture_swipe_update(&self.0, seat, data, event);
    }

    fn gesture_swipe_end(
        &self,
        seat: &Seat<DriftWm>,
        data: &mut DriftWm,
        event: &GestureSwipeEndEvent,
    ) {
        <WlSurface as PointerTarget<DriftWm>>::gesture_swipe_end(&self.0, seat, data, event);
    }

    fn gesture_pinch_begin(
        &self,
        seat: &Seat<DriftWm>,
        data: &mut DriftWm,
        event: &GesturePinchBeginEvent,
    ) {
        <WlSurface as PointerTarget<DriftWm>>::gesture_pinch_begin(&self.0, seat, data, event);
    }

    fn gesture_pinch_update(
        &self,
        seat: &Seat<DriftWm>,
        data: &mut DriftWm,
        event: &GesturePinchUpdateEvent,
    ) {
        <WlSurface as PointerTarget<DriftWm>>::gesture_pinch_update(&self.0, seat, data, event);
    }

    fn gesture_pinch_end(
        &self,
        seat: &Seat<DriftWm>,
        data: &mut DriftWm,
        event: &GesturePinchEndEvent,
    ) {
        <WlSurface as PointerTarget<DriftWm>>::gesture_pinch_end(&self.0, seat, data, event);
    }

    fn gesture_hold_begin(
        &self,
        seat: &Seat<DriftWm>,
        data: &mut DriftWm,
        event: &GestureHoldBeginEvent,
    ) {
        <WlSurface as PointerTarget<DriftWm>>::gesture_hold_begin(&self.0, seat, data, event);
    }

    fn gesture_hold_end(
        &self,
        seat: &Seat<DriftWm>,
        data: &mut DriftWm,
        event: &GestureHoldEndEvent,
    ) {
        <WlSurface as PointerTarget<DriftWm>>::gesture_hold_end(&self.0, seat, data, event);
    }

    fn leave(&self, seat: &Seat<DriftWm>, data: &mut DriftWm, serial: Serial, time: u32) {
        <WlSurface as PointerTarget<DriftWm>>::leave(&self.0, seat, data, serial, time);
    }
}

impl TouchTarget<DriftWm> for FocusTarget {
    fn down(
        &self,
        seat: &Seat<DriftWm>,
        data: &mut DriftWm,
        event: &TouchDownEvent,
        seq: Serial,
    ) {
        <WlSurface as TouchTarget<DriftWm>>::down(&self.0, seat, data, event, seq);
    }

    fn up(
        &self,
        seat: &Seat<DriftWm>,
        data: &mut DriftWm,
        event: &TouchUpEvent,
        seq: Serial,
    ) {
        <WlSurface as TouchTarget<DriftWm>>::up(&self.0, seat, data, event, seq);
    }

    fn motion(
        &self,
        seat: &Seat<DriftWm>,
        data: &mut DriftWm,
        event: &TouchMotionEvent,
        seq: Serial,
    ) {
        <WlSurface as TouchTarget<DriftWm>>::motion(&self.0, seat, data, event, seq);
    }

    fn frame(&self, seat: &Seat<DriftWm>, data: &mut DriftWm, seq: Serial) {
        <WlSurface as TouchTarget<DriftWm>>::frame(&self.0, seat, data, seq);
    }

    fn cancel(&self, seat: &Seat<DriftWm>, data: &mut DriftWm, seq: Serial) {
        <WlSurface as TouchTarget<DriftWm>>::cancel(&self.0, seat, data, seq);
    }

    fn shape(
        &self,
        seat: &Seat<DriftWm>,
        data: &mut DriftWm,
        event: &smithay::input::touch::ShapeEvent,
        seq: Serial,
    ) {
        <WlSurface as TouchTarget<DriftWm>>::shape(&self.0, seat, data, event, seq);
    }

    fn orientation(
        &self,
        seat: &Seat<DriftWm>,
        data: &mut DriftWm,
        event: &smithay::input::touch::OrientationEvent,
        seq: Serial,
    ) {
        <WlSurface as TouchTarget<DriftWm>>::orientation(&self.0, seat, data, event, seq);
    }
}

use smithay::input::dnd::{DndFocus, Source};
use smithay::utils::{Logical, Point};
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::input::SeatHandler;

impl<D> DndFocus<D> for FocusTarget
where
    D: SeatHandler + smithay::wayland::selection::data_device::DataDeviceHandler + 'static,
    WlSurface: DndFocus<D>,
{
    type OfferData<S> = <WlSurface as DndFocus<D>>::OfferData<S> where S: Source;

    fn enter<S: Source>(
        &self,
        data: &mut D,
        dh: &DisplayHandle,
        source: std::sync::Arc<S>,
        seat: &Seat<D>,
        location: Point<f64, Logical>,
        serial: &Serial,
    ) -> Option<Self::OfferData<S>> {
        <WlSurface as DndFocus<D>>::enter(&self.0, data, dh, source, seat, location, serial)
    }

    fn motion<S: Source>(
        &self,
        data: &mut D,
        offer: Option<&mut Self::OfferData<S>>,
        seat: &Seat<D>,
        location: Point<f64, Logical>,
        time: u32,
    ) {
        <WlSurface as DndFocus<D>>::motion(&self.0, data, offer, seat, location, time)
    }

    fn leave<S: Source>(&self, data: &mut D, offer: Option<&mut Self::OfferData<S>>, seat: &Seat<D>) {
        <WlSurface as DndFocus<D>>::leave(&self.0, data, offer, seat)
    }

    fn drop<S: Source>(&self, data: &mut D, offer: Option<&mut Self::OfferData<S>>, seat: &Seat<D>) {
        <WlSurface as DndFocus<D>>::drop(&self.0, data, offer, seat)
    }
}
