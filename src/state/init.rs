//! Compositor state constructor. Wires every smithay protocol state,
//! creates the seat, and initializes all runtime bookkeeping fields.

use smithay::{
    desktop::{PopupManager, Space},
    input::{
        Seat, SeatState,
        keyboard::{ModifiersState, XkbConfig},
    },
    reexports::{
        calloop::{LoopHandle, LoopSignal, ping::make_ping},
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
            data_device::DataDeviceState,
            ext_data_control::DataControlState as ExtDataControlState,
            primary_selection::PrimarySelectionState, wlr_data_control::DataControlState,
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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use super::{
    CursorState, DriftWm, ErrorSource, RenderCache, SessionLock, TapTracker, client_is_unrestricted,
};

impl DriftWm {
    pub fn new(
        dh: DisplayHandle,
        loop_handle: LoopHandle<'static, DriftWm>,
        loop_signal: LoopSignal,
    ) -> Self {
        Self::new_with_config(
            dh,
            loop_handle,
            loop_signal,
            driftwm::config::Config::load_collect(),
        )
    }

    /// Construct with an explicit config tuple instead of reading the user's
    /// `config.toml`. The seam the in-process test harness builds on: it feeds a
    /// parsed config so a live session's real config file never touches a test.
    pub fn new_with_config(
        dh: DisplayHandle,
        loop_handle: LoopHandle<'static, DriftWm>,
        loop_signal: LoopSignal,
        config: (driftwm::config::Config, Vec<String>),
    ) -> Self {
        // Scan system fonts off-thread so the first SSD title bar doesn't block
        // the event loop on a cold ~1s `FontSystem::new()`. The scan pings the
        // loop on completion to mark outputs dirty: udev renders are VBlank-gated,
        // so a bar drawn textless during the scan would otherwise stay blank
        // until some unrelated frame.
        let (font_ping, font_ping_source) = make_ping().expect("create font-ready ping");
        loop_handle
            .insert_source(font_ping_source, |_, _, data: &mut DriftWm| {
                data.mark_all_dirty();
            })
            .expect("insert font-ready ping source");
        driftwm::text::warm_fonts(move || font_ping.ping());

        let compositor_state = CompositorState::new_v6::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new_with_capabilities::<Self>(
            &dh,
            [
                xdg_toplevel::WmCapabilities::Fullscreen,
                xdg_toplevel::WmCapabilities::Maximize,
            ],
        );
        // wl_shm advertises Argb8888 + Xrgb8888 unconditionally; extra formats
        // here are needed by smithay-client-toolkit-based clients (xdph-cosmic,
        // sctk apps) that allocate buffers in renderer-native layouts.
        let shm_state = ShmState::new::<Self>(
            &dh,
            vec![
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Abgr8888,
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Xbgr8888,
            ],
        );
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
        let ext_data_control_state = ExtDataControlState::new::<Self, _>(
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
        let foreign_toplevel_list_state =
            smithay::wayland::foreign_toplevel_list::ForeignToplevelListState::new_with_filter::<
                Self,
            >(&dh, client_is_unrestricted);
        let ext_workspace_state = driftwm::protocols::ext_workspace::ExtWorkspaceManagerState::new::<
            Self,
            _,
        >(&dh, client_is_unrestricted);
        let screencopy_state = driftwm::protocols::screencopy::ScreencopyManagerState::new::<Self, _>(
            &dh,
            client_is_unrestricted,
        );
        let image_capture_source_state =
            smithay::wayland::image_capture_source::ImageCaptureSourceState::new();
        let output_capture_source_state =
            smithay::wayland::image_capture_source::OutputCaptureSourceState::new_with_filter::<
                Self,
                _,
            >(&dh, client_is_unrestricted);
        let toplevel_capture_source_state =
            smithay::wayland::image_capture_source::ToplevelCaptureSourceState::new_with_filter::<
                Self,
                _,
            >(&dh, client_is_unrestricted);
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
        let output_power_state = driftwm::protocols::output_power::OutputPowerState::new::<Self, _>(
            &dh,
            client_is_unrestricted,
        );
        let session_lock_manager_state =
            SessionLockManagerState::new::<Self, _>(&dh, client_is_unrestricted);
        let gamma_control_manager_state =
            driftwm::protocols::gamma_control::GammaControlManagerState::new::<Self, _>(
                &dh,
                client_is_unrestricted,
            );
        let xdg_foreign_state = XdgForeignState::new::<Self>(&dh);
        smithay::wayland::content_type::ContentTypeState::new::<Self>(&dh);
        {
            use smithay::wayland::shell::xdg::dialog::XdgDialogState;
            XdgDialogState::new::<Self>(&dh);
        }
        let background_effect_state =
            smithay::wayland::background_effect::BackgroundEffectState::new::<Self>(&dh);

        let (mut config, config_errors) = config;
        let mut init_errors: BTreeMap<ErrorSource, String> = BTreeMap::new();
        if let Some(msg) = super::errors::summarize_config_errors(&config_errors) {
            init_errors.insert(ErrorSource::Config, msg);
        }

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
        let keyboard = match seat.add_keyboard(xkb, config.repeat_delay, config.repeat_rate) {
            Ok(keyboard) => keyboard,
            Err(err) => {
                tracing::error!(
                    "Invalid keyboard config (layout={:?} variant={:?} options={:?} model={:?}), \
                     falling back to default: {err:?}",
                    kb.layout,
                    kb.variant,
                    kb.options,
                    kb.model,
                );
                init_errors.insert(
                    ErrorSource::Keyboard,
                    "keyboard: invalid layout config — using default (us)".into(),
                );
                // Pin the stored config to the keymap we actually loaded, so
                // index-by-group readers (`layout --short`) can't report a
                // rejected code the keymap never compiled.
                config.keyboard_layout = driftwm::config::KeyboardLayout {
                    layout: "us".into(),
                    variant: String::new(),
                    options: String::new(),
                    model: String::new(),
                };
                // Explicit "us" rather than XkbConfig::default(): an empty layout
                // defers to XKB_DEFAULT_LAYOUT, which could be the same garbage we
                // just rejected.
                let fallback = XkbConfig {
                    layout: "us",
                    ..Default::default()
                };
                seat.add_keyboard(fallback, config.repeat_delay, config.repeat_rate)
                    .expect("default keyboard layout 'us' failed to compile")
            }
        };
        keyboard.set_modifier_state(ModifiersState {
            num_lock: config.num_lock,
            caps_lock: config.caps_lock,
            ..Default::default()
        });
        seat.add_pointer();
        seat.add_touch();

        let autostart = config.autostart.clone();
        let edge_pan_cursor = config.edge_pan_cursor;
        let bookmarks = config.navigation_bookmarks.clone();
        Self {
            start_time: Instant::now(),
            display_handle: dh,
            loop_handle,
            loop_signal,
            stage: driftwm::stage::Stage::new(),
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
            dnd_icon: None,
            backend: None,
            ipc_server: None,
            ipc_subscribers: Vec::new(),
            ipc_last_event_hash: None,
            decorations: HashMap::new(),
            pending_ssd: HashSet::new(),
            decoration_scale: 1,
            render: RenderCache::new(),
            dmabuf_state: DmabufState::new(),
            dmabuf_global: None,
            render_device: None,
            render_dmabuf_formats: None,
            cursor_shape_state,
            viewporter_state,
            fractional_scale_state,
            xdg_activation_state,
            primary_selection_state,
            data_control_state,
            ext_data_control_state,
            pointer_constraints_state,
            relative_pointer_state,
            keyboard_shortcuts_inhibit_state,
            virtual_keyboard_state,
            virtual_kb_bindings: Default::default(),
            security_context_state,
            idle_inhibit_state,
            idle_inhibiting_surfaces: HashSet::new(),
            idle_notifier_state,
            presentation_state,
            decoration_state,
            layer_shell_state,
            foreign_toplevel_state,
            foreign_toplevel_list_state,
            ext_workspace_state,
            screencopy_state,
            output_management_state,
            output_power_state,
            dpms_off_outputs: HashSet::new(),
            pending_dpms: HashMap::new(),
            pending_screencopies: Vec::new(),
            image_capture_source_state,
            output_capture_source_state,
            toplevel_capture_source_state,
            image_copy_capture_state,
            pending_captures: Vec::new(),
            xdg_foreign_state,
            background_effect_state,
            session_lock_manager_state,
            gamma_control_manager_state,
            session_lock: SessionLock::Unlocked,
            lock_surfaces: HashMap::new(),
            pointer_over_layer: false,
            pointer_over_screen_space: false,
            canvas_layers: Vec::new(),
            config,
            pending_center: HashSet::new(),
            pending_size: HashSet::new(),
            pending_fit: HashSet::new(),
            pending_fullscreen: HashMap::new(),
            auto_anchor_snapshot: HashMap::new(),
            pending_recenter: HashMap::new(),
            stable_snap_rects: HashMap::new(),
            suspend_marks: HashMap::new(),
            real_close_marks: HashMap::new(),
            unmap_snapshots: HashMap::new(),
            desktop_entry_cache: None,
            next_suspended_id: 0,
            pending_relaunches: BTreeMap::new(),
            pending_adoptions: HashMap::new(),
            session_store: super::SessionStore::default(),
            bookmarks,
            window_focus: None,
            on_demand_layer: None,
            popup_grab: None,
            interactive_move: Vec::new(),
            held_action: None,
            wheel_notch_accum: 0.0,
            tap: TapTracker::default(),
            pending_tap_action: None,
            suppressed_keys: HashSet::new(),
            held_buttons: HashSet::new(),
            gesture_state: None,
            pending_middle_click: None,
            momentum_timer: None,
            session: None,
            input_devices: Vec::new(),
            state_file_cameras: HashMap::new(),
            state_file_last_write: Instant::now(),
            active_layout: String::new(),
            state_file_layout: String::new(),
            state_file_windows: Vec::new(),
            state_file_layer_count: 0,
            state_file_pinned: Vec::new(),
            state_file_canvas_layers: Vec::new(),
            state_file_fullscreen: Vec::new(),
            state_file_active_output: None,
            active_bookmark_dirty: false,
            autostart,
            active_outputs: HashSet::new(),
            redraws_needed: HashSet::new(),
            frames_pending: HashSet::new(),
            estimated_vblank_timers: HashMap::new(),
            config_file_mtime: None,
            last_animation_tick: Instant::now(),
            pending_pointer_resync: false,
            commits_since_render: 0,
            focused_output: None,
            gesture_output: None,
            pre_exited_fullscreen: None,
            cycle_navigating: false,
            disconnected_outputs: HashSet::new(),
            output_config_dirty: false,
            pending_mode_changes: HashMap::new(),
            satellite: None,
            udev_device: None,
            last_titlebar_click: None,
            hot_corner_latch: None,
            pending_click_navigate: None,
            click_navigate_timer: None,
            errors: init_errors,
            cursor_edge_pan: edge_pan_cursor,
            touch_state: crate::input::touch::TouchState::new(),
        }
    }
}
