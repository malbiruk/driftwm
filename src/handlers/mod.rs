pub mod compositor;
pub mod layer_shell;
pub mod xdg_shell;
pub mod xwayland;

use crate::state::{DriftWm, FocusTarget};
use driftwm::window_ext::WindowExt;
use smithay::wayland::seat::WaylandFocus;
use smithay::{
    backend::renderer::ImportDma,
    delegate_cursor_shape, delegate_data_control, delegate_data_device, delegate_dmabuf,
    delegate_fractional_scale, delegate_idle_inhibit, delegate_keyboard_shortcuts_inhibit,
    delegate_output, delegate_pointer_constraints, delegate_presentation,
    delegate_pointer_gestures, delegate_primary_selection, delegate_relative_pointer,
    delegate_seat, delegate_viewporter,
    delegate_xdg_activation,
    input::{
        Seat, SeatHandler, SeatState,
        pointer::{CursorIcon, CursorImageStatus, PointerHandle},
    },
    reexports::wayland_server::{
        Resource,
        protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    },
    utils::{Logical, Point},
    wayland::{
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        fractional_scale::FractionalScaleHandler,
        idle_inhibit::IdleInhibitHandler,
        keyboard_shortcuts_inhibit::{KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitor},
        output::OutputHandler,
        pointer_constraints::PointerConstraintsHandler,
        selection::{
            SelectionHandler,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
                set_data_device_focus,
            },
            primary_selection::{
                PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
            },
            wlr_data_control::{DataControlHandler, DataControlState},
        },
        tablet_manager::TabletSeatHandler,
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
    },
};

impl SeatHandler for DriftWm {
    type KeyboardFocus = FocusTarget;
    type PointerFocus = FocusTarget;
    type TouchFocus = FocusTarget;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        // During a compositor grab (pan, resize) or decoration hover,
        // we control the cursor. Ignore client updates.
        if self.grab_cursor || self.decoration_cursor {
            return;
        }
        // During exec loading (after grace period), replace default cursor with
        // Wait but let client surface cursors through (they take priority).
        if self.exec_cursor_deadline.is_some()
            && self.exec_cursor_show_at.is_none_or(|t| std::time::Instant::now() >= t)
            && matches!(&image, CursorImageStatus::Named(icon) if *icon == CursorIcon::Default)
        {
            self.cursor_status = CursorImageStatus::Named(CursorIcon::Wait);
            return;
        }
        self.cursor_status = image;
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&Self::KeyboardFocus>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|f| dh.get_client(f.0.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);

        // Update focus history (skip during Alt-Tab cycling — history is frozen)
        if self.cycle_state.is_none()
            && let Some(focus) = focused
        {
            self.update_focus_history(&focus.0);
        }
    }
}

delegate_seat!(DriftWm);

impl SelectionHandler for DriftWm {
    type SelectionUserData = ();
}

impl DataDeviceHandler for DriftWm {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for DriftWm {}
impl ServerDndGrabHandler for DriftWm {}

delegate_data_device!(DriftWm);

impl OutputHandler for DriftWm {}

delegate_output!(DriftWm);

impl TabletSeatHandler for DriftWm {}

delegate_cursor_shape!(DriftWm);

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

delegate_viewporter!(DriftWm);

impl FractionalScaleHandler for DriftWm {
    fn new_fractional_scale(&mut self, _surface: WlSurface) {}
}

delegate_fractional_scale!(DriftWm);

impl XdgActivationHandler for DriftWm {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // Only honor tokens created from user input (has a serial).
        // Tokens without a serial are spontaneous attention requests from
        // background apps — ignore those to prevent focus stealing.
        if token_data.serial.is_none() {
            return;
        }
        if driftwm::config::applied_rule(&surface).is_some_and(|r| r.no_focus) {
            return;
        }
        let window = self
            .space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(&surface))
            .cloned();
        if let Some(window) = window {
            let mostly_visible = self.space.element_location(&window).is_some_and(|loc| {
                driftwm::canvas::visible_fraction(
                    loc,
                    window.geometry().size,
                    self.camera(),
                    self.get_viewport_size(),
                    self.zoom(),
                ) >= 0.5
            });
            if mostly_visible {
                self.space.raise_element(&window, true);
                self.enforce_below_windows();
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                let keyboard = self.seat.get_keyboard().unwrap();
                keyboard.set_focus(self, Some(FocusTarget(surface)), serial);
            } else {
                self.navigate_to_window(&window, false);
            }
        }
    }
}

delegate_xdg_activation!(DriftWm);

impl PrimarySelectionHandler for DriftWm {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

delegate_primary_selection!(DriftWm);

impl DataControlHandler for DriftWm {
    fn data_control_state(&self) -> &DataControlState {
        &self.data_control_state
    }
}

delegate_data_control!(DriftWm);

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

delegate_relative_pointer!(DriftWm);
delegate_pointer_gestures!(DriftWm);

impl KeyboardShortcutsInhibitHandler for DriftWm {
    fn keyboard_shortcuts_inhibit_state(
        &mut self,
    ) -> &mut smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit_state
    }

    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        inhibitor.activate();
    }

    fn inhibitor_destroyed(&mut self, _inhibitor: KeyboardShortcutsInhibitor) {}
}

delegate_keyboard_shortcuts_inhibit!(DriftWm);

impl IdleInhibitHandler for DriftWm {
    fn inhibit(&mut self, _surface: WlSurface) {}
    fn uninhibit(&mut self, _surface: WlSurface) {}
}

delegate_idle_inhibit!(DriftWm);

delegate_presentation!(DriftWm);

use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::delegate_xdg_decoration;

impl XdgDecorationHandler for DriftWm {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
        // CSD-first: tell client to draw its own decorations
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ClientSide);
        });
        toplevel.send_configure();
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode) {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
        // Accept the client's preference. Apps that genuinely need SSD (Qt/Vorta)
        // will request ServerSide; CSD-capable apps (GTK4) typically accept our
        // initial ClientSide and never call request_mode.
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        toplevel.send_configure();

        let wl_surface = toplevel.wl_surface().clone();
        if mode == Mode::ServerSide {
            self.pending_ssd.insert(wl_surface.id());
            // If the window is already mapped (request_mode came after first commit),
            // create the SSD decoration immediately.
            let window = self.space.elements()
                .find(|w| w.wl_surface().as_deref() == Some(&wl_surface))
                .cloned();
            if let Some(window) = window {
                let geo = window.geometry();
                if geo.size.w > 0 && !self.decorations.contains_key(&wl_surface.id()) {
                    let deco = crate::decorations::WindowDecoration::new(
                        geo.size.w, true, &self.config.decorations,
                    );
                    self.decorations.insert(wl_surface.id(), deco);
                }
            }
        } else {
            self.pending_ssd.remove(&wl_surface.id());
            self.decorations.remove(&wl_surface.id());
        }
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ClientSide);
        });
        toplevel.send_configure();
    }
}

delegate_xdg_decoration!(DriftWm);

use driftwm::protocols::foreign_toplevel::{ForeignToplevelHandler, ForeignToplevelManagerState};

impl ForeignToplevelHandler for DriftWm {
    fn foreign_toplevel_manager_state(&mut self) -> &mut ForeignToplevelManagerState {
        &mut self.foreign_toplevel_state
    }

    fn foreign_toplevel_outputs(&self) -> Vec<smithay::output::Output> {
        self.space.outputs().cloned().collect()
    }

    fn activate(&mut self, wl_surface: WlSurface) {
        if driftwm::config::applied_rule(&wl_surface).is_some_and(|r| r.no_focus) {
            return;
        }
        let window = self
            .space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(&wl_surface))
            .cloned();
        if let Some(window) = window {
            self.navigate_to_window(&window, true);
        }
    }

    fn close(&mut self, wl_surface: WlSurface) {
        let window = self
            .space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(&wl_surface))
            .cloned();
        if let Some(window) = window {
            window.send_close();
        }
    }

    fn set_fullscreen(&mut self, wl_surface: WlSurface, _wl_output: Option<WlOutput>) {
        let window = self
            .space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(&wl_surface))
            .cloned();
        if let Some(window) = window {
            self.enter_fullscreen(&window);
        }
    }

    fn unset_fullscreen(&mut self, wl_surface: WlSurface) {
        if let Some(output) = self.find_fullscreen_output_for_surface(&wl_surface) {
            self.exit_fullscreen_on(&output);
        }
    }
}

driftwm::delegate_foreign_toplevel!(DriftWm);

use driftwm::protocols::screencopy::{ScreencopyHandler, ScreencopyManagerState, Screencopy};

impl ScreencopyHandler for DriftWm {
    fn frame(&mut self, screencopy: Screencopy) {
        self.pending_screencopies.push(screencopy);
    }

    fn screencopy_state(&mut self) -> &mut ScreencopyManagerState {
        &mut self.screencopy_state
    }
}

driftwm::delegate_screencopy!(DriftWm);

driftwm::delegate_image_capture_source!(DriftWm);

use driftwm::protocols::image_copy_capture::{ImageCopyCaptureHandler, ImageCopyCaptureState, PendingCapture};

impl ImageCopyCaptureHandler for DriftWm {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.image_copy_capture_state
    }

    fn capture_frame(&mut self, capture: PendingCapture) {
        self.pending_captures.push(capture);
    }
}

driftwm::delegate_image_copy_capture!(DriftWm);

use driftwm::protocols::output_management::{
    OutputManagementHandler, OutputManagementState, RequestedHeadConfig,
};

impl OutputManagementHandler for DriftWm {
    fn output_management_state(&mut self) -> &mut OutputManagementState {
        &mut self.output_management_state
    }

    fn apply_output_config(&mut self, configs: Vec<RequestedHeadConfig>) -> bool {
        for cfg in &configs {
            let output = self
                .space
                .outputs()
                .find(|o| o.name() == cfg.output_name)
                .cloned();
            let Some(output) = output else {
                return false;
            };

            let current_mode = output.current_mode();
            let new_transform = cfg.transform.or_else(|| Some(output.current_transform()));
            let new_scale = cfg.scale.map(smithay::output::Scale::Fractional);

            let new_position = cfg.position.map(|(x, y)| {
                let mut os = crate::state::output_state(&output);
                os.layout_position = (x, y).into();
                os.layout_position
            });

            output.change_current_state(current_mode, new_transform, new_scale, new_position);

            self.cached_bg_elements.remove(&cfg.output_name);
        }
        self.mark_all_dirty();
        self.output_config_dirty = true;
        true
    }
}

driftwm::delegate_output_management!(DriftWm);

use smithay::delegate_session_lock;
use smithay::wayland::session_lock::{
    LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
};
use crate::state::SessionLock;

impl SessionLockHandler for DriftWm {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_manager_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        tracing::info!("Session lock requested");
        self.session_lock = SessionLock::Pending(confirmation);

        // Kill all transient input/animation state so nothing fires during lock
        self.gesture_state = None;
        for output in self.space.outputs().cloned().collect::<Vec<_>>() {
            let mut os = crate::state::output_state(&output);
            os.momentum.stop();
            os.edge_pan_velocity = None;
            os.panning = false;
            os.camera_target = None;
            os.zoom_target = None;
            os.zoom_animation_center = None;
        }
        self.held_action = None;
        self.grab_cursor = false;
        if let Some(pending) = self.pending_middle_click.take() {
            self.loop_handle.remove(pending.timer_token);
        }
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let pointer = self.seat.get_pointer().unwrap();
        pointer.unset_grab(self, serial, 0);

        self.exec_cursor_show_at = None;
        self.exec_cursor_deadline = None;
        self.cursor_status = smithay::input::pointer::CursorImageStatus::default_named();
        // Clear keyboard focus — no window should be interactable
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, None::<FocusTarget>, serial);
        self.mark_all_dirty();
    }

    fn unlock(&mut self) {
        tracing::info!("Session unlocked");
        self.session_lock = SessionLock::Unlocked;
        self.lock_surfaces.clear();
        // Restore focus to the most recent window
        if let Some(window) = self.focus_history.first().cloned() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            let keyboard = self.seat.get_keyboard().unwrap();
            let focus = window.wl_surface().map(|s| FocusTarget(s.into_owned()));
            keyboard.set_focus(self, focus, serial);
        }
        self.mark_all_dirty();
    }

    fn new_surface(&mut self, surface: LockSurface, wl_output: WlOutput) {
        let output = smithay::output::Output::from_resource(&wl_output)
            .or_else(|| self.active_output());
        let Some(output) = output else { return };

        let output_size = crate::state::output_logical_size(&output);

        surface.with_pending_state(|state| {
            state.size = Some((output_size.w as u32, output_size.h as u32).into());
        });
        surface.send_configure();
        self.lock_surfaces.insert(output, surface);
    }

}

delegate_session_lock!(DriftWm);
