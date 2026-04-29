use std::cell::RefCell;

use crate::grabs::{ResizeState, has_left, has_top};
use crate::handlers::layer_shell::LayerDestroyedMarker;
use crate::state::{ClientState, DriftWm, FocusTarget, PendingRecenter};
use driftwm::window_ext::WindowExt;
use smithay::utils::Rectangle;
use smithay::desktop::layer_map_for_output;
use smithay::wayland::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, LayerSurfaceData, LayerSurfaceCachedState,
};
use smithay::{
    delegate_compositor, delegate_shm,
    reexports::{
        calloop::Interest,
        wayland_server::{Resource, protocol::wl_buffer::WlBuffer, Client},
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            add_blocker, add_pre_commit_hook, get_parent, is_sync_subsurface, with_states,
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            RectangleKind, SurfaceAttributes,
        },
        dmabuf::get_dmabuf,
        seat::WaylandFocus,
        shell::xdg::XdgToplevelSurfaceData,
        shm::{ShmHandler, ShmState},
    },
};

impl CompositorHandler for DriftWm {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("client has no ClientState")
            .compositor_state
    }

    fn new_surface(&mut self, surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface) {
        // Register an early pre-commit hook. Since this runs at surface creation
        // (before get_layer_surface registers smithay's validation hook), it fires
        // first on every commit. For destroyed layer surfaces, it sets full anchors
        // so smithay's size validation passes on the orphaned final commit.
        add_pre_commit_hook::<DriftWm, _>(surface, |_state, _dh, surface| {
            with_states(surface, |states| {
                if states.data_map.get::<LayerDestroyedMarker>().is_some_and(|m| m.0.load(std::sync::atomic::Ordering::Relaxed)) {
                    let mut guard = states.cached_state.get::<LayerSurfaceCachedState>();
                    guard.pending().anchor =
                        Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT;
                }
            });
        });
    }

    fn commit(&mut self, surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface) {
        self.mark_all_dirty();

        // DMA-BUF readiness blocker: if a pending buffer is a dmabuf, add a
        // calloop source that waits for the GPU fence and then unblocks the
        // compositor transaction. Without this, GPU-rendered frames may commit
        // before the buffer is ready.
        let maybe_dmabuf = with_states(surface, |surface_data| {
            surface_data
                .cached_state
                .get::<SurfaceAttributes>()
                .pending()
                .buffer
                .as_ref()
                .and_then(|assignment| match assignment {
                    BufferAssignment::NewBuffer(buffer) => get_dmabuf(buffer).cloned().ok(),
                    _ => None,
                })
        });
        if let Some(dmabuf) = maybe_dmabuf
            && let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ)
            && let Some(client) = surface.client()
        {
            let ok = self
                .loop_handle
                .insert_source(source, move |_, _, data: &mut DriftWm| {
                    if let Some(client_state) = client.get_data::<ClientState>() {
                        let dh = data.display_handle.clone();
                        client_state
                            .compositor_state
                            .blocker_cleared(data, &dh);
                    }
                    Ok(())
                })
                .is_ok();
            if ok {
                add_blocker(surface, blocker);
            }
        }

        // Trim corners from CSD toplevels' opaque regions so the background renders
        // behind rounded corners. Some CSD apps (e.g. LibreOffice/GTK3) declare the
        // full rect as opaque while rendering transparent corner pixels, causing black
        // artifacts where the damage tracker skips background redraws.
        // NOTE: only effective for ARGB buffers — XRGB buffers are handled in
        // RoundedCornerElement::opaque_regions() at render time.
        if !self.decorations.contains_key(&surface.id()) {
            with_states(surface, |states| {
                if states.data_map.get::<XdgToplevelSurfaceData>().is_none() {
                    return;
                }
                let mut guard = states.cached_state.get::<SurfaceAttributes>();
                let attrs = guard.current();
                if let Some(ref mut region) = attrs.opaque_region {
                    let Some(bounds) = region.rects.iter()
                        .filter(|(k, _)| matches!(k, RectangleKind::Add))
                        .map(|(_, r)| *r)
                        .reduce(|a, b| a.merge(b))
                    else { return };
                    let r = self.config.decorations.corner_radius + 2;
                    if bounds.size.w > 2 * r && bounds.size.h > 2 * r {
                        let (x, y, w, h) = (bounds.loc.x, bounds.loc.y, bounds.size.w, bounds.size.h);
                        for corner in [
                            Rectangle::new((x, y).into(), (r, r).into()),
                            Rectangle::new((x + w - r, y).into(), (r, r).into()),
                            Rectangle::new((x + w - r, y + h - r).into(), (r, r).into()),
                            Rectangle::new((x, y + h - r).into(), (r, r).into()),
                        ] {
                            region.rects.push((RectangleKind::Subtract, corner));
                        }
                    }
                }
            });
        }

        // Update renderer surface state (buffer dimensions, surface_view, textures).
        // Without this, bbox_from_surface_tree() can't see any surfaces and returns 0x0.
        smithay::backend::renderer::utils::on_commit_buffer_handler::<DriftWm>(surface);

        // Session lock: confirm lock on first buffer commit from the lock surface
        if let crate::state::SessionLock::Pending(_) = &self.session_lock {
            let is_lock_surface = self
                .lock_surfaces
                .values()
                .any(|ls| ls.wl_surface() == surface);
            if is_lock_surface {
                // Take the locker out of the enum to call lock() (consumes it)
                let old = std::mem::replace(&mut self.session_lock, crate::state::SessionLock::Locked);
                if let crate::state::SessionLock::Pending(locker) = old {
                    locker.lock();
                    tracing::info!("Session lock confirmed");
                    // Give keyboard focus to the lock surface
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    let keyboard = self.seat.get_keyboard().unwrap();
                    keyboard.set_focus(self, Some(FocusTarget(surface.clone())), serial);
                }
                return;
            }
        }

        // For subsurfaces, walk up to root and notify the window
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            let window = self
                .space
                .elements()
                .find(|w| w.wl_surface().as_deref() == Some(&root))
                .cloned();
            if let Some(window) = window {
                window.on_commit();

                // Center window on first commit once size is known
                if self.pending_center.remove(&root) {
                    let geo = window.geometry();
                    let has_size = geo.size.w > 0 && geo.size.h > 0;
                    let is_fullscreen = self.fullscreen.values().any(|fs| fs.window == window);

                    // Capture preferred size once. Later updated only on user
                    // resize-grab completion (see handle_resize_commit).
                    if has_size && !crate::state::fit::is_fit(&window) && !is_fullscreen {
                        crate::state::fit::set_restore_size_if_missing(&root, geo.size);
                    }

                    // Read app_id/title and check window rules
                    let (app_id, title) = with_states(&root, |states| {
                        states
                            .data_map
                            .get::<XdgToplevelSurfaceData>()
                            .and_then(|d| d.lock().ok())
                            .map(|guard| (guard.app_id.clone(), guard.title.clone()))
                            .unwrap_or_default()
                    });

                    let applied = self.config.resolve_window_rules(
                        app_id.as_deref().unwrap_or(""),
                        title.as_deref().unwrap_or(""),
                    );

                    // Check if rule side-effects were already applied on a
                    // previous commit (happens when the first commit had zero
                    // size and we re-inserted into pending_center for retry).
                    let already_applied = with_states(&root, |states| {
                        states.data_map.get::<std::sync::Mutex<driftwm::config::AppliedWindowRule>>().is_some()
                    });

                    if let Some(ref a) = applied {
                        // Store merged applied rule in surface data_map
                        let stored = a.clone();
                        with_states(&root, |states| {
                            states.data_map.insert_if_missing_threadsafe(|| {
                                std::sync::Mutex::new(stored.clone())
                            });
                            *states.data_map.get::<std::sync::Mutex<driftwm::config::AppliedWindowRule>>()
                                .unwrap().lock().unwrap() = stored;
                        });
                    }

                    // Resolve effective decoration mode. Priority:
                    //   1. Explicit window rule wins — we override whatever the
                    //      client negotiated.
                    //   2. Otherwise, honor what xdg-decoration negotiation
                    //      already produced (state.decoration_mode).
                    //   3. If the client never bound xdg-decoration (pending
                    //      mode is None), fall back to default_mode.
                    //
                    // Resolved BEFORE positioning so the centering math can
                    // account for the SSD title bar that will be drawn above
                    // the client content (self.decorations isn't populated
                    // yet — that happens later in this same commit).
                    //
                    // The previous logic unconditionally forced `rule OR default_mode`
                    // onto the wire, which clobbered a client's request_mode(Server)
                    // any time default_mode was Client (the default). That caused
                    // Qt apps to CSD and Alacritty to fall back to SCTK chrome.
                    let rule_explicit = applied
                        .as_ref()
                        .and_then(|a| a.decoration.as_ref())
                        .cloned();

                    let effective = if let Some(ref m) = rule_explicit {
                        m.clone()
                    } else if let Some(toplevel) = window.toplevel() {
                        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
                        let negotiated = toplevel.with_pending_state(|s| s.decoration_mode);
                        let default = &self.config.decorations.default_mode;
                        let default_wire = crate::handlers::decoration_mode_to_wire(default);
                        // If the client accepted what we advertised, keep the
                        // full DecorationMode (preserves Borderless / None,
                        // which both map to ServerSide on the wire and would
                        // otherwise be lost in a round-trip).
                        match negotiated {
                            None => default.clone(),
                            Some(w) if w == default_wire => default.clone(),
                            Some(Mode::ServerSide) => driftwm::config::DecorationMode::Server,
                            Some(Mode::ClientSide) => driftwm::config::DecorationMode::Client,
                            _ => default.clone(),
                        }
                    } else {
                        self.config.decorations.default_mode.clone()
                    };

                    // Force size on first commit: send a configure with the
                    // rule's size, re-insert into pending_center, and wait for
                    // the client to re-render at the new dimensions.
                    if let Some(ref applied) = applied
                        && let Some((w, h)) = applied.size
                        && self.pending_size.insert(root.clone())
                    {
                        if let Some(toplevel) = window.toplevel() {
                            toplevel.with_pending_state(|state| {
                                state.size = Some(smithay::utils::Size::from((w, h)));
                            });
                            toplevel.send_configure();
                            self.pending_center.insert(root.clone());
                        } else {
                            self.pending_size.remove(&root);
                        }
                    } else if has_size && !is_fullscreen {
                        // Skip positioning for fullscreen windows: enter_fullscreen
                        // already mapped them to camera_i32, and the bar-shifted
                        // centering below would override that by bar/2.
                        //
                        // Position: rule coords are window-center with Y-up convention
                        // (positive = above origin). Negate Y for internal canvas coords.
                        let pos = if let Some(ref applied) = applied
                            && let Some((x, y)) = applied.position
                        {
                            (x - geo.size.w / 2, -y - geo.size.h / 2)
                        } else if let Some(parent_surface) = window.parent_surface()
                            && let Some(parent_win) = self.window_for_surface(&parent_surface)
                            && let Some(parent_loc) = self.space.element_location(&parent_win)
                        {
                            // Center child dialog on parent window
                            let parent_size = parent_win.geometry().size;
                            (
                                parent_loc.x + parent_size.w / 2 - geo.size.w / 2,
                                parent_loc.y + parent_size.h / 2 - geo.size.h / 2,
                            )
                        } else {
                            let output_geo = {
                                let output = self.active_output();
                                output.and_then(|o| self.space.output_geometry(&o))
                            };
                            let centered = if output_geo.is_some() {
                                // SSD title bar is drawn above client content, so
                                // shift the client down by bar/2 to center the
                                // visible frame (titlebar + content) on-screen.
                                let bar_px = if matches!(effective, driftwm::config::DecorationMode::Server) {
                                    driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT as f64
                                } else {
                                    0.0
                                };
                                let vc = self.usable_center_screen();
                                let cam = self.camera(); let z = self.zoom();
                                let cx = (cam.x + vc.x / z).round() as i32 - geo.size.w / 2;
                                let cy = (cam.y + bar_px / 2.0 + vc.y / z).round() as i32 - geo.size.h / 2;
                                (cx, cy)
                            } else {
                                (0, 0)
                            };
                            self.cascade_position(centered, &window)
                        };
                        let activate = applied.as_ref().is_none_or(|a| !a.widget);
                        self.space.map_element(window.clone(), pos, activate);
                    }

                    if let Some(toplevel) = window.toplevel() {
                        // Only overwrite the wire mode when a rule is forcing
                        // it — don't undo the client's own negotiated choice.
                        if rule_explicit.is_some() {
                            let wire = crate::handlers::decoration_mode_to_wire(&effective);
                            toplevel.with_pending_state(|state| {
                                state.decoration_mode = Some(wire);
                            });
                        }

                        // Sync Tiled hint to the resolved intent. Skip Tiled when
                        // the user wants the client left alone: widget rules
                        // (explicit position/size) and `None` mode (truly bare).
                        // Otherwise Tiled tells GTK et al. to drop their shadow
                        // and rounded corners since we draw uniform chrome.
                        let skip_tiled = applied.as_ref().is_some_and(|a| a.widget)
                            || matches!(effective, driftwm::config::DecorationMode::None);
                        if skip_tiled {
                            crate::handlers::unset_tiled_states(toplevel);
                        } else {
                            crate::handlers::set_tiled_states(toplevel);
                            // Configure with explicit size alongside Tiled.
                            // SCTK (Alacritty) reads "Tiled + size=None" as
                            // "stay at current tile size" rather than "pick
                            // preferred"; libadwaita can desync its reported
                            // geometry from its buffer size across the same
                            // transition. Anchoring to the current size keeps
                            // both stable through the flip. Only overwrite if
                            // neither pending nor current state already has a
                            // size — avoids clobbering a rule-forced size or
                            // an ack'd configure from an earlier commit.
                            let already_sized =
                                toplevel.with_pending_state(|s| s.size.is_some());
                            if !already_sized {
                                let current_size = geo.size;
                                toplevel.with_pending_state(|state| {
                                    state.size = Some(current_size);
                                });
                            }
                        }

                        toplevel.send_configure();
                    }
                    if effective != driftwm::config::DecorationMode::Client {
                        self.pending_ssd.insert(root.id());
                    }

                    // Widget side-effects: only on first apply
                    if let Some(ref applied) = applied && !already_applied {
                        if applied.widget {
                            self.enforce_below_windows();
                        }

                        if applied.widget {
                            self.focus_history.retain(|w| w != &window);
                            if let Some(prev) = self.focus_history.first().cloned() {
                                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                                let keyboard = self.seat.get_keyboard().unwrap();
                                let focus = prev.wl_surface().map(|s| FocusTarget(s.into_owned()));
                                keyboard.set_focus(self, focus, serial);
                            }
                        }
                    }

                    if has_size {
                        // Create the title bar widget BEFORE navigate_to_window so
                        // window_ssd_bar() returns the correct height. Otherwise
                        // the camera target centers the client body (ignoring the
                        // titlebar drawn above it) and drifts by bar/2.
                        // Borderless still gets shadow + corner clip via the render
                        // path; None gets nothing; Client never has a widget.
                        if effective == driftwm::config::DecorationMode::Server
                            && !self.decorations.contains_key(&root.id())
                        {
                            let deco = crate::decorations::WindowDecoration::new(
                                geo.size.w,
                                true,
                                &self.config.decorations,
                            );
                            self.decorations.insert(root.id(), deco);
                        }

                        let is_widget = applied.as_ref().is_some_and(|a| a.widget);
                        // Skip for fullscreen: window_ssd_bar() returns 25 once
                        // the decoration is in the map (above), so the camera
                        // target drifts by bar/2 and breaks fullscreen alignment.
                        if !is_widget && !is_fullscreen {
                            let reset = self.config.zoom_reset_on_new_window;
                            self.navigate_to_window(&window, reset);
                        }

                        // New window arrived — clear loading cursor
                        if self.cursor.exec_cursor_deadline.take().is_some() {
                            self.cursor.exec_cursor_show_at = None;
                            self.cursor.cursor_status =
                                smithay::input::pointer::CursorImageStatus::default_named();
                        }
                        self.pending_size.remove(&root);
                    } else {
                        // No size yet — retry next commit
                        self.pending_center.insert(root.clone());
                    }
                }

                // During resize, adjust window position for top/left edge drags
                self.handle_resize_commit(&window, &root);

                // Re-center after unfit once the client has actually shrunk
                // from the fit-era geometry. Waiting for the size change
                // avoids firing while the client is still reporting the
                // pre-exit size (which would re-center around the big fit
                // size and place the window far off-screen).
                if let Some(&PendingRecenter { target_center, pre_exit_size }) =
                    self.pending_recenter.get(&root.id())
                {
                    let geo = window.geometry();
                    if geo.size.w > 0 && geo.size.h > 0 && geo.size != pre_exit_size {
                        let bar = self.window_ssd_bar(&window);
                        let total_h = geo.size.h + bar;
                        let new_loc = smithay::utils::Point::from((
                            (target_center.x - geo.size.w as f64 / 2.0) as i32,
                            (target_center.y - total_h as f64 / 2.0) as i32 + bar,
                        ));
                        self.space.map_element(window.clone(), new_loc, false);
                        self.pending_recenter.remove(&root.id());
                    }
                }
            }
        }

        // Check if this is a canvas-positioned layer surface
        if self.handle_canvas_layer_commit(surface) {
            return;
        }

        // Check if this is a layer surface commit (or subsurface of one)
        if self.handle_layer_commit(surface) {
            self.popups.commit(surface);
            return;
        }

        // Handle popup commits
        self.popups.commit(surface);

        // Send initial configure for unmapped xdg toplevels
        ensure_initial_configure(surface, self);
    }
}

/// If a surface belongs to an xdg toplevel that hasn't been configured yet,
/// send the initial configure event so the client can start rendering.
fn ensure_initial_configure(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    state: &DriftWm,
) {
    if let Some(window) = state
        .space
        .elements()
        .find(|w| w.wl_surface().as_deref() == Some(surface))
    {
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        let initial_configure_sent = smithay::wayland::compositor::with_states(
            toplevel.wl_surface(),
            |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .initial_configure_sent
            },
        );
        if !initial_configure_sent {
            toplevel.send_configure();
        }
    }
}

impl DriftWm {
    /// Give keyboard focus to a layer surface if it doesn't already have it.
    fn focus_exclusive_layer(&mut self, surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface) {
        let keyboard = self.seat.get_keyboard().unwrap();
        let already_focused = keyboard
            .current_focus()
            .as_ref()
            .is_some_and(|f| f.0 == *surface);
        if !already_focused {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, Some(FocusTarget(surface.clone())), serial);
        }
    }

    /// Handle a commit for a canvas-positioned layer surface (or subsurface of one).
    /// Returns true if the surface belonged to a canvas layer.
    fn handle_canvas_layer_commit(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> bool {
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        let idx = self
            .canvas_layers
            .iter()
            .position(|cl| cl.surface.wl_surface() == &root);
        let Some(idx) = idx else { return false; };

        // Resolve position on first commit (once surface size is known)
        if self.canvas_layers[idx].position.is_none() {
            let geo = self.canvas_layers[idx].surface.bbox();
            if geo.size.w > 0 && geo.size.h > 0 {
                let (rx, ry) = self.canvas_layers[idx].rule_position;
                self.canvas_layers[idx].position = Some(smithay::utils::Point::from((
                    rx - geo.size.w / 2,
                    -ry - geo.size.h / 2,
                )));
            }
        }

        // Keyboard interactivity (same logic as handle_layer_commit)
        let interactivity = self.canvas_layers[idx]
            .surface
            .cached_state()
            .keyboard_interactivity;

        let initial_configure_sent = with_states(&root, |states| {
            states
                .data_map
                .get::<LayerSurfaceData>()
                .map(|data| data.lock().unwrap().initial_configure_sent)
                .unwrap_or(true)
        });

        if !initial_configure_sent {
            self.canvas_layers[idx]
                .surface
                .layer_surface()
                .send_configure();
        }

        if interactivity == KeyboardInteractivity::Exclusive {
            self.focus_exclusive_layer(&root);
        }

        self.popups.commit(surface);
        true
    }

    /// Handle a commit for a layer surface (or subsurface of one).
    /// Returns true if the surface belonged to a layer, false otherwise.
    fn handle_layer_commit(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> bool {
        // Walk up from surface to find root
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        // Check if the root surface belongs to any output's layer map
        let output = self.space.outputs().cloned().collect::<Vec<_>>();
        let mut found_output = None;
        for o in &output {
            let map = layer_map_for_output(o);
            if map
                .layer_for_surface(&root, smithay::desktop::WindowSurfaceType::ALL)
                .is_some()
            {
                found_output = Some(o.clone());
                break;
            }
        }

        let Some(output) = found_output else {
            return false;
        };

        // Re-arrange layer surfaces and collect state in a single lookup
        let mut map = layer_map_for_output(&output);
        map.arrange();

        let initial_configure_sent = with_states(&root, |states| {
            states
                .data_map
                .get::<LayerSurfaceData>()
                .map(|data| data.lock().unwrap().initial_configure_sent)
                .unwrap_or(true)
        });

        let layer_info = map
            .layer_for_surface(&root, smithay::desktop::WindowSurfaceType::ALL)
            .map(|l| {
                let interactivity = l.cached_state().keyboard_interactivity;
                let layer_surface = l.layer_surface().clone();
                (interactivity, layer_surface)
            });

        // Must drop the map guard before calling set_focus (which calls into SeatHandler)
        drop(map);

        if let Some((interactivity, layer_surface)) = layer_info {
            if !initial_configure_sent {
                layer_surface.send_configure();
            }

            if interactivity == KeyboardInteractivity::Exclusive {
                self.focus_exclusive_layer(&root);
            }
        }

        true
    }

    /// When resizing from top or left edges, the window position must shift
    /// to compensate for the size change — otherwise the opposite edge moves.
    fn handle_resize_commit(
        &mut self,
        window: &smithay::desktop::Window,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        let resize_state = with_states(surface, |states| {
            *states
                .data_map
                .get_or_insert(|| RefCell::new(ResizeState::Idle))
                .borrow()
        });

        let (edges, initial_window_location, initial_window_size) = match resize_state {
            ResizeState::Resizing { edges, initial_window_location, initial_window_size }
            | ResizeState::WaitingForLastCommit { edges, initial_window_location, initial_window_size } => {
                (edges, initial_window_location, initial_window_size)
            }
            ResizeState::Idle => return,
        };

        let current_geo = window.geometry();
        let mut new_loc = initial_window_location;

        // Compute position absolutely from initial location to avoid cumulative drift
        if has_top(edges) {
            new_loc.y = initial_window_location.y + (initial_window_size.h - current_geo.size.h);
        }
        if has_left(edges) {
            new_loc.x = initial_window_location.x + (initial_window_size.w - current_geo.size.w);
        }

        self.space.map_element(window.clone(), new_loc, false);

        // If we're waiting for the final commit, go idle
        if matches!(resize_state, ResizeState::WaitingForLastCommit { .. }) {
            // User finished resizing — anchor the restore size to their choice
            // so a subsequent fit/fullscreen round-trip restores to this.
            crate::state::fit::set_restore_size(surface, current_geo.size);
            with_states(surface, |states| {
                states
                    .data_map
                    .get_or_insert(|| RefCell::new(ResizeState::Idle))
                    .replace(ResizeState::Idle);
            });
        }
    }
}

impl BufferHandler for DriftWm {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for DriftWm {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

delegate_compositor!(DriftWm);
delegate_shm!(DriftWm);

impl smithay::wayland::drm_syncobj::DrmSyncobjHandler for DriftWm {
    fn drm_syncobj_state(&mut self) -> Option<&mut smithay::wayland::drm_syncobj::DrmSyncobjState> {
        self.drm_syncobj_state.as_mut()
    }
}
smithay::delegate_drm_syncobj!(DriftWm);
