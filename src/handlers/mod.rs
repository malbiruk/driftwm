pub mod compositor;
pub mod xdg_shell;

use crate::state::{DriftWm, FocusTarget};
use smithay::{
    backend::renderer::ImportDma,
    delegate_cursor_shape, delegate_data_control, delegate_data_device, delegate_dmabuf,
    delegate_fractional_scale, delegate_idle_inhibit,
    delegate_keyboard_shortcuts_inhibit, delegate_output, delegate_pointer_constraints,
    delegate_presentation, delegate_primary_selection, delegate_relative_pointer, delegate_seat,
    delegate_viewporter, delegate_xdg_activation,
    input::{
        Seat, SeatHandler, SeatState,
        pointer::{CursorImageStatus, PointerHandle},
    },
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    utils::{Logical, Point},
    wayland::{
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        fractional_scale::FractionalScaleHandler,
        idle_inhibit::IdleInhibitHandler,
        keyboard_shortcuts_inhibit::{
            KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitor,
        },
        output::OutputHandler,
        pointer_constraints::PointerConstraintsHandler,
        selection::{
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
                set_data_device_focus,
            },
            primary_selection::{PrimarySelectionHandler, PrimarySelectionState, set_primary_focus},
            wlr_data_control::{DataControlHandler, DataControlState},
            SelectionHandler,
        },
        tablet_manager::TabletSeatHandler,
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken,
            XdgActivationTokenData,
        },
    },
};

// --- SeatHandler ---

impl SeatHandler for DriftWm {
    type KeyboardFocus = FocusTarget;
    type PointerFocus = FocusTarget;
    type TouchFocus = FocusTarget;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        // During a compositor grab (pan, resize), we control the cursor.
        // Ignore client updates so they don't stomp our grab cursor.
        if self.grab_cursor {
            return;
        }
        self.cursor_status = image;
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&Self::KeyboardFocus>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|f| dh.get_client(f.0.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);
    }
}

delegate_seat!(DriftWm);

// --- SelectionHandler ---

impl SelectionHandler for DriftWm {
    type SelectionUserData = ();
}

// --- DataDeviceHandler ---

impl DataDeviceHandler for DriftWm {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for DriftWm {}
impl ServerDndGrabHandler for DriftWm {}

delegate_data_device!(DriftWm);

// --- OutputHandler ---

impl OutputHandler for DriftWm {}

delegate_output!(DriftWm);

// --- TabletSeatHandler (required by cursor_shape) ---

impl TabletSeatHandler for DriftWm {}

// --- CursorShapeManager ---

delegate_cursor_shape!(DriftWm);

// --- DmabufHandler ---

impl DmabufHandler for DriftWm {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        let Some(backend) = self.backend.as_mut() else {
            notifier.failed();
            return;
        };
        if backend.renderer().import_dmabuf(&dmabuf, None).is_ok() {
            let _ = notifier.successful::<DriftWm>();
        } else {
            notifier.failed();
        }
    }
}

delegate_dmabuf!(DriftWm);

// --- Viewporter ---

delegate_viewporter!(DriftWm);

// --- FractionalScale ---

impl FractionalScaleHandler for DriftWm {
    fn new_fractional_scale(&mut self, _surface: WlSurface) {}
}

delegate_fractional_scale!(DriftWm);

// --- XdgActivation ---

impl XdgActivationHandler for DriftWm {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        _token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // Simple focus-and-raise: find window by surface, raise it, set keyboard focus.
        let window = self
            .space
            .elements()
            .find(|w| w.toplevel().unwrap().wl_surface() == &surface)
            .cloned();
        if let Some(window) = window {
            self.space.raise_element(&window, true);
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            let keyboard = self.seat.get_keyboard().unwrap();
            keyboard.set_focus(self, Some(FocusTarget(surface)), serial);
        }
    }
}

delegate_xdg_activation!(DriftWm);

// --- PrimarySelection ---

impl PrimarySelectionHandler for DriftWm {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

delegate_primary_selection!(DriftWm);

// --- DataControl (wlr) ---

impl DataControlHandler for DriftWm {
    fn data_control_state(&self) -> &DataControlState {
        &self.data_control_state
    }
}

delegate_data_control!(DriftWm);

// --- PointerConstraints ---

impl PointerConstraintsHandler for DriftWm {
    fn new_constraint(&mut self, _surface: &WlSurface, _pointer: &PointerHandle<Self>) {}

    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        _location: Point<f64, Logical>,
    ) {
    }
}

delegate_pointer_constraints!(DriftWm);

// --- RelativePointer ---

delegate_relative_pointer!(DriftWm);

// --- KeyboardShortcutsInhibit ---

impl KeyboardShortcutsInhibitHandler for DriftWm {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit_state
    }

    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        inhibitor.activate();
    }

    fn inhibitor_destroyed(&mut self, _inhibitor: KeyboardShortcutsInhibitor) {}
}

delegate_keyboard_shortcuts_inhibit!(DriftWm);

// --- IdleInhibit ---

impl IdleInhibitHandler for DriftWm {
    fn inhibit(&mut self, _surface: WlSurface) {}
    fn uninhibit(&mut self, _surface: WlSurface) {}
}

delegate_idle_inhibit!(DriftWm);

// --- Presentation ---

delegate_presentation!(DriftWm);
