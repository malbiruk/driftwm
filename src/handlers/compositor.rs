use std::cell::RefCell;

use crate::grabs::{ResizeState, has_left, has_top};
use crate::handlers::layer_shell::LayerDestroyedMarker;
use crate::state::{ClientState, DriftWm, FocusTarget, PendingRecenter, PinnedState};
use driftwm::window_ext::WindowExt;
use smithay::desktop::layer_map_for_output;
use smithay::utils::{Logical, Point, Rectangle};
use smithay::wayland::shell::wlr_layer::{Anchor, LayerSurfaceCachedState, LayerSurfaceData};
use smithay::{
    delegate_compositor, delegate_shm,
    reexports::{
        calloop::Interest,
        wayland_server::{Client, Resource, protocol::wl_buffer::WlBuffer},
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            RectangleKind, SurfaceAttributes, add_blocker, add_pre_commit_hook, get_parent,
            is_sync_subsurface, with_states,
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

    fn destroyed(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        // Safety net for crash path — toplevel_destroyed handles normal xdg
        // shutdown, but a client crash destroys wl_surface without it.
        self.cleanup_surface_state(surface);
        // lock_surfaces is keyed by output — sweep values.
        self.lock_surfaces
            .retain(|_, ls| ls.wl_surface() != surface);
        self.focus_history
            .retain(|w| w.wl_surface().as_deref() != Some(surface));
        if self.cycle_state.is_some() {
            if self.focus_history.is_empty() {
                self.cycle_state = None;
            } else if let Some(ref mut idx) = self.cycle_state {
                *idx = (*idx).min(self.focus_history.len() - 1);
            }
        }
    }

    fn new_surface(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        // Registered before get_layer_surface installs smithay's validation
        // hook, so this fires first. For destroyed layer surfaces, sets full
        // anchors so size validation passes on the orphaned final commit.
        add_pre_commit_hook::<DriftWm, _>(surface, |_state, _dh, surface| {
            with_states(surface, |states| {
                if states
                    .data_map
                    .get::<LayerDestroyedMarker>()
                    .is_some_and(|m| m.0.load(std::sync::atomic::Ordering::Relaxed))
                {
                    let mut guard = states.cached_state.get::<LayerSurfaceCachedState>();
                    guard.pending().anchor =
                        Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT;
                }
            });
        });

        // DMA-BUF readiness blocker. Must inspect the *pending* buffer here
        // (not in commit()) so the blocker delays the commit it belongs to —
        // by commit() time pending has already merged into current.
        add_pre_commit_hook::<DriftWm, _>(surface, |state, _dh, surface| {
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
            let Some(dmabuf) = maybe_dmabuf else { return };
            let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ) else {
                return;
            };
            let Some(client) = surface.client() else {
                return;
            };
            let inserted = state
                .loop_handle
                .insert_source(source, move |_, _, data: &mut DriftWm| {
                    if let Some(client_state) = client.get_data::<ClientState>() {
                        let dh = data.display_handle.clone();
                        client_state.compositor_state.blocker_cleared(data, &dh);
                    }
                    Ok(())
                })
                .is_ok();
            if inserted {
                add_blocker(surface, blocker);
            }
        });
    }

    fn commit(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        self.commits_since_render = self.commits_since_render.wrapping_add(1);

        // Per-surface damage; global dirty would force every CRTC to redraw
        // on every commit and defeat the per-output damage tracker.
        self.mark_dirty_for_surface(surface);

        // Trim corner rects from CSD toplevels' opaque regions so the
        // background can render through. Some CSD apps (LibreOffice/GTK3)
        // declare the full rect opaque while rendering transparent corners,
        // leaving black artifacts where damage tracking skips redraws.
        // ARGB only — XRGB is handled in RoundedCornerElement::opaque_regions.
        // Skipped for `decoration = "none"` (pass-through promise).
        let csd_corner_carve = !self.decorations.contains_key(&surface.id()) && {
            let applied = driftwm::config::applied_rule(surface);
            let mode = driftwm::config::effective_decoration_mode(
                applied.as_ref().and_then(|r| r.decoration.as_ref()),
                &self.config.decorations.default_mode,
            );
            !matches!(mode, driftwm::config::DecorationMode::None)
        };
        if csd_corner_carve {
            with_states(surface, |states| {
                if states.data_map.get::<XdgToplevelSurfaceData>().is_none() {
                    return;
                }
                let mut guard = states.cached_state.get::<SurfaceAttributes>();
                let attrs = guard.current();
                if let Some(ref mut region) = attrs.opaque_region {
                    let Some(bounds) = region
                        .rects
                        .iter()
                        .filter(|(k, _)| matches!(k, RectangleKind::Add))
                        .map(|(_, r)| *r)
                        .reduce(|a, b| a.merge(b))
                    else {
                        return;
                    };
                    let r = self.config.decorations.corner_radius + 2;
                    if bounds.size.w > 2 * r && bounds.size.h > 2 * r {
                        let (x, y, w, h) =
                            (bounds.loc.x, bounds.loc.y, bounds.size.w, bounds.size.h);
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

        // Without this, bbox_from_surface_tree returns 0x0.
        smithay::backend::renderer::utils::on_commit_buffer_handler::<DriftWm>(surface);
        if let Some(backend) = self.backend.as_mut() {
            backend.early_import(surface);
        }

        // Accumulate `wl_surface.attach` offset onto the DnD icon so it
        // stays anchored to the client's grab point.
        if matches!(&self.dnd_icon, Some(icon) if &icon.surface == surface) {
            let dnd_icon = self.dnd_icon.as_mut().unwrap();
            with_states(&dnd_icon.surface, |states| {
                let buffer_delta = states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .buffer_delta
                    .take()
                    .unwrap_or_default();
                dnd_icon.offset += buffer_delta;
            });
        }

        // Confirm session lock on the lock surface's first buffer commit.
        if let crate::state::SessionLock::Pending(_) = &self.session_lock {
            let is_lock_surface = self
                .lock_surfaces
                .values()
                .any(|ls| ls.wl_surface() == surface);
            if is_lock_surface {
                // locker.lock() consumes — take it out of the enum.
                let old =
                    std::mem::replace(&mut self.session_lock, crate::state::SessionLock::Locked);
                if let crate::state::SessionLock::Pending(locker) = old {
                    locker.lock();
                    tracing::info!("Session lock confirmed");
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    self.set_keyboard_focus(Some(FocusTarget(surface.clone())), serial);
                }
                return;
            }
        }

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

                if self.pending_center.remove(&root) {
                    let geo = window.geometry();
                    let has_size = geo.size.w > 0 && geo.size.h > 0;
                    let is_fullscreen = self.fullscreen.values().any(|fs| fs.window == window);

                    // Capture preferred size once; later updated only on
                    // user resize-grab completion.
                    if has_size && !crate::state::fit::is_fit(&window) && !is_fullscreen {
                        crate::state::fit::set_restore_size_if_missing(&root, geo.size);
                    }

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

                    // Rule side-effects may already have run on a previous
                    // commit (first commit had zero size; retried).
                    let already_applied = with_states(&root, |states| {
                        states
                            .data_map
                            .get::<std::sync::Mutex<driftwm::config::AppliedWindowRule>>()
                            .is_some()
                    });

                    if let Some(ref a) = applied {
                        let stored = a.clone();
                        with_states(&root, |states| {
                            states.data_map.insert_if_missing_threadsafe(|| {
                                std::sync::Mutex::new(stored.clone())
                            });
                            *states
                                .data_map
                                .get::<std::sync::Mutex<driftwm::config::AppliedWindowRule>>()
                                .unwrap()
                                .lock()
                                .unwrap() = stored;
                        });
                    }

                    // Effective decoration mode priority:
                    //   1. Explicit window rule wins.
                    //   2. Otherwise honor xdg-decoration negotiation.
                    //   3. If client never bound xdg-decoration, default_mode.
                    // Resolved before positioning so centering math accounts
                    // for the SSD title bar (decorations map gets populated
                    // later in the same commit).
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
                        // Client accepted what we advertised: keep full
                        // DecorationMode (Minimal / None both map to
                        // ServerSide on the wire and would otherwise be
                        // lost in a round-trip).
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

                    let mut placed_at_cursor = false;
                    let mut place_in_background = false;
                    // One-shot: when a rule forces a size, first commit
                    // arrives at the client's preferred size; configure with
                    // the rule size and defer positioning/decoration/nav to
                    // the follow-up commit. `pending_size` gate prevents
                    // re-forcing later, so the user can still resize.
                    let mut force_pending = false;

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
                            force_pending = true;
                        } else {
                            self.pending_size.remove(&root);
                        }
                    } else if applied.as_ref().is_some_and(|a| a.pinned_to_screen)
                        && has_size
                        && !is_fullscreen
                        && let Some(output) = self.active_output()
                    {
                        // Screen-pinned: live in the active output's screen
                        // space, not the canvas. `position` (if any) is the
                        // output-relative center, Y-up (output center = origin).
                        let (rx, ry) = applied.as_ref().and_then(|a| a.position).unwrap_or((0, 0));
                        let out_size = crate::state::output_logical_size(&output);
                        let internal = driftwm::canvas::rule_to_internal(rx, ry, geo.size);
                        // Clamp the top-left into the output so an off-screen rule
                        // `position` (e.g. [1000, 1000] on a 1080p monitor) still
                        // lands fully visible. Mirrors `reassign_orphaned_pinned`.
                        let screen_pos: Point<i32, Logical> = (
                            (out_size.w / 2 + internal.x)
                                .clamp(0, (out_size.w - geo.size.w).max(0)),
                            (out_size.h / 2 + internal.y)
                                .clamp(0, (out_size.h - geo.size.h).max(0)),
                        )
                            .into();
                        // Seed the Space loc to the canvas point this screen
                        // position currently maps to; the per-frame loc-sync
                        // keeps it correct as the camera moves.
                        let (camera, zoom) = {
                            let os = crate::state::output_state(&output);
                            (os.camera, os.zoom)
                        };
                        let canvas = driftwm::canvas::screen_to_canvas(
                            driftwm::canvas::ScreenPos(screen_pos.to_f64()),
                            camera,
                            zoom,
                        )
                        .0
                        .to_i32_round();
                        let activate = applied.as_ref().is_none_or(|a| !a.widget);
                        self.pinned
                            .insert(root.id(), PinnedState { output, screen_pos });
                        self.space.map_element(window.clone(), canvas, activate);
                    } else if has_size && !is_fullscreen && !crate::state::fit::is_fit(&window) {
                        // Fullscreen / fit windows already sit at their final
                        // location — skip positioning so bar-shifted
                        // centering doesn't override that.
                        let pos = if let Some(ref applied) = applied
                            && let Some((x, y)) = applied.position
                        {
                            let p = driftwm::canvas::rule_to_internal(x, y, geo.size);
                            (p.x, p.y)
                        } else if let Some(parent_surface) = window.parent_surface()
                            && let Some(parent_win) = self.window_for_surface(&parent_surface)
                            && let Some(parent_loc) = self.space.element_location(&parent_win)
                        {
                            let parent_size = parent_win.geometry().size;
                            (
                                parent_loc.x + parent_size.w / 2 - geo.size.w / 2,
                                parent_loc.y + parent_size.h / 2 - geo.size.h / 2,
                            )
                        } else {
                            // Both placement paths need the SSD bar to
                            // center the *visible frame* (titlebar +
                            // content) on the target.
                            let bar_px =
                                if matches!(effective, driftwm::config::DecorationMode::Server) {
                                    self.config.decorations.title_bar_height
                                } else {
                                    0
                                };
                            // Fullscreen takes precedence over the auto/cursor/
                            // center placement handled here: a new window must
                            // never land on top of a fullscreen window on its own
                            // output.
                            let bg_pos = self.fullscreen_background_pos(&window, geo.size, bar_px);
                            place_in_background = bg_pos.is_some();
                            let cursor_pos = if bg_pos.is_none()
                                && matches!(
                                    self.config.window_placement,
                                    driftwm::config::WindowPlacement::Cursor
                                ) {
                                self.cursor_placement_pos(geo.size, bar_px)
                            } else {
                                None
                            };
                            placed_at_cursor = cursor_pos.is_some();
                            let auto_pos = if bg_pos.is_none()
                                && cursor_pos.is_none()
                                && matches!(
                                    self.config.window_placement,
                                    driftwm::config::WindowPlacement::Auto
                                ) {
                                self.auto_placement_pos(&window, geo.size, bar_px)
                            } else {
                                None
                            };
                            let placed = bg_pos.or(cursor_pos).or(auto_pos).unwrap_or_else(|| {
                                let output_geo = self
                                    .active_output()
                                    .and_then(|o| self.space.output_geometry(&o));
                                if output_geo.is_some() {
                                    let bar_f = bar_px as f64;
                                    let vc = self.usable_center_screen();
                                    let cam = self.camera();
                                    let z = self.zoom();
                                    let cx = (cam.x + vc.x / z).round() as i32 - geo.size.w / 2;
                                    let cy = (cam.y + bar_f / 2.0 + vc.y / z).round() as i32
                                        - geo.size.h / 2;
                                    (cx, cy)
                                } else {
                                    (0, 0)
                                }
                            });
                            if place_in_background {
                                // Already anchored to the fullscreen window's
                                // saved home; cascade would only fight that.
                                placed
                            } else {
                                self.cascade_position(placed, &window)
                            }
                        };
                        // Background-placed windows never activate: keep the
                        // fullscreen window focused and on top.
                        let activate =
                            !place_in_background && applied.as_ref().is_none_or(|a| !a.widget);
                        self.space.map_element(window.clone(), pos, activate);
                    }

                    if let Some(toplevel) = window.toplevel() {
                        // Only overwrite wire mode when a rule forces it;
                        // otherwise the client's negotiated choice stands.
                        if rule_explicit.is_some() {
                            let wire = crate::handlers::decoration_mode_to_wire(&effective);
                            toplevel.with_pending_state(|state| {
                                state.decoration_mode = Some(wire);
                            });
                        }

                        // Sync Tiled hint. Skip for widgets (explicit
                        // pos/size) and `None` mode (truly bare); otherwise
                        // Tiled tells GTK et al. to drop their own shadow /
                        // rounded corners since we draw uniform chrome.
                        let skip_tiled = applied.as_ref().is_some_and(|a| a.widget)
                            || matches!(effective, driftwm::config::DecorationMode::None);
                        if skip_tiled {
                            crate::handlers::unset_tiled_states(toplevel);
                        } else {
                            crate::handlers::set_tiled_states(toplevel);
                            // Send size alongside Tiled. SCTK (Alacritty)
                            // reads "Tiled + size=None" as "stay at current
                            // tile size" rather than "pick preferred";
                            // libadwaita can desync geometry from buffer size
                            // across the flip. Skip if already sized to
                            // avoid clobbering a rule-forced size or an
                            // ack'd configure.
                            let already_sized = toplevel.with_pending_state(|s| s.size.is_some());
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

                    // Widget side-effects fire only on first apply.
                    if let Some(ref applied) = applied
                        && !already_applied
                    {
                        if applied.widget {
                            self.enforce_below_windows();
                        }

                        if applied.widget {
                            self.focus_history.retain(|w| w != &window);
                            if let Some(prev) = self.focus_history.first().cloned() {
                                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                                let focus = prev.wl_surface().map(|s| FocusTarget(s.into_owned()));
                                self.set_window_focus(focus, serial);
                            }
                        }
                    }

                    if has_size && !force_pending {
                        // Create the title bar widget BEFORE navigate_to_window
                        // so window_ssd_bar() returns the right height;
                        // otherwise camera target drifts by bar/2.
                        // Minimal gets shadow + corner clip in the render path;
                        // None gets nothing; Client never has a widget.
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
                        // Deferred fit/fullscreen will override camera/zoom/raise
                        // /focus — skip navigate_to_window then. Pinned windows
                        // have no canvas position to navigate the camera to.
                        let deferred_fit_or_fs = self.pending_fit.contains(&root)
                            || self.pending_fullscreen.contains_key(&root);
                        if !is_widget
                            && !is_fullscreen
                            && !place_in_background
                            && !deferred_fit_or_fs
                            && !self.pinned.contains_key(&root.id())
                        {
                            let reset = self.config.zoom_reset_on_new_window;
                            // Cursor mode is "stay put" by default; only
                            // override in the overview-rescue case (user is
                            // zoomed out and asked for reset).
                            let cursor_overview_rescue =
                                placed_at_cursor && reset && self.zoom() < 1.0 - 1e-9;
                            if placed_at_cursor && !cursor_overview_rescue {
                                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                                self.raise_and_focus(&window, serial);
                            } else {
                                self.navigate_to_window(&window, reset);
                            }
                        }

                        // Clear loading cursor on new window arrival.
                        if self.cursor.exec_cursor_deadline.take().is_some() {
                            self.cursor.exec_cursor_show_at = None;
                            self.cursor.cursor_status =
                                smithay::input::pointer::CursorImageStatus::default_named();
                        }
                        self.pending_size.remove(&root);
                        // Snapshot is one-shot; later commits use mapped state.
                        self.auto_anchor_snapshot.remove(&root);
                        // Cache the auto-placed (pre-fit/-fullscreen) rect.
                        // `fit_window_snapped` overwrites with the post-fit
                        // rect; non-snapped fit and fullscreen keep this.
                        self.refresh_stable_snap_rect(&window);

                        if let Some(client_output) = self.pending_fullscreen.remove(&root) {
                            let target = self.resolve_fullscreen_output(&root, client_output);
                            self.enter_fullscreen(&window, target);
                        } else if self.pending_fit.remove(&root) {
                            self.decoration_fit(&window);
                        }
                    } else if !has_size {
                        self.pending_center.insert(root.clone());
                    }
                }

                self.handle_resize_commit(&window, &root);

                // Re-center after unfit once the client has actually shrunk
                // from fit-era geometry — firing earlier would re-center
                // around the big fit size and land off-screen.
                if let Some(&PendingRecenter {
                    target_center,
                    pre_exit_size,
                }) = self.pending_recenter.get(&root.id())
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
                        self.refresh_stable_snap_rect(&window);
                        self.pending_recenter.remove(&root.id());
                    }
                }

                self.reflow_grown_snapped_window(&window, &root);
            }
        }

        if self.handle_canvas_layer_commit(surface) {
            return;
        }

        if self.handle_layer_commit(surface) {
            self.popups.commit(surface);
            return;
        }

        self.popups.commit(surface);

        ensure_initial_configure(surface, self);
    }
}

/// Send the initial configure for an xdg toplevel that hasn't been
/// configured yet, so the client can start rendering.
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
        let initial_configure_sent =
            smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .initial_configure_sent
            });
        if !initial_configure_sent {
            toplevel.send_configure();
        }
    }
}

impl DriftWm {
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
        let Some(idx) = idx else {
            return false;
        };

        // First commit: resolve position once surface size is known.
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

        self.update_keyboard_focus(smithay::utils::SERIAL_COUNTER.next_serial());

        self.popups.commit(surface);
        true
    }

    /// Returns true if the surface belonged to a layer.
    fn handle_layer_commit(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> bool {
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

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

        let mut map = layer_map_for_output(&output);
        map.arrange();

        let initial_configure_sent = with_states(&root, |states| {
            states
                .data_map
                .get::<LayerSurfaceData>()
                .map(|data| data.lock().unwrap().initial_configure_sent)
                .unwrap_or(true)
        });

        let layer_surface = map
            .layer_for_surface(&root, smithay::desktop::WindowSurfaceType::ALL)
            .map(|l| l.layer_surface().clone());

        // Drop the map guard before set_focus reenters SeatHandler.
        drop(map);

        if let Some(layer_surface) = layer_surface {
            if !initial_configure_sent {
                layer_surface.send_configure();
            }
            self.update_keyboard_focus(smithay::utils::SERIAL_COUNTER.next_serial());
        }

        true
    }

    /// Resizing from top/left edges shifts the window position to compensate
    /// for the size change; otherwise the opposite edge would move.
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

        let (edges, initial_window_location, initial_window_size, initial_screen_pos) =
            match resize_state {
                ResizeState::Resizing {
                    edges,
                    initial_window_location,
                    initial_window_size,
                    initial_screen_pos,
                }
                | ResizeState::WaitingForLastCommit {
                    edges,
                    initial_window_location,
                    initial_window_size,
                    initial_screen_pos,
                } => (
                    edges,
                    initial_window_location,
                    initial_window_size,
                    initial_screen_pos,
                ),
                ResizeState::Idle => return,
            };

        let current_geo = window.geometry();

        // Compute from initial position to avoid cumulative drift.
        if let Some(initial_screen_pos) = initial_screen_pos {
            // Pinned: top/left-edge resize moves `screen_pos` so the opposite
            // edge stays fixed. The Space loc is re-synced here directly because
            // the per-frame loc-sync only fires on camera changes.
            let mut new_sp = initial_screen_pos;
            if has_top(edges) {
                new_sp.y = initial_screen_pos.y + (initial_window_size.h - current_geo.size.h);
            }
            if has_left(edges) {
                new_sp.x = initial_screen_pos.x + (initial_window_size.w - current_geo.size.w);
            }
            let output = self.pinned.get_mut(&surface.id()).map(|p| {
                p.screen_pos = new_sp;
                p.output.clone()
            });
            if let Some(output) = output {
                let (camera, zoom) = {
                    let os = crate::state::output_state(&output);
                    (os.camera, os.zoom)
                };
                let canvas = driftwm::canvas::screen_to_canvas(
                    driftwm::canvas::ScreenPos(new_sp.to_f64()),
                    camera,
                    zoom,
                )
                .0
                .to_i32_round();
                self.space.map_element(window.clone(), canvas, false);
            }
        } else {
            let mut new_loc = initial_window_location;
            if has_top(edges) {
                new_loc.y =
                    initial_window_location.y + (initial_window_size.h - current_geo.size.h);
            }
            if has_left(edges) {
                new_loc.x =
                    initial_window_location.x + (initial_window_size.w - current_geo.size.w);
            }
            self.space.map_element(window.clone(), new_loc, false);
        }

        if matches!(resize_state, ResizeState::WaitingForLastCommit { .. }) {
            // Anchor restore_size to the user's final choice so a subsequent
            // fit/fullscreen round-trip restores to this.
            crate::state::fit::set_restore_size(surface, current_geo.size);
            with_states(surface, |states| {
                states
                    .data_map
                    .get_or_insert(|| RefCell::new(ResizeState::Idle))
                    .replace(ResizeState::Idle);
            });
            self.refresh_stable_snap_rect(window);
        }
    }

    /// A snapped window that resizes *itself* larger — not via a resize grab —
    /// can grow over its neighbors. The classic case is a game that maps at a
    /// small size then jumps to its full render resolution a frame later. Move
    /// it beside its former cluster so the snap gaps survive, and recenter the
    /// camera if it's the focused window.
    ///
    /// No-ops unless the footprint actually grew into an overlap: shrinks and
    /// grows into free space keep their position. Resize-grab motion (and its
    /// cluster cascade) is owned by `handle_resize_commit`, so this fires only
    /// on `ResizeState::Idle`.
    fn reflow_grown_snapped_window(
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
        if !matches!(resize_state, ResizeState::Idle) {
            return;
        }
        if self.is_window_fullscreen(window) || crate::state::fit::is_fit(window) {
            return;
        }

        let Some(&stable) = self.stable_snap_rects.get(&surface.id()) else {
            return;
        };
        // `snap_rect_for` returns `None` for widgets / pinned / fullscreen, so
        // this also filters those out.
        let Some(current) = self.snap_rect_for(window) else {
            return;
        };

        // Cheap early-out: `commit` runs on every frame, so bail before any
        // cluster math unless the footprint grew past its settled size.
        const EPS: f64 = 1.0;
        let grew = (current.x_high - current.x_low) > (stable.x_high - stable.x_low) + EPS
            || (current.y_high - current.y_low) > (stable.y_high - stable.y_low) + EPS;
        if !grew {
            return;
        }

        let gap = self.config.snap_gap;
        let others: Vec<(smithay::desktop::Window, driftwm::layout::snap::SnapRect)> = self
            .space
            .elements()
            .filter(|w| *w != window)
            .filter_map(|w| self.snap_rect_for(w).map(|r| (w.clone(), r)))
            .collect();

        // Gate on "was snapped", measured from the pre-grow (stable) rect: the
        // grown rect may already overlap a neighbor and no longer read as
        // edge-adjacent. The first such neighbor also anchors re-placement.
        let anchor = others
            .iter()
            .find(|(_, r)| driftwm::layout::cluster::adjacent_side(&stable, r, gap).is_some())
            .map(|(w, _)| w.clone());
        let Some(anchor) = anchor else {
            return;
        };

        // Only reflow when the grow actually collided; growing into free space
        // keeps the window put.
        if !others.iter().any(|(_, r)| current.overlaps(r)) {
            return;
        }

        let content_size = window.geometry().size;
        let bar = self.window_ssd_bar(window);
        let Some((x, y)) = self.place_adjacent_to(&anchor, window, content_size, bar) else {
            return;
        };
        let new_loc = Point::from((x, y));
        if self.space.element_location(window) == Some(new_loc) {
            return;
        }
        self.space.map_element(window.clone(), new_loc, false);
        self.refresh_stable_snap_rect(window);

        // Recenter only when the reflow pushed the focused window (partly) out
        // of view — a large jump (the game landing beside its neighbor) follows
        // the window; an in-view nudge (sidebar toggle, font bump) leaves the
        // camera alone. `0.999` absorbs subpixel rounding at the viewport edge.
        const FULLY_VISIBLE: f64 = 0.999;
        if self.focused_window().as_ref() == Some(window)
            && !self.window_visible_at_least(window, FULLY_VISIBLE)
        {
            self.navigate_to_window(window, false);
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
