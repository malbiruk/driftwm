pub mod background_effect;
pub mod compositor;
pub mod layer_shell;
pub mod xdg_shell;

use crate::decorations::DecorationKey;
use crate::state::{DriftWm, FocusIntent, FocusTarget};
use driftwm::window_ext::WindowExt;
use smithay::wayland::seat::WaylandFocus;
use smithay::{
    backend::renderer::ImportDma,
    delegate_cursor_shape, delegate_data_control, delegate_data_device, delegate_dmabuf,
    delegate_ext_data_control, delegate_fractional_scale, delegate_idle_inhibit,
    delegate_input_method_manager, delegate_keyboard_shortcuts_inhibit, delegate_output,
    delegate_pointer_constraints, delegate_pointer_gestures, delegate_presentation,
    delegate_primary_selection, delegate_relative_pointer, delegate_seat,
    delegate_security_context, delegate_single_pixel_buffer, delegate_text_input_manager,
    delegate_viewporter, delegate_xdg_activation,
    input::{
        Seat, SeatHandler, SeatState,
        dnd::{self, DnDGrab},
        keyboard,
        pointer::{CursorIcon, CursorImageStatus, Focus, PointerHandle},
    },
    reexports::input::DeviceCapability as LibinputCapability,
    reexports::wayland_server::{
        Resource,
        protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    },
    utils::Serial,
    utils::{Logical, Point},
    wayland::{
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        fractional_scale::FractionalScaleHandler,
        idle_inhibit::IdleInhibitHandler,
        input_method::{InputMethodHandler, PopupSurface},
        keyboard_shortcuts_inhibit::{KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitor},
        output::OutputHandler,
        pointer_constraints::PointerConstraintsHandler,
        security_context::{
            SecurityContext, SecurityContextHandler, SecurityContextListenerSource,
        },
        selection::{
            SelectionHandler,
            data_device::{
                DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
            },
            ext_data_control::{
                DataControlHandler as ExtDataControlHandler,
                DataControlState as ExtDataControlState,
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
        if self.cursor.grab_cursor || self.cursor.decoration_cursor {
            return;
        }
        // During exec loading (after grace period), replace default cursor with
        // Wait but let client surface cursors through (they take priority).
        if self.cursor.exec_cursor_deadline.is_some()
            && self
                .cursor
                .exec_cursor_show_at
                .is_none_or(|t| std::time::Instant::now() >= t)
            && matches!(&image, CursorImageStatus::Named(icon) if *icon == CursorIcon::Default)
        {
            self.cursor.cursor_status = CursorImageStatus::Named(CursorIcon::Wait);
        } else {
            self.cursor.cursor_status = image;
        }
        // A wp_cursor_shape change (set_shape) commits no buffer, so nothing
        // else marks the scene dirty; without this the new cursor isn't
        // composited until the next unrelated damage. Surface cursors dodge
        // this via their own buffer commit.
        self.mark_all_dirty();
    }

    fn led_state_changed(&mut self, _seat: &Seat<Self>, led_state: keyboard::LedState) {
        for device in self
            .input_devices
            .iter_mut()
            .filter(|d| d.has_capability(LibinputCapability::Keyboard))
        {
            device.led_update(led_state.into());
        }
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&Self::KeyboardFocus>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|f| dh.get_client(f.0.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);

        if let Some(focus) = focused {
            // A non-cycle focus change during a session (a click, a newly mapped
            // window, an IPC focus, …) ends it first — committing the stale
            // selection — then the newly focused window promotes normally on top.
            // A cycle step's own navigate is exempt so it keeps the history frozen.
            if self.stage.cycle_state().is_some() && !self.cycle_navigating {
                self.end_cycle();
            }
            // Skip during Alt-Tab cycling — history is frozen until the session ends.
            if self.stage.cycle_state().is_none() {
                self.update_focus_history(&focus.0);
            }
        }

        // Track the last window that actually held focus so the recompute can
        // restore it after a layer surface (launcher) or lock screen goes away.
        // Layer / lock surfaces aren't windows, so they never overwrite it.
        if let Some(focus) = focused
            && self.window_for_surface(&focus.0).is_some()
        {
            self.window_focus = Some(FocusIntent::Surface(focus.clone()));
        }
    }
}

delegate_seat!(DriftWm);
delegate_text_input_manager!(DriftWm);

impl SelectionHandler for DriftWm {
    type SelectionUserData = ();
}

impl DataDeviceHandler for DriftWm {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl WaylandDndGrabHandler for DriftWm {
    fn dnd_requested<S: dnd::Source>(
        &mut self,
        source: S,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: dnd::GrabType,
    ) {
        self.dnd_icon = icon.map(|surface| crate::state::DndIcon {
            surface,
            offset: (0, 0).into(),
        });
        match type_ {
            dnd::GrabType::Pointer => {
                let pointer = seat.get_pointer().unwrap();
                let start_data = pointer.grab_start_data().unwrap();
                let grab = DnDGrab::new_pointer(&self.display_handle, start_data, source, seat);
                pointer.set_grab(self, grab, serial, Focus::Keep);
            }
            dnd::GrabType::Touch => {
                let touch = seat.get_touch().unwrap();
                let start_data = touch.grab_start_data().unwrap();
                let grab = DnDGrab::new_touch(&self.display_handle, start_data, source, seat);
                touch.set_grab(self, grab, serial);
            }
        }
    }
}
impl dnd::DndGrabHandler for DriftWm {
    fn dropped(
        &mut self,
        _target: Option<dnd::DndTarget<'_, Self>>,
        _validated: bool,
        _seat: Seat<Self>,
        _location: Point<f64, Logical>,
    ) {
        self.dnd_icon = None;
    }
}

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
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self
            .active_output()
            .map(|o| o.current_scale().fractional_scale())
            .unwrap_or(1.0);
        smithay::wayland::compositor::with_states(&surface, |data| {
            smithay::wayland::fractional_scale::with_fractional_scale(data, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }
}

delegate_fractional_scale!(DriftWm);

impl XdgActivationHandler for DriftWm {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        if data.serial.is_some() {
            let now = std::time::Instant::now();
            self.cursor.exec_cursor_show_at = Some(now + std::time::Duration::from_millis(150));
            self.cursor.exec_cursor_deadline = Some(now + std::time::Duration::from_secs(5));
        }
        true
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        let self_activation = token_data.surface.as_ref().is_some_and(|req_surface| {
            let req_client = self.display_handle.get_client(req_surface.id()).ok();
            let act_client = self.display_handle.get_client(surface.id()).ok();
            req_client.is_some() && req_client == act_client
        });

        // Same client activating itself (e.g. Telegram switching chats) — cancel loading cursor
        if self_activation {
            self.cursor.exec_cursor_show_at = None;
            self.cursor.exec_cursor_deadline = None;
        }

        // A compositor-minted relaunch token adopts its target suspended window
        // instead of following the normal activation path. Honored before the
        // serial gate (our tokens carry no serial) and before the zero-size
        // early return (a pre-first-commit adopt is stashed for the placement
        // arm). A stale marker (target dismissed/expired) falls through to the
        // normal path.
        if let Some(sid) = token_data
            .user_data
            .get::<crate::state::RelaunchMarker>()
            .map(|m| m.0)
            && self.pending_relaunches.contains_key(&sid)
            && self.find_suspended(sid).is_some()
        {
            let window = self.window_for_surface(&surface);
            let root = window
                .as_ref()
                .and_then(|w| w.wl_surface().map(|s| s.into_owned()))
                .unwrap_or_else(|| surface.clone());
            if self.pending_center.contains(&root) {
                // Not yet placed: the first-commit placement arm adopts it.
                self.pending_adoptions.insert(root, sid);
                return;
            }
            // Already placed. Any window presenting our token is the app's own
            // answer to this relaunch: the token traveled from the spawn through
            // the child env into the app, so a single-instance app forwarding it
            // to its running window is fulfilling the press, not being hijacked.
            // The press expressed placement intent at the stand-in's slot, so
            // adopt the window into it.
            if let Some(window) = window {
                // A window already fullscreen, pinned, rule-placed as a widget,
                // or living as a dialog/modal of another window is where policy
                // (or its parent) wants it; adopting would rip it out of that
                // membership — and, for a dialog, tear it off its parent. Every
                // suspend path excludes dialogs, so no stand-in ever stands for
                // one. Drop the stand-in instead and leave the window alone.
                if self.is_window_fullscreen(&window)
                    || self.is_pinned(&window)
                    || window.is_widget()
                    || window.parent_surface().is_some()
                    || window.is_modal()
                {
                    tracing::debug!(
                        "relaunch adopt of {sid:?} skipped: window is fullscreen/pinned/widget/dialog; dismissing stand-in"
                    );
                    self.dismiss_suspended(sid);
                    return;
                }
                // About to adopt — but not while the window is under an active
                // interactive move/resize grab: teleporting it would fight the
                // grab. Transient (unlike the carve-outs above), so leave the
                // pending relaunch to its TTL and don't dismiss the stand-in.
                if self.window_under_interactive_grab(&window, &root) {
                    return;
                }
                self.adopt_relaunched(&window, &root, sid);
                if let Some(toplevel) = window.toplevel() {
                    toplevel.send_configure();
                }
                return;
            }
        }

        // Only honor tokens created from user input (has a serial).
        // Tokens without a serial are spontaneous attention requests from
        // background apps — ignore those to prevent focus stealing.
        if token_data.serial.is_none() {
            return;
        }
        let window = self.window_for_surface(&surface);
        if let Some(window) = window {
            // Skip windows that haven't rendered yet — navigate_to_window on a
            // zero-sized window sets a fractional camera that breaks cascade.
            if window.geometry().size.w == 0 || window.geometry().size.h == 0 {
                return;
            }
            // Chromium mints its activation token from the enter serial it was
            // just handed, so under focus-follows-mouse a mere hover could
            // self-activate and pan the camera onto a clipped window. Tokens
            // from another client (e.g. a notification daemon) still navigate.
            if self_activation && self.window_already_active(&window) {
                return;
            }
            self.activate_window_output_local(&window);
        }
    }
}

delegate_xdg_activation!(DriftWm);

impl PrimarySelectionHandler for DriftWm {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

delegate_primary_selection!(DriftWm);

impl DataControlHandler for DriftWm {
    fn data_control_state(&mut self) -> &mut DataControlState {
        &mut self.data_control_state
    }
}

delegate_data_control!(DriftWm);

impl ExtDataControlHandler for DriftWm {
    fn data_control_state(&mut self) -> &mut ExtDataControlState {
        &mut self.ext_data_control_state
    }
}

delegate_ext_data_control!(DriftWm);

impl PointerConstraintsHandler for DriftWm {
    fn new_constraint(&mut self, _surface: &WlSurface, _pointer: &PointerHandle<Self>) {
        // Pointer constraints track pointer focus internally, so bring it up to
        // date before activating: a client that re-creates a oneshot constraint
        // (destroyed on deactivation) needs current focus for the new one to
        // re-arm, e.g. a game whose cursor returns to its fullscreen surface.
        self.refresh_pointer_focus();
        self.maybe_activate_pointer_constraint();
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        use smithay::wayland::pointer_constraints::with_pointer_constraint;

        let is_active =
            with_pointer_constraint(surface, pointer, |c| c.is_some_and(|c| c.is_active()));
        if !is_active {
            return;
        }

        // The pointer's internal canvas location must track the game's expected
        // cursor position; otherwise, when the client briefly destroys and
        // recreates its lock (Wine/Proton does this constantly), motion events
        // delivered during the gap reach the surface with stale surface-local
        // coordinates and the game snaps the camera back.
        let window = self.window_for_surface(surface);
        if let Some(window) = window
            && let Some(loc) = self.stage.position_of(&window)
        {
            pointer.set_location(loc.to_f64() + location);
        }
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
        // Smithay 0.7 has no per-client filter for this protocol, and the
        // inhibitor is already registered before this callback fires. Refusing
        // means leaving it inactive — the client gets a dead inhibitor resource
        // and shortcuts continue to flow to the compositor.
        let allowed = inhibitor
            .wl_surface()
            .client()
            .as_ref()
            .map(crate::state::client_is_unrestricted)
            .unwrap_or(true);
        if allowed {
            inhibitor.activate();
        }
    }

    fn inhibitor_destroyed(&mut self, _inhibitor: KeyboardShortcutsInhibitor) {}
}

delegate_keyboard_shortcuts_inhibit!(DriftWm);

impl SecurityContextHandler for DriftWm {
    fn context_created(&mut self, source: SecurityContextListenerSource, context: SecurityContext) {
        let result = self
            .loop_handle
            .insert_source(source, move |client, _, state| {
                tracing::debug!("inserting restricted client from security context: {context:?}");
                let data = std::sync::Arc::new(crate::state::ClientState {
                    compositor_state: Default::default(),
                    is_restricted: true,
                });
                if let Err(err) = state.display_handle.insert_client(client, data) {
                    tracing::warn!("failed to insert restricted client: {err}");
                }
            });
        if let Err(err) = result {
            tracing::warn!("failed to register security context listener: {err}");
        }
    }
}
delegate_security_context!(DriftWm);

// Replaces smithay's virtual-keyboard delegate so OSK key presses run through
// compositor bindings first (see `protocols::virtual_keyboard`).
impl driftwm::protocols::virtual_keyboard::VirtualKeyboardBindingHandler for DriftWm {
    fn virtual_keyboard_bindings(
        &mut self,
    ) -> &mut driftwm::protocols::virtual_keyboard::VirtualKeyboardBindings {
        &mut self.virtual_kb_bindings
    }

    fn virtual_key_binding(
        &mut self,
        modifiers: &keyboard::ModifiersState,
        sym: keyboard::Keysym,
    ) -> bool {
        // While locked, the lock surface owns all input (an OSK may well be
        // typing the password); bindings stay off, everything forwards.
        if !matches!(self.session_lock, crate::state::SessionLock::Unlocked) {
            return false;
        }
        // Respect the focused window's pass_keys rule, as the physical path
        // does: a combo the window claims forwards even when bound. Skipped
        // when a suspended window holds the gated focus — no client to claim.
        if self.gated_suspended_focus().is_none() {
            let pass_keys = self.focused_window().and_then(|w| {
                let app_id = w.app_id_or_class().unwrap_or_default();
                let title = w.window_title().unwrap_or_default();
                self.config
                    .resolve_window_rules(&app_id, &title)
                    .map(|r| r.pass_keys)
            });
            if pass_keys.is_some_and(|pk| pk.allows_raw(modifiers, sym)) {
                return false;
            }
        }
        let Some(action) = self.config.lookup(modifiers, sym) else {
            return false;
        };
        let action = action.clone();
        self.execute_action(&action);
        true
    }
}
driftwm::delegate_virtual_keyboard_bindings!(DriftWm);

impl InputMethodHandler for DriftWm {
    fn new_popup(&mut self, surface: PopupSurface) {
        if let Err(err) = self
            .popups
            .track_popup(smithay::desktop::PopupKind::from(surface))
        {
            tracing::warn!("Failed to track input-method popup: {err}");
        }
    }

    fn dismiss_popup(&mut self, surface: PopupSurface) {
        if let Some(parent) = surface.get_parent().map(|parent| parent.surface.clone()) {
            let _ = smithay::desktop::PopupManager::dismiss_popup(
                &parent,
                &smithay::desktop::PopupKind::from(surface),
            );
        }
    }

    fn popup_repositioned(&mut self, _surface: PopupSurface) {}

    fn parent_geometry(&self, parent: &WlSurface) -> smithay::utils::Rectangle<i32, Logical> {
        self.stage
            .windows()
            .find_map(|window| {
                (window.wl_surface().as_deref() == Some(parent)).then(|| window.geometry())
            })
            .unwrap_or_default()
    }
}

delegate_input_method_manager!(DriftWm);

impl IdleInhibitHandler for DriftWm {
    fn inhibit(&mut self, surface: WlSurface) {
        self.idle_inhibiting_surfaces.insert(surface);
    }
    fn uninhibit(&mut self, surface: WlSurface) {
        self.idle_inhibiting_surfaces.remove(&surface);
    }
}

delegate_idle_inhibit!(DriftWm);

use smithay::delegate_idle_notify;
use smithay::wayland::idle_notify::{IdleNotifierHandler, IdleNotifierState};

impl IdleNotifierHandler for DriftWm {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.idle_notifier_state
    }
}
delegate_idle_notify!(DriftWm);

delegate_presentation!(DriftWm);
delegate_single_pixel_buffer!(DriftWm);

use smithay::delegate_xdg_foreign;
use smithay::wayland::xdg_foreign::{XdgForeignHandler, XdgForeignState};

impl XdgForeignHandler for DriftWm {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        &mut self.xdg_foreign_state
    }
}
delegate_xdg_foreign!(DriftWm);

use smithay::delegate_content_type;
delegate_content_type!(DriftWm);

use smithay::delegate_xdg_dialog;
use smithay::wayland::shell::xdg::dialog::XdgDialogHandler;

impl XdgDialogHandler for DriftWm {
    fn dialog_hint_changed(
        &mut self,
        toplevel: ToplevelSurface,
        hint: smithay::wayland::shell::xdg::dialog::ToplevelDialogHint,
    ) {
        if hint == smithay::wayland::shell::xdg::dialog::ToplevelDialogHint::Modal {
            // Redirect focus from parent to this modal dialog
            let wl_surface = toplevel.wl_surface().clone();
            let window = self.window_for_surface(&wl_surface);
            if let Some(window) = window {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                self.raise_and_focus(&window, serial);
            }
        }
    }
}
delegate_xdg_dialog!(DriftWm);

use smithay::delegate_xdg_decoration;
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;

pub use driftwm::window_ext::{decoration_mode_to_wire, set_tiled_states, unset_tiled_states};

impl XdgDecorationHandler for DriftWm {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        // Advertise the global default mode. Per-window rules override this in the
        // commit handler once app_id is known.
        let mode = decoration_mode_to_wire(&self.config.decorations.default_mode);
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        // Pre-initial-configure: state is folded into the upcoming initial configure.
        // Sending one now would race the initial configure — SDL2/SCTK desync on this.
        if toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
    }

    fn request_mode(
        &mut self,
        toplevel: ToplevelSurface,
        mode: smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
    ) {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;

        // Always honor the client's wire-mode request
        // (SDL2 has a bug where overriding leaves windows hidden).
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }

        // Decide whether this client gets a driftwm title bar:
        //   - Explicit rule decoration → only `Server` gets a bar.
        //   - No explicit rule → defer to default_mode, but fall back to
        //     creating a bar when default is `Client` and the client itself
        //     asked for SSD (Alacritty / no-CSD apps need *some* chrome).
        let wl_surface = toplevel.wl_surface().clone();
        let applied = driftwm::config::applied_rule(&wl_surface);
        let rule_explicit = applied
            .as_ref()
            .and_then(|a| a.decoration.as_ref())
            .cloned();
        let create_titlebar = match rule_explicit {
            Some(driftwm::config::DecorationMode::Server) => true,
            Some(_) => false,
            None => matches!(
                (&mode, &self.config.decorations.default_mode),
                (Mode::ServerSide, driftwm::config::DecorationMode::Client)
                    | (_, driftwm::config::DecorationMode::Server)
            ),
        };

        if create_titlebar {
            self.pending_ssd.insert(wl_surface.id());
            let window = self.window_for_surface(&wl_surface);
            if let Some(window) = window {
                let geo = window.geometry();
                if geo.size.w > 0
                    && !self
                        .decorations
                        .contains_key(&DecorationKey::Surface(wl_surface.id()))
                {
                    let deco = crate::decorations::WindowDecoration::new(
                        geo.size.w,
                        true,
                        &self.config.decorations,
                    );
                    self.decorations
                        .insert(DecorationKey::Surface(wl_surface.id()), deco);
                }
            }
        } else if mode == Mode::ClientSide {
            // Client switching back to CSD: drop any stale SSD chrome.
            self.pending_ssd.remove(&wl_surface.id());
            self.decorations
                .remove(&DecorationKey::Surface(wl_surface.id()));
            self.render
                .shadow_cache
                .remove(&DecorationKey::Surface(wl_surface.id()));
            self.render
                .border_cache
                .remove(&DecorationKey::Surface(wl_surface.id()));
        }
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        let mode = decoration_mode_to_wire(&self.config.decorations.default_mode);
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
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
        let window = self.window_for_surface(&wl_surface);
        if let Some(window) = window {
            self.activate_window_output_local(&window);
        }
    }

    fn close(&mut self, wl_surface: WlSurface) {
        let window = self.window_for_surface(&wl_surface);
        if let Some(window) = window {
            // A taskbar close is explicit user intent to close for real — mark
            // it so `suspend_on_close` doesn't leave a ghost the taskbar can't
            // see.
            self.mark_real_close(&window);
            window.send_close();
        }
    }

    fn set_fullscreen(&mut self, wl_surface: WlSurface, wl_output: Option<WlOutput>) {
        let client_output = wl_output.and_then(|wo| smithay::output::Output::from_resource(&wo));
        if self.pending_center.contains(&wl_surface) {
            self.pending_fullscreen.insert(wl_surface, client_output);
            return;
        }
        let window = self.window_for_surface(&wl_surface);
        if let Some(window) = window {
            let target = self.resolve_fullscreen_output(&wl_surface, client_output);
            self.enter_fullscreen(&window, target);
        }
    }

    fn unset_fullscreen(&mut self, wl_surface: WlSurface) {
        self.pending_fullscreen.remove(&wl_surface);
        if let Some(output) = self.find_fullscreen_output_for_surface(&wl_surface) {
            self.exit_fullscreen_on(&output);
        }
    }

    fn set_maximized(&mut self, wl_surface: WlSurface) {
        if self.pending_center.contains(&wl_surface) {
            self.pending_fit.insert(wl_surface);
            return;
        }
        let window = self.window_for_surface(&wl_surface);
        if let Some(window) = window {
            self.decoration_fit(&window);
        }
    }

    fn unset_maximized(&mut self, wl_surface: WlSurface) {
        self.pending_fit.remove(&wl_surface);
        let window = self.window_for_surface(&wl_surface);
        if let Some(window) = window {
            self.decoration_unfit(&window);
        }
    }
}

driftwm::delegate_foreign_toplevel!(DriftWm);

use driftwm::protocols::ext_workspace::{ExtWorkspaceHandler, ExtWorkspaceManagerState};

impl ExtWorkspaceHandler for DriftWm {
    fn ext_workspace_state(&mut self) -> &mut ExtWorkspaceManagerState {
        &mut self.ext_workspace_state
    }

    fn ext_workspace_outputs(&self) -> Vec<smithay::output::Output> {
        // Skip virtual placeholders for disconnected monitors — their wl_output
        // global is gone, so advertising them to a late-binding client would
        // enter an output the client can't resolve.
        self.space
            .outputs()
            .filter(|o| !self.disconnected_outputs.contains(&o.name()))
            .cloned()
            .collect()
    }

    fn workspace_activate(&mut self, name: String) {
        self.execute_action(&driftwm::config::Action::GoToBookmark(name));
    }

    fn workspace_create(&mut self, name: String) {
        if name.is_empty() {
            tracing::info!("ext-workspace create_workspace: ignoring empty name");
            return;
        }
        // Capture the focused viewport center under this name — set-bookmark
        // semantics (overwrites an existing bookmark; registry keys are unique).
        self.execute_action(&driftwm::config::Action::SetBookmark(name));
        // The registry change reaches clients only through the per-frame
        // refresh, so poke a render (nothing else self-schedules one at idle).
        self.mark_all_dirty();
    }

    fn workspace_remove(&mut self, name: String) {
        if self.bookmarks.remove(&name).is_some() {
            self.session_store_mark_dirty();
            self.mark_all_dirty();
        }
    }
}

driftwm::delegate_ext_workspace!(DriftWm);

impl smithay::wayland::foreign_toplevel_list::ForeignToplevelListHandler for DriftWm {
    fn foreign_toplevel_list_state(
        &mut self,
    ) -> &mut smithay::wayland::foreign_toplevel_list::ForeignToplevelListState {
        &mut self.foreign_toplevel_list_state
    }
}

smithay::delegate_foreign_toplevel_list!(DriftWm);

use driftwm::protocols::screencopy::{Screencopy, ScreencopyHandler, ScreencopyManagerState};

impl ScreencopyHandler for DriftWm {
    fn frame(&mut self, screencopy: Screencopy) {
        // A plain `copy` (e.g. grim) wants the current frame now, so kick a
        // redraw: render_if_needed bails when redraws_needed is empty, and an
        // idle-system capture would otherwise stall until unrelated damage.
        //
        // For copy_with_damage, forcing a render per pull would re-composite a
        // static scene every frame, keeping the GPU busy and defeating direct
        // scanout of a fullscreen client behind it; let real damage drive it.
        if !screencopy.with_damage() {
            self.redraws_needed.insert(screencopy.output().clone());
        }
        self.pending_screencopies.push(screencopy);
    }

    fn screencopy_state(&mut self) -> &mut ScreencopyManagerState {
        &mut self.screencopy_state
    }
}

driftwm::delegate_screencopy!(DriftWm);

use driftwm::protocols::output_power::{OutputPowerHandler, OutputPowerState};

impl OutputPowerHandler for DriftWm {
    fn output_power_state(&mut self) -> &mut OutputPowerState {
        &mut self.output_power_state
    }

    fn get_dpms(&mut self, output: &smithay::output::Output) -> Option<bool> {
        // Winit (nested) doesn't have DPMS — reply `failed` to the client.
        self.session.as_ref()?;
        Some(!self.dpms_off_outputs.contains(output))
    }

    fn set_dpms(&mut self, output: &smithay::output::Output, on: bool) {
        if self.session.is_none() {
            return;
        }
        let already = !self.dpms_off_outputs.contains(output);
        if already == on {
            self.pending_dpms.remove(output);
            return;
        }
        // Reflect the new state immediately so the inline `mode` event the
        // protocol sends after this call reports the requested state. The
        // backend transition (compositor.clear / re-schedule render) happens
        // when the udev render loop drains `pending_dpms`.
        if on {
            self.dpms_off_outputs.remove(output);
        } else {
            self.dpms_off_outputs.insert(output.clone());
        }
        self.pending_dpms.insert(output.clone(), on);
    }
}

driftwm::delegate_output_power!(DriftWm);

use smithay::wayland::foreign_toplevel_list::ForeignToplevelHandle;
use smithay::wayland::image_capture_source::{
    ImageCaptureSource, ImageCaptureSourceHandler, OutputCaptureSourceHandler,
    OutputCaptureSourceState, ToplevelCaptureSourceHandler, ToplevelCaptureSourceState,
};

impl ImageCaptureSourceHandler for DriftWm {
    fn source_destroyed(&mut self, _source: ImageCaptureSource) {}
}

impl OutputCaptureSourceHandler for DriftWm {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.output_capture_source_state
    }

    fn output_source_created(
        &mut self,
        source: ImageCaptureSource,
        output: &smithay::output::Output,
    ) {
        source.user_data().insert_if_missing(|| {
            driftwm::protocols::image_capture_source::SourceKind::Output(output.clone())
        });
    }
}

impl ToplevelCaptureSourceHandler for DriftWm {
    fn toplevel_capture_source_state(&mut self) -> &mut ToplevelCaptureSourceState {
        &mut self.toplevel_capture_source_state
    }

    fn toplevel_source_created(
        &mut self,
        source: ImageCaptureSource,
        toplevel: ForeignToplevelHandle,
    ) {
        let kind = match driftwm::protocols::foreign_toplevel::surface_for_ext_handle(&toplevel) {
            Some(surface) => {
                let initial_size = self
                    .stage
                    .windows()
                    .find(|w| w.wl_surface().as_deref() == Some(&surface))
                    .map(|w| {
                        let geo = w.geometry().size;
                        smithay::utils::Size::from((geo.w.max(1), geo.h.max(1)))
                    })
                    .unwrap_or_else(|| (1, 1).into());
                driftwm::protocols::image_capture_source::SourceKind::Toplevel {
                    surface,
                    initial_size,
                }
            }
            None => driftwm::protocols::image_capture_source::SourceKind::Destroyed,
        };
        source.user_data().insert_if_missing(|| kind);
    }
}

smithay::delegate_image_capture_source!(DriftWm);
smithay::delegate_output_capture_source!(DriftWm);
smithay::delegate_toplevel_capture_source!(DriftWm);

use driftwm::protocols::image_copy_capture::{
    ImageCopyCaptureHandler, ImageCopyCaptureState, PendingCapture,
};

impl ImageCopyCaptureHandler for DriftWm {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.image_copy_capture_state
    }

    fn capture_frame(&mut self, capture: PendingCapture) {
        use driftwm::protocols::image_copy_capture::PendingCaptureKind;
        // Kick a redraw so an idle-system capture is fulfilled promptly instead
        // of stalling until unrelated damage. Toplevel captures drain on any
        // output's render path, so the active output suffices.
        match &capture.kind {
            PendingCaptureKind::Output(output) => {
                self.redraws_needed.insert(output.clone());
            }
            PendingCaptureKind::Toplevel(_) => {
                if let Some(output) = self.active_output() {
                    self.redraws_needed.insert(output);
                }
            }
        }
        self.pending_captures.push(capture);
    }

    fn dmabuf_constraints(&self) -> Option<(u64, smithay::backend::allocator::format::FormatSet)> {
        Some((self.render_device?, self.render_dmabuf_formats.clone()?))
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
        let is_udev = matches!(self.backend, Some(crate::backend::Backend::Udev(_)));

        // Phase 1: validate everything and stage results. wlr-output-management
        // Apply is supposed to be all-or-nothing — if any head fails, we
        // commit nothing.
        struct Staged {
            output: smithay::output::Output,
            output_name: String,
            mode_intent: Option<crate::state::ModeIntent>,
            new_transform: Option<smithay::utils::Transform>,
            new_scale: Option<smithay::output::Scale>,
            new_position: Option<smithay::utils::Point<i32, smithay::utils::Logical>>,
        }
        let mut staged: Vec<Staged> = Vec::with_capacity(configs.len());

        for cfg in &configs {
            let Some(output) = self
                .space
                .outputs()
                .find(|o| o.name() == cfg.output_name)
                .cloned()
            else {
                return false;
            };

            if !is_udev && (cfg.mode_index.is_some() || cfg.custom_mode.is_some()) {
                tracing::warn!(
                    "Mode change for '{}' ignored: not supported on winit backend",
                    cfg.output_name
                );
                return false;
            }

            let mut mode_intent: Option<crate::state::ModeIntent> = None;
            if let Some(idx) = cfg.mode_index {
                let modes_len = self
                    .output_management_state
                    .current_state_for(&cfg.output_name)
                    .map(|s| s.modes.len())
                    .unwrap_or(0);
                if idx >= modes_len {
                    tracing::warn!(
                        "Mode index {idx} out of range for '{}' ({modes_len} modes known)",
                        cfg.output_name
                    );
                    return false;
                }
                mode_intent = Some(crate::state::ModeIntent::EdidIndex(idx));
            }
            if let Some((w, h, refresh_mhz)) = cfg.custom_mode {
                let ok = (320..=16384).contains(&w)
                    && (200..=16384).contains(&h)
                    && (1000..=500_000).contains(&refresh_mhz);
                if !ok {
                    tracing::warn!(
                        "Custom mode {w}x{h}@{refresh_mhz}mHz for '{}' out of bounds",
                        cfg.output_name
                    );
                    return false;
                }
                mode_intent = Some(crate::state::ModeIntent::Custom { w, h, refresh_mhz });
            }

            let new_transform = cfg.transform;
            let new_scale = cfg.scale.map(smithay::output::Scale::Fractional);
            let new_position = cfg.position.map(|(x, y)| (x, y).into());

            staged.push(Staged {
                output,
                output_name: cfg.output_name.clone(),
                mode_intent,
                new_transform,
                new_scale,
                new_position,
            });
        }

        // Phase 2: commit. Validation already succeeded for every head.
        for s in staged {
            if let Some(intent) = s.mode_intent {
                self.pending_mode_changes
                    .insert(s.output_name.clone(), intent);
            }

            if let Some(pos) = s.new_position {
                let mut os = crate::state::output_state(&s.output);
                os.layout_position = pos;
            }

            let new_transform = s
                .new_transform
                .or_else(|| Some(s.output.current_transform()));
            s.output.change_current_state(
                s.output.current_mode(),
                new_transform,
                s.new_scale,
                s.new_position,
            );

            {
                let mut map = smithay::desktop::layer_map_for_output(&s.output);
                map.arrange();
            }
            let size = crate::state::output_logical_size(&s.output);
            self.resize_fullscreen_for_output(&s.output, size);

            self.render.remove_output(&s.output_name);
        }
        self.recompute_decoration_scale();
        self.mark_all_dirty();
        self.output_config_dirty = true;
        true
    }
}

driftwm::delegate_output_management!(DriftWm);

use crate::state::SessionLock;
use smithay::delegate_session_lock;
use smithay::wayland::session_lock::{
    LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
};

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
            os.edge_pan_screen_pos = None;
            os.edge_pan_delay = None;
            os.panning = false;
            os.camera_target = None;
            os.zoom_target = None;
            os.zoom_animation_anchor = None;
        }
        self.held_action = None;
        self.cursor.grab_cursor = false;
        // Pick mode makes decoration_cursor true over whole window bodies, so
        // locking while hovering a pick target would leave it set through the
        // lock and until the next motion after unlock.
        self.cursor.decoration_cursor = false;
        // Withheld touch events must never reach an app beneath the lock
        // surface — their deadline timer would otherwise replay them mid-lock.
        self.discard_touch_holdback();
        // Lock may swallow key releases and prevents focus history updates while
        // mid-cycle; reset these so none survive the locked window.
        self.stage.cancel_cycle();
        self.suppressed_keys.clear();
        self.tap.reset();
        if let Some(pending) = self.pending_middle_click.take() {
            self.loop_handle.remove(pending.timer_token);
        }
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let pointer = self.seat.get_pointer().unwrap();
        pointer.unset_grab(self, serial, 0);

        // Deactivate any pointer constraint held on the current focus surface.
        // Without this, a Wine game (or any client with a Locked constraint)
        // keeps the cursor pinned through unlock — the lock screen can't move.
        if let Some(focus) = pointer.current_focus() {
            smithay::wayland::pointer_constraints::with_pointer_constraint(
                &focus.0,
                &pointer,
                |c| {
                    if let Some(c) = c
                        && c.is_active()
                    {
                        c.deactivate();
                    }
                },
            );
        }

        self.cursor.exec_cursor_show_at = None;
        self.cursor.exec_cursor_deadline = None;
        self.cursor.cursor_status = smithay::input::pointer::CursorImageStatus::default_named();
        // Clear keyboard focus — no window should be interactable
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        self.set_keyboard_focus(None, serial);
        self.mark_all_dirty();
    }

    fn unlock(&mut self) {
        tracing::info!("Session unlocked");
        self.session_lock = SessionLock::Unlocked;
        self.lock_surfaces.clear();
        // Restore focus to the window (or layer) that owned it before locking.
        self.update_keyboard_focus(smithay::utils::SERIAL_COUNTER.next_serial());
        self.mark_all_dirty();
    }

    fn new_surface(&mut self, surface: LockSurface, wl_output: WlOutput) {
        let output =
            smithay::output::Output::from_resource(&wl_output).or_else(|| self.active_output());
        let Some(output) = output else { return };

        let output_size = crate::state::output_logical_size(&output);

        surface.with_pending_state(|state| {
            state.size = Some((output_size.w as u32, output_size.h as u32).into());
        });
        surface.send_configure();
        self.lock_surfaces.insert(output, surface);
    }
}

use driftwm::protocols::gamma_control::{GammaControlHandler, GammaControlManagerState};

impl GammaControlHandler for DriftWm {
    fn gamma_control_manager_state(&mut self) -> &mut GammaControlManagerState {
        &mut self.gamma_control_manager_state
    }

    fn get_gamma_size(&mut self, output: &smithay::output::Output) -> Option<u32> {
        self.udev_device.as_ref()?.get_gamma_size(output)
    }

    fn set_gamma(
        &mut self,
        output: &smithay::output::Output,
        ramp: Option<Vec<u16>>,
    ) -> Option<()> {
        self.udev_device.as_ref()?.set_gamma(output, ramp)
    }
}

driftwm::delegate_gamma_control!(DriftWm);

delegate_session_lock!(DriftWm);
