//! Compositor state constructor. Wires every smithay protocol state,
//! creates the seat, and initializes all runtime bookkeeping fields.

use smithay::{
    desktop::{PopupManager, Space},
    input::{
        Seat, SeatState,
        keyboard::{ModifiersState, XkbConfig},
    },
    reexports::{
        calloop::{LoopHandle, LoopSignal},
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::DisplayHandle,
    },
    wayland::{
        compositor::CompositorState,
        cursor_shape::CursorShapeManagerState,
        dmabuf::DmabufState,
        fractional_scale::FractionalScaleManagerState,
        idle_inhibit::IdleInhibitManagerState,
        idle_notify::IdleNotifierState,
        input_method::InputMethodManagerState,
        keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState,
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        pointer_gestures::PointerGesturesState,
        presentation::PresentationState,
        relative_pointer::RelativePointerManagerState,
        security_context::SecurityContextState,
        selection::{
            data_device::DataDeviceState, primary_selection::PrimarySelectionState,
            wlr_data_control::DataControlState,
        },
        session_lock::SessionLockManagerState,
        shell::{
            wlr_layer::WlrLayerShellState,
            xdg::{XdgShellState, decoration::XdgDecorationState},
        },
        shm::ShmState,
        single_pixel_buffer::SinglePixelBufferState,
        text_input::TextInputManagerState,
        viewporter::ViewporterState,
        virtual_keyboard::VirtualKeyboardManagerState,
        xdg_activation::XdgActivationState,
        xdg_foreign::XdgForeignState,
    },
};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use driftwm::config::Config;

use super::{
    CursorState, DriftWm, RenderCache, SessionLock, client_is_unrestricted,
};

impl DriftWm {
    pub fn new(
        dh: DisplayHandle,
        loop_handle: LoopHandle<'static, DriftWm>,
        loop_signal: LoopSignal,
    ) -> Self {
        let compositor_state = CompositorState::new_v6::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new_with_capabilities::<Self>(
            &dh,
            [
                xdg_toplevel::WmCapabilities::Fullscreen,
                xdg_toplevel::WmCapabilities::Maximize,
            ],
        );
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);

        let cursor_shape_state = CursorShapeManagerState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(&dh);
        let xdg_activation_state = XdgActivationState::new::<Self>(&dh);
        SinglePixelBufferState::new::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        let data_control_state = DataControlState::new::<Self, _>(
            &dh,
            Some(&primary_selection_state),
            client_is_unrestricted,
        );
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&dh);
        let relative_pointer_state = RelativePointerManagerState::new::<Self>(&dh);
        let _pointer_gestures_state = PointerGesturesState::new::<Self>(&dh);
        let keyboard_shortcuts_inhibit_state = KeyboardShortcutsInhibitState::new::<Self>(&dh);
        TextInputManagerState::new::<Self>(&dh);
        InputMethodManagerState::new::<Self, _>(&dh, client_is_unrestricted);
        let security_context_state =
            SecurityContextState::new::<Self, _>(&dh, client_is_unrestricted);
        let virtual_keyboard_state =
            VirtualKeyboardManagerState::new::<Self, _>(&dh, client_is_unrestricted);
        let idle_inhibit_state = IdleInhibitManagerState::new::<Self>(&dh);
        let idle_notifier_state = IdleNotifierState::new(&dh, loop_handle.clone());
        let presentation_state = PresentationState::new::<Self>(&dh, 1); // CLOCK_MONOTONIC
        let decoration_state = XdgDecorationState::new::<Self>(&dh);
        let layer_shell_state =
            WlrLayerShellState::new_with_filter::<Self, _>(&dh, client_is_unrestricted);
        let foreign_toplevel_state =
            driftwm::protocols::foreign_toplevel::ForeignToplevelManagerState::new::<Self, _>(
                &dh,
                client_is_unrestricted,
            );
        let screencopy_state =
            driftwm::protocols::screencopy::ScreencopyManagerState::new::<Self, _>(
                &dh,
                client_is_unrestricted,
            );
        let image_capture_source_state =
            driftwm::protocols::image_capture_source::ImageCaptureSourceState::new::<Self, _>(
                &dh,
                client_is_unrestricted,
            );
        let image_copy_capture_state =
            driftwm::protocols::image_copy_capture::ImageCopyCaptureState::new::<Self, _>(
                &dh,
                client_is_unrestricted,
            );
        let output_management_state =
            driftwm::protocols::output_management::OutputManagementState::new::<Self, _>(
                &dh,
                client_is_unrestricted,
            );
        let session_lock_manager_state =
            SessionLockManagerState::new::<Self, _>(&dh, client_is_unrestricted);
        let xdg_foreign_state = XdgForeignState::new::<Self>(&dh);
        smithay::wayland::content_type::ContentTypeState::new::<Self>(&dh);
        {
            use smithay::wayland::shell::xdg::dialog::XdgDialogState;
            XdgDialogState::new::<Self>(&dh);
        }

        let config = Config::load();

        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "seat-0");
        let kb = &config.keyboard_layout;
        let xkb = XkbConfig {
            layout: &kb.layout,
            variant: &kb.variant,
            options: if kb.options.is_empty() {
                None
            } else {
                Some(kb.options.clone())
            },
            model: &kb.model,
            ..Default::default()
        };
        let keyboard = seat
            .add_keyboard(xkb, config.repeat_delay, config.repeat_rate)
            .expect("Failed to add keyboard");
        keyboard.set_modifier_state(ModifiersState {
            num_lock: config.num_lock,
            caps_lock: config.caps_lock,
            ..Default::default()
        });
        seat.add_pointer();

        let autostart = config.autostart.clone();
        Self {
            start_time: Instant::now(),
            display_handle: dh,
            loop_handle,
            loop_signal,
            space: Space::default(),
            popups: PopupManager::default(),
            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            seat,
            cursor: CursorState::new(),
            backend: None,
            decorations: HashMap::new(),
            pending_ssd: HashSet::new(),
            render: RenderCache::new(),
            dmabuf_state: DmabufState::new(),
            dmabuf_global: None,
            cursor_shape_state,
            viewporter_state,
            fractional_scale_state,
            xdg_activation_state,
            primary_selection_state,
            data_control_state,
            pointer_constraints_state,
            relative_pointer_state,
            keyboard_shortcuts_inhibit_state,
            virtual_keyboard_state,
            security_context_state,
            idle_inhibit_state,
            idle_notifier_state,
            presentation_state,
            decoration_state,
            layer_shell_state,
            foreign_toplevel_state,
            screencopy_state,
            output_management_state,
            pending_screencopies: Vec::new(),
            image_capture_source_state,
            image_copy_capture_state,
            pending_captures: Vec::new(),
            xdg_foreign_state,
            session_lock_manager_state,
            session_lock: SessionLock::Unlocked,
            lock_surfaces: HashMap::new(),
            pointer_over_layer: false,
            canvas_layers: Vec::new(),
            config,
            pending_center: HashSet::new(),
            pending_size: HashSet::new(),
            auto_anchor_snapshot: HashMap::new(),
            pending_recenter: HashMap::new(),
            focus_history: Vec::new(),
            cycle_state: None,
            held_action: None,
            suppressed_keys: HashSet::new(),
            gesture_state: None,
            pending_middle_click: None,
            momentum_timer: None,
            fullscreen: HashMap::new(),
            session: None,
            input_devices: Vec::new(),
            state_file_cameras: HashMap::new(),
            state_file_last_write: Instant::now(),
            active_layout: String::new(),
            state_file_layout: String::new(),
            state_file_windows: Vec::new(),
            state_file_layer_count: 0,
            autostart,
            active_crtcs: HashSet::new(),
            redraws_needed: HashSet::new(),
            frames_pending: HashSet::new(),
            estimated_vblank_timers: HashMap::new(),
            config_file_mtime: None,
            last_animation_tick: Instant::now(),
            focused_output: None,
            gesture_output: None,
            gesture_exited_fullscreen: None,
            disconnected_outputs: HashSet::new(),
            output_config_dirty: false,
            satellite: None,
            last_titlebar_click: None,
        }
    }
}
